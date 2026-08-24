'use strict';
/*
 * The player-facing polls page (private/polls.html), run against the DOM shim.
 *
 * This is the page everyone on the server actually touches, so what is pinned
 * here is what a voter sees and does: which controls a poll offers in each
 * state, that results stay hidden until you answer, that the vote request
 * carries the right body, and that a failure says so rather than looking like
 * a successful vote.
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

const PAGE = path.join(__dirname, '..', '..', 'private', 'polls.html');

// ---------------------------------------------------------------------------
// Lift the page's script
// ---------------------------------------------------------------------------

const html = fs.readFileSync(PAGE, 'utf8');
const open = html.indexOf('<script>');
const close = html.lastIndexOf('</script>');

assert.ok(
    open >= 0 && close > open,
    `could not find a <script> block in ${PAGE}`
);

const source = html.slice(open + '<script>'.length, close);

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/**
 * The page calls load() as soon as it is evaluated. Its fetch is left pending
 * for ever so that first call never reaches the DOM, and each test then drives
 * load() itself with a stub it controls -- otherwise the boot call could land
 * after a test's own and overwrite what was just asserted.
 */
function setup() {
    const doc = buildPage();
    const ctx = {
        document: doc,
        console: { error() {}, warn() {}, log() {} },
        window: {
            matchMedia: () => ({ matches: true }),
            addEventListener() {},
            innerWidth: 1200,
            innerHeight: 800,
            devicePixelRatio: 1,
        },
        requestAnimationFrame: fn => fn(),
        setTimeout: () => 0,
        clearTimeout: () => {},
        setInterval: () => 1,
        clearInterval: () => {},
        Promise, Date, Math, JSON, Number, String, Boolean, Array, Object, isFinite,
        _fetch: () => new Promise(() => {}),
        fetch: (...args) => ctx._fetch(...args),
    };
    ctx.globalThis = ctx;
    vm.createContext(ctx);
    vm.runInContext(source, ctx);

    return { doc, ctx };
}

/** Only the ids and containers the page's script reaches for. */
function buildPage() {
    const doc = createDocument();

    const add = (parent, tag, id, cls) => {
        const el = doc.createElement(tag);
        if (id) el.id = id;
        if (cls) el.className = cls;
        parent.appendChild(el);
        return el;
    };

    const who = add(doc.body, 'span', 'who', 'who');
    who.hidden = true;
    add(who, 'strong', 'who-name');

    add(doc.body, 'span', 'open-tally');
    add(doc.body, 'span', 'past-tally');
    add(doc.body, 'div', 'open-polls', 'polls');
    add(doc.body, 'div', 'past-polls', 'polls');

    const toast = add(doc.body, 'div', 'toast');
    add(toast, 'svg', null, 'tick');
    add(toast, 'span', 'toast-text');

    return doc;
}

const DAY = 86400000;

/** A poll shaped the way /api/polls returns one. */
const poll = (over = {}) => Object.assign({
    id: 1,
    title: 'What should Season 9 be?',
    description: null,
    allowMultiple: false,
    live: true,
    endedEarly: false,
    closesAt: new Date(Date.now() + 2 * DAY).toISOString(),
    endedAt: null,
    audienceLabels: ['Members'],
    voters: 84,
    selected: [],
    canVote: true,
    options: [
        { id: 11, label: 'Heavy modpack', votes: 31, percent: 36.9 },
        { id: 12, label: 'Close to vanilla', votes: 53, percent: 63.1 },
    ],
}, over);

/** A fetch stub answering the page's two listing calls. */
function listing(live = [], past = [], you = 'Joe') {
    return async url => ({
        ok: true,
        json: async () => ({ polls: String(url).includes('past') ? past : live, you }),
    });
}

// ---------------------------------------------------------------------------
// Remaining time
// ---------------------------------------------------------------------------

test('remaining time reads in days, hours and minutes', () => {
    const { ctx } = setup();
    // Half a minute of slack: the display floors, so a target of exactly four
    // hours lands on "3h" if any time passes before the call.
    const at = ms => new Date(Date.now() + ms + 30000).toISOString();

    assert.equal(ctx.remaining(at(2 * DAY + 4 * 3600000)), '2d 4h');
    assert.equal(ctx.remaining(at(3 * DAY)), '3d');
    assert.equal(ctx.remaining(at(3 * 3600000 + 20 * 60000)), '3h 20m');
    assert.equal(ctx.remaining(at(43 * 60000)), '43m');
});

