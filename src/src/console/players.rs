//! Player roster for the admin console, plus the fixed set of per-player
//! actions.
//!
//! Almost everything the server knows about players lives in files next to
//! `server.properties` — `usercache.json`, `ops.json`, `banned-players.json`,
//! `whitelist.json` — or in the world's `playerdata` directory, which holds one
//! `.dat` per player who has *ever* joined. Reading those directly means the
//! roster still works while the server is stopped; RCON only ever answers who
//! is online right now.
//!
//! The two kinds of source complement each other rather than overlap.
//! `usercache.json` has names but expires entries after a month, so a player
//! who last joined in spring has vanished from it; `playerdata` never forgets
//! them but knows only their UUID. Merging the two gives the full history, with
//! names wherever the cache still has one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::strip_formatting;

/// Longest name Mojang issues.
const MAX_NAME_LEN: usize = 16;

/// Reasons are free text, so they get a cap of their own — long enough for a
/// real explanation, short enough that it cannot pad out an RCON packet.
const MAX_REASON_LEN: usize = 200;

/// Ceiling on `playerdata` entries walked in one request. A server that has
/// been up for years accumulates a lot of these, and the roster is not worth an
/// unbounded directory walk.
const MAX_PLAYERDATA: usize = 20_000;

/// The complete set of per-player operations the console offers.
///
/// Each maps to one hardcoded RCON verb. The action name from the URL never
/// reaches the command string itself, so an unexpected value fails to parse
/// rather than becoming a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayerAction {
    Op,
    Deop,
    Kick,
    Ban,
    Pardon,
    ClearInventory,
}

impl PlayerAction {
    /// Canonical names, in the order the UI offers them.
    pub const NAMES: [&'static str; 6] = ["op", "deop", "kick", "ban", "pardon", "clear"];

    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "op" => Some(PlayerAction::Op),
            "deop" => Some(PlayerAction::Deop),
            "kick" => Some(PlayerAction::Kick),
            "ban" => Some(PlayerAction::Ban),
            // Both spellings, because operators say "unban" and Minecraft says
            // "pardon".
            "pardon" | "unban" => Some(PlayerAction::Pardon),
            "clear" | "clearinventory" => Some(PlayerAction::ClearInventory),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PlayerAction::Op => "op",
            PlayerAction::Deop => "deop",
            PlayerAction::Kick => "kick",
            PlayerAction::Ban => "ban",
            PlayerAction::Pardon => "pardon",
            PlayerAction::ClearInventory => "clear",
        }
    }

    /// Whether a trailing free-text reason is meaningful for this action.
    pub fn takes_reason(self) -> bool {
        matches!(self, PlayerAction::Kick | PlayerAction::Ban)
    }
}

/// Build the RCON command for one action.
pub fn command_for(action: PlayerAction, player: &str, reason: Option<&str>) -> String {
    let mut command = format!("{} {}", action.as_str(), player);

    if action.takes_reason() {
        let reason = reason.map(sanitize_reason).unwrap_or_default();
        if !reason.is_empty() {
            command.push(' ');
            command.push_str(&reason);
        }
    }

    command
}

/// Whether a name is safe to interpolate into an RCON command.
///
/// This is the injection defence for the player actions. An RCON command is one
/// string that the server splits on whitespace, so a name containing a space
/// silently becomes extra arguments — `ban Steve` with a "name" of `Steve 30d`
/// is not the command the operator asked for. Mojang names are alphanumeric
/// plus underscore; the dot additionally allows the prefix Floodgate puts on
/// Bedrock players.
pub fn is_valid_name(raw: &str) -> bool {
    !raw.is_empty()
        && raw.chars().count() <= MAX_NAME_LEN
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
}

/// Clean up a free-text reason.
///
/// A reason is the trailing argument, so spaces are fine and wanted. Control
/// characters are not: this comes straight back out as a console line.
pub fn sanitize_reason(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(MAX_REASON_LEN)
        .collect::<String>()
        .trim()
        .to_string()
}

