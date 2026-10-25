'use strict';
/*
 * The Chat Prompt tab's real JavaScript, lifted out of rdadmin.html and run
 * against the DOM shim in ./dom.js, the same way polls.test.js does it.
 *
 * Covers what the form decides on its own: which button rows get a value
 * field, the 0..4 button bounds, the checks that stop a half-filled prompt
 * from being sent, the exact body that is sent, and the status line.
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

const MARK_START = '/* --------------------------------------------------------- Chat prompt */';
const MARK_END = '/* --------------------------------------------------------- Tab Routing Implementation */';

const html = fs.readFileSync(PAGE, 'utf8');
const start = html.indexOf(MARK_START);
const end = html.indexOf(MARK_END);

assert.ok(
    start >= 0 && end > start,
    `could not find the Chat prompt block in ${PAGE}. Both marker comments must be ` +
    `present and in order:\n  ${MARK_START}\n  ${MARK_END}`
);

const source = html.slice(start, end);

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

function freshContext(doc, fetchImpl) {
    const ctx = {
        document: doc,
        console: { error() {}, warn() {}, log() {} },
        confirm: () => true,
        setTimeout: () => 0,
        clearTimeout: () => {},
        Date, Math, JSON, Set, Map, Array, Object, Number, String, Boolean, isNaN,
        fetch: (...args) => ctx._fetch(...args),
        _fetch: fetchImpl || (() => { throw new Error('no fetch stub set'); }),
    };
    ctx.globalThis = ctx;
    vm.createContext(ctx);
    vm.runInContext(source, ctx);
    return ctx;
}

/** Just the markup the Chat prompt code reaches for, matching rdadmin.html's ids. */
function buildPage() {
    const doc = createDocument();
    const add = (parent, tag, id) => {
        const el = doc.createElement(tag);
        if (id) el.id = id;
        parent.appendChild(el);
        return el;
    };

    add(doc.body, 'div', 'cprompt-alert');
    add(doc.body, 'p', 'cpromptStatus');
    const form = add(doc.body, 'form', 'cpromptForm');
    add(form, 'input', 'cpromptEnabled');
    add(form, 'input', 'cpromptTitle');
    add(form, 'textarea', 'cpromptBody');
    add(form, 'div', 'cpromptButtons');
    add(form, 'button', 'cpromptAddButton');
    add(form, 'p', 'cpromptButtonCount');
    add(form, 'input', 'cpromptShowAgain');
    add(form, 'p', 'cpromptShowAgainNote');
    add(form, 'button', 'cpromptSaveBtn');
    add(doc.body, 'div', 'cpromptPreview');
    return doc;
}

function setup(fetchImpl) {
    const doc = buildPage();
    const ctx = freshContext(doc, fetchImpl);
    return {
        doc,
        ctx,
        alertText: () => doc.getElementById('cprompt-alert').textContent,
        rows: () => doc.querySelectorAll('#cpromptButtons .cprompt-btn-row'),
    };
}

/** What GET/PUT /api/admin/chat-prompt answer with. */
const saved = (over = {}) => ({
    prompt: Object.assign({
        enabled: true,
        title: 'Welcome',
        body: 'Say hi.',
        buttons: [
            { label: 'Rules', action: 'link', value: 'https://example.com/rules', primary: false },
            { label: 'Got it', action: 'close', primary: true },
        ],
        version: 2,
        updatedAt: '2026-10-25T12:00:00.000Z',
        updatedBy: 'bako',
    }, over),
    seen: 1,
    chatUsers: 1,
});

const ok = body => async () => ({ ok: true, status: 200, json: async () => body });

// ---------------------------------------------------------------------------
// Filling the form
// ---------------------------------------------------------------------------

test('with nothing saved yet, the form starts with one "Got it" button', () => {
    const { doc, ctx, rows } = setup();
    ctx.fillCpromptForm({ prompt: null, seen: 0, chatUsers: 3 });

    assert.equal(rows().length, 1);
    assert.equal(rows()[0].querySelector('.cprompt-label').value, 'Got it');
    assert.equal(rows()[0].querySelector('.cprompt-action').value, 'close');
    assert.match(doc.getElementById('cpromptStatus').textContent, /Not set up yet/);
    assert.equal(doc.getElementById('cpromptShowAgain').disabled, true, 'there is nobody to show it "again" to');
});

test('a saved prompt fills the form and the status line', () => {
    const { doc, ctx, rows } = setup();
    ctx.fillCpromptForm(saved());

    assert.equal(doc.getElementById('cpromptTitle').value, 'Welcome');
    assert.equal(rows().length, 2);
    assert.equal(rows()[0].querySelector('.cprompt-value').value, 'https://example.com/rules');
    assert.equal(rows()[1].querySelector('.cprompt-primary').checked, true);

    const status = doc.getElementById('cpromptStatus').textContent;
    assert.match(status, /^Showing in the chat/);
    assert.match(status, /seen by 1 of 1 person with chat access/);
    assert.match(status, /last saved by bako/);
    assert.equal(doc.getElementById('cpromptShowAgain').disabled, false);
});

test('a saved prompt with no buttons stays without buttons', () => {
    const { ctx, rows } = setup();
    ctx.fillCpromptForm(saved({ buttons: [] }));
    assert.equal(rows().length, 0);
});

