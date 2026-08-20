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
//! Both are cached, for the same reason the RCON connection is held open: the
//! server logs a line for every RCON command, into the very log this console
//! streams — so a metrics panel that polls carelessly buries the thing it sits
//! next to. See [`SnapshotCache`].

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

/// Read the tick rates out of a `tps` reply.
///
/// Returns the values in the order the server gave them, which is 1m / 5m / 15m
/// on every implementation that has the command. Anything that does not look
/// like tick rates — `Unknown or incomplete command`, most often, on a server
/// without it — is `None`, which the UI shows as unavailable.
///
/// The reply is searched line by line because EssentialsX answers `tps` with
/// several: tick rates, then memory, then disk. Requiring the word "TPS" on the
/// line is what stops the others being mined for numbers that merely happen to
/// parse — `Free disk space: 20` would otherwise pass for a perfect tick rate.
pub fn parse_tps(raw: &str) -> Option<Vec<f32>> {
    let plain = super::strip_formatting(raw);

    let line = plain
        .lines()
        .find(|line| line.to_ascii_lowercase().contains("tps"))?;
    let colon = line.rfind(':')?;

    let values: Option<Vec<f32>> = line[colon + 1..]
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

/// JVM heap usage, as EssentialsX reports it alongside the tick rates.
///
/// Worth having in addition to the container's memory: the container figure is
/// resident set size, which a JVM launched with `-Xms4G -XX:+AlwaysPreTouch`
/// pins near its limit from the moment it starts. It is real, but it never
/// moves, so it cannot answer "is the server running out of memory?" — heap
/// can.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct HeapUsage {
    #[serde(rename = "usedBytes")]
    pub used: u64,
    /// Heap the JVM currently has committed, which grows towards `max`.
    #[serde(rename = "allocatedBytes")]
    pub allocated: u64,
    /// The `-Xmx` ceiling.
    #[serde(rename = "maxBytes")]
    pub max: u64,
    /// `used` as a share of `max`, computed here so the UI has no chance to
    /// divide by a zero it did not expect.
    pub percent: f32,
}

/// Leading numeric run of a string, ignoring whatever follows it.
fn leading_number(raw: &str) -> Option<f64> {
    let trimmed = raw.trim_start();
    let end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(trimmed.len());

    let value: f64 = trimmed[..end].parse().ok()?;
    value.is_finite().then_some(value)
}

/// First alphabetic run of a string — the unit word after a number.
fn unit_of(raw: &str) -> &str {
    let trimmed = raw.trim();
    let start = trimmed
        .find(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(trimmed.len());

    let rest = &trimmed[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_alphabetic())
        .unwrap_or(rest.len());
    &rest[..end]
}

/// EssentialsX labels its figures `mb` but computes them by dividing twice by
/// 1024, so they are mebibytes. Every unit here is read as binary — the JVM
/// deals in powers of two, and treating `mb` as a megabyte would overstate a
/// 4 GiB heap by nearly 200 MiB.
fn heap_scale(unit: &str) -> u64 {
    match unit.to_ascii_lowercase().as_str() {
        "kb" | "kib" => 1024,
        "gb" | "gib" => 1024 * 1024 * 1024,
        // `mb`, and the default for an unlabelled figure.
        _ => 1024 * 1024,
    }
}

/// Read JVM heap usage out of an EssentialsX `tps` (or `gc`) reply.
///
/// The line looks like `Current Memory Usage: 1485/4096 mb (Max: 4096 mb)` —
/// used, then committed, then the `-Xmx` ceiling. A server without EssentialsX
/// simply has no such line, which is `None` rather than an error.
pub fn parse_heap(raw: &str) -> Option<HeapUsage> {
    let plain = super::strip_formatting(raw);

    let line = plain
        .lines()
        .find(|line| line.to_ascii_lowercase().contains("memory usage"))?;

    // Split off the parenthesised maximum before touching the used/committed
    // pair, so the `Max:` colon cannot be mistaken for the field separator.
    let (_, values) = line.split_once(':')?;
    let (pair, tail) = match values.split_once('(') {
        Some((pair, tail)) => (pair, Some(tail)),
        None => (values, None),
    };

    let (used_text, allocated_text) = pair.split_once('/')?;
    // Both figures on the line share a unit, so one reading covers all three.
    let scale = heap_scale(unit_of(allocated_text)) as f64;

    let used = (leading_number(used_text)? * scale) as u64;
    let allocated = (leading_number(allocated_text)? * scale) as u64;

    let max = tail
        .and_then(|tail| tail.split_once(':'))
        .and_then(|(_, value)| leading_number(value))
        .map(|value| (value * scale) as u64)
        .filter(|max| *max > 0)
        // A reply without the `(Max: …)` suffix still gives a usable ratio
        // against what the JVM has committed.
        .unwrap_or(allocated);

    if max == 0 {
        return None;
    }

    Some(HeapUsage {
        used,
        allocated,
        max,
        percent: used as f32 / max as f32 * 100.0,
    })
}

/// What the game server reports about its own health.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Health {
    /// `None` when the server has no `tps` command, or is not reachable.
    pub tps: Option<Vec<f32>>,
    /// `None` without EssentialsX, which is what puts heap figures in the
    /// `tps` reply.
    pub heap: Option<HeapUsage>,
    /// Set when the probe could not talk to the server at all, so the UI can
    /// say why the numbers are missing instead of showing a silent blank.
    #[serde(rename = "rconError")]
    pub rcon_error: Option<String>,
}

