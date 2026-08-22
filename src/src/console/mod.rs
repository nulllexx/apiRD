//! Live server console plumbing.
//!
//! One background task tails Minecraft's `latest.log` and pushes each complete
//! line into a [`LogHub`]. HTTP clients subscribe to the hub over SSE, so N
//! viewers cost N channel receivers rather than N open file handles and N
//! independent read loops.
//!
//! The pieces here are deliberately split into small pure functions
//! (line splitting, ANSI stripping, SSE framing) because the surrounding I/O —
//! a rotating log file and a long-lived HTTP stream — is awkward to test
//! directly, while the parsing that actually goes wrong is not.

pub mod audit;
pub mod control;
pub mod inventory;
pub mod mod_assets;
pub mod models;
pub mod players;
pub mod presence;
pub mod snbt;
pub mod stats;
pub mod tail;
pub mod textures;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

/// The two log sources the console can show.
///
/// They differ in what they can capture: `latest.log` is what the server chose
/// to write, while the captured stdout additionally carries JVM crashes, EULA
/// refusals and start-up output that never reach the log file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSource {
    ServerLog,
    Stdout,
}

impl LogSource {
    /// Parse the `?source=` query value, defaulting to the server log.
    pub fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("stdout") => LogSource::Stdout,
            _ => LogSource::ServerLog,
        }
    }
}

/// Both live log streams, each fed by its own tail task — plus the audit
/// channel, which is fed by this API rather than by the server.
pub struct Consoles {
    pub server_log: Arc<LogHub>,
    pub stdout: Arc<LogHub>,
    /// Attribution lines: who ran what, as it happens.
    ///
    /// A hub of its own, and not a [`LogSource`], because it is not a view of
    /// the server's output — it is a claim this API makes about its own
    /// callers. Keeping it off the log channels is what stops a chat message
    /// from arriving as one. See [`audit`].
    pub audit: Arc<LogHub>,
}

/// How much attribution a reconnecting console replays.
///
/// Much smaller than the log buffer, and deliberately so: the two hold lines at
/// wildly different rates. A busy server fills a few hundred log lines in
/// minutes, while a few hundred console actions can span days, so matching the
/// sizes would open every reload with a wall of old attribution above the
/// oldest log line. Bounding the buffer is the right place to solve that --
/// filtering by age at replay time cuts off the entry that explains the very
/// first log line.
const AUDIT_BACKLOG_LINES: usize = 40;

impl Consoles {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            server_log: LogHub::new(capacity),
            stdout: LogHub::new(capacity),
            audit: LogHub::new(AUDIT_BACKLOG_LINES),
        })
    }

    pub fn get(&self, source: LogSource) -> &Arc<LogHub> {
        match source {
            LogSource::ServerLog => &self.server_log,
            LogSource::Stdout => &self.stdout,
        }
    }
}

/// Arrival order, counted across every hub in the process.
///
/// The console shows two channels at once — the server's log and this API's own
/// attribution lines — and a reconnecting client replays both. Replaying them
/// one after the other groups them, which is exactly wrong: the attribution for
/// a command belongs next to the log line the command produced, not in a block
/// at the bottom. A shared counter is what lets the two buffers be merged back
/// into the order they actually arrived in.
///
/// Arrival order, not the order things happened on the server. The log is
/// polled, so a line can be written to the file before an audit entry and still
/// be read after it — but that is also the order a viewer who never refreshed
/// would have seen them, and matching that is the point.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One buffered line, with the order it arrived in.
///
/// Dereferences to its text, so a caller that only wants the line can ignore
/// the sequence entirely.
#[derive(Debug, Clone)]
pub struct Line {
    pub seq: u64,
    pub text: Arc<str>,
}

