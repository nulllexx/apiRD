//! Who ran what on the console.
//!
//! Every console action reaches the game server over one shared RCON
//! connection, so the game's own log records all of them identically — as
//! coming from RCON. From inside Minecraft there is no way to tell two
//! operators apart, which makes "prove I did not do that" impossible to answer
//! and "make it look like someone else did" easy to attempt.
//!
//! So attribution is recorded here, on the way through, where the
//! authenticated identity still exists. Two halves, and the difference between
//! them matters:
//!
//! * **The table is the evidence.** Written before the command runs and
//!   updated with its outcome, so a command that killed the process still left
//!   a record that it was attempted and by whom.
//! * **The stream is the display.** A line pushed to whoever has the console
//!   open, so an action is attributed as it happens rather than only in
//!   hindsight.
//!
//! ## Why the stream half rides its own event name
//!
//! Anything that can make the *server* print a line — `/say`, a player typing
//! in chat — can print a line that reads like an attribution. If those arrived
//! on the same SSE channel as real ones, forging an entry would be a chat
//! message, which is precisely the accusation this exists to settle.
//!
//! Audit entries are therefore delivered as `event: audit` frames carrying
//! JSON, on a channel the log tail cannot reach. The API decides the event
//! name; no amount of log content can put a line on it. A viewer renders those
//! distinctly, and a forged log line stays a log line.

use std::sync::Arc;

use serde::Serialize;
use sqlx::MySqlPool;

use super::LogHub;

/// How many entries a history request may ask for.
const MAX_LIMIT: u32 = 500;
const DEFAULT_LIMIT: u32 = 100;

/// What kind of console action an entry describes.
///
/// Kept as a small closed set rather than free text so the history can be
/// filtered on it and a new call site cannot invent a spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Free-text command typed into the console.
    Command,
    /// One of the moderation buttons: op, kick, ban, clear.
    Player,
    /// Start, stop or restart of the server itself.
    Power,
    /// An offline edit of a player's saved file.
    Vitals,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Command => "command",
            Kind::Player => "player",
            Kind::Power => "power",
            Kind::Vitals => "vitals",
        }
    }
}

/// How an action ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Recorded before the action runs. An entry left in this state is one
    /// whose command never reported back — worth being able to see.
    Pending,
    Ok,
    Failed,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Pending => "pending",
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
        }
    }
}

/// One recorded action, as the history endpoint returns it.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: i64,
    /// RFC 3339, in UTC.
    pub at: String,
    pub username: String,
    pub kind: String,
    pub command: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A handle to one recorded action, used to fill in how it ended.
///
/// Carries the context it was opened with so [`finish`] cannot be handed a
/// different command than the one that was recorded — the pair would otherwise
/// have to be kept in step by hand at every call site, in a module whose entire
/// job is that the record matches what happened.
///
/// Deliberately not `Clone`: an action has one outcome, and the borrow checker
/// enforcing that is cheaper than a convention.
pub struct Record {
    id: i64,
    username: String,
    kind: Kind,
    command: String,
    target: Option<String>,
}

fn now() -> chrono::NaiveDateTime {
    chrono::Utc::now().naive_utc()
}

