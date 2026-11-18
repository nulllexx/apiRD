'use strict';
/*
 * The AI Access tab's real JavaScript, lifted out of rdadmin.html and run
 * against the DOM shim in ./dom.js, the same way chat-prompt.test.js does it.
 *
 * Covers what the page decides on its own: the request it sends for a new
 * token (and the panel header that request needs), that the token is shown
 * once and cleared, the snippets it builds, how token states read, and that
 * nothing a server sends is ever parsed as markup.
 *
 * Run by `cargo test` via tests/panel.rs, or directly:
 *
 *     node --test tests/panel
 */

const { test } = require('node:test');
const assert = require('node:assert');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');

const { createDocument } = require('./dom');

const PAGE = path.join(__dirname, '..', '..', 'private', 'rdadmin.html');

const MARK_START = '/* --------------------------------------------------------- AI access (MCP) */';
const MARK_END = '/* --------------------------------------------------------- Tab Routing Implementation */';

const html = fs.readFileSync(PAGE, 'utf8');
const start = html.indexOf(MARK_START);
const end = html.indexOf(MARK_END);

assert.ok(
    start >= 0 && end > start,
    `could not find the AI access block in ${PAGE}. Both marker comments must be ` +
    `present and in order:\n  ${MARK_START}\n  ${MARK_END}`
);

const source = html.slice(start, end);

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

const IDS = [
    ['div', 'mcp-alert'], ['div', 'mcpOff'], ['div', 'mcpNew'], ['code', 'mcpNewToken'],
    ['button', 'mcpCopyToken'], ['code', 'mcpSnippetCli'], ['button', 'mcpCopyCli'],
    ['pre', 'mcpSnippetJson'], ['button', 'mcpCopyJson'], ['button', 'mcpDone'],
    ['div', 'mcpTokens'], ['div', 'mcpCreateCard'], ['form', 'mcpForm'], ['input', 'mcpName'],
    ['div', 'mcpPermissions'], ['span', 'mcpAlways'], ['select', 'mcpExpiry'],
    ['input', 'mcpPassword'], ['span', 'mcpLimitNote'], ['button', 'mcpSubmit'],
    ['tbody', 'mcpActivityBody'],
];

function setup(routes = {}) {
    const doc = createDocument();
    for (const [tag, id] of IDS) {
        const el = doc.createElement(tag);
        el.id = id;
        doc.body.appendChild(el);
    }
    doc.getElementById('mcpExpiry').value = '30';
    // Hidden in the page's markup until a token is made.
    doc.getElementById('mcpNew').hidden = true;

    const calls = [];
    const ctx = {
        document: doc,
        console: { error() {}, warn() {}, log() {} },
        confirm: () => true,
        setTimeout: () => 0,
        location: { origin: 'https://panel.example' },
        fetch: async (url, init = {}) => {
            calls.push({ url, init });
            const key = `${init.method || 'GET'} ${url}`;
            const route = routes[key];
            if (!route) throw new Error(`unexpected request: ${key}`);
            const { status = 200, body } = typeof route === 'function' ? route(init) : route;
            return { ok: status < 400, status, json: async () => body };
        },
    };
    ctx.globalThis = ctx;
    vm.createContext(ctx);
    vm.runInContext(source, ctx);
    return { doc, ctx, calls, text: id => doc.getElementById(id).textContent };
}

const overview = (over = {}) => Object.assign({
    enabled: true,
    canCreate: true,
    endpoint: '/api/mcp',
    grantable: [
        { permission: 'console.view', description: 'Watch the console', tools: ['read_console_log', 'list_players'] },
        { permission: 'users.view', description: 'Browse accounts', tools: ['list_users'] },
    ],
    alwaysAvailable: ['whoami', 'get_current_season', 'get_site_status'],
    tokens: [],
    limits: { maxTokens: 10, maxDays: 90, defaultDays: 30 },
}, over);

const token = (over = {}) => Object.assign({
    id: 3,
    name: 'Claude Code',
    hint: 'rdmcp_AbCd',
    permissions: ['console.view'],
    effective: ['console.view'],
    createdAt: '2026-11-01T10:00:00.000Z',
    expiresAt: '2026-12-01T10:00:00.000Z',
    lastUsedAt: null,
    status: 'active',
}, over);

const loaded = (data = overview(), entries = []) => ({
    'GET /api/account/mcp': { body: data },
    'GET /api/account/mcp/activity?limit=50': { body: { entries } },
});

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