impl AsRef<str> for Line {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

/// Buffered lines per subscriber before the broadcast channel starts dropping.
/// A client that falls this far behind gets a `Lagged` notice rather than a
/// silently truncated stream.
const CHANNEL_CAPACITY: usize = 1024;

/// A fan-out point for console lines: a bounded replay buffer plus a broadcast
/// channel for live delivery.
pub struct LogHub {
    tx: broadcast::Sender<Arc<str>>,
    backlog: RwLock<VecDeque<Line>>,
    capacity: usize,
}

impl LogHub {
    pub fn new(capacity: usize) -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(CHANNEL_CAPACITY);
        Arc::new(Self {
            tx,
            backlog: RwLock::new(VecDeque::with_capacity(capacity)),
            capacity: capacity.max(1),
        })
    }

    /// Append a line, evicting the oldest once the replay buffer is full.
    pub fn push(&self, line: impl Into<Arc<str>>) {
        let line: Arc<str> = line.into();
        // A poisoned lock would otherwise disable the console for the process
        // lifetime; the buffer holds no invariant worth preserving, so recover.
        let mut backlog = self.backlog.write().unwrap_or_else(|e| e.into_inner());
        while backlog.len() >= self.capacity {
            backlog.pop_front();
        }
        backlog.push_back(Line {
            seq: SEQUENCE.fetch_add(1, Ordering::Relaxed),
            text: Arc::clone(&line),
        });
        // Sent while the write lock is still held. Combined with `subscribe`
        // taking its snapshot under the read lock, this guarantees a new
        // subscriber sees every line exactly once: either the line is already
        // in the snapshot, or the subscription exists before the line is sent.
        let _ = self.tx.send(line);
    }

    /// Snapshot the replay buffer and subscribe to live lines atomically.
    pub fn subscribe(&self) -> (Vec<Line>, broadcast::Receiver<Arc<str>>) {
        let backlog = self.backlog.read().unwrap_or_else(|e| e.into_inner());
        let rx = self.tx.subscribe();
        (backlog.iter().cloned().collect(), rx)
    }

    #[cfg(test)]
    fn backlog_len(&self) -> usize {
        self.backlog.read().unwrap().len()
    }
}

/// Accumulates raw bytes and yields complete lines.
///
/// Operates on bytes rather than `str` on purpose: a read chunk can end in the
/// middle of a multi-byte UTF-8 sequence, and decoding per-chunk would turn
/// that into a replacement character. Line boundaries are always valid UTF-8
/// boundaries, so decoding is deferred until a whole line is in hand.
#[derive(Default)]
pub struct LineSplitter {
    carry: Vec<u8>,
}

/// A line longer than this is emitted as-is rather than buffered forever — a
/// log file containing no newline at all must not grow the carry unbounded.
const MAX_CARRY: usize = 1024 * 1024;

impl LineSplitter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk, returning every line completed by it. The trailing
    /// partial line (if any) is retained for the next call.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.carry.extend_from_slice(chunk);
        let mut lines = Vec::new();
        let mut start = 0;

        while let Some(offset) = self.carry[start..].iter().position(|&b| b == b'\n') {
            let end = start + offset;
            lines.push(decode_line(&self.carry[start..end]));
            start = end + 1;
        }
        self.carry.drain(..start);

        if self.carry.len() > MAX_CARRY {
            lines.push(decode_line(&self.carry));
            self.carry.clear();
        }
        lines
    }

    /// Drop any partial line. Used when the underlying file is rotated or
    /// truncated, where the retained bytes no longer continue anything.
    pub fn reset(&mut self) {
        self.carry.clear();
    }
}

fn decode_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    ansi_to_section(text.trim_end_matches('\r'))
}

