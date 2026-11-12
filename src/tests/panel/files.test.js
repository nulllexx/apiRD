'use strict';
/*
 * The Minecraft Server tab's file browser, lifted out of rdadmin.html and run
 * against the DOM shim in ./dom.js, the same way roles.test.js does it.
 *
 * Everything this view shows came off the game server's disk, which mods,
 * plugins and the server itself can all write to. So the property most worth
 * pinning down is that a file name or a file's contents can never become
 * markup. The server's own guarantees -- confinement, refusing credential
 * stores, masking secrets, auditing -- are tested in console/files.rs and
 * tests/api.rs; these check the page does not undo them.
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

const MARK_START = '/* --------------------------------------------------------- Server Files */';
const MARK_END = '/* --------------------------------------------------------- Roles & Permissions */';

const html = fs.readFileSync(PAGE, 'utf8');
const start = html.indexOf(MARK_START);
const end = html.indexOf(MARK_END);

assert.ok(
    start >= 0 && end > start,
    `could not find the Server Files block in ${PAGE}. Both marker comments must be ` +
    `present and in order:\n  ${MARK_START}\n  ${MARK_END}`
);

const source = html.slice(start, end);

function buildPage() {
    const doc = createDocument();
    const el = (tag, id, parent = doc.body) => {
        const node = doc.createElement(tag);
        if (id) node.id = id;
        parent.appendChild(node);
        return node;
    };
    const mc = el('div', 'minecraft-tab');
    mc.className = 'tab-content active';
    const view = el('div', 'mc-files', mc);
    view.className = 'mc-view active';
    el('nav', 'filesCrumbs', view);
    el('input', 'filesFilter', view);
    el('p', 'filesNote', view);
    const layout = el('div', 'filesLayout', view);
    const table = el('table', 'fileTable', layout);
    el('tbody', null, table);
    const viewer = el('section', 'fileViewer', layout);
    viewer.hidden = true;
    for (const id of ['fileViewerName', 'fileViewerMeta', 'fileViewerNote', 'fileViewerText']) {
        el(id === 'fileViewerText' ? 'pre' : 'span', id, viewer);
    }
    el('a', 'fileViewerDownload', viewer);
    for (const id of ['fileViewerClose', 'fileViewerEdit', 'fileViewerSave', 'fileViewerCancel', 'fileViewerMax']) {
        el('button', id, viewer).hidden = id !== 'fileViewerClose' && id !== 'fileViewerMax';
    }
    const editor = el('div', 'fileEditor', viewer);
    editor.hidden = true;
    el('pre', 'fileEditorHighlight', editor);
    el('textarea', 'fileEditorInput', editor);
    el('button', 'filesRefresh', view);
    el('button', 'filesUpload', view).hidden = true;
    el('button', 'filesNew', view).hidden = true;
    el('input', 'filesUploadInput', view);
    el('div', 'filesTableWrap', view);
    el('div', 'filesDrop', view).hidden = true;
    return doc;
}

const json = (body, status = 200) => ({
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
});

function setup(fetchImpl = () => json({ path: '', entries: [], total: 0, truncated: false }),
               { permissions = null, confirm = true, prompt = null } = {}) {
    const doc = buildPage();
    const requests = [];
    const calls = { confirms: [], sent: [] };
    // `new URL(window.location)` works in a browser because a Location
    // stringifies to its href.
    const location = {
        href: 'https://example.test/rdadmin.html?page=files',
        search: '?page=files',
        toString() { return this.href; },
    };
    const ctx = {
        document: doc,
        console: { error() {}, warn() {}, log() {} },
        JSON, Array, Object, String, Number, Math, Promise, Error, Date, isNaN, isFinite,
        encodeURIComponent, URL, URLSearchParams,
        window: {
            location,
            history: { replaceState(_s, _t, url) { location.href = url; location.search = new URL(url).search; } },
        },
        // Defined elsewhere in the page, which this does not lift.
        minecraftTabActive: () => true,
        canAccess: (p) => permissions === null || permissions.includes(p),
        askConfirm: async (opts) => { calls.confirms.push(opts); return confirm; },
        askPrompt: async (opts) => { calls.prompts = (calls.prompts || []).concat([opts]); return prompt; },
        Blob,
        FormData: class { constructor() { this.parts = []; } append(...a) { this.parts.push(a); } },
        fetch: async (url, init = {}) => {
            requests.push(url);
            calls.sent.push({ url, init });
            return fetchImpl(url, init);
        },
    };
    ctx.globalThis = ctx;
    vm.createContext(ctx);
    vm.runInContext(source, ctx);
    return { doc, ctx, requests, calls, location, run: (code) => vm.runInContext(code, ctx) };
}