/// One row of the roster.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Player {
    /// `None` for a player whose `usercache.json` entry has expired — they are
    /// still listed, by UUID, rather than dropped.
    pub name: Option<String>,
    /// `None` for an online player none of the server files have recorded.
    pub uuid: Option<String>,
    pub online: bool,
    pub op: bool,
    #[serde(rename = "opLevel")]
    pub op_level: Option<u8>,
    pub banned: bool,
    pub whitelisted: bool,
    /// RFC 3339, from the player's `playerdata` file — effectively their last
    /// logout, since the server writes it on disconnect.
    #[serde(rename = "lastSeen")]
    pub last_seen: Option<String>,
}

/// One entry of any of Minecraft's `[{ "uuid": …, "name": … }]` files.
#[derive(Debug, Default, Deserialize)]
struct MojangEntry {
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    name: Option<String>,
    /// Only `ops.json` carries this.
    #[serde(default)]
    level: Option<u8>,
}

/// Parse one of those files, tolerating anything unexpected.
///
/// A missing, empty or malformed file yields an empty list rather than an
/// error. The server rewrites these while it runs, so a read can legitimately
/// catch one mid-write — and a partial roster beats no roster.
fn parse_entries(raw: &str) -> Vec<MojangEntry> {
    serde_json::from_str(raw).unwrap_or_default()
}

/// Everything [`build_roster`] merges together.
pub struct RosterSources<'a> {
    pub usercache: &'a str,
    pub ops: &'a str,
    pub banned: &'a str,
    pub whitelist: &'a str,
    /// Names from an RCON `list`, which carry no UUID.
    pub online: &'a [String],
    /// UUID -> RFC 3339 timestamp, from [`last_seen_map`].
    pub last_seen: &'a BTreeMap<String, String>,
}

fn normalise_uuid(raw: &str) -> Option<String> {
    let uuid = raw.trim().to_ascii_lowercase();
    (!uuid.is_empty()).then_some(uuid)
}

/// Record every UUID-bearing entry in a file, learning its name along the way.
fn seed(
    raw: &str,
    players: &mut BTreeMap<String, Player>,
    key_of_name: &mut BTreeMap<String, String>,
) {
    for entry in parse_entries(raw) {
        let Some(uuid) = entry.uuid.as_deref().and_then(normalise_uuid) else {
            continue;
        };

        let slot = players.entry(uuid.clone()).or_insert_with(|| Player {
            uuid: Some(uuid.clone()),
            ..Player::default()
        });

        if let Some(name) = entry.name.filter(|n| !n.trim().is_empty()) {
            key_of_name.insert(name.to_ascii_lowercase(), uuid.clone());
            if slot.name.is_none() {
                slot.name = Some(name);
            }
        }
    }
}

/// Find the row an entry refers to, by UUID when it has one and by name
/// otherwise.
fn lookup_mut<'a>(
    players: &'a mut BTreeMap<String, Player>,
    key_of_name: &BTreeMap<String, String>,
    uuid: Option<&str>,
    name: Option<&str>,
) -> Option<&'a mut Player> {
    let key = uuid
        .and_then(normalise_uuid)
        .or_else(|| name.and_then(|n| key_of_name.get(&n.to_ascii_lowercase()).cloned()))?;
    players.get_mut(&key)
}

