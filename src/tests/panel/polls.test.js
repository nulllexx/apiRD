'use strict';
/*
 * The Polls tab's real JavaScript, lifted straight out of rdadmin.html and run
 * against the DOM shim in ./dom.js.
 *
 * The point is to execute the branches rather than read them: which bar is
 * marked as leading, whether one voter reads as "1 voter", the option
 * repeater's 2..10 bounds, the pager's ends, and every check that stops a
 * malformed poll from being sent.
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

// ---------------------------------------------------------------------------
// Lift the Polls block out of the page
// ---------------------------------------------------------------------------

const MARK_START = '/* --------------------------------------------------------- Polls */';
const MARK_END = '/* --------------------------------------------------------- Tab Routing Implementation */';

const html = fs.readFileSync(PAGE, 'utf8');
const start = html.indexOf(MARK_START);
const end = html.indexOf(MARK_END);

// A clear failure here beats every test below failing for an unrelated reason:
// if someone moves or renames these sections, this is what says so.
assert.ok(
    start >= 0 && end > start,
    `could not find the Polls block in ${PAGE}. Both marker comments must be ` +
    `present and in order:\n  ${MARK_START}\n  ${MARK_END}`
);

const source = html.slice(start, end);

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/** The globals the Polls block expects the rest of the page to have declared. */
function freshContext(doc, fetchImpl) {
    const ctx = {
        document: doc,
        console: { error() {}, warn() {}, log() {} },
        POLL_MIN_OPTIONS: 2,
        POLL_MAX_OPTIONS: 10,
        POLL_REFRESH_INTERVAL: 15000,
        pollsInited: false,
        pollsTimer: null,
        pollPage: { live: 1, past: 1 },
        pollExclusions: [],
        askConfirm: async () => true,
        // Runs the callback at once: the shim has no frames, and what matters
        // is the width the bar ends up on.
        requestAnimationFrame: fn => fn(),
        setTimeout: () => 0,
        clearTimeout: () => {},
        setInterval: () => 1,
        clearInterval: () => {},
        Date, Math, JSON, Set, Map, Array, Object, Number, String, Boolean,
        isFinite, encodeURIComponent,
        fetch: (...args) => ctx._fetch(...args),
        _fetch: fetchImpl || (() => { throw new Error('no fetch stub set'); }),
    };
    ctx.globalThis = ctx;
    vm.createContext(ctx);
    vm.runInContext(source, ctx);
    return ctx;
}

/** Just the markup the Polls code reaches for, matching rdadmin.html's ids. */
function buildPage() {
    const doc = createDocument();

    const add = (parent, tag, id, cls) => {
        const el = doc.createElement(tag);
        if (id) el.id = id;
        if (cls) el.className = cls;
        parent.appendChild(el);
        return el;
    };

    add(doc.body, 'div', 'poll-alert');

    const form = add(doc.body, 'form', 'pollCreateForm');
    add(form, 'input', 'pollTitle').value = '';
    add(form, 'textarea', 'pollDescription').value = '';
    add(form, 'select', 'pollDuration').value = '3d';
    add(form, 'div', 'pollOptions');
    add(form, 'button', 'pollAddOption');
    add(form, 'p', 'pollOptionCount');
    add(form, 'input', 'pollAllowMultiple').checked = false;

    for (const value of ['everyone', 'admins', 'members', 'ogs']) {
        const box = add(form, 'input', null, 'pollAudience');
        box.value = value;
        box.checked = value === 'members';
    }
    add(form, 'p', 'pollAudienceNote');

    add(form, 'div', 'pollExcludeChips', 'poll-chips');
    const wrap = add(form, 'div', null, 'poll-typeahead');
    add(wrap, 'input', 'pollExclude');
    add(wrap, 'ul', 'pollExcludeList').hidden = true;
    add(form, 'button', 'pollCreateBtn');

    for (const [listId, pagerId, kind] of [
        ['pollsLive', 'pollsLivePager', 'live'],
        ['pollsPast', 'pollsPastPager', 'past'],
    ]) {
        add(doc.body, 'div', listId, 'poll-list');
        const pager = add(doc.body, 'div', pagerId, 'poll-pager');
        for (const step of ['-1', '1']) {
            const btn = doc.createElement('button');
            btn.setAttribute('data-poll-page', kind);
            btn.setAttribute('data-poll-step', step);
            pager.appendChild(btn);
        }
        const label = doc.createElement('span');
        label.setAttribute('data-poll-label', kind);
        pager.appendChild(label);
    }

    return doc;
}

