//! Polling tail of Minecraft's `latest.log`.
//!
//! Polling rather than `notify`: rotation detection needs a `stat` every cycle
//! regardless, inotify events coalesce (so offsets must be tracked anyway), and
//! the file arrives over a bind mount whose event delivery is not worth
//! betting on. A `metadata()` plus a delta read every 500ms is negligible.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::time::MissedTickBehavior;

use super::{LineSplitter, LogHub};

const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// On first attach, replay at most this much history so the console has
/// context immediately without re-reading a log that may be many megabytes.
const COLD_START_BYTES: u64 = 64 * 1024;
/// Maximum bytes consumed per tick. A burst larger than this is picked up over
/// subsequent ticks instead of being buffered all at once.
const MAX_CHUNK: u64 = 1024 * 1024;

/// Where we are in the file we are currently following.
#[derive(Clone, Copy)]
struct Cursor {
    id: u128,
    pos: u64,
}

/// Follow `path` forever, pushing each complete line into `hub`.
///
/// Survives the file being absent (server not yet started), rotated (Minecraft
/// writes a fresh `latest.log` on every restart) and truncated in place.
pub async fn tail_log(hub: Arc<LogHub>, path: String) {
    let mut cursor: Option<Cursor> = None;
    let mut splitter = LineSplitter::new();
    let mut ticker = tokio::time::interval(POLL_INTERVAL);
    // The default Burst behaviour fires back-to-back ticks to "catch up" after
    // a slow read, which buys nothing for a poll loop.
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;

        let meta = match tokio::fs::metadata(&path).await {
            Ok(meta) => meta,
            Err(_) => {
                // Not worth logging every 500ms: the file legitimately does
                // not exist while the server is stopped.
                if cursor.is_some() {
                    log::info!("console: {} disappeared, waiting for it to return", path);
                    hub.push("── log file closed ──");
                }
                cursor = None;
                splitter.reset();
                continue;
            }
        };

        let id = file_identity(&meta);
        let len = meta.len();

        // `skip_partial` marks a read that starts mid-line, whose first
        // fragment is the tail of a line we never saw the beginning of.
        let (pos, skip_partial) = match cursor {
            None => {
                splitter.reset();
                let start = len.saturating_sub(COLD_START_BYTES);
                (start, start > 0)
            }
            // A different file at the same path: the server restarted.
            Some(prev) if prev.id != id => {
                log::info!("console: {} rotated, following the new file", path);
                splitter.reset();
                hub.push("── log rotated ──");
                (0, false)
            }
            // Shorter than our offset — either truncated in place, or (on
            // platforms without a usable file identity) a rotation that the
            // identity check could not see.
            Some(prev) if len < prev.pos => {
                splitter.reset();
                hub.push("── log truncated ──");
                (0, false)
            }
            Some(prev) => (prev.pos, false),
        };

        if len <= pos {
            cursor = Some(Cursor { id, pos });
            continue;
        }

        let want = (len - pos).min(MAX_CHUNK);
        let chunk = match read_delta(&path, pos, want).await {
            Ok(chunk) => chunk,
            Err(e) => {
                log::warn!("console: failed reading {}: {}", path, e);
                // Leave the cursor untouched so the next tick retries the same
                // offset rather than skipping over unread bytes.
                continue;
            }
        };
        if chunk.is_empty() {
            continue;
        }

        cursor = Some(Cursor {
            id,
            pos: pos + chunk.len() as u64,
        });

        let chunk = if skip_partial {
            match chunk.iter().position(|&b| b == b'\n') {
                Some(nl) => &chunk[nl + 1..],
                None => continue,
            }
        } else {
            &chunk[..]
        };

        for line in splitter.push(chunk) {
            hub.push(line);
        }
    }
}

async fn read_delta(path: &str, pos: u64, want: u64) -> std::io::Result<Vec<u8>> {
    let mut file = tokio::fs::File::open(path).await?;
    file.seek(std::io::SeekFrom::Start(pos)).await?;

    // `take` + `read_to_end` rather than `read_exact`: the file may shrink
    // between the stat and this read, in which case a short read is expected
    // and the next tick will notice.
    let mut buf = Vec::with_capacity(want as usize);
    file.take(want).read_to_end(&mut buf).await?;
    Ok(buf)
}

/// A value that changes when the path starts pointing at a different file.
#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> u128 {
    use std::os::unix::fs::MetadataExt;
    // Device is folded in because inode numbers are only unique per filesystem.
    ((meta.dev() as u128) << 64) | meta.ino() as u128
}