const rows = (doc) => doc.querySelectorAll('#fileTable tbody tr');

/* The shim has no click(); run the listeners the page attached. */
const click = (el) => (el._listeners.click || []).forEach(fn => fn({ preventDefault() {} }));

test('a hostile file name is shown as text, never parsed as markup', () => {
    const { doc, ctx, run } = setup();
    ctx.hostile = [{ name: '<img src=x onerror=alert(1)>.yml', path: 'x.yml', kind: 'file', size: 3 }];
    run('filesEntries = hostile; renderFileRows()');

    const body = doc.querySelector('#fileTable tbody');
    assert.equal(body.querySelectorAll('img').length, 0);
    const label = body.querySelector('.file-name span');
    assert.equal(label.textContent, '<img src=x onerror=alert(1)>.yml');
});

test('file contents are shown as text, and masking and truncation are reported', async () => {
    const preview = {
        path: 'server.properties', name: 'server.properties', size: 2048,
        modified: '2026-11-12T10:00:00Z', binary: false, compressed: false,
        truncated: true, masked: 2, text: '<script>steal()</script>\nrcon.password=********\n',
    };
    const { doc, run } = setup(() => json(preview));
    await run(`openFile('server.properties')`);

    const text = doc.getElementById('fileViewerText');
    assert.equal(text.textContent, preview.text);
    assert.equal(text.querySelectorAll('script').length, 0);
    assert.equal(doc.getElementById('fileViewer').hidden, false);
    const note = doc.getElementById('fileViewerNote').textContent;
    assert.match(note, /2 secret values are masked/);
    assert.match(note, /first 512 KiB/);
});

test('a binary file shows a placeholder instead of contents', async () => {
    const { doc, run } = setup(() => json({
        path: 'world/level.dat', name: 'level.dat', size: 10, binary: true,
        compressed: false, truncated: false, masked: 0,
    }));
    await run(`openFile('world/level.dat')`);
    const text = doc.getElementById('fileViewerText');
    assert.match(text.textContent, /Binary file/);
    assert.ok(text.classList.contains('empty'));
});

test('the server\'s refusal is what the viewer shows', async () => {
    const { doc, run } = setup(() => json({ error: 'This file holds credentials and is never served' }, 403));
    await run(`openFile('authedPlayers.json')`);
    assert.equal(doc.getElementById('fileViewerText').textContent,
        'This file holds credentials and is never served');
});

test('restricted entries get no size, date, view or download', () => {
    const { doc, ctx, run } = setup();
    ctx.entries = [
        { name: 'authedPlayers.json', path: 'authedPlayers.json', kind: 'file',
          restricted: 'This file holds credentials and is never served' },
        { name: 'ops.json', path: 'ops.json', kind: 'file', size: 2048, modified: '2026-11-12T10:00:00Z' },
    ];
    run('filesEntries = entries; renderFileRows()');

    const [locked, open] = rows(doc);
    assert.equal(locked.querySelectorAll('button').length, 0);
    assert.equal(locked.querySelectorAll('a').length, 0);
    assert.equal(locked.querySelector('.file-size').textContent, '');
    assert.equal(locked.querySelector('.file-when').textContent, '');
    assert.match(locked.querySelector('.file-lock').textContent, /credentials/);

    assert.equal(open.querySelector('.file-size').textContent, '2.0 KiB');
    assert.ok(open.querySelector('a'), 'an ordinary file can be downloaded');
});

test('paths are percent-encoded into every request and link', async () => {
    const { doc, ctx, requests, run } = setup();
    ctx.entries = [{ name: 'a&b c.txt', path: 'odd dir/a&b c.txt', kind: 'file', size: 1 }];
    run('filesEntries = entries; renderFileRows()');
    assert.equal(doc.querySelector('#fileTable tbody a').href,
        '/api/admin/console/files/download?path=odd%20dir%2Fa%26b%20c.txt');

    await run(`loadFiles('odd dir/?x=1#y')`);
    assert.equal(requests.pop(), '/api/admin/console/files?path=odd%20dir%2F%3Fx%3D1%23y');
});