// ---------------------------------------------------------------------------
// Button rows
// ---------------------------------------------------------------------------

test('only links and rooms get a value field', () => {
    const { ctx } = setup();
    for (const [action, hidden] of [['close', true], ['notifications', true], ['link', false], ['room', false]]) {
        const row = ctx.cpromptButtonRow({ label: 'x', action });
        assert.equal(row.querySelector('.cprompt-value').hidden, hidden, action);
    }

    // Switching the action re-decides it.
    const row = ctx.cpromptButtonRow({ label: 'x', action: 'close' });
    const select = row.querySelector('.cprompt-action');
    select.value = 'room';
    select.dispatch('change');
    assert.equal(row.querySelector('.cprompt-value').hidden, false);
    assert.match(row.querySelector('.cprompt-value').placeholder, /Room name/);
});

test('an unknown stored action falls back to closing', () => {
    const { ctx } = setup();
    const row = ctx.cpromptButtonRow({ label: 'x', action: 'teleport' });
    assert.equal(row.querySelector('.cprompt-action').value, 'close');
});

test('no more than four buttons can be added', () => {
    const { doc, ctx, rows } = setup();
    ctx.fillCpromptForm(saved({ buttons: [] }));
    ctx._fetch = ok(saved({ buttons: [] }));
    ctx.initChatPrompt();

    const add = doc.getElementById('cpromptAddButton');
    for (let i = 0; i < 6; i++) add.dispatch('click');
    assert.equal(rows().length, 4);
    assert.equal(add.disabled, true);

    rows()[0].querySelector('.btn-danger').dispatch('click');
    assert.equal(rows().length, 3);
    assert.equal(add.disabled, false);
});

test('with no buttons the note and the preview both say "Got it"', () => {
    const { doc, ctx } = setup();
    ctx.fillCpromptForm(saved({ buttons: [] }));
    assert.match(doc.getElementById('cpromptButtonCount').textContent, /Got it/);
    assert.match(doc.getElementById('cpromptPreview').textContent, /Got it/);
});

// ---------------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------------

test('a half-filled prompt is refused without asking the server', async () => {
    const calls = [];
    const { doc, ctx, rows, alertText } = setup(async (...args) => { calls.push(args); return ok(saved())(); });
    ctx.fillCpromptForm(saved());
    const submit = () => ctx.submitCprompt({ preventDefault() {} });

    doc.getElementById('cpromptTitle').value = '  ';
    await submit();
    assert.match(alertText(), /title/);

    doc.getElementById('cpromptTitle').value = 'Welcome';
    rows()[0].querySelector('.cprompt-value').value = '';
    await submit();
    assert.match(alertText(), /Button 1 needs a link/);

    rows()[0].querySelector('.cprompt-value').value = 'https://example.com';
    rows()[1].querySelector('.cprompt-label').value = '';
    await submit();
    assert.match(alertText(), /Button 2 needs some text/);

    assert.equal(calls.length, 0);
});

test('saving sends the form as the API expects it', async () => {
    const calls = [];
    const { doc, ctx, rows, alertText } = setup(async (url, opts) => {
        calls.push([url, opts]);
        return ok(saved({ version: 3 }))();
    });
    ctx.fillCpromptForm(saved());

    // A value typed under a link, then switched to close, is not sent.
    const second = rows()[1];
    second.querySelector('.cprompt-value').value = 'left over';
    doc.getElementById('cpromptShowAgain').checked = true;

    await ctx.submitCprompt({ preventDefault() {} });

    assert.equal(calls.length, 1);
    const [url, opts] = calls[0];
    assert.equal(url, '/api/admin/chat-prompt');
    assert.equal(opts.method, 'PUT');
    assert.deepEqual(JSON.parse(opts.body), {
        enabled: true,
        title: 'Welcome',
        body: 'Say hi.',
        buttons: [
            { label: 'Rules', action: 'link', value: 'https://example.com/rules', primary: false },
            { label: 'Got it', action: 'close', value: '', primary: true },
        ],
        showAgain: true,
    });
    assert.match(alertText(), /see it again/);
    assert.equal(doc.getElementById('cpromptShowAgain').checked, false, 'the box clears after a save');
});

test('"show again" asks first, and a no sends nothing', async () => {
    const calls = [];
    const { doc, ctx } = setup(async (...args) => { calls.push(args); return ok(saved())(); });
    ctx.fillCpromptForm(saved());
    ctx.confirm = () => false;
    doc.getElementById('cpromptShowAgain').checked = true;

    await ctx.submitCprompt({ preventDefault() {} });
    assert.equal(calls.length, 0);
});

test('a refusal from the server is shown as it was worded', async () => {
    const { ctx, alertText } = setup(async () => ({
        ok: false,
        status: 400,
        json: async () => ({ error: 'Button 1: the link must start with https://, http:// or /' }),
    }));
    ctx.fillCpromptForm(saved());

    await ctx.submitCprompt({ preventDefault() {} });
    assert.match(alertText(), /the link must start with/);
    assert.equal(ctx.document.getElementById('cpromptSaveBtn').disabled, false, 'the button comes back');
});