/// Who is on the server right now.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Online {
    pub names: Vec<String>,
    #[serde(rename = "rconError")]
    pub rcon_error: Option<String>,
}

/// Caches what has to be asked of the game server itself.
///
/// Every value here costs an RCON command, and the server logs a line for each
/// one — into the very log this console streams. A poll that refreshed on a
/// timer therefore buries the thing it sits next to, which is why the two
/// halves are cached and fetched separately:
///
/// * [`health`](Self::health) is what a metrics panel refreshes on a timer, so
///   it is deliberately one command.
/// * [`online`](Self::online) costs a second command and is only fetched when
///   someone actually opens or refreshes the player list.
///
/// Each lock is held across its refresh, so concurrent callers wait on one
/// result instead of each issuing their own command.
pub struct SnapshotCache {
    ttl: Duration,
    health: Mutex<Option<(Instant, Arc<Health>)>>,
    online: Mutex<Option<(Instant, Arc<Online>)>>,
}

impl SnapshotCache {
    pub fn new(ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            ttl,
            health: Mutex::new(None),
            online: Mutex::new(None),
        })
    }

    /// Tick rate and heap, from a single `tps` command.
    pub async fn health(&self, rcon: &RconClient) -> Arc<Health> {
        let mut entry = self.health.lock().await;

        if let Some((probed_at, health)) = entry.as_ref() {
            if probed_at.elapsed() < self.ttl {
                return Arc::clone(health);
            }
        }

        let fresh = Arc::new(probe_health(rcon).await);
        *entry = Some((Instant::now(), Arc::clone(&fresh)));
        fresh
    }

    /// Names of everyone online, from a single `list` command.
    pub async fn online(&self, rcon: &RconClient) -> Arc<Online> {
        let mut entry = self.online.lock().await;

        if let Some((probed_at, online)) = entry.as_ref() {
            if probed_at.elapsed() < self.ttl {
                return Arc::clone(online);
            }
        }

        let fresh = Arc::new(probe_online(rcon).await);
        *entry = Some((Instant::now(), Arc::clone(&fresh)));
        fresh
    }

    /// Drop both cached values so the next read reflects something the operator
    /// just did — opping someone should not take a TTL to show up in the list.
    pub async fn invalidate(&self) {
        *self.health.lock().await = None;
        *self.online.lock().await = None;
    }
}

/// Reason RCON is unusable, or `None` if it looks usable.
fn unconfigured(rcon: &RconClient) -> Option<String> {
    (!rcon.is_configured()).then(|| "RCON is not configured on this server".to_string())
}

async fn probe_health(rcon: &RconClient) -> Health {
    let mut health = Health::default();

    if let Some(reason) = unconfigured(rcon) {
        health.rcon_error = Some(reason);
        return health;
    }

    match rcon.execute("tps").await {
        Ok(output) => {
            health.tps = parse_tps(&output);
            // EssentialsX answers `tps` with heap usage on a following line, so
            // this costs nothing beyond the command already being run.
            health.heap = parse_heap(&output);
        }
        Err(e) => {
            log::debug!("console: health probe failed: {e}");
            health.rcon_error = Some(e.user_message());
        }
    }

    health
}