/// The ANSI foreground colours Minecraft's console appender emits, and the
/// section code each one means.
///
/// The order is the terminal's, not the game's: ANSI numbers its colours
/// black-red-green-yellow-blue-magenta-cyan-white, which is a different
/// sequence from Minecraft's. Mapping by position rather than by name is how
/// blue and green get swapped, so this pairs them explicitly.
const ANSI_COLOURS: [(u32, char); 16] = [
    (30, '0'), // black
    (34, '1'), // dark blue
    (32, '2'), // dark green
    (36, '3'), // dark aqua
    (31, '4'), // dark red
    (35, '5'), // dark purple
    (33, '6'), // gold
    (37, '7'), // gray
    (90, '8'), // dark gray
    (94, '9'), // blue
    (92, 'a'), // green
    (96, 'b'), // aqua
    (91, 'c'), // red
    (95, 'd'), // light purple
    (93, 'e'), // yellow
    (97, 'f'), // white
];

/// Rewrite ANSI colour escapes as Minecraft section codes, dropping the rest.
///
/// The server's console appender renders rank prefixes and chat colours as ANSI
/// before writing them to stdout — which is why `docker compose logs` shows
/// them in colour. This used to call [`strip_ansi`], which threw that away, so
/// every line reached the browser grey. Deleting the codes was the right call at
/// the time, because the alternative then was rendering them as literal
/// `[0;32m` noise; the panel can render section codes now, so translating beats
/// deleting.
///
/// Section codes rather than passing the escapes through, because the panel
/// already has one renderer for those — the same one the item tooltip and the
/// RCON echo use — and a second colour syntax in the same stream would mean two.
///
/// Everything that is not a colour or a style still goes: cursor movement,
/// window titles and the rest have no meaning in a browser and would show as
/// gibberish.
pub fn ansi_to_section(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI — runs until a byte in the @..~ range. Only SGR (`m`) can
            // carry a colour; anything else is dropped as before.
            Some('[') => {
                let mut params = String::new();
                let mut final_byte = None;
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        final_byte = Some(c);
                        break;
                    }
                    params.push(c);
                }
                if final_byte == Some('m') {
                    push_sgr(&params, &mut out);
                }
            }
            // OSC — terminated by BEL or by ST (ESC \).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            // A two-character escape; the second character is the whole thing.
            Some(_) => {}
            None => {}
        }
    }

    out
}

/// Translate the parameters of one SGR escape.
///
/// A single escape can carry several, semicolon-separated and applied in order
/// — `ESC[0;32m` is a reset followed by a colour — so each is handled in turn
/// rather than only the first.
fn push_sgr(params: &str, out: &mut String) {
    let mut parts = params.split(';');

    while let Some(part) = parts.next() {
        // An omitted parameter means zero: `ESC[m` is `ESC[0m`.
        let code: u32 = if part.is_empty() {
            0
        } else {
            match part.parse() {
                Ok(code) => code,
                Err(_) => continue,
            }
        };

        match code {
            0 => out.push_str("\u{a7}r"),
            1 => out.push_str("\u{a7}l"),
            3 => out.push_str("\u{a7}o"),
            4 => out.push_str("\u{a7}n"),
            9 => out.push_str("\u{a7}m"),
            // Extended colour: `38;2;r;g;b` is 24-bit, `38;5;n` is indexed.
            // Paper emits the 24-bit form for hex rank colours, which most
            // networks now use, so this is not a theoretical branch.
            38 | 48 => match parts.next().and_then(|v| v.parse::<u32>().ok()) {
                Some(2) => {
                    let mut rgb = [0u8; 3];
                    for channel in &mut rgb {
                        *channel = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                    }
                    // Only a foreground colour has a section-code equivalent;
                    // a background one is dropped rather than painted as text.
                    if code == 38 {
                        push_hex(rgb, out);
                    }
                }
                // Indexed colour eats one parameter and is not translated: the
                // 256-colour cube has no section code, and the nearest of
                // sixteen would be a guess presented as a fact.
                Some(5) => {
                    parts.next();
                }
                _ => {}
            },
            _ => {
                if let Some((_, section)) = ANSI_COLOURS.iter().find(|(ansi, _)| *ansi == code) {
                    out.push('\u{a7}');
                    out.push(*section);
                }
            }
        }
    }
}