test('the overview fills the permission list, the token list and the limits', async () => {
    const { doc, ctx, text } = setup(loaded(overview({ tokens: [token()] })));
    await ctx.loadMcp();

    const boxes = doc.querySelectorAll('#mcpPermissions .mcp-perm-box');
    assert.deepEqual(boxes.map(b => b.value), ['console.view', 'users.view']);
    assert.ok(boxes.every(b => !b.checked), 'nothing is ticked for you');
    assert.match(text('mcpPermissions'), /read_console_log, list_players/);
    assert.equal(text('mcpAlways'), 'whoami, get_current_season, get_site_status');
    assert.equal(text('mcpLimitNote'), '1 of 10 tokens');
    assert.equal(doc.getElementById('mcpOff').hidden, true);
    assert.equal(doc.getElementById('mcpSubmit').disabled, false);

    const items = doc.querySelectorAll('#mcpTokens .mcp-token');
    assert.equal(items.length, 1);
    assert.match(items[0].textContent, /Claude Code/);
    assert.match(items[0].textContent, /rdmcp_AbCd…/);
    assert.match(items[0].textContent, /Last used never/);
});

test('when the endpoint is off, the notice shows and nothing can be created', async () => {
    const { doc, ctx } = setup(loaded(overview({ enabled: false })));
    await ctx.loadMcp();
    assert.equal(doc.getElementById('mcpOff').hidden, false);
    assert.equal(doc.getElementById('mcpSubmit').disabled, true);
});

test('an account without mcp.access sees no create form', async () => {
    const { doc, ctx } = setup(loaded(overview({ canCreate: false, grantable: [] })));
    await ctx.loadMcp();
    assert.equal(doc.getElementById('mcpCreateCard').hidden, true);
});

test('at the limit the form says so and will not submit', async () => {
    const tokens = Array.from({ length: 10 }, (_, i) => token({ id: i + 1 }));
    const { doc, ctx, text } = setup(loaded(overview({ tokens })));
    await ctx.loadMcp();
    assert.equal(doc.getElementById('mcpSubmit').disabled, true);
    assert.match(text('mcpLimitNote'), /revoke one first/);
});

test('each token state reads plainly, and lost permissions are marked', async () => {
    const tokens = [
        token({ id: 1, status: 'expired' }),
        token({ id: 2, status: 'signed_out' }),
        token({ id: 3, status: 'no_access' }),
        token({ id: 4, permissions: ['console.view', 'users.view'], effective: ['users.view'] }),
        token({ id: 5, permissions: [], effective: [] }),
    ];
    const { doc, ctx } = setup(loaded(overview({ tokens })));
    await ctx.loadMcp();

    const status = doc.querySelectorAll('#mcpTokens .mcp-status').map(s => s.textContent);
    assert.deepEqual(status.slice(0, 3), [
        'Expired',
        'Ended: password changed or signed out everywhere',
        'Paused: your account no longer has mcp.access',
    ]);
    const inactive = doc.querySelectorAll('#mcpTokens .mcp-perm-inactive').map(b => b.textContent);
    assert.deepEqual(inactive, ['console.view']);
    assert.match(doc.querySelectorAll('#mcpTokens .mcp-token')[4].textContent, /always-available tools only/);
});

test('names and activity from the server are text, never markup', async () => {
    const hostile = '<img src=x onerror=alert(1)>';
    const { doc, ctx } = setup(loaded(
        overview({ tokens: [token({ name: hostile })] }),
        [{ id: 1, at: '2026-11-02T10:00:00.000Z', tokenId: 3, tokenName: hostile, tool: 'read_console_log',
           arguments: '{"contains":"<b>"}', outcome: 'ok' }]
    ));
    await ctx.loadMcp();

    const all = doc.body._descendants();
    assert.ok(all.every(el => !el._html), 'nothing may be written with innerHTML');
    assert.ok(all.every(el => el.tagName !== 'IMG'));
    assert.match(doc.getElementById('mcpTokens').textContent, /<img src=x/);
    assert.match(doc.getElementById('mcpActivityBody').textContent, /<img src=x/);
});

test('an empty activity log says so', async () => {
    const { ctx, text } = setup(loaded());
    await ctx.loadMcp();
    assert.equal(text('mcpActivityBody'), 'Nothing yet.');
});

// ---------------------------------------------------------------------------
// Creating
// ---------------------------------------------------------------------------