function setup(fetchImpl) {
    const doc = buildPage();
    const ctx = freshContext(doc, fetchImpl);
    return {
        doc,
        ctx,
        // pollAlert is the real function; read its output back rather than
        // stubbing, so the message a person would see is what is asserted.
        alertText: () => doc.getElementById('poll-alert').textContent,
    };
}

/** A poll shaped the way the API sends one. */
const poll = (over = {}) => Object.assign({
    id: 1,
    title: 'New spawn build?',
    description: null,
    allowMultiple: false,
    live: true,
    endedEarly: false,
    closesAt: new Date(Date.now() + 86400000).toISOString(),
    endedAt: null,
    audienceLabels: ['Members'],
    excludedCount: 0,
    voters: 3,
    totalVotes: 3,
    options: [
        { id: 10, label: 'Medieval', votes: 2, percent: 66.7, leading: true },
        { id: 11, label: 'Modern', votes: 1, percent: 33.3, leading: false },
    ],
}, over);

// ---------------------------------------------------------------------------
// Remaining time
// ---------------------------------------------------------------------------

test('remaining time reads in days, hours and minutes', () => {
    const { ctx } = setup();
    // Half a minute of slack: the display floors, so a target of exactly four
    // hours lands on "3h" if any time at all passes before the call.
    const at = ms => new Date(Date.now() + ms + 30000).toISOString();

    assert.equal(ctx.pollRemaining(at(2 * 86400000 + 4 * 3600000)), '2d 4h');
    assert.equal(ctx.pollRemaining(at(3 * 86400000)), '3d', 'whole days drop a zero hour');
    assert.equal(ctx.pollRemaining(at(3 * 3600000 + 20 * 60000)), '3h 20m');
    assert.equal(ctx.pollRemaining(at(43 * 60000)), '43m');
});

test('a poll about to close never reads as a negative time', () => {
    const { ctx } = setup();

    assert.equal(ctx.pollRemaining(new Date(Date.now() + 20000).toISOString()), 'under a minute');
    assert.equal(ctx.pollRemaining(new Date(Date.now() - 60000).toISOString()), 'closing');
    assert.equal(ctx.pollRemaining('not a date'), 'closing', 'an unparseable date must not print NaN');
});

// ---------------------------------------------------------------------------
// The option repeater
// ---------------------------------------------------------------------------

test('the option list starts at the minimum and cannot go below it', () => {
    const { doc, ctx } = setup();
    ctx.resetPollOptions();

    const rows = doc.querySelectorAll('.poll-option-row');
    assert.equal(rows.length, 2);
    for (const row of rows) {
        assert.ok(
            row.querySelector('.poll-option-remove').disabled,
            'remove must be switched off at the minimum'
        );
    }
});

test('options grow to ten and no further', () => {
    const { doc, ctx } = setup();
    ctx.resetPollOptions();
    const list = doc.getElementById('pollOptions');

    for (let i = 2; i < 10; i++) {
        list.appendChild(ctx.pollOptionRow());
        ctx.syncPollOptions();
    }

    assert.equal(doc.querySelectorAll('.poll-option-row').length, 10);
    assert.ok(doc.getElementById('pollAddOption').disabled, 'add must be off at ten');
    assert.equal(doc.getElementById('pollOptionCount').textContent, '10 options is the maximum.');

    doc.querySelectorAll('.poll-option-row')[0].remove();
    ctx.syncPollOptions();
    assert.ok(!doc.getElementById('pollAddOption').disabled, 'add returns below the cap');
    assert.equal(doc.getElementById('pollOptionCount').textContent, '9 of 10 options.');
});

