'use strict';
/*
 * The Archives view -- End season and the archive records -- lifted out of
 * rdadmin.html and run against the DOM shim in ./dom.js.
 *
 * The server owns the record's shape and refuses anything else; these pin
 * the page to it: the form sends exactly the fields the server accepts,
 * links are built from the season number and never taken from a record,
 * nothing from a record becomes markup, and the progress view tracks the job.
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
const MARK_START = '/* --------------------------------------------------------- Season Archives */';
const MARK_END = '/* --------------------------------------------------------- Roles & Permissions */';

const html = fs.readFileSync(PAGE, 'utf8');
const start = html.indexOf(MARK_START);
const end = html.indexOf(MARK_END);
assert.ok(start >= 0 && end > start, `could not find the Season Archives block in ${PAGE}`);
const source = html.slice(start, end);

function buildPage() {
    const doc = createDocument();
    const el = (tag, id, parent = doc.body, extra = {}) => {
        const node = doc.createElement(tag);
        if (id) node.id = id;
        Object.assign(node, extra);
        parent.appendChild(node);
        return node;
    };
    const table = el('table', 'archiveTable', el('div', 'archiveTableWrap'));
    el('tbody', null, table);
    el('p', 'archivesNote');
    el('button', 'seasonEndOpen', doc.body, { hidden: true });
    el('button', 'archiveNew', doc.body, { hidden: true });
    el('button', 'archivesRefresh');

    const editor = el('section', 'archiveEditor', doc.body, { hidden: true });
    el('h2', 'archiveEditorTitle', editor);
    el('p', 'archiveEditorMeta', editor);
    const form = el('form', 'archiveForm', editor);
    for (const id of ['archiveSeason', 'archiveTheme', 'archiveStarted', 'archiveEnded', 'archiveMinecraft', 'archiveHasWorld', 'archiveWorldUrl', 'archiveInfo']) {
        el('input', id, form);
    }
    el('p', 'archiveWorldInfo', form);
    const mods = el('table', 'archiveMods', form);
    el('tbody', null, mods);
    el('button', 'archiveAddMod', form);
    el('p', 'archiveError', form);
    el('button', 'archiveSave', form);
    el('button', 'archiveCancel', form);

    const job = el('section', 'seasonJob', doc.body, { hidden: true });
    for (const id of ['seasonJobTitle', 'seasonJobPercent', 'seasonJobDetail', 'seasonJobResult']) el('span', id, job);
    el('div', 'seasonJobFill', el('div', 'seasonJobBar', job));
    el('ol', 'seasonJobStages', job);
    el('ul', 'seasonJobNotes', job);

    const overlay = el('div', 'seasonEndOverlay');
    for (const id of ['seasonEndTitle', 'seasonEndLoading', 'seasonEndError', 'seasonEndModsLabel']) el('p', id, overlay);
    const body = el('div', 'seasonEndBody', overlay, { hidden: true });
    el('dl', 'seasonEndSummary', body);
    el('ul', 'seasonEndMods', body);
    el('ul', 'seasonEndProblems', body);
    for (const id of ['seasonEndStarted', 'seasonEndEnded', 'seasonEndInfo', 'seasonEndAck',
                      'seasonEndNextSeason', 'seasonEndNextTheme', 'seasonEndSeed']) el('input', id, body);
    for (const id of ['seasonEndSeedHint', 'seasonEndAckText']) el('span', id, body);
    el('button', 'seasonEndCancel', overlay);
    el('button', 'seasonEndConfirm', overlay);
    return doc;
}

const json = (body, status = 200) => ({ ok: status >= 200 && status < 300, status, json: async () => body });

function setup(fetchImpl = () => json({ archives: [] }), { permissions = null } = {}) {
    const doc = buildPage();
    const sent = [];
    const ctx = {
        document: doc,
        console: { error() {}, warn() {}, log() {} },
        JSON, Array, Object, String, Number, Math, Promise, Error, Date, isNaN,
        setTimeout: () => 0, clearTimeout: () => {},
        canAccess: (p) => permissions === null || permissions.includes(p),
        formatBytes: (n) => `${n} B`,
        formatWhen: (iso) => `when(${iso})`,
        fetch: async (url, init = {}) => { sent.push({ url, init }); return fetchImpl(url, init); },
    };
    ctx.globalThis = ctx;
    vm.createContext(ctx);
    vm.runInContext(source, ctx);
    return { doc, ctx, sent, run: (code) => vm.runInContext(code, ctx) };
}