test('creating sends exactly the ticked permissions, the expiry and the password, with the panel header', async () => {
    const made = Object.assign(token(), { token: 'rdmcp_' + 'A'.repeat(43) });
    let sent = null;
    const routes = Object.assign(loaded(), {
        'POST /api/account/mcp/tokens': init => { sent = init; return { status: 201, body: made }; },
    });
    const { doc, ctx, text } = setup(routes);
    await ctx.loadMcp();

    doc.getElementById('mcpName').value = '  Claude Code  ';
    doc.querySelectorAll('#mcpPermissions .mcp-perm-box')[0].checked = true;
    doc.getElementById('mcpExpiry').value = '7';
    doc.getElementById('mcpPassword').value = 'hunter2';
    await ctx.createMcpToken({ preventDefault() {} });

    assert.equal(sent.headers['X-RD-Panel'], '1');
    assert.equal(sent.headers['Content-Type'], 'application/json');
    assert.deepEqual(JSON.parse(sent.body), {
        name: 'Claude Code', permissions: ['console.view'], expiresInDays: 7, password: 'hunter2',
    });

    assert.equal(doc.getElementById('mcpPassword').value, '', 'the password is not left in the form');
    assert.equal(doc.getElementById('mcpName').value, '');
    assert.equal(doc.getElementById('mcpNew').hidden, false);
    assert.equal(text('mcpNewToken'), made.token);
    assert.equal(
        text('mcpSnippetCli'),
        `claude mcp add --transport http raindrippy https://panel.example/api/mcp --header "Authorization: Bearer ${made.token}"`
    );
    assert.deepEqual(JSON.parse(text('mcpSnippetJson')), {
        mcpServers: { raindrippy: { type: 'http', url: 'https://panel.example/api/mcp',
                                    headers: { Authorization: `Bearer ${made.token}` } } },
    });

    ctx.hideMcpToken();
    assert.equal(doc.getElementById('mcpNew').hidden, true);
    for (const id of ['mcpNewToken', 'mcpSnippetCli', 'mcpSnippetJson']) {
        assert.equal(text(id), '', `${id} must not keep the token once closed`);
    }
});

test('a blank password is left out, for accounts that sign in with Google only', async () => {
    let sent = null;
    const routes = Object.assign(loaded(), {
        'POST /api/account/mcp/tokens': init => {
            sent = JSON.parse(init.body);
            return { status: 201, body: Object.assign(token(), { token: 'rdmcp_x' }) };
        },
    });
    const { doc, ctx } = setup(routes);
    await ctx.loadMcp();
    doc.getElementById('mcpName').value = 'assistant';
    await ctx.createMcpToken();
    assert.equal('password' in sent, false);
    assert.deepEqual(sent.permissions, []);
});

test('a missing name is caught before anything is sent', async () => {
    const { doc, ctx, calls, text } = setup(loaded());
    await ctx.loadMcp();
    const before = calls.length;
    doc.getElementById('mcpName').value = '   ';
    await ctx.createMcpToken();
    assert.equal(calls.length, before);
    assert.match(text('mcp-alert'), /Give the token a name/);
});

test("the server's refusal is shown as-is and the form can be tried again", async () => {
    const routes = Object.assign(loaded(), {
        'POST /api/account/mcp/tokens': { status: 401, body: { error: 'That password is not right.' } },
    });
    const { doc, ctx, text } = setup(routes);
    await ctx.loadMcp();
    doc.getElementById('mcpName').value = 'assistant';
    doc.getElementById('mcpPassword').value = 'wrong';
    await ctx.createMcpToken();
    assert.match(text('mcp-alert'), /That password is not right\./);
    assert.equal(doc.getElementById('mcpSubmit').disabled, false);
    assert.equal(doc.getElementById('mcpPassword').value, '');
    assert.equal(doc.getElementById('mcpNew').hidden, true, 'no token card for a refusal');
});

// ---------------------------------------------------------------------------
// Revoking
// ---------------------------------------------------------------------------

test('revoking deletes that token, with the panel header, and reloads', async () => {
    let deleted = null;
    let listed = 0;
    const routes = {
        'GET /api/account/mcp': () => { listed += 1; return { body: overview({ tokens: [token({ id: 42 })] }) }; },
        'GET /api/account/mcp/activity?limit=50': { body: { entries: [] } },
        'DELETE /api/account/mcp/tokens/42': init => { deleted = init; return { body: { revoked: 42 } }; },
    };
    const { doc, ctx, text } = setup(routes);
    await ctx.loadMcp();

    doc.querySelector('#mcpTokens .mcp-revoke').dispatch('click');
    await new Promise(r => setImmediate(r));
    await new Promise(r => setImmediate(r));

    assert.equal(deleted.headers['X-RD-Panel'], '1');
    assert.equal(listed, 2, 'the list is reloaded');
    assert.match(text('mcp-alert'), /Revoked "Claude Code"/);
});

test('revoking asks first, and a no sends nothing', async () => {
    const routes = loaded(overview({ tokens: [token()] }));
    const { doc, ctx, calls } = setup(routes);
    ctx.confirm = () => false;
    await ctx.loadMcp();
    const before = calls.length;
    doc.querySelector('#mcpTokens .mcp-revoke').dispatch('click');
    await new Promise(r => setImmediate(r));
    assert.equal(calls.length, before);
});
