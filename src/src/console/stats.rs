//! Performance metrics for the console.
//!
//! Two independent sources, because neither alone covers the question "is the
//! server healthy?":
//!
//! * **TPS** comes from the game server over RCON. It only exists on the
//!   Spigot family — vanilla has no `tps` command — so a reply that does not
//!   parse is reported as unavailable rather than guessed at.
//! * **CPU and memory** come from `docker stats`, which needs the Docker
//!   socket. The API deliberately has none (see `console::control`), so the
//!   `mc-control` sidecar publishes a stats line onto the shared control volume
//!   and this module reads that file.
//!
//! Both are cached, for the same reason the RCON connection is held open: every
//! probe of the game server is echoed into the very console the operator is
//! watching. See [`SnapshotCache`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Serialize;
use tokio::sync::Mutex;

use super::players;
use crate::rcon::RconClient;

/// How long a published stats line stays believable.
///
/// The sidecar republishes every few seconds, so anything older than this means
/// it has stopped — and reporting its last numbers as current would be worse
/// than reporting nothing.
pub const STATS_MAX_AGE: Duration = Duration::from_secs(30);

/// How long a probe of the game server is reused for.
pub const SNAPSHOT_TTL: Duration = Duration::from_secs(10);

/// A plausible tick rate. Anything outside this means the reply was not a TPS
/// reading at all.
const MAX_PLAUSIBLE_TPS: f32 = 100.0;

/// Resource usage of the Minecraft container.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct ContainerStats {
    #[serde(rename = "cpuPercent")]
    pub cpu_percent: f32,
    #[serde(rename = "memoryUsedBytes")]
    pub memory_used: u64,
    #[serde(rename = "memoryLimitBytes")]
    pub memory_limit: u64,
    #[serde(rename = "memoryPercent")]
    pub memory_percent: f32,
}

/// Parse the line the sidecar publishes: `CPU%|MemUsage|MemPerc`, for example
/// `12.34%|1.234GiB / 4GiB|30.85%`.
pub fn parse_container_stats(raw: &str) -> Option<ContainerStats> {
    let line = raw.lines().find(|line| !line.trim().is_empty())?;
    let mut fields = line.split('|');

    let cpu_percent = parse_percent(fields.next()?)?;

    let usage = fields.next()?;
    let (used, limit) = usage.split_once('/')?;
    let memory_used = parse_size(used)?;
    let memory_limit = parse_size(limit)?;

    let memory_percent = parse_percent(fields.next()?)?;

    // `docker stats` reports a stopped container as all zeroes rather than
    // failing. No running container has a zero memory limit — Docker reports
    // the host's total when none is configured — so this is what tells "not
    // running" apart from a genuine reading of near-idle.
    if memory_limit == 0 {
        return None;
    }

    Some(ContainerStats {
        cpu_percent,
        memory_used,
        memory_limit,
        memory_percent,
    })
}

fn parse_percent(raw: &str) -> Option<f32> {
    raw.trim().trim_end_matches('%').trim().parse().ok()
}

/// Parse a size as Docker renders it.
///
/// Docker emits binary units (`GiB`) in most versions and decimal ones (`GB`)
/// in others, so both have to be understood — treating one as the other
/// misreports memory by 7% at the gigabyte scale, which is exactly the size
/// that matters here.
pub fn parse_size(raw: &str) -> Option<u64> {
    let text = raw.trim();
    let split = text
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(text.len());
    let (number, unit) = text.split_at(split);

    let value: f64 = number.trim().parse().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }

    let multiplier = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1.0,
        "kib" => 1024.0,
        "mib" => 1024f64.powi(2),
        "gib" => 1024f64.powi(3),
        "tib" => 1024f64.powi(4),
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        _ => return None,
    };

    Some((value * multiplier) as u64)
}

pub fn stats_path(control_dir: &str) -> PathBuf {
    Path::new(control_dir).join("stats")
}

/// Read the sidecar's published stats, or `None` if they are missing, stale or
/// unreadable.
pub async fn read_container_stats(control_dir: &str, max_age: Duration) -> Option<ContainerStats> {
    let path = stats_path(control_dir);
    let metadata = tokio::fs::metadata(&path).await.ok()?;

    // `elapsed` fails on a clock that has gone backwards; treat that as fresh
    // rather than throwing away a reading over it.
    if metadata.modified().ok()?.elapsed().unwrap_or_default() > max_age {
        log::debug!("console: {} is stale, is mc-control running?", path.display());
        return None;
    }

    parse_container_stats(&tokio::fs::read_to_string(&path).await.ok()?)
}