test('a poll about to close never reads as a negative time', () => {
    const { ctx } = setup();

    assert.equal(ctx.remaining(new Date(Date.now() + 20000).toISOString()), 'under a minute');
    assert.equal(ctx.remaining(new Date(Date.now() - 60000).toISOString()), 'closing');
    assert.equal(ctx.remaining('not a date'), 'closing', 'an unparseable date must not print NaN');
});

// ---------------------------------------------------------------------------
// What each state offers
// ---------------------------------------------------------------------------

test('an unanswered poll offers choices, not results', () => {
    const { ctx } = setup();
    const card = ctx.renderPoll(poll());

    assert.equal(card.querySelectorAll('.choice').length, 2, 'both options are selectable');
    assert.equal(card.querySelectorAll('.result').length, 0, 'no results before you answer');
    assert.ok(card.querySelector('.btn-vote'), 'and a way to answer');
});

/// Showing the running total first anchors people to whatever is winning, so
/// the page withholds it — and says why, rather than leaving a gap.
test('results are withheld until you answer, and the absence is explained', () => {
    const { ctx } = setup();
    const card = ctx.renderPoll(poll());

    const hint = card.querySelectorAll('.thin').map(n => n.textContent).join(' ');
    assert.match(hint, /Results appear once you have voted/);

    const shown = card.textContent;
    assert.ok(!shown.includes('36.9'), 'no percentage may leak before voting');
    assert.ok(!shown.includes('63.1'), 'no percentage may leak before voting');
});

test('the vote button stays disabled until something is picked', () => {
    const { ctx } = setup();
    const card = ctx.renderPoll(poll());
    const button = card.querySelector('.btn-vote');

    assert.ok(button.disabled, 'nothing is selected yet');

    const input = card.querySelectorAll('.choice input')[0];
    input.checked = true;
    input.dispatch('change');

    assert.ok(!button.disabled, 'picking an option enables it');
});

test('a multi-answer poll uses checkboxes and says so on the button', () => {
    const { ctx } = setup();

    const single = ctx.renderPoll(poll());
    assert.equal(single.querySelectorAll('.choice input')[0].type, 'radio');
    assert.equal(single.querySelector('.btn-vote').textContent, 'Vote');

    const multi = ctx.renderPoll(poll({ allowMultiple: true }));
    assert.equal(multi.querySelectorAll('.choice input')[0].type, 'checkbox');
    assert.equal(multi.querySelector('.btn-vote').textContent, 'Submit answers');
    assert.match(multi.querySelector('.foot-note').textContent, /percentages are of voters/);
});

test('an answered poll shows results, your pick, and a way to change it', () => {
    const { ctx } = setup();
    const card = ctx.renderPoll(poll({ selected: [11] }));

    assert.equal(card.querySelectorAll('.result').length, 2);
    assert.equal(card.querySelectorAll('.choice').length, 0, 'the ballot is put away');

    const yours = card.querySelectorAll('.yours');
    assert.equal(yours.length, 1, 'exactly one option is marked as yours');
    assert.match(card.querySelectorAll('.result')[0].textContent, /Heavy modpack/);

    assert.ok(!card.querySelector('.btn-vote'), 'no second vote to cast');
    assert.match(card.querySelector('.btn-quiet').textContent, /Change my vote/);

    // The bars have to reach the figure beside them, or the picture and the
    // number disagree about the same result.
    const widths = card.querySelectorAll('.fill').map(f => f.style.getPropertyValue('--w'));
    assert.deepEqual(widths, ['36.9%', '63.1%']);
});

test('changing your vote returns the ballot', () => {
    const { doc, ctx } = setup();
    const answered = poll({ selected: [11] });

    const card = ctx.renderPoll(answered);
    doc.body.appendChild(card);
    card.querySelector('.btn-quiet').dispatch('click');

    const fresh = doc.body.querySelectorAll('.poll')[0];
    assert.equal(fresh.querySelectorAll('.choice').length, 2, 'the options come back');
    assert.deepEqual(answered.selected, [], 'and the old answer is cleared');
});

test('the leading option is marked, and nothing leads an empty poll', () => {
    const { ctx } = setup();

    const rows = ctx.renderPoll(poll({ selected: [11] })).querySelectorAll('.result');
    assert.ok(!rows[0].classList.contains('leading'), '31 votes does not beat 53');
    assert.ok(rows[1].classList.contains('leading'));

    const empty = ctx.renderPoll(poll({
        live: false,
        options: [
            { id: 11, label: 'A', votes: 0, percent: 0 },
            { id: 12, label: 'B', votes: 0, percent: 0 },
        ],
    }));
    for (const row of empty.querySelectorAll('.result')) {
        assert.ok(!row.classList.contains('leading'), 'a poll nobody answered has no winner');
    }
});