/// Write a 24-bit colour in the `\u{a7}x\u{a7}r\u{a7}r\u{a7}g\u{a7}g\u{a7}b\u{a7}b` form.
///
/// Minecraft's own encoding for hex in a section-code string, introduced by
/// BungeeCord and understood everywhere since: one section code per hex digit,
/// behind a leading `x`. Using it means the panel learns one syntax rather than
/// acquiring a bespoke one.
fn push_hex(rgb: [u8; 3], out: &mut String) {
    out.push_str("\u{a7}x");
    for channel in rgb {
        for digit in [channel >> 4, channel & 0x0f] {
            out.push('\u{a7}');
            out.push(char::from_digit(u32::from(digit), 16).unwrap_or('0'));
        }
    }
}

/// Remove ANSI escape sequences.
///
/// The `minecraft` service runs with `tty: true`, so `docker logs` output (and
/// some plugin output in the log file) carries SGR colour codes that would
/// otherwise render as literal `[0;32m` noise in the browser.
pub fn strip_ansi(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();

    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        match chars.next() {
            // CSI — runs until a byte in the @..~ range (e.g. the `m` of SGR).
            Some('[') => {
                for c in chars.by_ref() {
                    if ('\x40'..='\x7e').contains(&c) {
                        break;
                    }
                }
            }
            // OSC — terminated by BEL or by ST (ESC \).
            Some(']') => {
                while let Some(c) = chars.next() {
                    if c == '\x07' {
                        break;
                    }
                    if c == '\x1b' {
                        chars.next();
                        break;
                    }
                }
            }
            // A two-character escape; the second character is the whole thing.
            Some(_) => {}
            None => {}
        }
    }
    out
}

/// Remove Minecraft's legacy formatting codes: a section sign followed by one
/// character.
///
/// The console *renders* these rather than stripping them, so this exists for
/// the code that has to read command output as data — `list` and `tps` replies
/// arrive coloured, and plugins reformat them freely, so the parsers work on
/// plain text.
pub fn strip_formatting(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars();

    while let Some(c) = chars.next() {
        if c == '\u{a7}' {
            // Drop the code character too; a trailing lone sign drops itself.
            chars.next();
        } else {
            out.push(c);
        }
    }
    out
}

/// Render one console line as an SSE event.
///
/// Every physical line needs its own `data: ` prefix — a bare newline inside
/// the payload would otherwise terminate the event early and desynchronise the
/// stream for everything that follows.
pub fn sse_frame(line: &str) -> String {
    let mut out = String::with_capacity(line.len() + 8);
    for part in line.split('\n') {
        out.push_str("data: ");
        out.push_str(part.trim_end_matches('\r'));
        out.push('\n');
    }
    out.push('\n');
    out
}

