'use strict';
/*
 * The Roles tab's real JavaScript, lifted out of rdadmin.html and run against
 * the DOM shim in ./dom.js, the same way polls.test.js does it.
 *
 * Covers what the page decides on its own: escaping everything a username or
 * role name could smuggle into markup, which tabs an account sees, and that
 * saving an account's roles keeps the roles the editor was not allowed to
 * touch. The server enforces all of this regardless; these pin the page to it.
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

const MARK_START = '/* --------------------------------------------------------- Roles & Permissions */';
const MARK_END = '/* --------------------------------------------------------- Polls */';

const html = fs.readFileSync(PAGE, 'utf8');
const start = html.indexOf(MARK_START);
const end = html.indexOf(MARK_END);

assert.ok(
    start >= 0 && end > start,
    `could not find the Roles block in ${PAGE}. Both marker comments must be ` +
    `present and in order:\n  ${MARK_START}\n  ${MARK_END}`
);

const source = html.slice(start, end);

const TABS = ['users', 'roles', 'status', 'minecraft', 'polls', 'chatprompt'];
const MC_VIEWS = ['console', 'players', 'files', 'archives'];

function buildPage(activeTab = 'users', activeView = 'console') {
    const doc = createDocument();
    const content = doc.createElement('div');
    content.id = 'content';
    doc.body.appendChild(content);
    const strip = doc.createElement('div');
    strip.className = 'tab-container';
    content.appendChild(strip);
    for (const name of TABS) {
        const tab = doc.createElement('div');
        tab.className = name === activeTab ? 'tab active' : 'tab';
        tab.setAttribute('data-tab', name);
        strip.appendChild(tab);
    }

    // The Minecraft Server tab: a shared header, then its own view strip.
    const mc = doc.createElement('div');
    mc.id = 'minecraft-tab';
    mc.className = activeTab === 'minecraft' ? 'tab-content active' : 'tab-content';
    content.appendChild(mc);
    const overview = doc.createElement('div');
    overview.id = 'mcOverview';
    mc.appendChild(overview);
    const views = doc.createElement('div');
    views.className = 'tab-container mc-subtabs';
    mc.appendChild(views);
    for (const name of MC_VIEWS) {
        const tab = doc.createElement('div');
        tab.className = name === activeView ? 'tab active' : 'tab';
        tab.setAttribute('data-mc-tab', name);
        views.appendChild(tab);
    }
    return doc;
}

function setup({ fetchImpl, activeTab, activeView } = {}) {
    const doc = buildPage(activeTab, activeView);
    const calls = { switched: [], usersFetched: 0, overlays: [], requests: [] };
    const ctx = {
        document: doc,
        console: { error() {}, warn() {}, log() {} },
        JSON, Array, Set, Map, Object, String, Number, Math, Promise, Error,
        encodeURIComponent,
        searchUser: null,
        currentPage: 1,
        switchTab: (name) => calls.switched.push(name),
        fetchUsers: () => { calls.usersFetched++; },
        showOverlay: (title, message, buttons = []) => calls.overlays.push({ title, message, buttons }),
        hideOverlay: () => {},
        askConfirm: async () => true,
        fetch: async (url, init = {}) => {
            calls.requests.push({ url, init });
            return fetchImpl(url, init);
        },
    };
    ctx.globalThis = ctx;
    vm.createContext(ctx);
    vm.runInContext(source, ctx);
    return { ctx, doc, calls, run: (code) => vm.runInContext(code, ctx) };
}

const json = (body, status = 200) => ({
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
});

test('escapeHtml neutralises every character that can break out of markup', () => {
    const { run } = setup({ fetchImpl: () => json({}) });
    assert.equal(
        run(`escapeHtml('<img src=x onerror="a()">\\'&')`),
        '&lt;img src=x onerror=&quot;a()&quot;&gt;&#39;&amp;'
    );
    assert.equal(run('escapeHtml(null)'), '');
});

test('a hostile username or role name is escaped in the roles cell', () => {
    const { run } = setup({ fetchImpl: () => json({}) });
    const cell = run(`renderRolesCell({ username: '"><script>x()</script>', roles: ['<b>mod</b>'] })`);
    assert.ok(!cell.includes('<script>'), cell);
    assert.ok(!cell.includes('<b>'), cell);
    assert.ok(cell.includes('&lt;b&gt;mod&lt;/b&gt;'), cell);
    // The roles list rides in an attribute as JSON, so its quotes must be escaped too.
    assert.ok(cell.includes('data-roles="[&quot;&lt;b&gt;mod&lt;/b&gt;&quot;]"'), cell);
});