test('a subfolder gets an up row pointing at its parent', async () => {
    const { doc, requests, run } = setup((url) => json({
        path: 'plugins/Foo', entries: [], total: 0, truncated: false,
    }));
    await run(`loadFiles('plugins/Foo')`);
    const up = rows(doc)[0];
    assert.equal(up.querySelector('.file-name span').textContent, '..');
    click(up.querySelector('.file-name'));
    assert.equal(requests.pop(), '/api/admin/console/files?path=plugins');

    const crumbs = doc.getElementById('filesCrumbs').querySelectorAll('.crumb');
    assert.deepEqual(crumbs.map(c => c.textContent), ['server', 'plugins', 'Foo']);
});

test('a slow listing cannot overwrite a newer one', async () => {
    let releaseFirst;
    const first = new Promise(resolve => { releaseFirst = resolve; });
    const { run } = setup((url) => url.endsWith('path=old')
        ? first
        : json({ path: 'new', entries: [], total: 0, truncated: false }));

    const slow = run(`loadFiles('old')`);
    await run(`loadFiles('new')`);
    releaseFirst(json({ path: 'old', entries: [{ name: 'stale', path: 'old/stale', kind: 'file' }] }));
    await slow;
    assert.equal(run('filesPath'), 'new');
    assert.equal(run('filesEntries.length'), 0);
});

test('the folder and open file follow the address bar, and closing clears it', async () => {
    const { location, run } = setup((url) => url.includes('/view')
        ? json({ path: 'logs/latest.log', name: 'latest.log', size: 1, text: 'x', masked: 0 })
        : json({ path: 'logs', entries: [], total: 0, truncated: false }));
    await run(`loadFiles('logs')`);
    await run(`openFile('logs/latest.log')`);
    let params = new URL(location.href).searchParams;
    assert.equal(params.get('path'), 'logs');
    assert.equal(params.get('file'), 'logs/latest.log');

    await run('closeViewer()');
    params = new URL(location.href).searchParams;
    assert.equal(params.get('file'), null);
    assert.equal(params.get('path'), 'logs');
});

test('sizes read naturally', () => {
    const { run } = setup();
    assert.equal(run('formatBytes(0)'), '0 B');
    assert.equal(run('formatBytes(1023)'), '1023 B');
    assert.equal(run('formatBytes(1536)'), '1.5 KiB');
    assert.equal(run('formatBytes(50 * 1024 * 1024)'), '50 MiB');
    assert.equal(run('formatBytes(undefined)'), '');
});

/* ------------------------------------------------------------ highlighting */

const spans = (el) => el.querySelectorAll('span');
const classesOf = (el) => spans(el).map(sp => [sp.className, sp.textContent]);

test('highlighting builds text and spans only, and keeps every character', () => {
    const { doc, run } = setup();
    const el = doc.createElement('pre');
    doc.body.appendChild(el);
    doc.body.lastTarget = el;
    const hostile = '{\n  "<img src=x onerror=alert(1)>": "<script>steal()</script>",\n' +
                    '  // </pre><iframe src=//evil>\n  "n": 12\n}\n';
    for (const lang of ['json', 'yaml', 'toml', 'props', 'log', 'md']) {
        run(`highlightInto(document.body.lastTarget, ${JSON.stringify(hostile)}, ${JSON.stringify(lang)})`);
        assert.equal(el.textContent, hostile, lang);
        for (const tag of ['img', 'script', 'iframe']) {
            assert.equal(el.querySelectorAll(tag).length, 0, `${lang}: ${tag}`);
        }
        assert.ok(spans(el).every(sp => /^hl-[a-z]+$/.test(sp.className)), lang);
    }
});

