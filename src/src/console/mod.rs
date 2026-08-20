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

pub mod control;
pub mod players;
pub mod stats;
pub mod tail;

use std::collections::VecDeque;
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

/// Both live log streams, each fed by its own tail task.
pub struct Consoles {
    pub server_log: Arc<LogHub>,
    pub stdout: Arc<LogHub>,
}

impl Consoles {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            server_log: LogHub::new(capacity),
            stdout: LogHub::new(capacity),
        })
    }

    pub fn get(&self, source: LogSource) -> &Arc<LogHub> {
        match source {
            LogSource::ServerLog => &self.server_log,
            LogSource::Stdout => &self.stdout,
        }
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
    backlog: RwLock<VecDeque<Arc<str>>>,
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
        backlog.push_back(Arc::clone(&line));
        // Sent while the write lock is still held. Combined with `subscribe`
        // taking its snapshot under the read lock, this guarantees a new
        // subscriber sees every line exactly once: either the line is already
        // in the snapshot, or the subscription exists before the line is sent.
        let _ = self.tx.send(line);
    }

    /// Snapshot the replay buffer and subscribe to live lines atomically.
    pub fn subscribe(&self) -> (Vec<Arc<str>>, broadcast::Receiver<Arc<str>>) {
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
    strip_ansi(text.trim_end_matches('\r'))
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
    fn splitter_strips_crlf_and_ansi_per_line() {
        let mut s = LineSplitter::new();
        assert_eq!(s.push(b"\x1b[32mok\x1b[0m\r\n"), vec!["ok"]);
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