test('removing a row re-locks the controls at the floor', () => {
    const { doc, ctx } = setup();
    ctx.resetPollOptions();

    doc.getElementById('pollOptions').appendChild(ctx.pollOptionRow());
    ctx.syncPollOptions();
    for (const btn of doc.querySelectorAll('.poll-option-remove')) {
        assert.ok(!btn.disabled, 'three rows are all removable');
    }

    doc.querySelectorAll('.poll-option-remove')[0].dispatch('click');
    assert.equal(doc.querySelectorAll('.poll-option-row').length, 2, 'the click removes a row');
    for (const btn of doc.querySelectorAll('.poll-option-remove')) {
        assert.ok(btn.disabled, 'and the floor locks again');
    }
});

// ---------------------------------------------------------------------------
// The exclusion picker
// ---------------------------------------------------------------------------

test('chips add, render and remove by name', () => {
    const { doc, ctx } = setup();

    ctx.choosePollExclusion('Joe');
    ctx.choosePollExclusion('MapiccOnMC');
    assert.deepEqual(ctx.pollExclusions, ['Joe', 'MapiccOnMC']);
    assert.equal(doc.querySelectorAll('.poll-chip').length, 2);

    ctx.choosePollExclusion('Joe');
    assert.equal(ctx.pollExclusions.length, 2, 'the same person twice is still one exclusion');

    doc.querySelectorAll('.poll-chip button')[0].dispatch('click');
    assert.deepEqual(ctx.pollExclusions, ['MapiccOnMC'], 'the right chip is removed');
    assert.equal(doc.querySelectorAll('.poll-chip').length, 1);
});

// ---------------------------------------------------------------------------
// The audience note
// ---------------------------------------------------------------------------

test('ticking Everyone alongside a narrower group says so', () => {
    const { doc, ctx } = setup();
    const box = value => doc.querySelectorAll('.pollAudience').find(b => b.value === value);

    box('everyone').checked = true;
    ctx.syncPollAudienceNote();

    const note = doc.getElementById('pollAudienceNote');
    assert.match(note.textContent, /no difference/);
    assert.match(note.className, /warn/);

    box('members').checked = false;
    ctx.syncPollAudienceNote();
    assert.equal(note.textContent, '', 'Everyone on its own is unremarkable');
});

test('a poll nobody can vote on is called out', () => {
    const { doc, ctx } = setup();
    for (const box of doc.querySelectorAll('.pollAudience')) box.checked = false;

    ctx.syncPollAudienceNote();
    const note = doc.getElementById('pollAudienceNote');
    assert.match(note.textContent, /Nobody can vote/);
    assert.match(note.className, /warn/);
});

// ---------------------------------------------------------------------------
// Rendering a poll
// ---------------------------------------------------------------------------

test('a live poll shows its remaining time and an End early button', () => {
    const { ctx } = setup();
    const card = ctx.renderPoll(poll());

    assert.match(card.querySelector('.poll-state').textContent, /Live/);
    assert.match(card.querySelector('.poll-state').textContent, /closes in/);
    assert.ok(card.querySelector('.btn-danger'), 'a live poll can be ended');
});

test('a permanent poll says so rather than showing a countdown', () => {
    const { ctx } = setup();

    assert.equal(
        ctx.renderPoll(poll({ closesAt: null })).querySelector('.poll-state').textContent,
        '● Live · permanent'
    );
});

test('a finished poll loses the End button and says how it finished', () => {
    const { ctx } = setup();
    const at = new Date('2026-08-20T10:00:00Z').toISOString();

    const early = ctx.renderPoll(poll({ live: false, endedEarly: true, endedAt: at }));
    assert.match(early.querySelector('.poll-state').textContent, /^Ended early/);
    assert.ok(!early.querySelector('.btn-danger'), 'a closed poll cannot be ended again');

    const expired = ctx.renderPoll(poll({ live: false, endedAt: null, closesAt: at }));
    assert.match(expired.querySelector('.poll-state').textContent, /^Closed/);
});

test('the leading option is marked and the others are not', () => {
    const { ctx } = setup();
    const rows = ctx.renderPoll(poll()).querySelectorAll('.poll-bar-row');

    assert.ok(rows[0].classList.contains('leading'));
    assert.ok(!rows[1].classList.contains('leading'));
});