test('each language marks what an operator scans for', () => {
    const { doc, run } = setup();
    const el = doc.createElement('pre');
    doc.body.appendChild(el);
    doc.body.lastTarget = el;
    const hl = (text, lang) => {
        run(`highlightInto(document.body.lastTarget, ${JSON.stringify(text)}, ${JSON.stringify(lang)})`);
        return classesOf(el);
    };

    assert.deepEqual(hl('#c\nmotd=Hi\nrcon.password=********\nmax-players=20\n', 'props'), [
        ['hl-com', '#c'], ['hl-key', 'motd'], ['hl-punc', '='],
        ['hl-key', 'rcon.password'], ['hl-punc', '='], ['hl-mask', '********'],
        ['hl-key', 'max-players'], ['hl-punc', '='], ['hl-num', '20'],
    ]);
    assert.deepEqual(hl('[general]\nname = "x" # note\non = true\n', 'toml'), [
        ['hl-sec', '[general]'], ['hl-key', 'name'], ['hl-punc', '='], ['hl-str', '"x"'],
        ['hl-com', '# note'], ['hl-key', 'on'], ['hl-punc', '='], ['hl-kw', 'true'],
    ]);
    assert.deepEqual(hl('web:\n  token: "********"\n  - port: 25565 # x\n', 'yaml'), [
        ['hl-key', 'web'], ['hl-key', 'token'], ['hl-mask', '"********"'],
        ['hl-punc', '-'], ['hl-key', 'port'], ['hl-num', '25565'], ['hl-com', '# x'],
    ]);
    assert.deepEqual(hl('{"a": [1, true, null], "b": "s"}', 'json').map(([c]) => c), [
        'hl-punc', 'hl-key', 'hl-punc', 'hl-punc', 'hl-num', 'hl-punc', 'hl-kw', 'hl-punc',
        'hl-kw', 'hl-punc', 'hl-punc', 'hl-key', 'hl-punc', 'hl-str', 'hl-punc',
    ]);
    const log = hl('[12:00:00] [Server thread/INFO]: <Steve> hi\n' +
                   '[12:00:01] [Server thread/ERROR]: boom\n\tat a.b(C.java:1)\n', 'log');
    assert.deepEqual(log, [
        ['hl-time', '[12:00:00]'], ['hl-info', '[Server thread/INFO]'], ['hl-key', '<Steve>'],
        ['hl-err', '[12:00:01] [Server thread/ERROR]: boom'], ['hl-dim', '\tat a.b(C.java:1)'],
    ]);
});

test('languages come from the file name', () => {
    const { run } = setup();
    const cases = {
        'server.properties': 'props', 'config/a.toml': 'toml', 'plugins/X/config.yml': 'yaml',
        'b.YAML': 'yaml', 'pack.mcmeta': 'json', 'a.json5': 'json', 'logs/latest.log': 'log',
        'logs/2026-01-01-1.log.gz': 'log', 'crash-reports/crash.txt': 'log', 'README.md': 'md',
        'eula.txt': null, 'mods/a.jar': null,
    };
    for (const [path, lang] of Object.entries(cases)) {
        assert.equal(run(`fileLanguage(${JSON.stringify(path)})`), lang, path);
    }
});

test('oversized text is left plain, and hostile input cannot stall the page', () => {
    const { doc, run } = setup();
    const el = doc.createElement('pre');
    doc.body.appendChild(el);
    doc.body.lastTarget = el;

    run(`highlightInto(document.body.lastTarget, 'a = 1\\n'.repeat(60000), 'toml')`);
    assert.equal(spans(el).length, 0, 'past the limit, no spans');

    // Each of these once made a rule quadratic -- tens of seconds on one
    // 250 KB line. Every language gets every shape: they must stay linear.
    const n = 250000;
    const spaces = ' '.repeat(n);
    const shapes = [
        '"'.repeat(n), '"\\'.repeat(n / 2), "'".repeat(n), '/*'.repeat(n / 2),
        spaces + 'x', 'k' + spaces + 'k', 'x =' + spaces, '= ' + spaces + '1', ': ' + spaces + '1',
        'a.'.repeat(n / 2), '['.repeat(n), '[a/'.repeat(n / 3), '**'.repeat(n / 2), '`'.repeat(n),
    ];
    const cases = [];
    for (const lang of ['json', 'yaml', 'toml', 'props', 'log', 'md']) {
        for (const text of shapes) cases.push([text, lang]);
    }
    for (const [text, lang] of cases) {
        const started = Date.now();
        run(`highlightInto(document.body.lastTarget, ${JSON.stringify(text)}, ${JSON.stringify(lang)})`);
        assert.ok(Date.now() - started < 2000, `${lang}: ${text.slice(0, 4)}… took ${Date.now() - started}ms`);
        assert.equal(el.textContent, text);
    }
});

/* ---------------------------------------------------------------- editing */

const editable = {
    path: 'config/a.toml', name: 'a.toml', size: 6, binary: false, compressed: false,
    truncated: false, masked: 1, text: 'x = 1\ntoken = "********"\n', editable: true, version: 'v1',
};