test('a closed poll shows its results and offers no controls', () => {
    const { ctx } = setup();
    const card = ctx.renderPoll(poll({ live: false, canVote: false, selected: [12] }));

    assert.equal(card.dataset.state, 'closed');
    assert.equal(card.querySelectorAll('.result').length, 2, 'the result is the point');
    assert.ok(!card.querySelector('.btn-vote'));
    assert.ok(!card.querySelector('.btn-quiet'));
    assert.match(card.querySelector('.state').textContent, /Closed/);
});

test('a poll ended early says so rather than looking expired', () => {
    const { ctx } = setup();
    const card = ctx.renderPoll(poll({
        live: false,
        canVote: false,
        endedEarly: true,
        endedAt: new Date(Date.now() - 3 * DAY).toISOString(),
    }));

    assert.match(card.querySelector('.state').textContent, /Ended early/);
});

test('a poll you may not answer explains itself instead of showing a ballot', () => {
    const { ctx } = setup();
    const card = ctx.renderPoll(poll({ canVote: false, audienceLabels: ['OGs'] }));

    assert.equal(card.dataset.state, 'locked');
    assert.equal(card.querySelectorAll('.choice').length, 0);
    assert.equal(card.querySelectorAll('.result').length, 0, 'and no peeking at results');
    assert.match(card.querySelector('.locked-note').textContent, /OGs only/);
});

/// Titles and option labels are written by people; the page builds every node
/// with textContent, and this is what stops that from being undone.
test('a title containing markup is text, not markup', () => {
    const { ctx } = setup();
    const nasty = '<img src=x onerror=alert(1)>';
    const card = ctx.renderPoll(poll({ title: nasty, description: nasty }));

    assert.equal(card.querySelector('.poll-title').textContent, nasty);
    assert.equal(card.querySelector('.poll-title').innerHTML, '');
    assert.equal(card.querySelector('.poll-desc').innerHTML, '');
});

// ---------------------------------------------------------------------------
// Casting a vote
// ---------------------------------------------------------------------------

test('voting posts the chosen ids and renders what the server returns', async () => {
    const { doc, ctx } = setup();
    let sent = null;

    const answered = poll({ selected: [11], voters: 85, options: [
        { id: 11, label: 'Heavy modpack', votes: 32, percent: 37.6 },
        { id: 12, label: 'Close to vanilla', votes: 53, percent: 62.4 },
    ] });

    ctx._fetch = async (url, init) => {
        sent = { url, body: JSON.parse(init.body), method: init.method };
        return { ok: true, json: async () => answered };
    };

    const subject = poll();
    const card = ctx.renderPoll(subject);
    doc.body.appendChild(card);

    const input = card.querySelectorAll('.choice input')[0];
    input.checked = true;
    input.dispatch('change');
    await ctx.castVote(subject, card);

    assert.equal(sent.method, 'POST');
    assert.equal(sent.url, '/api/polls/1/vote');
    assert.deepEqual(sent.body, { optionIds: [11] }, 'the ballot, as the API expects it');

    const fresh = doc.body.querySelectorAll('.poll')[0];
    assert.equal(fresh.querySelectorAll('.result').length, 2, 'results replace the ballot');
    assert.match(fresh.querySelector('.foot-note').textContent, /85 votes/,
        'and the count comes from the server, not from adding one locally');
});

test('a multi-answer vote sends every ticked option', async () => {
    const { doc, ctx } = setup();
    let sent = null;
    ctx._fetch = async (url, init) => {
        sent = JSON.parse(init.body);
        return { ok: true, json: async () => poll({ allowMultiple: true, selected: [11, 12] }) };
    };

    const subject = poll({ allowMultiple: true });
    const card = ctx.renderPoll(subject);
    doc.body.appendChild(card);

    for (const input of card.querySelectorAll('.choice input')) {
        input.checked = true;
        input.dispatch('change');
    }
    await ctx.castVote(subject, card);

    assert.deepEqual(sent, { optionIds: [11, 12] });
});