test('bars land on their percentage and carry the vote count', () => {
    const { ctx } = setup();
    const card = ctx.renderPoll(poll());

    assert.equal(card.querySelectorAll('.poll-bar-fill')[0].style.width, '66.7%');
    assert.equal(card.querySelectorAll('.poll-bar-fill')[1].style.width, '33.3%');
    assert.equal(card.querySelectorAll('.poll-bar-count')[0].textContent, '66.7%  (2)');
});

test('nothing leads a poll where every option is on zero', () => {
    const { ctx } = setup();
    const card = ctx.renderPoll(poll({
        voters: 0,
        totalVotes: 0,
        options: [
            { id: 10, label: 'Medieval', votes: 0, percent: 0, leading: false },
            { id: 11, label: 'Modern', votes: 0, percent: 0, leading: false },
        ],
    }));

    for (const row of card.querySelectorAll('.poll-bar-row')) {
        assert.ok(!row.classList.contains('leading'));
    }
    assert.match(card.querySelector('.poll-meta').textContent, /0 voters/);
});

test('one voter is a voter, not voters', () => {
    const { ctx } = setup();

    assert.match(ctx.renderPoll(poll({ voters: 1 })).querySelector('.poll-meta').textContent, /1 voter ·/);
    assert.match(ctx.renderPoll(poll({ voters: 2 })).querySelector('.poll-meta').textContent, /2 voters/);
});

/// Anonymity is not a promise a result this small can keep, and the card has
/// to say so rather than imply otherwise.
test('a result too small to hide anyone admits it', () => {
    const { ctx } = setup();
    const meta = voters => ctx.renderPoll(poll({ voters })).querySelector('.poll-meta').textContent;

    assert.match(meta(1), /identifies them/, 'a one-voter poll must not imply anonymity');
    assert.doesNotMatch(meta(40), /identifies them/, 'a real poll should not carry the warning');
    assert.doesNotMatch(meta(0), /identifies them/, 'nobody has voted, so nobody is identified');
});

test('a multi-answer poll explains why its bars can exceed one hundred', () => {
    const { ctx } = setup();
    const meta = allowMultiple =>
        ctx.renderPoll(poll({ allowMultiple })).querySelector('.poll-meta').textContent;

    assert.match(meta(true), /exceed 100%/);
    assert.doesNotMatch(meta(false), /exceed 100%/, 'a single-answer poll needs no such note');
});

test('the exclusion count shows only when there is one', () => {
    const { ctx } = setup();
    const meta = excludedCount =>
        ctx.renderPoll(poll({ excludedCount })).querySelector('.poll-meta').textContent;

    assert.match(meta(2), /2 excluded/);
    assert.doesNotMatch(meta(0), /excluded/);
});

/// Titles and option labels are typed by people. The panel has no escaping
/// helper, so this pins the choice to build these nodes with textContent.
test('a title containing markup is text, not markup', () => {
    const { ctx } = setup();
    const nasty = '<img src=x onerror=alert(1)>';
    const card = ctx.renderPoll(poll({ title: nasty, description: nasty }));

    assert.equal(card.querySelector('.poll-card-title').textContent, nasty);
    assert.equal(card.querySelector('.poll-card-title').innerHTML, '', 'never parsed as HTML');
    assert.equal(card.querySelector('.poll-desc').innerHTML, '');
});

// ---------------------------------------------------------------------------
// Description clamping
// ---------------------------------------------------------------------------

test('a description that fits gets no Show more toggle', () => {
    const { doc, ctx } = setup();
    const box = doc.getElementById('pollsLive');
    box.appendChild(ctx.renderPoll(poll({ description: 'Short.' })));

    const desc = box.querySelector('.poll-desc');
    desc.scrollHeight = 40;
    desc.clientHeight = 40;

    ctx.attachPollDescToggles(box);
    assert.equal(box.querySelectorAll('.poll-more').length, 0, 'nothing was cut off');
});