test('the roles cell falls back to the legacy isAdmin flag and marks admin distinctly', () => {
    const { run } = setup({ fetchImpl: () => json({}) });
    const legacy = run(`renderRolesCell({ username: 'a', isAdmin: true })`);
    assert.ok(legacy.includes('badge-role-super'), legacy);
    assert.ok(legacy.includes('>admin<'), legacy);
    const none = run(`renderRolesCell({ username: 'a', roles: [] })`);
    assert.ok(none.includes('roles-none'), none);
});

test('the Edit button appears only for accounts that can manage roles', async () => {
    const { run } = setup({
        fetchImpl: () => json({ username: 'mod', superuser: false, roles: ['moderator'],
                                permissions: ['panel.access', 'users.view'] }),
    });
    await run('loadAccessMe()');
    assert.ok(!run(`renderRolesCell({ username: 'a', roles: [] })`).includes('editRolesBtn'));
    run(`accessMe = { superuser: false, permissions: ['roles.manage'] }`);
    assert.ok(run(`renderRolesCell({ username: 'a', roles: [] })`).includes('editRolesBtn'));
    run(`accessMe = { superuser: true, permissions: [] }`);
    assert.ok(run(`renderRolesCell({ username: 'a', roles: [] })`).includes('editRolesBtn'));
});

test('tabs the account cannot use are hidden, and a hidden active tab is left', async () => {
    const { doc, calls, run } = setup({
        activeTab: 'minecraft',
        fetchImpl: () => json({ username: 'mod', superuser: false, roles: ['moderator'],
                                permissions: ['panel.access', 'users.view', 'users.moderate'] }),
    });
    await run('loadAccessMe()');

    const hidden = TABS.filter(name =>
        doc.querySelector(`.tab[data-tab="${name}"]`).hidden);
    assert.deepEqual(hidden, ['roles', 'status', 'minecraft', 'polls', 'chatprompt']);
    assert.deepEqual(calls.switched, ['users']);
});

const mcHidden = (doc) => MC_VIEWS.filter(name =>
    doc.querySelector(`[data-mc-tab="${name}"]`).hidden);

/* Files is the one part of the server tab that reads configs off disk, so it
   has its own permission; the console's never opened it. */
test('console.view alone shows the server tab without its Files view', async () => {
    const { doc, calls, run } = setup({
        activeTab: 'minecraft',
        activeView: 'files',
        fetchImpl: () => json({ username: 'ops', superuser: false, roles: ['ops'],
                                permissions: ['panel.access', 'console.view'] }),
    });
    await run('loadAccessMe()');

    assert.equal(doc.querySelector('.tab[data-tab="minecraft"]').hidden, false);
    assert.deepEqual(mcHidden(doc), ['files', 'archives']);
    assert.equal(doc.getElementById('mcOverview').hidden, false);
    // Sitting on Files, which it cannot use, it is moved to the first view it can.
    assert.deepEqual(calls.switched, ['console']);
});

test('console.files alone shows only the Files view, without the console header', async () => {
    const { doc, calls, run } = setup({
        activeTab: 'minecraft',
        activeView: 'console',
        fetchImpl: () => json({ username: 'builder', superuser: false, roles: ['builder'],
                                permissions: ['panel.access', 'console.files'] }),
    });
    await run('loadAccessMe()');

    assert.equal(doc.querySelector('.tab[data-tab="minecraft"]').hidden, false);
    assert.deepEqual(mcHidden(doc), ['console', 'players', 'archives']);
    assert.equal(doc.getElementById('mcOverview').hidden, true);
    assert.deepEqual(calls.switched, ['files']);
});

/* Either season permission opens the tab and its Archives view, and nothing else in it. */
test('a season permission alone shows only the Archives view', async () => {
    for (const permission of ['season.end', 'season.archives']) {
        const { doc, calls, run } = setup({
            activeTab: 'minecraft',
            activeView: 'console',
            fetchImpl: () => json({ username: 'arch', superuser: false, roles: ['arch'],
                                    permissions: ['panel.access', permission] }),
        });
        await run('loadAccessMe()');
        assert.equal(doc.querySelector('.tab[data-tab="minecraft"]').hidden, false, permission);
        assert.deepEqual(mcHidden(doc), ['console', 'players', 'files'], permission);
        assert.deepEqual(calls.switched, ['archives'], permission);
    }
});