test('the Edit button appears only with console.edit, and only for editable files', async () => {
    let preview = editable;
    const withEdit = setup(() => json(preview));
    await withEdit.run(`openFile('config/a.toml')`);
    const edit = withEdit.doc.getElementById('fileViewerEdit');
    assert.equal(edit.hidden, false);
    assert.equal(edit.disabled, false);

    preview = Object.assign({}, editable, { editable: false, readonly_reason: 'This folder holds code' });
    await withEdit.run(`openFile('config/a.toml', { force: true })`);
    assert.equal(edit.disabled, true);
    assert.equal(edit.title, 'This folder holds code');
    assert.match(withEdit.doc.getElementById('fileViewerNote').textContent, /Read-only: This folder holds code/);

    const without = setup(() => json(editable), { permissions: ['console.files'] });
    await without.run(`openFile('config/a.toml')`);
    assert.equal(without.doc.getElementById('fileViewerEdit').hidden, true);
});

test('saving sends the edited text, the version and the panel header', async () => {
    const { doc, calls, run } = setup((url, init) => init.method === 'PUT'
        ? json({ path: 'config/a.toml', size: 6, version: 'v2', restored: 1, unchanged: false })
        : url.includes('/view') ? json(editable) : json({ path: 'config', entries: [] }));
    await run(`openFile('config/a.toml')`);
    run('startEdit()');

    const input = doc.getElementById('fileEditorInput');
    assert.equal(input.value, editable.text);
    assert.equal(doc.getElementById('fileEditor').hidden, false);
    assert.match(doc.getElementById('fileViewerNote').textContent, /kept as they are/);
    assert.ok(spans(doc.getElementById('fileEditorHighlight')).length > 0, 'the editor is highlighted');

    input.value = editable.text.replace('x = 1', 'x = 2');
    await run('saveEdit()');

    const put = calls.sent.find(c => c.init.method === 'PUT');
    assert.equal(put.url, '/api/admin/console/files/content');
    assert.equal(put.init.headers['X-RD-Panel'], '1');
    assert.deepEqual(JSON.parse(put.init.body),
        { path: 'config/a.toml', content: 'x = 2\ntoken = "********"\n', version: 'v1' });
    assert.equal(run('editing'), false);
    assert.match(doc.getElementById('fileViewerNote').textContent, /Saved/);
});

test('a refused save keeps the editor open with the reason', async () => {
    const { doc, run } = setup((url, init) => init.method === 'PUT'
        ? json({ error: '`online-mode` can\'t be changed from the panel' }, 400)
        : json(editable));
    await run(`openFile('config/a.toml')`);
    run('startEdit()');
    doc.getElementById('fileEditorInput').value = 'changed';
    await run('saveEdit()');
    assert.equal(run('editing'), true);
    assert.equal(doc.getElementById('fileEditorInput').value, 'changed', 'edits survive');
    const note = doc.getElementById('fileViewerNote');
    assert.match(note.textContent, /online-mode/);
    assert.ok(note.classList.contains('error'));
});

test('a conflict asks before discarding, and keeps the edits if told no', async () => {
    const { doc, calls, run } = setup(
        (url, init) => init.method === 'PUT' ? json({ error: 'The file changed' }, 409) : json(editable),
        { confirm: false });
    await run(`openFile('config/a.toml')`);
    run('startEdit()');
    doc.getElementById('fileEditorInput').value = 'mine';
    await run('saveEdit()');
    assert.equal(calls.confirms.length, 1);
    assert.equal(run('editing'), true);
    assert.equal(doc.getElementById('fileEditorInput').value, 'mine');
});

test('unsaved edits are not thrown away without asking', async () => {
    const { doc, calls, run } = setup(() => json(editable), { confirm: false });
    await run(`openFile('config/a.toml')`);
    run('startEdit()');
    doc.getElementById('fileEditorInput').value = 'unsaved';
    await run('closeViewer()');
    await run(`openFile('config/other.toml')`);
    assert.equal(calls.confirms.length, 2);
    assert.equal(run('editing'), true);
    assert.equal(run('filesOpen'), 'config/a.toml');
});

/* ---------------------------------------------------------------- uploads */