/// Render one line as a *named* SSE event.
///
/// The event name is what separates channels that must not be confusable. A
/// browser dispatches `event: audit` frames only to a listener registered for
/// "audit", and nothing in the payload can move a frame between names — the
/// name is written here, by this API, from a `&'static str`.
///
/// That is what makes an attribution line unforgeable from inside the game.
/// Anything that can make the server print a line can print one that *reads*
/// like an attribution, and on a single channel that would be indistinguishable
/// from the real thing. Server output goes out unnamed and lands in the log
/// view; attribution goes out named and lands nowhere else.
pub fn sse_event(name: &'static str, line: &str) -> String {
    let mut out = String::with_capacity(line.len() + name.len() + 16);
    out.push_str("event: ");
    out.push_str(name);
    out.push('\n');
    out.push_str(&sse_frame(line));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_formatting_removes_the_code_and_its_argument() {
        assert_eq!(
            strip_formatting("\u{a7}6There are \u{a7}c0\u{a7}6 players"),
            "There are 0 players"
        );
    }

    #[test]
    fn strip_formatting_drops_a_trailing_lone_sign() {
        // Truncated output must not leave the sign behind as literal text.
        assert_eq!(strip_formatting("done\u{a7}"), "done");
    }

    #[test]
    fn strip_formatting_leaves_plain_text_alone() {
        assert_eq!(strip_formatting("There are 2 players"), "There are 2 players");
    }

    #[test]
    fn sse_frame_wraps_a_simple_line() {
        assert_eq!(sse_frame("hello"), "data: hello\n\n");
    }

    #[test]
    fn sse_frame_prefixes_every_physical_line() {
        // Without a prefix per line the blank line inside the payload would end
        // the event early and desync every following frame.
        assert_eq!(sse_frame("a\nb"), "data: a\ndata: b\n\n");
    }

    #[test]
    fn sse_frame_strips_carriage_returns() {
        assert_eq!(sse_frame("a\r\nb\r"), "data: a\ndata: b\n\n");
    }

    #[test]
    fn sse_frame_handles_empty_input() {
        assert_eq!(sse_frame(""), "data: \n\n");
    }

    #[test]
    fn sse_event_names_the_frame() {
        assert_eq!(sse_event("audit", "hello"), "event: audit\ndata: hello\n\n");
    }

    /// The name is written once, before the payload. A payload that tried to
    /// declare its own name would be sending `data: event: ...`, which is data.
    #[test]
    fn a_payload_cannot_smuggle_its_own_event_name() {
        let forged = sse_event("audit", "event: something-else");

        assert_eq!(forged, "event: audit\ndata: event: something-else\n\n");
        assert_eq!(forged.matches("event: audit").count(), 1);
        assert!(forged.starts_with("event: audit\n"));
    }

    /// A multi-line payload gets one name and several data lines, not one
    /// event per line — otherwise half of it would arrive unnamed.
    #[test]
    fn a_multi_line_payload_stays_one_named_event() {
        assert_eq!(
            sse_event("audit", "a\nb"),
            "event: audit\ndata: a\ndata: b\n\n"
        );
    }

    /* -------------------------------------------- ANSI to section codes */

    /// The case this exists for. `docker compose logs` shows a rank prefix in
    /// colour because the server's console appender already wrote it as ANSI;
    /// the panel used to strip that and render the line grey.
    #[test]
    fn a_coloured_rank_prefix_survives_as_section_codes() {
        assert_eq!(
            ansi_to_section("\x1b[0;32m[CITIZEN]\x1b[0m Joe: hello"),
            "\u{a7}r\u{a7}2[CITIZEN]\u{a7}r Joe: hello"
        );
    }

    /// ANSI numbers its colours in a different order from Minecraft, so a
    /// positional mapping silently swaps blue with green. These four are the
    /// pairs that get confused.
    #[test]
    fn the_blues_and_greens_map_to_the_right_codes() {
        for (ansi, section) in [(34, '1'), (32, '2'), (94, '9'), (92, 'a')] {
            assert_eq!(
                ansi_to_section(&format!("\x1b[{ansi}mx")),
                format!("\u{a7}{section}x"),
                "ANSI {ansi}"
            );
        }
    }

    #[test]
    fn every_ansi_colour_has_a_distinct_section_code() {
        let codes: std::collections::HashSet<char> =
            ANSI_COLOURS.iter().map(|(_, code)| *code).collect();
        assert_eq!(codes.len(), 16, "no two colours may share a code");
    }

    /// One escape can carry several parameters, applied in order. Handling only
    /// the first would drop the colour in `ESC[0;32m`, which is the exact shape
    /// Minecraft emits.
    #[test]
    fn every_parameter_of_one_escape_is_applied() {
        assert_eq!(
            ansi_to_section("\x1b[1;4;31mloud\x1b[0m"),
            "\u{a7}l\u{a7}n\u{a7}4loud\u{a7}r"
        );
    }

    #[test]
    fn styles_become_their_section_codes() {
        assert_eq!(ansi_to_section("\x1b[1mb"), "\u{a7}lb");
        assert_eq!(ansi_to_section("\x1b[3mi"), "\u{a7}oi");
        assert_eq!(ansi_to_section("\x1b[4mu"), "\u{a7}nu");
        assert_eq!(ansi_to_section("\x1b[9ms"), "\u{a7}ms");
    }

    /// `ESC[m` with no parameter is a reset.
    #[test]
    fn an_empty_parameter_is_a_reset() {
        assert_eq!(ansi_to_section("\x1b[mplain"), "\u{a7}rplain");
    }

    /// Paper emits 24-bit colour for the hex rank colours most networks use
    /// now, so this is not a theoretical branch.
    #[test]
    fn a_24_bit_colour_becomes_the_hex_section_form() {
        // #FFAA00, written the way BungeeCord encodes hex.
        assert_eq!(
            ansi_to_section("\x1b[38;2;255;170;0mgold"),
            "\u{a7}x\u{a7}f\u{a7}f\u{a7}a\u{a7}a\u{a7}0\u{a7}0gold"
        );
    }

    #[test]
    fn a_24_bit_background_is_dropped_rather_than_painted() {
        // 48 is a background colour, which has no section-code equivalent.
        assert_eq!(ansi_to_section("\x1b[48;2;255;0;0mtext"), "text");
    }

    /// The 256-colour cube has no section code, and the nearest of sixteen
    /// would be a guess. It must consume its parameter and emit nothing.
    #[test]
    fn an_indexed_colour_is_skipped_without_eating_the_text() {
        assert_eq!(ansi_to_section("\x1b[38;5;213mtext"), "text");
    }

    /// Anything that is not a colour still has to go: cursor movement and
    /// window titles have no meaning in a browser.
    #[test]
    fn non_colour_escapes_are_still_removed() {
        assert_eq!(ansi_to_section("\x1b[2Ktext"), "text");
        assert_eq!(ansi_to_section("\x1b]0;title\x07text"), "text");
        assert_eq!(ansi_to_section("\x1b[1;1Htext"), "text");
    }

    #[test]
    fn plain_text_is_untouched() {
        let plain = "[12:00:00] [Server thread/INFO]: Done (1.234s)!";
        assert_eq!(ansi_to_section(plain), plain);
    }

    /// A line that already carries section codes — an RCON reply echoed into
    /// the log, say — must pass through rather than being escaped again.
    #[test]
    fn existing_section_codes_pass_through() {
        let already = "\u{a7}6There are \u{a7}c1\u{a7}6 players online.";
        assert_eq!(ansi_to_section(already), already);
    }

    /// An escape cut off by a chunk boundary must not swallow the rest of the
    /// line looking for a terminator that never comes.
    #[test]
    fn a_truncated_escape_does_not_eat_the_line() {
        assert_eq!(ansi_to_section("text\x1b["), "text");
        assert_eq!(ansi_to_section("text\x1b"), "text");
    }

    #[test]
    fn strip_ansi_removes_colour_codes() {
        assert_eq!(strip_ansi("\x1b[0;32mINFO\x1b[0m ready"), "INFO ready");
    }

    #[test]
    fn strip_ansi_leaves_plain_text_untouched() {
        let plain = "[12:00:00] [Server thread/INFO]: Done (1.234s)!";
        assert_eq!(strip_ansi(plain), plain);
    }

    #[test]
    fn strip_ansi_removes_osc_sequences() {
        assert_eq!(strip_ansi("\x1b]0;title\x07text"), "text");
    }

    #[test]
    fn splitter_emits_only_complete_lines() {
        let mut s = LineSplitter::new();
        assert_eq!(s.push(b"one\ntwo\npar"), vec!["one", "two"]);
        // "par" is incomplete and must not be emitted yet.
        assert_eq!(s.push(b"tial\n"), vec!["partial"]);
    }

    #[test]
    fn splitter_survives_a_split_multibyte_character() {
        // "é" is 0xC3 0xA9. Split across chunks it must not become U+FFFD.
        let mut s = LineSplitter::new();
        assert!(s.push(&[b'c', b'a', b'f', 0xC3]).is_empty());
        assert_eq!(s.push(&[0xA9, b'\n']), vec!["café"]);
    }

    #[test]
    fn splitter_strips_crlf_and_translates_ansi_per_line() {
        let mut s = LineSplitter::new();
        // The carriage return goes; the colour is kept, as a section code.
        assert_eq!(
            s.push(b"\x1b[32mok\x1b[0m\r\n"),
            vec!["\u{a7}2ok\u{a7}r"]
        );
    }

    /// Colour has to survive a line arriving in pieces, which is the normal
    /// case for a log being tailed: the escape and the text it colours can land
    /// in different reads.
    #[test]
    fn a_line_split_across_chunks_keeps_its_colour() {
        let mut s = LineSplitter::new();
        assert!(s.push(b"\x1b[32m[CITIZEN]").is_empty());
        assert_eq!(
            s.push(b" Joe\x1b[0m\n"),
            vec!["\u{a7}2[CITIZEN] Joe\u{a7}r"]
        );
    }

    #[test]
    fn splitter_reset_drops_the_partial_line() {
        let mut s = LineSplitter::new();
        s.push(b"orphaned");
        s.reset();
        assert_eq!(s.push(b"fresh\n"), vec!["fresh"]);
    }

    #[test]
    fn hub_replays_backlog_to_new_subscribers() {
        let hub = LogHub::new(10);
        hub.push("first");
        hub.push("second");
        let (backlog, _rx) = hub.subscribe();
        assert_eq!(
            backlog.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
            vec!["first", "second"]
        );
    }

    #[test]
    fn hub_evicts_oldest_beyond_capacity() {
        let hub = LogHub::new(2);
        hub.push("a");
        hub.push("b");
        hub.push("c");
        let (backlog, _rx) = hub.subscribe();
        assert_eq!(hub.backlog_len(), 2);
        assert_eq!(
            backlog.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
            vec!["b", "c"]
        );
    }

    #[test]
    fn log_source_defaults_to_the_server_log() {
        assert_eq!(LogSource::parse(Some("stdout")), LogSource::Stdout);
        assert_eq!(LogSource::parse(Some("log")), LogSource::ServerLog);
        assert_eq!(LogSource::parse(None), LogSource::ServerLog);
        // An unrecognised value falls back rather than erroring, so a stale
        // bookmark still opens a working console.
        assert_eq!(LogSource::parse(Some("nonsense")), LogSource::ServerLog);
    }

    #[test]
    fn consoles_keep_the_two_sources_separate() {
        let consoles = Consoles::new(10);
        consoles.server_log.push("from latest.log");
        consoles.stdout.push("from stdout");

        let (log_backlog, _) = consoles.get(LogSource::ServerLog).subscribe();
        let (out_backlog, _) = consoles.get(LogSource::Stdout).subscribe();

        assert_eq!(log_backlog.len(), 1);
        assert_eq!(out_backlog.len(), 1);
        assert_eq!(log_backlog[0].as_ref(), "from latest.log");
        assert_eq!(out_backlog[0].as_ref(), "from stdout");
    }

    #[tokio::test]
    async fn hub_delivers_lines_pushed_after_subscribe() {
        let hub = LogHub::new(10);
        hub.push("before");
        let (backlog, mut rx) = hub.subscribe();
        hub.push("after");

        // "before" arrives via the snapshot, "after" via the channel — exactly
        // once each, with no gap between the two mechanisms.
        assert_eq!(backlog.len(), 1);
        assert_eq!(rx.recv().await.unwrap().as_ref(), "after");
    }
}