/// Merge every source into one deduplicated roster.
///
/// Keyed on UUID wherever one is known, so the same player appearing in four
/// files is one row. Online names, which arrive without a UUID, are resolved
/// through the names learned from those files first — otherwise everyone
/// currently playing would show up twice.
pub fn build_roster(sources: RosterSources<'_>) -> Vec<Player> {
    let mut players: BTreeMap<String, Player> = BTreeMap::new();
    let mut key_of_name: BTreeMap<String, String> = BTreeMap::new();

    for file in [
        sources.usercache,
        sources.ops,
        sources.banned,
        sources.whitelist,
    ] {
        seed(file, &mut players, &mut key_of_name);
    }

    // Players whose usercache entry has expired survive only here, as a UUID.
    for uuid in sources.last_seen.keys() {
        players.entry(uuid.clone()).or_insert_with(|| Player {
            uuid: Some(uuid.clone()),
            ..Player::default()
        });
    }

    for entry in parse_entries(sources.ops) {
        if let Some(player) = lookup_mut(
            &mut players,
            &key_of_name,
            entry.uuid.as_deref(),
            entry.name.as_deref(),
        ) {
            player.op = true;
            player.op_level = entry.level;
        }
    }

    for entry in parse_entries(sources.banned) {
        if let Some(player) = lookup_mut(
            &mut players,
            &key_of_name,
            entry.uuid.as_deref(),
            entry.name.as_deref(),
        ) {
            player.banned = true;
        }
    }

    for entry in parse_entries(sources.whitelist) {
        if let Some(player) = lookup_mut(
            &mut players,
            &key_of_name,
            entry.uuid.as_deref(),
            entry.name.as_deref(),
        ) {
            player.whitelisted = true;
        }
    }

    for name in sources.online {
        let key = key_of_name
            .get(&name.to_ascii_lowercase())
            .cloned()
            .unwrap_or_else(|| format!("name:{}", name.to_ascii_lowercase()));

        let slot = players.entry(key).or_default();
        if slot.name.is_none() {
            slot.name = Some(name.clone());
        }
        slot.online = true;
    }

    for (uuid, seen) in sources.last_seen {
        if let Some(player) = players.get_mut(uuid) {
            player.last_seen = Some(seen.clone());
        }
    }

    let mut roster: Vec<Player> = players.into_values().collect();
    // Online first — that is what an operator is looking for — then by name,
    // with the nameless UUID-only rows last rather than sorted among them.
    roster.sort_by(|a, b| {
        b.online.cmp(&a.online).then_with(|| {
            let key = |p: &Player| {
                (
                    p.name.is_none(),
                    p.name.as_deref().unwrap_or_default().to_ascii_lowercase(),
                    p.uuid.clone().unwrap_or_default(),
                )
            };
            key(a).cmp(&key(b))
        })
    });
    roster
}

/// Extract the player names from an RCON `list` reply.
///
/// The vanilla format is `There are 2 of a max of 20 players online: A, B`, but
/// plugins rewrite it freely — this server's own reply is
/// `§6There are §c0§6 out of maximum §c20§6 players online.`, which has no
/// colon at all and so correctly yields nobody. Taking everything after the
/// *last* colon and keeping only well-formed names is what makes that
/// tolerable: a reworded preamble cannot turn into a fake player.
pub fn parse_online_list(raw: &str) -> Vec<String> {
    let plain = strip_formatting(raw);
    let Some(colon) = plain.rfind(':') else {
        return Vec::new();
    };

    plain[colon + 1..]
        .split(',')
        .map(str::trim)
        .filter(|name| is_valid_name(name))
        .map(str::to_string)
        .collect()
}

/// How many players an RCON `list` reply *claims* are online.
///
/// Needed because [`parse_online_list`] cannot tell "nobody is online" apart
/// from "this reply is worded in a way I cannot read". Both yield no names, and
/// treating the second as the first silently empties the roster — which is
/// exactly what happened here: this server answers
/// `There are 1 out of maximum 20 players online.`, with the names in a form
/// this parser does not recognise, so every reconcile blanked whoever the log
/// had just reported joining.
///
/// The count is read from the text *before* the last colon, so a player whose
/// name contains digits cannot be mistaken for it.
pub fn parse_online_count(raw: &str) -> Option<usize> {
    let plain = strip_formatting(raw);
    let head = match plain.rfind(':') {
        Some(colon) => &plain[..colon],
        None => &plain[..],
    };

    let digits: String = head
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(char::is_ascii_digit)
        .collect();

    digits.parse().ok()
}