/// Read the tick rates out of a Spigot-family `tps` reply.
///
/// Returns the values in the order the server gave them, which is 1m / 5m / 15m
/// on every implementation that has the command. Anything that does not look
/// like tick rates — `Unknown or incomplete command`, most often, on a server
/// without it — is `None`, which the UI shows as unavailable.
pub fn parse_tps(raw: &str) -> Option<Vec<f32>> {
    let plain = super::strip_formatting(raw);
    let colon = plain.rfind(':')?;

    let values: Option<Vec<f32>> = plain[colon + 1..]
        .split(',')
        // Spigot marks a reading it considers degraded with a leading asterisk.
        .map(|value| value.trim().trim_start_matches('*').trim())
        .filter(|value| !value.is_empty())
        .map(|value| value.parse::<f32>().ok())
        .collect();

    let values = values?;
    if values.is_empty()
        || values.len() > 3
        || values
            .iter()
            .any(|v| !(0.0..=MAX_PLAUSIBLE_TPS).contains(v))
    {
        return None;
    }

    Some(values)
}

/// What one probe of the game server yields.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Snapshot {
    /// `None` when the server has no `tps` command, or is not reachable.
    pub tps: Option<Vec<f32>>,
    pub online: Vec<String>,
    /// Set when the probe could not talk to the server at all, so the UI can
    /// say why the numbers are missing instead of showing a silent blank.
    #[serde(rename = "rconError")]
    pub rcon_error: Option<String>,
}

/// Caches a [`Snapshot`] for [`SNAPSHOT_TTL`].
///
/// Every field of a snapshot costs an RCON command, and RCON commands are
/// echoed into the very console the operator is reading. Without this, each
/// admin with the tab open would add their own stream of `list` / `tps` chatter
/// to the log — the panel would degrade the thing it sits next to.
///
/// The lock is held across the refresh so that concurrent callers wait for one
/// result rather than each firing their own pair of commands.
pub struct SnapshotCache {
    ttl: Duration,
    entry: Mutex<Option<(Instant, Arc<Snapshot>)>>,
}

impl SnapshotCache {
    pub fn new(ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            ttl,
            entry: Mutex::new(None),
        })
    }

    pub async fn get(&self, rcon: &RconClient) -> Arc<Snapshot> {
        let mut entry = self.entry.lock().await;

        if let Some((probed_at, snapshot)) = entry.as_ref() {
            if probed_at.elapsed() < self.ttl {
                return Arc::clone(snapshot);
            }
        }

        let fresh = Arc::new(probe(rcon).await);
        *entry = Some((Instant::now(), Arc::clone(&fresh)));
        fresh
    }

    /// Drop the cached value so the next read reflects something the operator
    /// just did — opping someone should not take a TTL to show up in the list.
    pub async fn invalidate(&self) {
        *self.entry.lock().await = None;
    }
}

async fn probe(rcon: &RconClient) -> Snapshot {
    let mut snapshot = Snapshot::default();

    if !rcon.is_configured() {
        snapshot.rcon_error = Some("RCON is not configured on this server".to_string());
        return snapshot;
    }

    match rcon.execute("list").await {
        Ok(output) => snapshot.online = players::parse_online_list(&output),
        Err(e) => {
            log::debug!("console: probe could not list players: {e}");
            // The server is unreachable, so `tps` would only fail the same way
            // and cost another five-second connect timeout.
            snapshot.rcon_error = Some(e.user_message());
            return snapshot;
        }
    }

    if let Ok(output) = rcon.execute("tps").await {
        snapshot.tps = parse_tps(&output);
    }

    snapshot
}

#[cfg(test)]
mod tests {
    use super::*;

    /* ------------------------------------------------------------------ tps */

    #[test]
    fn reads_a_paper_tps_reply() {
        assert_eq!(
            parse_tps("\u{a7}6TPS from last 1m, 5m, 15m: \u{a7}a20.0, \u{a7}a19.98, \u{a7}a19.5"),
            Some(vec![20.0, 19.98, 19.5])
        );
    }