const record = (over = {}) => Object.assign({
    schema: 1, season: 9, theme: 'S8 reboot', started: '2026-08-01', ended: '2026-11-12',
    minecraft_version: '1.21.1',
    world: { file: 'Season_9.zip', url: 'javascript:alert(1)', size: 1234 },
    mods: [{ name: 'Mekanism', version: '10.7.7' }], additional_info: 'GG',
    created_at: '2026-11-12T20:00:00Z', created_by: 'bako', updated_at: '2026-11-12T20:00:00Z', updated_by: 'bako',
}, over);

test('records render as text, and links come from the season number', async () => {
    const hostile = record({ theme: '<img src=x onerror=alert(1)>' });
    const { doc, run } = setup(() => json({ archives: [
        { season: 9, file: 'Season_9.json', url: 'javascript:evil()', archive: hostile },
        { season: 2, file: 'Season_2.json', url: '/archives/Season_2.json', error: 'Not a valid archive record: <b>x</b>' },
    ] }));
    await run('loadArchives()');
    const body = doc.querySelector('#archiveTable tbody');
    assert.equal(body.querySelectorAll('img').length, 0);
    assert.equal(body.querySelectorAll('b').length, 0);
    const hrefs = body.querySelectorAll('a').map(a => a.href);
    assert.deepEqual(hrefs, ['/raindrippy/content/Season_9.zip', '/archives/Season_9.json', '/archives/Season_2.json']);
    const rows = body.querySelectorAll('tr');
    assert.match(rows[0].textContent, /<img src=x onerror=alert\(1\)>/);
    assert.match(rows[1].textContent, /Repair/);
});

test('without season.archives the records are not requested', async () => {
    const { doc, sent, run } = setup(undefined, { permissions: ['season.end'] });
    run('applyArchivesPermissions()');
    await run('loadArchives()');
    assert.equal(sent.length, 0);
    assert.equal(doc.getElementById('archiveNew').hidden, true);
    assert.equal(doc.getElementById('seasonEndOpen').hidden, false);
    assert.match(doc.getElementById('archivesNote').textContent, /season\.archives/);
});

test('the editor sends exactly the fields the server accepts, with the version', async () => {
    const { doc, sent, run } = setup((url, init) => {
        const own = record({ world: { file: 'Season_9.zip', url: '/raindrippy/content/Season_9.zip', size: 1234 } });
        if (init.method === 'PUT') return json({ archive: own, version: 'v2' });
        if (url.endsWith('/archives/9')) return json({ archive: own, version: 'v1' });
        return json({ archives: [] });
    });
    await run('openArchive(9)');
    assert.equal(doc.getElementById('archiveEditor').hidden, false);
    assert.equal(doc.getElementById('archiveSeason').readOnly, true, 'the season of an existing record is fixed');
    doc.getElementById('archiveTheme').value = '  S8 Reboot ';
    run(`addModRow('Create', '6.0.4')`);
    run(`addModRow('', '')`);
    await run('saveArchive()');

    const put = sent.find(s => s.init.method === 'PUT');
    assert.equal(put.url, '/api/admin/season/archives/9');
    assert.equal(put.init.headers['X-RD-Panel'], '1');
    const body = JSON.parse(put.init.body);
    assert.deepEqual(Object.keys(body), ['archive', 'version']);
    assert.equal(body.version, 'v1');
    assert.deepEqual(Object.keys(body.archive),
        ['season', 'theme', 'started', 'ended', 'minecraft_version', 'has_world', 'world_url', 'mods', 'additional_info']);
    assert.equal(body.archive.world_url, null, 'the default link is sent as none');
    assert.equal(body.archive.theme, 'S8 Reboot');
    assert.equal(body.archive.has_world, true);
    assert.deepEqual(body.archive.mods, [{ name: 'Mekanism', version: '10.7.7' }, { name: 'Create', version: '6.0.4' }]);
    assert.equal(doc.getElementById('archiveEditor').hidden, true, 'closed after saving');
});