/// Directory holding `server.properties` — and, next to it, every file the
/// roster is built from.
///
/// Derived from the configured path rather than being its own setting, so there
/// is no second value to keep in sync with it.
pub fn server_dir(server_properties_path: &str) -> PathBuf {
    Path::new(server_properties_path)
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// `level-name` from the contents of `server.properties`.
///
/// The value becomes a path segment, so anything that could escape the server
/// directory falls back to the default instead. `server.properties` is not
/// attacker-controlled today, but a world name that walks out of the data
/// directory would be a confusing way to discover otherwise.
pub fn level_name(properties: &str) -> String {
    properties
        .lines()
        .find_map(|line| line.trim().strip_prefix("level-name="))
        .map(|value| value.trim().to_string())
        .filter(|value| {
            !value.is_empty() && value != ".." && !value.contains('/') && !value.contains('\\')
        })
        .unwrap_or_else(|| "world".to_string())
}

/// Read a file, treating any failure as "not there".
///
/// Every caller here wants a degraded roster rather than a failed request: a
/// server that has never had an op simply has no `ops.json`.
pub async fn read_optional(path: &Path) -> String {
    tokio::fs::read_to_string(path).await.unwrap_or_default()
}

/// Last-seen times, keyed by UUID, from the mtime of each `playerdata` file.
///
/// The server rewrites a player's `.dat` when they disconnect, so the mtime is
/// a good proxy for when they were last here — and it is the only such record
/// that survives `usercache.json` expiring.
pub async fn last_seen_map(playerdata_dir: &Path) -> BTreeMap<String, String> {
    let mut seen = BTreeMap::new();

    let Ok(mut entries) = tokio::fs::read_dir(playerdata_dir).await else {
        return seen;
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        if seen.len() >= MAX_PLAYERDATA {
            log::warn!("console: stopped reading playerdata at {MAX_PLAYERDATA} entries");
            break;
        }

        let path = entry.path();
        // The server also keeps a `.dat_old` backup of the same player.
        if path.extension().and_then(|e| e.to_str()) != Some("dat") {
            continue;
        }
        let Some(uuid) = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(normalise_uuid)
        else {
            continue;
        };

        if let Ok(modified) = entry.metadata().await.and_then(|m| m.modified()) {
            let stamp: chrono::DateTime<chrono::Utc> = modified.into();
            seen.insert(uuid, stamp.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        }
    }

    seen
}

/// Load and merge the whole roster from disk.
pub async fn load_roster(server_properties_path: &str, online: &[String]) -> Vec<Player> {
    let dir = server_dir(server_properties_path);
    let properties = read_optional(Path::new(server_properties_path)).await;
    let world = dir.join(level_name(&properties));

    let last_seen = last_seen_map(&world.join("playerdata")).await;
    let usercache = read_optional(&dir.join("usercache.json")).await;
    let ops = read_optional(&dir.join("ops.json")).await;
    let banned = read_optional(&dir.join("banned-players.json")).await;
    let whitelist = read_optional(&dir.join("whitelist.json")).await;

    build_roster(RosterSources {
        usercache: &usercache,
        ops: &ops,
        banned: &banned,
        whitelist: &whitelist,
        online,
        last_seen: &last_seen,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sources<'a>(
        usercache: &'a str,
        ops: &'a str,
        banned: &'a str,
        whitelist: &'a str,
        online: &'a [String],
        last_seen: &'a BTreeMap<String, String>,
    ) -> RosterSources<'a> {
        RosterSources {
            usercache,
            ops,
            banned,
            whitelist,
            online,
            last_seen,
        }
    }

    fn named(roster: &[Player], name: &str) -> Player {
        roster
            .iter()
            .find(|p| p.name.as_deref() == Some(name))
            .unwrap_or_else(|| panic!("{name} missing from {roster:?}"))
            .clone()
    }

    /* -------------------------------------------------------------- actions */

    #[test]
    fn parses_every_offered_action() {
        for name in PlayerAction::NAMES {
            assert!(PlayerAction::parse(name).is_some(), "{name} must parse");
        }
    }

    #[test]
    fn accepts_the_operator_spellings_too() {
        assert_eq!(PlayerAction::parse("unban"), Some(PlayerAction::Pardon));
        assert_eq!(
            PlayerAction::parse("clearinventory"),
            Some(PlayerAction::ClearInventory)
        );
        assert_eq!(PlayerAction::parse(" OP "), Some(PlayerAction::Op));
    }

    #[test]
    fn rejects_anything_outside_the_action_set() {
        for bogus in ["", "stop", "say", "op x", "../op", "deop;stop"] {
            assert_eq!(PlayerAction::parse(bogus), None, "{bogus:?} must be refused");
        }
    }

    #[test]
    fn builds_the_expected_commands() {
        assert_eq!(command_for(PlayerAction::Op, "Steve", None), "op Steve");
        assert_eq!(
            command_for(PlayerAction::ClearInventory, "Steve", None),
            "clear Steve"
        );
        assert_eq!(
            command_for(PlayerAction::Kick, "Steve", Some("griefing")),
            "kick Steve griefing"
        );
    }

    #[test]
    fn a_reason_is_dropped_by_actions_that_take_none() {
        // `op Steve because` is a syntax error at the server, so the reason is
        // dropped rather than passed through.
        assert_eq!(
            command_for(PlayerAction::Op, "Steve", Some("because")),
            "op Steve"
        );
    }

    #[test]
    fn a_blank_reason_does_not_leave_a_trailing_space() {
        assert_eq!(
            command_for(PlayerAction::Kick, "Steve", Some("   ")),
            "kick Steve"
        );
        assert_eq!(command_for(PlayerAction::Kick, "Steve", None), "kick Steve");
    }

    /* ----------------------------------------------------------- validation */

    #[test]
    fn accepts_real_usernames() {
        for name in [
            "Steve",
            "Notch",
            "a",
            "Player_123",
            ".BedrockGuy",
            "0123456789abcdef",
        ] {
            assert!(is_valid_name(name), "{name} should be accepted");
        }
    }

    #[test]
    fn rejects_names_that_would_change_the_command() {
        // The space is the whole risk: `ban Steve 30d` is not `ban "Steve 30d"`.
        for name in [
            "Steve Jobs",
            "Steve\nop me",
            "Steve;stop",
            "",
            "01234567890abcdefg",
        ] {
            assert!(!is_valid_name(name), "{name:?} should be refused");
        }
    }

    #[test]
    fn reason_keeps_spaces_but_drops_control_characters() {
        assert_eq!(sanitize_reason("  being a nuisance  "), "being a nuisance");
        assert_eq!(sanitize_reason("stop\nban everyone"), "stopban everyone");
    }

    #[test]
    fn reason_is_capped() {
        assert_eq!(sanitize_reason(&"a".repeat(400)).len(), MAX_REASON_LEN);
    }

    /* --------------------------------------------------------- list parsing */

    #[test]
    fn reads_the_vanilla_list_format() {
        assert_eq!(
            parse_online_list("There are 2 of a max of 20 players online: Steve, Alex"),
            vec!["Steve", "Alex"]
        );
    }

    #[test]
    fn reads_a_coloured_list() {
        assert_eq!(
            parse_online_list("\u{a7}6Online \u{a7}c2\u{a7}6: \u{a7}aSteve, \u{a7}aAlex"),
            vec!["Steve", "Alex"]
        );
    }

    #[test]
    fn an_empty_server_yields_nobody() {
        assert_eq!(
            parse_online_list("There are 0 of a max of 20 players online:"),
            Vec::<String>::new()
        );
        // This server's own plugin-rewritten reply, which has no colon at all.
        assert_eq!(
            parse_online_list(
                "\u{a7}6There are \u{a7}c0\u{a7}6 out of maximum \u{a7}c20\u{a7}6 players online."
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn prose_after_the_colon_does_not_become_a_player() {
        // The name filter is what stops a reworded reply from inventing players
        // out of whatever words follow the last colon.
        assert_eq!(
            parse_online_list("Error: the server is still starting up"),
            Vec::<String>::new()
        );
    }

    /* -------------------------------------------------------------- roster */

    #[test]
    fn merges_the_four_files_into_one_row_per_player() {
        let usercache =
            r#"[{"name":"Steve","uuid":"AAAA-1111"},{"name":"Alex","uuid":"bbbb-2222"}]"#;
        let ops = r#"[{"uuid":"aaaa-1111","name":"Steve","level":4}]"#;
        let banned = r#"[{"uuid":"bbbb-2222","name":"Alex"}]"#;
        let whitelist = r#"[{"uuid":"aaaa-1111","name":"Steve"}]"#;
        let last_seen = BTreeMap::new();

        let roster = build_roster(sources(usercache, ops, banned, whitelist, &[], &last_seen));

        assert_eq!(roster.len(), 2, "one row per player: {roster:?}");
        let steve = named(&roster, "Steve");
        assert!(steve.op && steve.whitelisted && !steve.banned);
        assert_eq!(steve.op_level, Some(4));
        // UUIDs are normalised, so the mixed-case usercache entry and the
        // lower-case ops entry are recognised as the same player.
        assert_eq!(steve.uuid.as_deref(), Some("aaaa-1111"));
        assert!(named(&roster, "Alex").banned);
    }

    #[test]
    fn an_online_player_does_not_become_a_second_row() {
        let usercache = r#"[{"name":"Steve","uuid":"aaaa-1111"}]"#;
        let online = vec!["Steve".to_string()];
        let last_seen = BTreeMap::new();

        let roster = build_roster(sources(usercache, "", "", "", &online, &last_seen));

        assert_eq!(roster.len(), 1, "matched by name onto the cached row");
        assert!(roster[0].online);
        assert_eq!(roster[0].uuid.as_deref(), Some("aaaa-1111"));
    }

    #[test]
    fn an_online_player_the_files_have_never_seen_still_appears() {
        let online = vec!["Newcomer".to_string()];
        let last_seen = BTreeMap::new();
        let roster = build_roster(sources("", "", "", "", &online, &last_seen));

        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].name.as_deref(), Some("Newcomer"));
        assert!(roster[0].online && roster[0].uuid.is_none());
    }

    #[test]
    fn a_player_whose_cache_entry_expired_survives_as_a_uuid() {
        // The whole reason playerdata is read: usercache drops entries after a
        // month, and "everyone who has joined" has to mean everyone.
        let mut last_seen = BTreeMap::new();
        last_seen.insert("cccc-3333".to_string(), "2026-01-02T03:04:05Z".to_string());

        let roster = build_roster(sources("", "", "", "", &[], &last_seen));

        assert_eq!(roster.len(), 1);
        assert_eq!(roster[0].uuid.as_deref(), Some("cccc-3333"));
        assert_eq!(roster[0].name, None);
        assert_eq!(roster[0].last_seen.as_deref(), Some("2026-01-02T03:04:05Z"));
    }

    #[test]
    fn online_players_sort_first_then_by_name() {
        let usercache = r#"[
            {"name":"Zoe","uuid":"1111"},
            {"name":"adam","uuid":"2222"},
            {"name":"Mia","uuid":"3333"}
        ]"#;
        let online = vec!["Zoe".to_string()];
        let last_seen = BTreeMap::new();

        let roster = build_roster(sources(usercache, "", "", "", &online, &last_seen));
        let names: Vec<_> = roster.iter().filter_map(|p| p.name.as_deref()).collect();

        // Case-insensitive, or "adam" would sort after both capitalised names.
        assert_eq!(names, vec!["Zoe", "adam", "Mia"]);
    }

    #[test]
    fn a_corrupt_file_degrades_instead_of_failing() {
        // These files are rewritten live, so a read can catch one half-written.
        let usercache = r#"[{"name":"Steve","uuid":"aaaa"}]"#;
        let last_seen = BTreeMap::new();
        let roster = build_roster(sources(
            usercache,
            "{ truncated",
            "",
            "not json",
            &[],
            &last_seen,
        ));

        assert_eq!(roster.len(), 1);
        assert!(!roster[0].op, "an unreadable ops.json must not grant op");
    }

    /* ---------------------------------------------------------- filesystem */

    #[test]
    fn level_name_defaults_when_absent_or_unsafe() {
        assert_eq!(level_name("level-name=survival\nmax-players=20"), "survival");
        assert_eq!(level_name("max-players=20"), "world");
        assert_eq!(level_name("level-name="), "world");
        // A world name that walks out of the data directory is refused.
        assert_eq!(level_name("level-name=../../etc"), "world");
        assert_eq!(level_name("level-name=.."), "world");
    }

    #[test]
    fn server_dir_is_the_properties_file_parent() {
        assert_eq!(
            server_dir("/mcserver/server.properties"),
            PathBuf::from("/mcserver")
        );
    }

    #[tokio::test]
    async fn last_seen_reads_dat_files_and_skips_the_backups() {
        let dir = std::env::temp_dir().join(format!("apird-playerdata-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.unwrap();

        tokio::fs::write(dir.join("AAAA-1111.dat"), b"x").await.unwrap();
        tokio::fs::write(dir.join("bbbb-2222.dat_old"), b"x").await.unwrap();
        tokio::fs::write(dir.join("notes.txt"), b"x").await.unwrap();

        let seen = last_seen_map(&dir).await;

        assert_eq!(seen.len(), 1, "only .dat counts: {seen:?}");
        // Lower-cased so it matches the UUIDs the JSON files carry.
        assert!(seen.contains_key("aaaa-1111"));

        tokio::fs::remove_dir_all(&dir).await.ok();
    }

    #[tokio::test]
    async fn a_missing_playerdata_directory_is_empty_not_an_error() {
        let missing = std::env::temp_dir().join("apird-playerdata-does-not-exist");
        assert!(last_seen_map(&missing).await.is_empty());
    }
}

#[cfg(test)]
mod online_count_tests {
    use super::*;

    #[test]
    fn reads_the_count_from_the_vanilla_reply() {
        assert_eq!(
            parse_online_count("There are 2 of a max of 20 players online: Steve, Alex"),
            Some(2)
        );
    }

    /// The shape this server actually sends. It carries no colon, so
    /// `parse_online_list` finds no names -- but the count still says someone
    /// is on, which is what stops the roster being blanked.
    #[test]
    fn reads_the_count_when_no_names_can_be_parsed() {
        let reply = "\u{a7}6There are \u{a7}c1\u{a7}6 out of maximum \u{a7}c20\u{a7}6 players online.";
        assert!(parse_online_list(reply).is_empty(), "no names are parseable");
        assert_eq!(parse_online_count(reply), Some(1), "but the count is");
    }

    #[test]
    fn an_genuinely_empty_server_reports_zero() {
        let reply = "\u{a7}6There are \u{a7}c0\u{a7}6 out of maximum \u{a7}c20\u{a7}6 players online.";
        assert!(parse_online_list(reply).is_empty());
        // Some(0) is what permits the roster to be cleared.
        assert_eq!(parse_online_count(reply), Some(0));
    }

    #[test]
    fn digits_in_a_player_name_are_not_the_count() {
        // The count is read before the last colon; names come after it.
        assert_eq!(
            parse_online_count("There are 1 of a max of 20 players online: Robighost01"),
            Some(1)
        );
        assert_eq!(
            parse_online_list("There are 1 of a max of 20 players online: Robighost01"),
            vec!["Robighost01".to_string()]
        );
    }

    #[test]
    fn a_reply_with_no_number_at_all_is_unknown() {
        // Unknown is deliberately not zero: the caller must not clear on it.
        assert_eq!(parse_online_count("who knows"), None);
    }
}