test('a refused vote says why and leaves the ballot usable', async () => {
    const { doc, ctx } = setup();
    ctx._fetch = async () => ({
        ok: false,
        status: 409,
        json: async () => ({ error: 'This poll has closed' }),
    });

    const subject = poll();
    const card = ctx.renderPoll(subject);
    doc.body.appendChild(card);

    const input = card.querySelectorAll('.choice input')[0];
    input.checked = true;
    input.dispatch('change');
    await ctx.castVote(subject, card);

    assert.match(doc.getElementById('toast-text').textContent, /This poll has closed/,
        'the server said why; the page must repeat it');

    const button = card.querySelector('.btn-vote');
    assert.ok(!button.disabled, 'the button must not stay stuck on "Sending"');
    assert.equal(button.textContent, 'Vote');
    assert.equal(card.querySelectorAll('.result').length, 0, 'and no results are invented');
});

test('a vote with nothing selected is not sent', async () => {
    const { doc, ctx } = setup();
    let called = false;
    ctx._fetch = async () => { called = true; return { ok: true, json: async () => poll() }; };

    const subject = poll();
    const card = ctx.renderPoll(subject);
    doc.body.appendChild(card);

    await ctx.castVote(subject, card);
    assert.ok(!called, 'an empty ballot never reaches the server');
});

// ---------------------------------------------------------------------------
// Loading the page
// ---------------------------------------------------------------------------

test('the two halves land in their own sections', async () => {
    const { doc, ctx } = setup();
    ctx._fetch = listing(
        [poll({ id: 1 }), poll({ id: 2 })],
        [poll({ id: 3, live: false })]
    );

    await ctx.load();

    assert.equal(doc.getElementById('open-polls').querySelectorAll('.poll').length, 2);
    assert.equal(doc.getElementById('past-polls').querySelectorAll('.poll').length, 1);
    assert.equal(doc.getElementById('open-tally').textContent, '2 polls');
    assert.equal(doc.getElementById('past-tally').textContent, '1 poll');
});

/// The listings only ever contain polls you are entitled to, so anything still
/// open is answerable — and anything closed is not, however it arrived.
test('open polls arrive votable and closed ones do not', async () => {
    const { doc, ctx } = setup();
    ctx._fetch = listing([poll({ id: 1 })], [poll({ id: 3, live: false })]);

    await ctx.load();

    assert.ok(doc.getElementById('open-polls').querySelector('.btn-vote'), 'open: answerable');
    assert.ok(!doc.getElementById('past-polls').querySelector('.btn-vote'), 'closed: not');
    assert.equal(doc.getElementById('past-polls').querySelectorAll('.result').length, 2,
        'closed polls show their result');
});

test('the signed-in name comes from the session, not the markup', async () => {
    const { doc, ctx } = setup();
    ctx._fetch = listing([], [], 'MapiccOnMC');

    assert.ok(doc.getElementById('who').hidden, 'hidden until the session is known');

    await ctx.load();

    assert.equal(doc.getElementById('who-name').textContent, 'MapiccOnMC');
    assert.ok(!doc.getElementById('who').hidden);
});

test('an unnamed session leaves the pill hidden rather than blank', async () => {
    const { doc, ctx } = setup();
    // null, not undefined: undefined would fall through to listing()'s default.
    ctx._fetch = listing([], [], null);

    await ctx.load();
    assert.ok(doc.getElementById('who').hidden, 'no name, no pill');
    assert.equal(doc.getElementById('who-name').textContent, '');
});

test('empty sections say which one is empty', async () => {
    const { doc, ctx } = setup();
    ctx._fetch = listing([], []);

    await ctx.load();

    assert.match(doc.getElementById('open-polls').querySelector('.empty').textContent,
        /Nothing open right now/);
    assert.match(doc.getElementById('past-polls').querySelector('.empty').textContent,
        /No polls have closed yet/);
});

/// "We could not ask" and "there is nothing to show" look identical if a
/// failure is allowed to render as an empty list.
test('a failed load is reported, not shown as an empty page', async () => {
    const { doc, ctx } = setup();
    ctx._fetch = async () => ({ ok: false, status: 500, json: async () => ({}) });

    await ctx.load();

    const message = doc.getElementById('open-polls').querySelector('.empty').textContent;
    assert.match(message, /Could not load polls/);
    assert.doesNotMatch(message, /Nothing open/, 'must not read as "no polls exist"');
});

test('both halves are requested, each by its own status', async () => {
    const { ctx } = setup();
    const asked = [];
    ctx._fetch = async url => {
        asked.push(String(url));
        return { ok: true, json: async () => ({ polls: [], you: 'Joe' }) };
    };

    await ctx.load();

    assert.ok(asked.some(u => u.includes('status=live')), `no live request: ${asked}`);
    assert.ok(asked.some(u => u.includes('status=past')), `no past request: ${asked}`);
});