test('a clamped description gets a toggle that opens and closes it', () => {
    const { doc, ctx } = setup();
    const box = doc.getElementById('pollsLive');
    box.appendChild(ctx.renderPoll(poll({ description: 'A very long brief.' })));

    const desc = box.querySelector('.poll-desc');
    desc.scrollHeight = 300;
    desc.clientHeight = 60;
    assert.ok(desc.classList.contains('clamped'), 'starts clamped');

    ctx.attachPollDescToggles(box);
    const toggle = box.querySelector('.poll-more');
    assert.ok(toggle, 'an overflowing description needs a toggle');
    assert.equal(toggle.textContent, 'Show more');

    toggle.dispatch('click');
    assert.ok(!desc.classList.contains('clamped'), 'opens in full');
    assert.equal(toggle.textContent, 'Show less');

    toggle.dispatch('click');
    assert.ok(desc.classList.contains('clamped'), 'and closes again');
    assert.equal(toggle.textContent, 'Show more');
});

test('a poll with no description renders no description element at all', () => {
    const { ctx } = setup();
    assert.equal(ctx.renderPoll(poll()).querySelectorAll('.poll-desc').length, 0);
});

// ---------------------------------------------------------------------------
// The pager
// ---------------------------------------------------------------------------

function pagerFetch(page, pages, polls = []) {
    return async () => ({ ok: true, json: async () => ({ polls, page, pages, total: pages * 10 }) });
}

test('the pager hides itself when everything fits on one page', async () => {
    const { doc, ctx } = setup(pagerFetch(1, 1));
    await ctx.loadPolls('live');

    assert.ok(doc.getElementById('pollsLivePager').hidden, 'one page needs no control');
});

test('Previous is off on the first page and Next on the last', async () => {
    let { doc, ctx } = setup(pagerFetch(1, 3));
    await ctx.loadPolls('past');

    let pager = doc.getElementById('pollsPastPager');
    assert.ok(!pager.hidden, 'three pages need the control');
    assert.equal(pager.querySelector('[data-poll-label="past"]').textContent, 'Page 1 of 3');
    assert.ok(pager.querySelector('[data-poll-step="-1"]').disabled, 'no page before the first');
    assert.ok(!pager.querySelector('[data-poll-step="1"]').disabled);

    ({ doc, ctx } = setup(pagerFetch(3, 3)));
    ctx.pollPage.past = 3;
    await ctx.loadPolls('past');

    pager = doc.getElementById('pollsPastPager');
    assert.ok(!pager.querySelector('[data-poll-step="-1"]').disabled);
    assert.ok(pager.querySelector('[data-poll-step="1"]').disabled, 'no page after the last');
});

test('an empty section says which one it is', async () => {
    let { doc, ctx } = setup(pagerFetch(1, 1));
    await ctx.loadPolls('live');
    assert.equal(doc.getElementById('pollsLive').querySelector('.poll-empty').textContent, 'No live polls.');

    ({ doc, ctx } = setup(pagerFetch(1, 1)));
    await ctx.loadPolls('past');
    assert.equal(doc.getElementById('pollsPast').querySelector('.poll-empty').textContent, 'No past polls yet.');
});

test('a failed load says so rather than leaving stale results up', async () => {
    const { doc, ctx } = setup(async () => ({
        ok: false,
        status: 500,
        json: async () => ({ error: 'boom' }),
    }));

    await ctx.loadPolls('live');
    assert.equal(
        doc.getElementById('pollsLive').querySelector('.poll-empty').textContent,
        'Could not load live polls.'
    );
});

test('the page the server settled on is the one the control shows', async () => {
    const { doc, ctx } = setup(pagerFetch(2, 2));
    ctx.pollPage.live = 99;

    await ctx.loadPolls('live');
    assert.equal(ctx.pollPage.live, 2, 'follow the server rather than stay past the end');
    assert.equal(
        doc.getElementById('pollsLivePager').querySelector('[data-poll-label="live"]').textContent,
        'Page 2 of 2'
    );
});

// ---------------------------------------------------------------------------
// Creating a poll
// ---------------------------------------------------------------------------

