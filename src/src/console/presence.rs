//! Who is on the server, maintained live from the log the console already
//! tails.
//!
//! The obvious source is an RCON `list`, but the server writes a log line for
//! every RCON command — into the very log this console streams — so polling it
//! often enough to feel live is exactly the behaviour that made the panel
//! unpleasant. `plrCount.json` is no better: nothing in this repository writes
//! it, so it is only as correct as whatever does.
//!
//! The log, on the other hand, is already being read for free. Connects and
//! disconnects are logged by the server core rather than by the join-message
//! broadcast, which is the part plugins rewrite:
//!
//! ```text
//! [12:34:56] [Server thread/INFO]: Steve[/10.0.0.4:52134] logged in with entity id 214 at (...)
//! [12:35:10] [Server thread/INFO]: Steve lost connection: Disconnected
//! ```
//!
//! That gives instant updates at zero cost. What it cannot give is a starting
//! point — the log only reports *changes* — so an occasional `list` reconciles
//! the set, and a server restart forces one immediately.

use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use super::players::is_valid_name;

/// How long a reconciled reading stays trusted before the server is asked
/// again. Drift only happens if a log line is missed, so this is a backstop
/// rather than the main mechanism.
pub const RESYNC_AFTER: Duration = Duration::from_secs(300);

/// Floor on how often a *failed* reconcile is retried. Without it an
/// unreachable server would be dialled once per poll, and each dial costs a
/// five-second connect timeout that the request sits and waits out.
pub const RETRY_AFTER: Duration = Duration::from_secs(30);

/// What a log line says about who is on the server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PresenceEvent {
    Joined(String),
    Left(String),
    /// The server started or stopped. Nobody is on, and the reading should be
    /// reconciled rather than trusted.
    Reset,
}

/// Strip Minecraft's log prefix, leaving the message itself.
///
/// The prefix is a timestamp followed by one or more bracketed tags —
/// `[12:34:56] [Server thread/INFO] [Essentials/]: `. It is peeled off the
/// *front* rather than found by searching for the last `]: `, because a
/// disconnect reason can carry a stack trace full of brackets of its own and
/// searching backwards would land in the middle of it.
fn message(line: &str) -> &str {
    let mut rest = line.trim_start();

    while rest.starts_with('[') {
        match rest.find(']') {
            Some(end) => rest = rest[end + 1..].trim_start(),
            None => break,
        }
    }

    // The colon ends the prefix, so a message that itself begins with a bracket
    // survives the loop above intact.
    rest.strip_prefix(':').unwrap_or(rest).trim_start()
}

/// Read one log line as a presence change, if it is one.
pub fn read_line(line: &str) -> Option<PresenceEvent> {
    let message = message(line);

    // Anchored at the start of the message, not merely contained in it, so a
    // player typing "Stopping the server" in chat cannot clear the roster.
    if message.starts_with("Stopping the server")
        || message.starts_with("Stopping server")
        // `Done (21.402s)! For help, type "help"` — the server just came up.
        || message.starts_with("Done (")
    {
        return Some(PresenceEvent::Reset);
    }

    if let Some(index) = message.find(" logged in with entity id") {
        // `Steve[/10.0.0.4:52134] logged in with entity id 214 at (...)`.
        // Requiring the `[/` is what keeps a chat message quoting this text
        // from registering as a join.
        let (name, _address) = message[..index].split_once("[/")?;
        return is_valid_name(name).then(|| PresenceEvent::Joined(name.to_string()));
    }

    if let Some(index) = message.find(" lost connection:") {
        let name = &message[..index];
        // Chat would leave `<Steve> I` here, which is not a valid name.
        return is_valid_name(name).then(|| PresenceEvent::Left(name.to_string()));
    }

    None
}

#[derive(Default)]
struct SyncState {
    /// Last *successful* reconcile against the server.
    synced_at: Option<Instant>,
    /// Last attempt, successful or not — the retry floor.
    attempted_at: Option<Instant>,
}

/// The live set of online players.
pub struct Presence {
    online: RwLock<BTreeSet<String>>,
    sync: RwLock<SyncState>,
}

impl Default for Presence {
    fn default() -> Self {
        Self {
            online: RwLock::new(BTreeSet::new()),
            sync: RwLock::new(SyncState::default()),
        }
    }
}