test('the Upload and New file buttons follow console.upload', () => {
    const allowed = setup();
    allowed.run('applyFilesPermissions()');
    assert.equal(allowed.doc.getElementById('filesUpload').hidden, false);
    assert.equal(allowed.doc.getElementById('filesNew').hidden, false);
    const denied = setup(undefined, { permissions: ['console.files'] });
    denied.run('applyFilesPermissions()');
    assert.equal(denied.doc.getElementById('filesUpload').hidden, true);
    assert.equal(denied.doc.getElementById('filesNew').hidden, true);
});

test('New file creates an empty file in the open folder and opens it for editing', async () => {
    const blank = Object.assign({}, editable, { path: 'config/new.yml', name: 'new.yml', size: 0, masked: 0, text: '' });
    const { calls, run } = setup((url, init) => {
        if (init.method === 'POST') return json({ path: 'config/new.yml', size: 0, replaced: false });
        if (url.includes('/view')) return json(blank);
        return json({ path: 'config', entries: [] });
    }, { prompt: '  new.yml ' });
    await run(`loadFiles('config')`);
    await run('createFile()');

    const post = calls.sent.find(c => c.init.method === 'POST');
    assert.equal(post.url, '/api/admin/console/files/upload?path=config', 'never with overwrite');
    assert.equal(post.init.headers['X-RD-Panel'], '1');
    const [field, blob, name] = post.init.body.parts[0];
    assert.deepEqual([field, blob.size, name], ['file', 0, 'new.yml']);
    assert.equal(run('filesOpen'), 'config/new.yml');
    assert.equal(run('editing'), true);
});

test('New file refuses paths, does nothing on cancel, and reports the server\'s refusal', async () => {
    const withSlash = setup(undefined, { prompt: '../evil.yml' });
    await withSlash.run('createFile()');
    assert.equal(withSlash.calls.sent.filter(c => c.init.method === 'POST').length, 0);

    const cancelled = setup(undefined, { prompt: null });
    await cancelled.run('createFile()');
    assert.equal(cancelled.calls.sent.length, 0);

    const refused = setup((url, init) => init.method === 'POST'
        ? json({ error: '`.sh` files can\'t be uploaded' }, 400)
        : json({ path: '', entries: [] }), { prompt: 'run.sh' });
    await refused.run('createFile()');
    const note = refused.doc.getElementById('filesNote');
    assert.match(note.textContent, /run\.sh: `\.sh` files can't be uploaded/);
    assert.ok(note.classList.contains('error'));
    assert.equal(refused.run('filesOpen'), null);
});

test('uploads go to the open folder with the panel header, and replacing asks first', async () => {
    let first = true;
    const { calls, run, ctx } = setup((url, init) => {
        if (init.method !== 'POST') return json({ path: 'config', entries: [] });
        if (first) { first = false; return json({ error: 'A file with that name already exists' }, 409); }
        return json({ path: 'config/a.toml', size: 5, replaced: true });
    });
    await run(`loadFiles('config')`);
    ctx.picked = [{ name: 'a.toml', size: 5 }];
    await run('uploadFiles(picked)');

    const posts = calls.sent.filter(c => c.init.method === 'POST');
    assert.deepEqual(posts.map(p => p.url), [
        '/api/admin/console/files/upload?path=config',
        '/api/admin/console/files/upload?path=config&overwrite=true',
    ]);
    assert.ok(posts.every(p => p.init.headers['X-RD-Panel'] === '1'));
    assert.equal(calls.confirms.length, 1);
    assert.match(calls.confirms[0].title, /Replace a\.toml/);
});

test('without console.edit an existing file is reported, not replaced', async () => {
    const { calls, run, ctx, doc } = setup(
        (url, init) => init.method === 'POST'
            ? json({ error: 'A file with that name already exists' }, 409)
            : json({ path: '', entries: [] }),
        { permissions: ['console.files', 'console.upload'] });
    ctx.picked = [{ name: 'a.toml', size: 5 }, { name: 'huge.toml', size: 64 * 1024 * 1024 }];
    await run('uploadFiles(picked)');
    assert.equal(calls.confirms.length, 0);
    assert.equal(calls.sent.filter(c => c.init.method === 'POST').length, 1, 'the oversized one is never sent');
    const note = doc.getElementById('filesNote');
    assert.match(note.textContent, /already exists/);
    assert.match(note.textContent, /larger than 16 MiB/);
    assert.ok(note.classList.contains('error'));
});