test('a new record is created with create=true, and a bad season is caught first', async () => {
    const { doc, sent, run } = setup((url, init) => init.method === 'PUT' ? json({ archive: record({ season: 3 }) }) : json({ archives: [] }));
    run('newArchive()');
    assert.equal(doc.getElementById('archiveSeason').readOnly, false);
    await run('saveArchive()');
    assert.equal(sent.filter(s => s.init.method === 'PUT').length, 0);
    assert.match(doc.getElementById('archiveError').textContent, /whole number/);

    doc.getElementById('archiveSeason').value = '3';
    doc.getElementById('archiveHasWorld').checked = true;
    run('updateWorldInfo()');
    assert.equal(doc.getElementById('archiveWorldUrl').hidden, false, 'the link field shows with the checkbox');
    assert.equal(doc.getElementById('archiveWorldUrl').placeholder, '/raindrippy/content/Season_3.zip');
    doc.getElementById('archiveWorldUrl').value = ' https://cdn.example.com/s3.zip ';
    await run('saveArchive()');
    const put = sent.find(s => s.init.method === 'PUT');
    assert.equal(put.url, '/api/admin/season/archives/3?create=true');
    assert.equal(JSON.parse(put.init.body).archive.world_url, 'https://cdn.example.com/s3.zip');
});

test('a record with its own link opens with that link filled in', async () => {
    const { doc, run } = setup(() => json({
        archive: record({ world: { file: 'w.zip', url: 'https://cdn.example.com/w.zip', size: null } }), version: 'v1' }));
    await run('openArchive(9)');
    assert.equal(doc.getElementById('archiveWorldUrl').value, 'https://cdn.example.com/w.zip');
    assert.equal(doc.getElementById('archiveWorldUrl').hidden, false);
});

test('a refused save keeps the form open with the reason', async () => {
    const { doc, run } = setup((url, init) => init.method === 'PUT'
        ? json({ error: 'The record changed since you opened it.' }, 409)
        : json({ archive: record(), version: 'v1' }));
    await run('openArchive(9)');
    await run('saveArchive()');
    assert.equal(doc.getElementById('archiveEditor').hidden, false);
    assert.match(doc.getElementById('archiveError').textContent, /changed since.*still in the form/);
});

const plan = (over = {}) => Object.assign({
    season: 9, theme: 'S8 reboot', minecraft: '1.21.1', worlds: ['world', 'world_nether'],
    files: 1200, bytes: 500000, mods: [{ name: '<i>Mek</i>', version: '10.7.7', file: 'm.jar' }],
    zip_name: 'Season_9.zip', zip_url: '/raindrippy/content/Season_9.zip', record_url: '/archives/Season_9.json',
    zip_exists: false, record_exists: false, free_bytes: 9e9, enough_space: true,
    dimensions: ['world/DIM-1 (Nether)', 'world/DIM1 (The End)'],
    next_season: 10, current_seed: '-417', kept_worlds: ['eventhub', 'testing'], server_state: 'running',
}, over);

test('End season shows the plan, needs the acknowledgement, and sends the details', async () => {
    const { doc, sent, run } = setup((url, init) => {
        if (url.endsWith('/current')) return json(plan());
        if (init.method === 'POST') return json({ season: 9, state: 'running', stage_index: 0, stages: ['Check'], percent: 1 });
        return json({ job: null });
    });
    await run('openSeasonEnd()');
    assert.ok(doc.getElementById('seasonEndOverlay').classList.contains('open'));
    assert.equal(doc.getElementById('seasonEndTitle').textContent, 'End season 9?');
    assert.match(doc.getElementById('seasonEndSummary').textContent, /S8 reboot.*1\.21\.1.*world, world_nether.*Including.*world\/DIM-1 \(Nether\), world\/DIM1 \(The End\).*eventhub, testing.*stopped/s);
    assert.equal(doc.getElementById('seasonEndMods').querySelectorAll('i').length, 0, 'mod names are text');
    assert.equal(doc.getElementById('seasonEndNextSeason').value, '10');
    assert.match(doc.getElementById('seasonEndSeedHint').textContent, /-417/);
    assert.match(doc.getElementById('seasonEndAckText').textContent, /world, world_nether \(with the dimensions inside it\) are deleted.*eventhub, testing are kept/);
    const confirm = doc.getElementById('seasonEndConfirm');
    assert.equal(confirm.disabled, true, 'not until acknowledged');

    doc.getElementById('seasonEndAck').checked = true;
    run('updateSeasonConfirm()');
    assert.equal(confirm.disabled, true, 'not without a theme for the next season');
    doc.getElementById('seasonEndNextTheme').value = ' Glacier ';
    doc.getElementById('seasonEndNextSeason').value = '9';
    run('updateSeasonConfirm()');
    assert.equal(confirm.disabled, true, 'the next season must come after this one');
    doc.getElementById('seasonEndNextSeason').value = '10';
    run('updateSeasonConfirm()');
    assert.equal(confirm.disabled, false);
    doc.getElementById('seasonEndSeed').value = ' 42 ';
    doc.getElementById('seasonEndStarted').value = '2026-08-01';
    doc.getElementById('seasonEndInfo').value = 'GG';
    await run('confirmSeasonEnd()');

    const post = sent.find(s => s.init.method === 'POST');
    assert.equal(post.url, '/api/admin/season/end');
    assert.equal(post.init.headers['X-RD-Panel'], '1');
    const body = JSON.parse(post.init.body);
    assert.equal(body.season, 9, 'the season that was shown, so a change in between is caught');
    assert.equal(body.started, '2026-08-01');
    assert.equal(body.additional_info, 'GG');
    assert.match(body.ended, /^\d{4}-\d{2}-\d{2}$/, 'defaults to today');
    assert.deepEqual([body.next_season, body.next_theme, body.seed], [10, 'Glacier', '42']);
    assert.equal(doc.getElementById('seasonEndOverlay').classList.contains('open'), false);
    assert.equal(doc.getElementById('seasonJob').hidden, false, 'progress shows straight away');
});