impl Presence {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Fold one log line into the set. Returns whether anything changed.
    pub fn apply(&self, line: &str) -> bool {
        let Some(event) = read_line(line) else {
            return false;
        };

        match event {
            PresenceEvent::Joined(name) => self
                .online
                .write()
                .map(|mut set| set.insert(name))
                .unwrap_or(false),
            PresenceEvent::Left(name) => self
                .online
                .write()
                .map(|mut set| set.remove(&name))
                .unwrap_or(false),
            PresenceEvent::Reset => {
                let changed = self
                    .online
                    .write()
                    .map(|mut set| {
                        let had = !set.is_empty();
                        set.clear();
                        had
                    })
                    .unwrap_or(false);

                // A restart is exactly when the set is least trustworthy, so
                // clear both clocks and let the next read reconcile at once.
                if let Ok(mut sync) = self.sync.write() {
                    *sync = SyncState::default();
                }
                changed
            }
        }
    }

    pub fn names(&self) -> Vec<String> {
        self.online
            .read()
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn count(&self) -> usize {
        self.online.read().map(|set| set.len()).unwrap_or(0)
    }

    /// Whether the set is due a reconcile against the server itself.
    pub fn needs_sync(&self) -> bool {
        let Ok(sync) = self.sync.read() else {
            return true;
        };

        let stale = sync
            .synced_at
            .is_none_or(|at| at.elapsed() > RESYNC_AFTER);
        let retry_allowed = sync
            .attempted_at
            .is_none_or(|at| at.elapsed() > RETRY_AFTER);

        stale && retry_allowed
    }

    /// Adopt the server's own answer as the truth.
    pub fn replace(&self, names: &[String]) {
        if let Ok(mut set) = self.online.write() {
            *set = names.iter().cloned().collect();
        }
        if let Ok(mut sync) = self.sync.write() {
            let now = Instant::now();
            sync.synced_at = Some(now);
            sync.attempted_at = Some(now);
        }
    }

    /// Note that a reconcile was tried and failed, starting the retry floor.
    ///
    /// The set is deliberately left alone: an unreachable server does not mean
    /// an empty one, and reporting nobody online would be a worse lie than
    /// reporting a slightly stale list.
    pub fn record_failure(&self) {
        if let Ok(mut sync) = self.sync.write() {
            sync.attempted_at = Some(Instant::now());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOGIN: &str = "[12:34:56] [Server thread/INFO]: Steve[/10.0.0.4:52134] \
                         logged in with entity id 214 at ([world]-12.5, 68.0, 231.5)";
    const LOGOUT: &str = "[12:35:10] [Server thread/INFO]: Steve lost connection: Disconnected";

    /* ------------------------------------------------------- prefix parsing */

    #[test]
    fn strips_the_log_prefix() {
        assert_eq!(
            message("[12:34:56] [Server thread/INFO]: hello"),
            "hello"
        );
        assert_eq!(
            message("[12:34:56] [Server thread/INFO] [Essentials/]: hello"),
            "hello"
        );
    }

    #[test]
    fn leaves_a_message_that_starts_with_a_bracket_alone() {
        // The colon ends the prefix, so this bracket is content, not a tag.
        assert_eq!(
            message("[12:34:56] [Server thread/INFO]: [Rcon: done]"),
            "[Rcon: done]"
        );
    }

    /* -------------------------------------------------------------- events */

    #[test]
    fn reads_a_login() {
        assert_eq!(
            read_line(LOGIN),
            Some(PresenceEvent::Joined("Steve".to_string()))
        );
    }

    #[test]
    fn reads_a_disconnect() {
        assert_eq!(
            read_line(LOGOUT),
            Some(PresenceEvent::Left("Steve".to_string()))
        );
    }

    #[test]
    fn reads_a_bedrock_name() {
        // Floodgate prefixes Bedrock players with a dot.
        let line = "[12:34:56] [Server thread/INFO]: .BedrockGuy[/10.0.0.9:1234] \
                    logged in with entity id 7 at (0.5, 64.0, 0.5)";
        assert_eq!(
            read_line(line),
            Some(PresenceEvent::Joined(".BedrockGuy".to_string()))
        );
    }

    #[test]
    fn a_disconnect_reason_full_of_brackets_still_parses() {
        // Searching backwards for the last `]: ` would land inside the trace
        // and lose the disconnect entirely.
        let line = "[12:35:10] [Server thread/INFO]: Steve lost connection: \
                    Internal Exception: io.netty.handler.codec.DecoderException: [id]: boom";
        assert_eq!(
            read_line(line),
            Some(PresenceEvent::Left("Steve".to_string()))
        );
    }

    #[test]
    fn a_server_start_or_stop_resets_the_set() {
        for line in [
            "[12:30:00] [Server thread/INFO]: Done (21.402s)! For help, type \"help\"",
            "[12:40:00] [Server thread/INFO]: Stopping the server",
        ] {
            assert_eq!(read_line(line), Some(PresenceEvent::Reset), "{line}");
        }
    }

    /// Log lines carry player chat, which is attacker-controlled. None of these
    /// may move the roster.
    #[test]
    fn chat_cannot_forge_a_presence_event() {
        for line in [
            "[12:34:56] [Server thread/INFO]: <Steve> hey I logged in with entity id lol",
            "[12:34:56] [Server thread/INFO]: <Steve> I lost connection: rip",
            "[12:34:56] [Server thread/INFO]: <Steve> Stopping the server",
            "[12:34:56] [Server thread/INFO]: <Steve> Done (0.1s)!",
            "[12:34:56] [Server thread/INFO]: <Steve> Alex[/1.2.3.4:1] logged in with entity id 5",
        ] {
            assert_eq!(read_line(line), None, "{line} must not be an event");
        }
    }

    #[test]
    fn ordinary_lines_are_not_events() {
        for line in [
            "[12:34:56] [Server thread/INFO]: Steve joined the game",
            "[12:34:56] [Server thread/WARN]: Can't keep up! Is the server overloaded?",
            "",
        ] {
            assert_eq!(read_line(line), None, "{line:?}");
        }
    }

    /* ------------------------------------------------------------ the set */

    #[test]
    fn tracks_players_in_and_out() {
        let presence = Presence::new();

        assert!(presence.apply(LOGIN));
        assert_eq!(presence.names(), vec!["Steve"]);
        assert_eq!(presence.count(), 1);

        assert!(presence.apply(LOGOUT));
        assert!(presence.names().is_empty());
    }

    #[test]
    fn a_line_that_changes_nothing_reports_no_change() {
        let presence = Presence::new();

        assert!(presence.apply(LOGIN));
        // The same login twice is one player, and the second is not news.
        assert!(!presence.apply(LOGIN));
        assert!(!presence.apply("[12:34:56] [Server thread/INFO]: nothing to see"));
    }

    #[test]
    fn a_restart_empties_the_set_and_forces_a_reconcile() {
        let presence = Presence::new();
        presence.replace(&["Steve".to_string()]);
        assert!(!presence.needs_sync(), "just reconciled");

        presence.apply("[12:40:00] [Server thread/INFO]: Stopping the server");

        assert!(presence.names().is_empty());
        assert!(
            presence.needs_sync(),
            "the set is least trustworthy right after a restart"
        );
    }

    #[test]
    fn a_fresh_tracker_wants_a_reconcile() {
        // The log reports changes, not a starting point, so the first reading
        // has to come from the server.
        assert!(Presence::new().needs_sync());
    }

    #[test]
    fn a_successful_reconcile_adopts_the_server_answer() {
        let presence = Presence::new();
        presence.apply(LOGIN);

        presence.replace(&["Alex".to_string(), "Robin".to_string()]);

        assert_eq!(presence.names(), vec!["Alex", "Robin"]);
        assert!(!presence.needs_sync());
    }

    #[test]
    fn a_failed_reconcile_keeps_the_last_known_set() {
        let presence = Presence::new();
        presence.apply(LOGIN);

        presence.record_failure();

        // An unreachable server is not an empty one; blanking the list would
        // be a worse lie than a slightly stale one.
        assert_eq!(presence.names(), vec!["Steve"]);
        assert!(
            !presence.needs_sync(),
            "a down server must not be re-dialled on every single poll"
        );
    }
}