    #[test]
    fn reads_a_spigot_reply_with_its_asterisks() {
        // Spigot prefixes a reading it considers degraded with `*`.
        assert_eq!(
            parse_tps("TPS from last 1m, 5m, 15m: *18.42, *19.01, 20.0"),
            Some(vec![18.42, 19.01, 20.0])
        );
    }

    #[test]
    fn a_server_without_the_command_reports_nothing() {
        // The point of returning None rather than zeros: vanilla has no `tps`,
        // and "0 TPS" would read as a server on fire.
        assert_eq!(parse_tps("Unknown or incomplete command, see below"), None);
        assert_eq!(parse_tps(""), None);
    }

    #[test]
    fn prose_after_a_colon_is_not_mistaken_for_tick_rates() {
        assert_eq!(parse_tps("Error: something went wrong"), None);
        // Plausible shape, implausible values.
        assert_eq!(parse_tps("TPS: 5000, 6000, 7000"), None);
        assert_eq!(parse_tps("TPS: 20.0, 20.0, 20.0, 20.0"), None);
    }

    /* -------------------------------------------------------------- sizes */

    #[test]
    fn parses_binary_units() {
        assert_eq!(parse_size("1GiB"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size(" 512MiB "), Some(512 * 1024 * 1024));
        assert_eq!(parse_size("1.5KiB"), Some(1536));
    }

    #[test]
    fn parses_decimal_units_distinctly() {
        // The distinction is the point: GB and GiB differ by 7%.
        assert_eq!(parse_size("1GB"), Some(1_000_000_000));
        assert_ne!(parse_size("1GB"), parse_size("1GiB"));
    }

    #[test]
    fn parses_a_bare_byte_count() {
        assert_eq!(parse_size("512B"), Some(512));
        assert_eq!(parse_size("512"), Some(512));
    }

    #[test]
    fn rejects_nonsense_sizes() {
        for bogus in ["", "GiB", "abc", "1ZiB", "-4GiB"] {
            assert_eq!(parse_size(bogus), None, "{bogus:?} should not parse");
        }
    }

    /* ------------------------------------------------------ container stats */

    #[test]
    fn parses_the_sidecar_line() {
        let stats = parse_container_stats("12.34%|1.5GiB / 4GiB|37.50%\n").unwrap();
        assert_eq!(stats.cpu_percent, 12.34);
        assert_eq!(stats.memory_used, 1536 * 1024 * 1024);
        assert_eq!(stats.memory_limit, 4 * 1024 * 1024 * 1024);
        assert_eq!(stats.memory_percent, 37.50);
    }

    #[test]
    fn rejects_a_truncated_or_empty_stats_line() {
        // `docker stats` on a stopped container writes nothing useful, and a
        // half-written file must not turn into confident numbers.
        for bogus in [
            "",
            "\n",
            "12.34%",
            "12.34%|1.5GiB",
            "12.34%|1.5GiB|30%",
            // What a stopped container reports.
            "0.00%|0B / 0B|0.00%",
        ] {
            assert!(
                parse_container_stats(bogus).is_none(),
                "{bogus:?} should not parse"
            );
        }
    }

    #[tokio::test]
    async fn stale_stats_are_reported_as_absent() {
        let dir = std::env::temp_dir().join(format!("apird-stats-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let control = dir.to_string_lossy().into_owned();

        tokio::fs::write(stats_path(&control), "12.34%|1.5GiB / 4GiB|37.50%")
            .await
            .unwrap();

        assert!(read_container_stats(&control, Duration::from_secs(30))
            .await
            .is_some());
        // A sidecar that stopped publishing must not leave the panel showing
        // numbers from whenever it died.
        assert!(read_container_stats(&control, Duration::ZERO).await.is_none());

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn missing_stats_are_absent_not_an_error() {
        let missing = std::env::temp_dir().join("apird-stats-does-not-exist");
        assert!(
            read_container_stats(&missing.to_string_lossy(), STATS_MAX_AGE)
                .await
                .is_none()
        );
    }

    /* -------------------------------------------------------------- cache */

    #[tokio::test]
    async fn an_unconfigured_rcon_explains_itself_without_dialing() {
        let cache = SnapshotCache::new(SNAPSHOT_TTL);
        let rcon = RconClient::new("127.0.0.1:1".to_string(), String::new());

        let snapshot = cache.get(&rcon).await;

        assert!(snapshot.online.is_empty());
        assert!(snapshot.tps.is_none());
        assert!(snapshot
            .rcon_error
            .as_deref()
            .unwrap_or_default()
            .contains("not configured"));
    }

    /// Reads one RCON packet off a scripted server socket.
    async fn read_packet(socket: &mut tokio::net::TcpStream) -> (i32, String) {
        use tokio::io::AsyncReadExt;

        let mut length = [0u8; 4];
        socket.read_exact(&mut length).await.unwrap();
        let mut payload = vec![0u8; i32::from_le_bytes(length) as usize];
        socket.read_exact(&mut payload).await.unwrap();

        let (id, _, body) = crate::rcon::decode_payload(&payload).unwrap();
        (id, body)
    }

    /// Exercises the whole probe rather than only its parsers: which commands
    /// go out, in what order, and what comes back as a snapshot. The parsers
    /// being right is no use if the probe asks the wrong questions.
    #[tokio::test]
    async fn a_probe_asks_for_the_player_list_then_the_tick_rate() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();

            // Packet types 3/2/0 are AUTH / COMMAND / RESPONSE_VALUE.
            let (id, _) = read_packet(&mut socket).await;
            socket
                .write_all(&crate::rcon::encode_packet(id, 2, ""))
                .await
                .unwrap();

            let mut asked = Vec::new();
            for reply in [
                "There are 2 of a max of 20 players online: Steve, Alex",
                "\u{a7}6TPS from last 1m, 5m, 15m: \u{a7}a20.0, \u{a7}a19.9, \u{a7}a19.8",
            ] {
                let (id, body) = read_packet(&mut socket).await;
                asked.push(body);
                socket
                    .write_all(&crate::rcon::encode_packet(id, 0, reply))
                    .await
                    .unwrap();
            }
            asked
        });

        let cache = SnapshotCache::new(SNAPSHOT_TTL);
        let rcon = RconClient::new(address, "s3cret".to_string());
        let snapshot = cache.get(&rcon).await;

        assert_eq!(server.await.unwrap(), vec!["list", "tps"]);
        assert_eq!(snapshot.online, vec!["Steve", "Alex"]);
        assert_eq!(snapshot.tps, Some(vec![20.0, 19.9, 19.8]));
        assert!(snapshot.rcon_error.is_none());
    }

    #[tokio::test]
    async fn an_unreachable_server_explains_itself_instead_of_reading_as_idle() {
        let cache = SnapshotCache::new(SNAPSHOT_TTL);
        // Configured, but nothing is listening.
        let rcon = RconClient::new("127.0.0.1:1".to_string(), "s3cret".to_string());

        let snapshot = cache.get(&rcon).await;

        assert!(snapshot.online.is_empty());
        // Not `Some(vec![0.0, ...])` — an unreachable server has no tick rate,
        // and zero would render as a server in freefall.
        assert!(snapshot.tps.is_none());
        assert!(snapshot
            .rcon_error
            .as_deref()
            .unwrap_or_default()
            .contains("not reachable"));
    }

    #[tokio::test]
    async fn a_probe_is_reused_until_it_is_invalidated() {
        let cache = SnapshotCache::new(Duration::from_secs(600));
        let rcon = RconClient::new("127.0.0.1:1".to_string(), String::new());

        let first = cache.get(&rcon).await;
        // Same allocation, so no second probe ran.
        assert!(Arc::ptr_eq(&first, &cache.get(&rcon).await));

        cache.invalidate().await;
        assert!(
            !Arc::ptr_eq(&first, &cache.get(&rcon).await),
            "an operator action must not wait out the TTL"
        );
    }

    #[tokio::test]
    async fn a_probe_expires_once_its_ttl_passes() {
        let cache = SnapshotCache::new(Duration::from_millis(20));
        let rcon = RconClient::new("127.0.0.1:1".to_string(), String::new());

        let first = cache.get(&rcon).await;
        tokio::time::sleep(Duration::from_millis(40)).await;

        assert!(!Arc::ptr_eq(&first, &cache.get(&rcon).await));
    }
}