test('End season cannot be confirmed when the season already looks ended', async () => {
    const { doc, run } = setup(() => json(plan({ zip_exists: true, enough_space: false, server_state: 'unknown' })));
    await run('openSeasonEnd()');
    doc.getElementById('seasonEndAck').checked = true;
    run('updateSeasonConfirm()');
    assert.equal(doc.getElementById('seasonEndConfirm').disabled, true);
    const problems = doc.getElementById('seasonEndProblems').querySelectorAll('li').map(li => li.textContent);
    assert.equal(problems.length, 3);
    assert.match(problems[0], /Season_9\.zip already exists/);
    assert.match(problems[2], /can't be stopped and restarted safely/);
});

test('progress shows the stage, bytes and stages, then the results', () => {
    const { doc, run, ctx } = setup();
    ctx.running = {
        season: 9, state: 'running', stage: 'zip', stage_label: 'Zipping the world and mods', stage_index: 2,
        stages: ['Check', 'Pause', 'Zip', 'Resume', 'Publish', 'Record'], percent: 47.6,
        bytes_done: 100, bytes_total: 200, files_done: 3, files_total: 9,
        current_file: 'Season_9/world/<b>r.0.0.mca</b>', notes: [], started_by: 'bako', started_at: 'x',
    };
    run('renderSeasonJob(running)');
    assert.match(doc.getElementById('seasonJobTitle').textContent, /Zipping the world and mods/);
    assert.equal(doc.getElementById('seasonJobPercent').textContent, '48%');
    assert.equal(doc.getElementById('seasonJobFill').style.width, '48%');
    assert.match(doc.getElementById('seasonJobDetail').textContent, /100 B of 200 B · 3 of 9 files · Season_9\/world\/<b>r\.0\.0\.mca<\/b>/);
    const states = doc.getElementById('seasonJobStages').querySelectorAll('li').map(li => li.className);
    assert.deepEqual(states, ['done', 'done', 'current', 'pending', 'pending', 'pending']);

    ctx.finished = Object.assign({}, ctx.running, { state: 'done', percent: 100, zip_url: 'javascript:x', record_url: 'javascript:y',
                                                    notes: ['The server is stopped, so there was nothing to pause'] });
    run('renderSeasonJob(finished)');
    assert.ok(doc.getElementById('seasonJob').classList.contains('done'));
    assert.deepEqual(doc.getElementById('seasonJobResult').querySelectorAll('a').map(a => a.href),
        ['/raindrippy/content/Season_9.zip', '/archives/Season_9.json']);
    assert.match(doc.getElementById('seasonJobNotes').textContent, /stopped/);

    ctx.failed = Object.assign({}, ctx.running, { state: 'failed', stage_index: 1, error: 'World saving couldn\'t be paused' });
    run('renderSeasonJob(failed)');
    assert.deepEqual(doc.getElementById('seasonJobStages').querySelectorAll('li').map(li => li.className),
        ['done', 'failed', 'pending', 'pending', 'pending', 'pending']);
    assert.match(doc.getElementById('seasonJobResult').textContent, /couldn't be paused/);
});