async fn probe_online(rcon: &RconClient) -> Online {
    let mut online = Online::default();

    if let Some(reason) = unconfigured(rcon) {
        online.rcon_error = Some(reason);
        return online;
    }

    match rcon.execute("list").await {
        Ok(output) => online.names = players::parse_online_list(&output),
        Err(e) => {
            log::debug!("console: could not list players: {e}");
            online.rcon_error = Some(e.user_message());
        }
    }

    online
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

    /// The exact reply this server gives. EssentialsX puts a memory line after
    /// the tick rates, and its last colon is the one inside `(Max: 4096 mb)` —
    /// so reading the reply as a single string finds `4096 mb)` where the tick
    /// rates should be, and reports a perfectly healthy server as unavailable.
    const ESSENTIALS_TPS: &str = "TPS from last 1m, 5m, 15m: *20.0, *20.0, *20.0\n\
                                  Current Memory Usage: 1485/4096 mb (Max: 4096 mb)";

    #[test]
    fn reads_the_tick_rates_out_of_a_multi_line_essentials_reply() {
        assert_eq!(parse_tps(ESSENTIALS_TPS), Some(vec![20.0, 20.0, 20.0]));
    }

    #[test]
    fn reads_the_heap_out_of_the_same_reply() {
        let heap = parse_heap(ESSENTIALS_TPS).unwrap();

        // EssentialsX says "mb" but means mebibytes.
        assert_eq!(heap.used, 1485 * 1024 * 1024);
        assert_eq!(heap.allocated, 4096 * 1024 * 1024);
        assert_eq!(heap.max, 4096 * 1024 * 1024);
        assert!((heap.percent - 36.25).abs() < 0.1, "got {}", heap.percent);
    }

    #[test]
    fn heap_falls_back_to_the_committed_size_without_a_max() {
        let heap = parse_heap("Current Memory Usage: 512/2048 mb").unwrap();
        assert_eq!(heap.max, 2048 * 1024 * 1024);
        assert_eq!(heap.allocated, heap.max);
    }

    #[test]
    fn heap_reads_the_unit_when_one_is_given() {
        let heap = parse_heap("Current Memory Usage: 1/4 gb (Max: 4 gb)").unwrap();
        assert_eq!(heap.max, 4 * 1024 * 1024 * 1024);
    }

    #[test]
    fn a_reply_without_a_memory_line_has_no_heap() {
        // Paper's `tps` is tick rates only, and vanilla has neither.
        assert_eq!(parse_heap("TPS from last 1m, 5m, 15m: 20.0, 20.0, 20.0"), None);
        assert_eq!(parse_heap("Unknown or incomplete command"), None);
        // Present but unusable rather than absent.
        assert_eq!(parse_heap("Current Memory Usage: nonsense"), None);
    }

    #[test]
    fn a_line_that_is_not_about_tps_is_never_mined_for_numbers() {
        // Everything after the last colon here parses as a plausible tick
        // rate. Requiring the word is what keeps it from being reported as one.
        assert_eq!(parse_tps("Free disk space: 20"), None);
        assert_eq!(parse_tps("Current Memory Usage: 1485/4096 mb (Max: 4096 mb)"), None);
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
        // Port 1 would refuse instantly anyway; the point is that neither probe
        // touches the network without a password.
        let rcon = RconClient::new("127.0.0.1:1".to_string(), String::new());

        let health = cache.health(&rcon).await;
        let online = cache.online(&rcon).await;

        assert!(health.tps.is_none() && online.names.is_empty());
        for reason in [&health.rcon_error, &online.rcon_error] {
            assert!(reason
                .as_deref()
                .unwrap_or_default()
                .contains("not configured"));
        }
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

    /// Serves a fixed reply to each command in turn, and reports what was asked.
    ///
    /// Returning the commands is the point: what the probes *do not* send
    /// matters as much as what they do, because every one becomes a line in the
    /// log the console is displaying.
    fn scripted_server(
        replies: Vec<&'static str>,
    ) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap().to_string();

        let handle = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;

            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            let (mut socket, _) = listener.accept().await.unwrap();

            // Packet types 3/2/0 are AUTH / COMMAND / RESPONSE_VALUE.
            let (id, _) = read_packet(&mut socket).await;
            socket
                .write_all(&crate::rcon::encode_packet(id, 2, ""))
                .await
                .unwrap();

            let mut asked = Vec::new();
            for reply in replies {
                let (id, body) = read_packet(&mut socket).await;
                asked.push(body);
                socket
                    .write_all(&crate::rcon::encode_packet(id, 0, reply))
                    .await
                    .unwrap();
            }
            asked
        });

        (address, handle)
    }

    /// Exercises the whole health probe rather than only its parsers: which
    /// command goes out, and what comes back as a reading.
    #[tokio::test]
    async fn a_health_probe_costs_exactly_one_command() {
        let (address, server) = scripted_server(vec![ESSENTIALS_TPS]);

        let cache = SnapshotCache::new(SNAPSHOT_TTL);
        let rcon = RconClient::new(address, "s3cret".to_string());
        let health = cache.health(&rcon).await;

        // Not `["tps", "list"]`. A metrics poll that also asked who was online
        // would double the log lines it costs, for a number plrCount.json
        // already has.
        assert_eq!(server.await.unwrap(), vec!["tps"]);
        assert_eq!(health.tps, Some(vec![20.0, 20.0, 20.0]));
        // Both readings come out of the one reply, so heap is free.
        assert_eq!(health.heap.unwrap().used, 1485 * 1024 * 1024);
        assert!(health.rcon_error.is_none());
    }

    #[tokio::test]
    async fn the_online_probe_costs_exactly_one_command() {
        let (address, server) =
            scripted_server(vec!["There are 2 of a max of 20 players online: Steve, Alex"]);

        let cache = SnapshotCache::new(SNAPSHOT_TTL);
        let rcon = RconClient::new(address, "s3cret".to_string());
        let online = cache.online(&rcon).await;

        assert_eq!(server.await.unwrap(), vec!["list"]);
        assert_eq!(online.names, vec!["Steve", "Alex"]);
        assert!(online.rcon_error.is_none());
    }

    /// The halves are cached apart, so refreshing metrics on a timer never
    /// drags a `list` along with it.
    #[tokio::test]
    async fn refreshing_health_does_not_refetch_the_player_list() {
        let (address, server) = scripted_server(vec![
            "There are 1 of a max of 20 players online: Steve",
            ESSENTIALS_TPS,
            ESSENTIALS_TPS,
        ]);

        // Zero TTL, so nothing is reused and every call would go to the wire.
        let cache = SnapshotCache::new(Duration::ZERO);
        let rcon = RconClient::new(address, "s3cret".to_string());

        cache.online(&rcon).await;
        cache.health(&rcon).await;
        cache.health(&rcon).await;

        assert_eq!(server.await.unwrap(), vec!["list", "tps", "tps"]);
    }

    #[tokio::test]
    async fn an_unreachable_server_explains_itself_instead_of_reading_as_idle() {
        let cache = SnapshotCache::new(SNAPSHOT_TTL);
        // Configured, but nothing is listening.
        let rcon = RconClient::new("127.0.0.1:1".to_string(), "s3cret".to_string());

        let health = cache.health(&rcon).await;

        // Not `Some(vec![0.0, ...])` — an unreachable server has no tick rate,
        // and zero would render as a server in freefall.
        assert!(health.tps.is_none() && health.heap.is_none());
        assert!(health
            .rcon_error
            .as_deref()
            .unwrap_or_default()
            .contains("not reachable"));
    }

    #[tokio::test]
    async fn a_probe_is_reused_until_it_is_invalidated() {
        let cache = SnapshotCache::new(Duration::from_secs(600));
        let rcon = RconClient::new("127.0.0.1:1".to_string(), String::new());

        let first = cache.health(&rcon).await;
        // Same allocation, so no second probe ran.
        assert!(Arc::ptr_eq(&first, &cache.health(&rcon).await));

        cache.invalidate().await;
        assert!(
            !Arc::ptr_eq(&first, &cache.health(&rcon).await),
            "an operator action must not wait out the TTL"
        );
    }

    #[tokio::test]
    async fn a_probe_expires_once_its_ttl_passes() {
        let cache = SnapshotCache::new(Duration::from_millis(20));
        let rcon = RconClient::new("127.0.0.1:1".to_string(), String::new());

        let first = cache.health(&rcon).await;
        tokio::time::sleep(Duration::from_millis(40)).await;

        assert!(!Arc::ptr_eq(&first, &cache.health(&rcon).await));
    }
}