/// Windows exposes no usable file identity through `std::fs::Metadata`:
/// `file_index` is still unstable, and creation time is defeated by NTFS file
/// system tunneling, which deliberately restores the *old* creation time when a
/// file is recreated under the same name shortly after deletion — exactly the
/// rotation case we would be trying to detect.
///
/// So Windows detects rotation purely through the `len < pos` shrink check,
/// which holds in practice because a freshly rotated log starts at zero bytes.
/// This path only needs to be good enough for local development; production
/// runs on Linux.
#[cfg(windows)]
fn file_identity(_meta: &std::fs::Metadata) -> u128 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A scratch file that cleans itself up, named per-test to avoid collisions.
    struct TempLog(std::path::PathBuf);

    impl TempLog {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("apird-tail-test-{name}.log"));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }
        fn path(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }
        fn write(&self, contents: &str) {
            let mut f = std::fs::File::create(&self.0).unwrap();
            f.write_all(contents.as_bytes()).unwrap();
            f.flush().unwrap();
        }
        fn append(&self, contents: &str) {
            let mut f = std::fs::OpenOptions::new().append(true).open(&self.0).unwrap();
            f.write_all(contents.as_bytes()).unwrap();
            f.flush().unwrap();
        }
    }

    impl Drop for TempLog {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn identity_differs_between_distinct_files() {
        let a = TempLog::new("identity-a");
        let b = TempLog::new("identity-b");
        a.write("a\n");
        b.write("b\n");

        let ma = std::fs::metadata(&a.0).unwrap();
        let mb = std::fs::metadata(&b.0).unwrap();
        assert_ne!(file_identity(&ma), file_identity(&mb));
    }

    #[cfg(unix)]
    #[test]
    fn identity_is_stable_across_appends() {
        let log = TempLog::new("identity-stable");
        log.write("one\n");
        let before = file_identity(&std::fs::metadata(&log.0).unwrap());
        log.append("two\n");
        let after = file_identity(&std::fs::metadata(&log.0).unwrap());
        assert_eq!(before, after, "appending must not look like a rotation");
    }

    #[tokio::test]
    async fn read_delta_returns_only_the_new_bytes() {
        let log = TempLog::new("delta");
        log.write("hello\nworld\n");

        assert_eq!(read_delta(&log.path(), 0, 64).await.unwrap(), b"hello\nworld\n");
        assert_eq!(read_delta(&log.path(), 6, 64).await.unwrap(), b"world\n");
    }

    #[tokio::test]
    async fn read_delta_is_capped_by_want() {
        let log = TempLog::new("delta-cap");
        log.write("hello\nworld\n");
        assert_eq!(read_delta(&log.path(), 0, 5).await.unwrap(), b"hello");
    }

    #[tokio::test]
    async fn tail_replays_history_then_follows_appends() {
        let log = TempLog::new("follow");
        log.write("existing line\n");

        let hub = LogHub::new(100);
        let task = tokio::spawn(tail_log(Arc::clone(&hub), log.path()));

        tokio::time::sleep(POLL_INTERVAL * 3).await;
        log.append("appended line\n");
        tokio::time::sleep(POLL_INTERVAL * 3).await;
        task.abort();

        let (backlog, _rx) = hub.subscribe();
        let lines: Vec<&str> = backlog.iter().map(|l| l.as_ref()).collect();

        // A log under COLD_START_BYTES is replayed in full, so the console has
        // context the moment an operator opens it.
        assert!(lines.contains(&"existing line"), "got {lines:?}");
        assert!(lines.contains(&"appended line"), "got {lines:?}");
    }

    #[tokio::test]
    async fn tail_recovers_when_the_file_is_replaced() {
        let log = TempLog::new("rotate");
        // Long enough that the replacement is unambiguously shorter, which is
        // what the Windows shrink-detection path relies on.
        log.write("first run line one\nfirst run line two\n");

        let hub = LogHub::new(100);
        let task = tokio::spawn(tail_log(Arc::clone(&hub), log.path()));
        tokio::time::sleep(POLL_INTERVAL * 3).await;

        // Replace the file, as Minecraft does on restart.
        std::fs::remove_file(&log.0).unwrap();
        log.write("after rotation\n");

        tokio::time::sleep(POLL_INTERVAL * 6).await;
        task.abort();

        let (backlog, _rx) = hub.subscribe();
        let lines: Vec<&str> = backlog.iter().map(|l| l.as_ref()).collect();

        assert!(lines.contains(&"after rotation"), "got {lines:?}");
        // The rotation is announced so an operator can see why the log jumped.
        assert!(
            lines.iter().any(|l| l.contains("rotated") || l.contains("truncated")),
            "got {lines:?}"
        );
    }

    #[tokio::test]
    async fn tail_waits_for_a_missing_file() {
        let log = TempLog::new("missing");
        // Deliberately not created — the server is "stopped".
        let hub = LogHub::new(100);
        let task = tokio::spawn(tail_log(Arc::clone(&hub), log.path()));

        tokio::time::sleep(POLL_INTERVAL * 3).await;
        log.write("server started\n");
        tokio::time::sleep(POLL_INTERVAL * 4).await;
        task.abort();

        let (backlog, _rx) = hub.subscribe();
        let lines: Vec<&str> = backlog.iter().map(|l| l.as_ref()).collect();
        assert!(lines.contains(&"server started"), "got {lines:?}");
    }
}