/** Fills the form, submits it, and reports what reached the network. */
async function submit(over = {}) {
    let sent = null;
    const { doc, ctx, alertText } = setup(async (url, init) => {
        sent = JSON.parse(init.body);
        return { ok: true, json: async () => ({ id: 7 }) };
    });

    ctx.resetPollOptions();
    doc.getElementById('pollTitle').value =
        over.title !== undefined ? over.title : 'New spawn build?';
    doc.getElementById('pollDescription').value = over.description || '';

    const options = over.options || ['Medieval', 'Modern'];
    const list = doc.getElementById('pollOptions');
    while (list.querySelectorAll('.poll-option-row').length < options.length) {
        list.appendChild(ctx.pollOptionRow());
    }
    doc.querySelectorAll('#pollOptions .poll-option').forEach((input, i) => {
        input.value = options[i] !== undefined ? options[i] : '';
    });

    if (over.audiences) {
        for (const box of doc.querySelectorAll('.pollAudience')) {
            box.checked = over.audiences.includes(box.value);
        }
    }

    await ctx.submitPoll({ preventDefault() {} });
    return { sent, alert: alertText(), ctx, doc };
}

test('a well-formed poll is sent as the API expects it', async () => {
    const { sent } = await submit();

    assert.equal(sent.title, 'New spawn build?');
    assert.deepEqual(sent.options, ['Medieval', 'Modern']);
    assert.equal(sent.duration, '3d');
    assert.deepEqual(sent.audiences, ['members']);
    assert.equal(sent.allowMultiple, false);
    assert.deepEqual(sent.exclusions, []);
    assert.equal(sent.description, null, 'an empty description is null, not an empty string');
});

test('a poll with no question is never sent', async () => {
    const { sent, alert } = await submit({ title: '   ' });

    assert.equal(sent, null, 'nothing should reach the server');
    assert.match(alert, /question/);
});

test('a blank option stops the submission rather than being dropped', async () => {
    const { sent, alert } = await submit({ options: ['Medieval', '   '] });

    assert.equal(sent, null);
    assert.match(alert, /every option/);
});

test('two options that say the same thing are refused', async () => {
    const { sent, alert } = await submit({ options: ['Medieval', 'medieval'] });

    assert.equal(sent, null);
    assert.match(alert, /Two options both say/);
});

test('a poll with no audience is refused before it is sent', async () => {
    const { sent, alert } = await submit({ audiences: [] });

    assert.equal(sent, null);
    assert.match(alert, /who can vote/);
});

test('the form is cleared once a poll is open', async () => {
    const { ctx, doc } = await submit();

    assert.deepEqual(ctx.pollExclusions, [], 'exclusions must not carry into the next poll');
    assert.equal(doc.querySelectorAll('.poll-option-row').length, 2, 'options return to the floor');
    assert.equal(ctx.pollPage.live, 1, 'and the live list jumps to where the new poll is');
});

test('a server refusal is shown rather than swallowed', async () => {
    const { doc, ctx, alertText } = setup(async () => ({
        ok: false,
        status: 400,
        json: async () => ({ error: 'No such account: typo' }),
    }));

    ctx.resetPollOptions();
    doc.getElementById('pollTitle').value = 'New spawn build?';
    doc.querySelectorAll('#pollOptions .poll-option').forEach((input, i) => {
        input.value = ['Medieval', 'Modern'][i];
    });

    await ctx.submitPoll({ preventDefault() {} });
    assert.match(alertText(), /No such account: typo/, 'the reason must survive');
    assert.ok(!doc.getElementById('pollCreateBtn').disabled, 'the button must not stay stuck');
});

// ---------------------------------------------------------------------------
// Ending a poll
// ---------------------------------------------------------------------------

test('ending a poll asks first, then refreshes both lists', async () => {
    let ended = false;
    let listed = 0;

    const { ctx } = setup(async (url, init) => {
        if (init && init.method === 'POST') {
            ended = true;
            return { ok: true, json: async () => ({}) };
        }
        listed++;
        return { ok: true, json: async () => ({ polls: [], page: 1, pages: 1, total: 0 }) };
    });

    await ctx.endPoll(poll(), { disabled: false });

    assert.ok(ended, 'the end request should have been made');
    assert.equal(listed, 2, 'both lists are stale once a poll moves between them');
});

test('declining the confirmation ends nothing', async () => {
    let called = false;
    const { ctx } = setup(async () => {
        called = true;
        return { ok: true, json: async () => ({}) };
    });
    ctx.askConfirm = async () => false;

    await ctx.endPoll(poll(), { disabled: false });
    assert.ok(!called, 'saying no must not still end the poll');
});