/* Re-entering is what closes a console stream opened before access was known. */
test('an account that keeps its view is still re-routed once access is known', async () => {
    const { calls, run } = setup({
        activeTab: 'minecraft',
        activeView: 'players',
        fetchImpl: () => json({ username: 'ops', superuser: false, roles: ['ops'],
                                permissions: ['console.view', 'console.files'] }),
    });
    await run('loadAccessMe()');
    assert.deepEqual(calls.switched, ['players']);
});

test('a superuser sees every tab', async () => {
    const { doc, run } = setup({
        fetchImpl: () => json({ username: 'root', superuser: true, roles: ['admin'], permissions: [] }),
    });
    await run('loadAccessMe()');
    for (const name of TABS) {
        assert.equal(doc.querySelector(`.tab[data-tab="${name}"]`).hidden, false, name);
    }
    assert.deepEqual(mcHidden(doc), []);
});

/* If the page cannot learn what the account holds it must not guess: hiding
   everything would lock an admin out of a working panel over one failed
   request, and the server refuses whatever they lack anyway. */
test('when /api/access/me fails nothing is hidden', async () => {
    const { doc, calls, run } = setup({ fetchImpl: () => json({ error: 'boom' }, 500) });
    await run('loadAccessMe()');
    for (const name of TABS) {
        assert.equal(doc.querySelector(`.tab[data-tab="${name}"]`).hidden, false, name);
    }
    assert.deepEqual(calls.switched, []);
});

test('saving roles keeps a held role the editor could not touch', async () => {
    const roles = [
        { name: 'admin', description: 'Everything', system: true, superuser: true,
          permissions: [], members: 1, manageable: false },
        { name: 'moderator', description: 'Mods', system: false, superuser: false,
          permissions: ['users.view', 'users.moderate'], members: 2, manageable: true },
        { name: 'support', description: '', system: false, superuser: false,
          permissions: ['users.reset_password'], members: 0, manageable: true },
    ];
    const { doc, calls, run } = setup({
        fetchImpl: (url, init) => {
            if (url === '/api/admin/access/roles') return json(roles);
            return json({ message: 'Roles updated' });
        },
    });

    await run(`openRolePicker('alice', ['admin', 'moderator'])`);
    const overlay = calls.overlays.at(-1);
    assert.equal(overlay.title, 'Roles for alice');

    // The locked role the account holds is shown ticked and cannot be unticked.
    assert.match(overlay.message, /value="admin" checked disabled/);
    assert.match(overlay.message, /value="moderator" checked >/);
    assert.match(overlay.message, /value="support"  >/);

    // Stand in for what the browser would build from that markup, after the
    // editor unticks moderator and ticks support.
    const picker = doc.createElement('div');
    picker.id = 'rolePicker';
    doc.body.appendChild(picker);
    for (const [value, checked, disabled] of [
        ['admin', true, true], ['moderator', false, false], ['support', true, false],
    ]) {
        const cb = doc.createElement('input');
        cb.setAttribute('type', 'checkbox');
        cb.value = value;
        cb.checked = checked;
        cb.disabled = disabled;
        picker.appendChild(cb);
    }

    const save = overlay.buttons.find(b => b.text === 'Save roles');
    await save.onClick();

    const put = calls.requests.find(r => r.init.method === 'PUT');
    assert.ok(put, 'a PUT was sent');
    assert.equal(put.url, '/api/admin/access/users/alice/roles');
    assert.deepEqual(JSON.parse(put.init.body), { roles: ['admin', 'support'] });
    assert.equal(calls.usersFetched, 1, 'the list is refreshed after saving');
});

test('a refused save shows the server\'s reason', async () => {
    const { doc, calls, run } = setup({
        fetchImpl: (url) => url === '/api/admin/access/roles'
            ? json([])
            : json({ error: 'You cannot grant the admin role' }, 403),
    });
    await run(`openRolePicker('bob', [])`);
    const picker = doc.createElement('div');
    picker.id = 'rolePicker';
    doc.body.appendChild(picker);
    await calls.overlays.at(-1).buttons.find(b => b.text === 'Save roles').onClick();

    const shown = calls.overlays.at(-1);
    assert.equal(shown.title, 'Could not change roles');
    assert.equal(shown.message, 'You cannot grant the admin role');
});