fn rfc3339(at: chrono::NaiveDateTime) -> String {
    at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// Record that an action is about to run.
///
/// Written *before* the command is sent, because the alternative loses the
/// record of anything that does not come back — a command that stops the
/// server, or one that is still running when the process dies. An entry with
/// no outcome is more useful than no entry.
///
/// A failure here is returned rather than swallowed. Callers refuse to run the
/// action, which sounds severe until you notice that admin authentication is
/// itself a database query: if this cannot be written, nothing had any business
/// reaching the console in the first place.
pub async fn begin(
    pool: &MySqlPool,
    username: &str,
    kind: Kind,
    command: &str,
    target: Option<&str>,
) -> Result<Record, sqlx::Error> {
    let at = now();

    let result = sqlx::query(
        "INSERT INTO console_audit (at, username, kind, command, target, outcome)
         VALUES (?, ?, ?, ?, ?, ?)",
    )
    .bind(at)
    .bind(username)
    .bind(kind.as_str())
    .bind(command)
    .bind(target)
    .bind(Outcome::Pending.as_str())
    .execute(pool)
    .await?;

    Ok(Record {
        id: result.last_insert_id() as i64,
        username: username.to_string(),
        kind,
        command: command.to_string(),
        target: target.map(str::to_string),
    })
}

/// Fill in how an action ended, and announce it to anyone watching.
///
/// The broadcast happens here rather than in [`begin`] so the console shows a
/// settled fact — "banned Steve", not "is about to ban Steve" — and shows the
/// failure when there is one.
///
/// A failure to update is logged rather than returned: the action itself has
/// already happened, and turning a successful command into an error response
/// because its bookkeeping failed would misreport what the server did. The row
/// stays `pending`, which is visible in the history and is the honest record of
/// what is known.
pub async fn finish(
    pool: &MySqlPool,
    hub: &Arc<LogHub>,
    record: Record,
    outcome: Outcome,
    detail: Option<&str>,
) {
    if let Err(e) = sqlx::query("UPDATE console_audit SET outcome = ?, detail = ? WHERE id = ?")
        .bind(outcome.as_str())
        .bind(detail)
        .bind(record.id)
        .execute(pool)
        .await
    {
        log::error!("console: could not record the outcome of audit {}: {e}", record.id);
    }

    let entry = Entry {
        id: record.id,
        at: rfc3339(now()),
        username: record.username,
        kind: record.kind.as_str().to_string(),
        command: record.command,
        target: record.target,
        outcome: outcome.as_str().to_string(),
        detail: detail.map(str::to_string),
    };

    announce(hub, &entry);
}

/// Shorthand for the shape every call site has: run something, record how it
/// went, and give the caller back its result untouched.
///
/// Exists so no route has to remember to call [`finish`] on the error path.
/// A command that failed is exactly the one somebody will later want to prove
/// they did or did not run.
pub async fn settle<T, E: std::fmt::Display>(
    pool: &MySqlPool,
    hub: &Arc<LogHub>,
    record: Record,
    result: Result<T, E>,
) -> Result<T, E> {
    match &result {
        Ok(_) => finish(pool, hub, record, Outcome::Ok, None).await,
        Err(e) => finish(pool, hub, record, Outcome::Failed, Some(&e.to_string())).await,
    }
    result
}

/// Push one entry onto the audit channel as a single line of JSON.
///
/// One line because an SSE frame is line-oriented; JSON because the viewer
/// styles the parts differently and parsing a sentence back apart would be a
/// worse way to get them.
pub fn announce(hub: &Arc<LogHub>, entry: &Entry) {
    match serde_json::to_string(entry) {
        Ok(json) => hub.push(json),
        // Nothing in an Entry can fail to serialise, but a lost audit line must
        // not become a lost audit *row*, so this is a log line and not a panic.
        Err(e) => log::error!("console: could not encode audit entry: {e}"),
    }
}

/// The most recent entries, newest first.
///
/// Optionally narrowed to one operator, which is the shape the question
/// actually gets asked in: "what did this person run?"
pub async fn recent(
    pool: &MySqlPool,
    limit: Option<u32>,
    username: Option<&str>,
) -> Result<Vec<Entry>, sqlx::Error> {
    let limit = limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

    type Row = (
        i64,
        chrono::NaiveDateTime,
        String,
        String,
        String,
        Option<String>,
        String,
        Option<String>,
    );

    // Two statements rather than a concatenated WHERE clause: the filter is a
    // user-supplied string, and building SQL out of one is the mistake this
    // whole module exists to make people accountable for.
    let rows: Vec<Row> = match username {
        Some(name) => {
            sqlx::query_as(
                "SELECT id, at, username, kind, command, target, outcome, detail
                 FROM console_audit WHERE username = ? ORDER BY id DESC LIMIT ?",
            )
            .bind(name)
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
        None => {
            sqlx::query_as(
                "SELECT id, at, username, kind, command, target, outcome, detail
                 FROM console_audit ORDER BY id DESC LIMIT ?",
            )
            .bind(limit)
            .fetch_all(pool)
            .await?
        }
    };

    Ok(rows
        .into_iter()
        .map(
            |(id, at, username, kind, command, target, outcome, detail)| Entry {
                id,
                at: rfc3339(at),
                username,
                kind,
                command,
                target,
                outcome,
                detail,
            },
        )
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_and_outcomes_have_stable_names() {
        // These are stored in the database, so renaming one silently splits the
        // history in two.
        assert_eq!(Kind::Command.as_str(), "command");
        assert_eq!(Kind::Player.as_str(), "player");
        assert_eq!(Kind::Power.as_str(), "power");
        assert_eq!(Kind::Vitals.as_str(), "vitals");

        assert_eq!(Outcome::Pending.as_str(), "pending");
        assert_eq!(Outcome::Ok.as_str(), "ok");
        assert_eq!(Outcome::Failed.as_str(), "failed");
    }

    fn sample() -> Entry {
        Entry {
            id: 7,
            at: "2026-08-21T14:23:01.000Z".to_string(),
            username: "bs".to_string(),
            kind: Kind::Player.as_str().to_string(),
            command: "ban Steve griefing".to_string(),
            target: Some("Steve".to_string()),
            outcome: Outcome::Ok.as_str().to_string(),
            detail: None,
        }
    }

    /// An SSE frame is line-oriented, so an entry that serialised across
    /// several lines would arrive as several truncated events.
    #[test]
    fn an_entry_encodes_to_exactly_one_line() {
        let json = serde_json::to_string(&sample()).unwrap();

        assert!(!json.contains('\n'), "no raw newlines: {json}");
        assert!(!json.contains('\r'));
    }

    /// A command containing a newline cannot be typed through
    /// `validate_command`, but the audit line must survive one anyway — this is
    /// the record of what someone did, and it must not be the thing that
    /// desynchronises the stream it is reported on.
    #[test]
    fn a_command_with_a_newline_still_encodes_to_one_line() {
        let mut entry = sample();
        entry.command = "say hello\nsay goodbye".to_string();

        let json = serde_json::to_string(&entry).unwrap();
        assert!(!json.contains('\n'), "the newline must be escaped: {json}");
        assert!(json.contains("\\n"));
    }

    #[test]
    fn an_entry_carries_who_what_and_how_it_ended() {
        let json = serde_json::to_string(&sample()).unwrap();

        assert!(json.contains("\"username\":\"bs\""));
        assert!(json.contains("\"command\":\"ban Steve griefing\""));
        assert!(json.contains("\"outcome\":\"ok\""));
        assert!(json.contains("\"target\":\"Steve\""));
    }

    /// Absent fields are omitted rather than sent as null, so the viewer can
    /// test for presence without also testing for null.
    #[test]
    fn absent_fields_are_left_out() {
        let json = serde_json::to_string(&sample()).unwrap();
        assert!(!json.contains("detail"));
    }

    #[test]
    fn a_timestamp_renders_as_utc_rfc3339() {
        let at = chrono::NaiveDate::from_ymd_opt(2026, 8, 21)
            .unwrap()
            .and_hms_milli_opt(14, 23, 1, 500)
            .unwrap();

        assert_eq!(rfc3339(at), "2026-08-21T14:23:01.500Z");
    }
}
