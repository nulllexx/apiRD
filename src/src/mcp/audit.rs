//! Which tools each assistant called.
//!
//! Same shape as `console::audit`: a row is written *before* the tool runs
//! and given its outcome afterwards, and a call whose row cannot be written
//! is not run. The trail answers "what did that token read, and when?" --
//! the question asked after a token turns up somewhere it should not be.

use serde::Serialize;
use sqlx::MySqlPool;

use super::token::TokenContext;

/// Arguments are recorded for context, not replayed, so a long one is cut.
const MAX_ARGUMENTS: usize = 1000;
const MAX_DETAIL: usize = 500;
pub const MAX_LIMIT: u32 = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ok,
    /// The tool ran and reported an error.
    Failed,
    /// The token does not allow the tool.
    Refused,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Failed => "failed",
            Outcome::Refused => "refused",
        }
    }
}

/// One recorded call, for the panel.
#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub id: i64,
    /// RFC 3339, UTC.
    pub at: String,
    #[serde(rename = "tokenId")]
    pub token_id: i64,
    #[serde(rename = "tokenName")]
    pub token_name: String,
    pub tool: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arguments: Option<String>,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// A call that has been recorded and not yet settled. Not `Clone`: a call
/// has one outcome.
pub struct Pending {
    id: i64,
}

/// Cuts `s` to at most `max` characters, marking the cut.
pub fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s.to_string(),
        Some((at, _)) => format!("{}…", &s[..at]),
    }
}

pub async fn begin(
    pool: &MySqlPool,
    token: &TokenContext,
    tool: &str,
    arguments: &serde_json::Value,
) -> Result<Pending, sqlx::Error> {
    let arguments = match arguments {
        serde_json::Value::Null => None,
        serde_json::Value::Object(map) if map.is_empty() => None,
        other => Some(clip(&other.to_string(), MAX_ARGUMENTS)),
    };
    let result = sqlx::query(
        "INSERT INTO mcp_audit (at, username, token_id, token_name, tool, arguments, outcome)
         VALUES (UTC_TIMESTAMP(3), ?, ?, ?, ?, ?, 'pending')",
    )
    .bind(&token.principal.username)
    .bind(token.id)
    .bind(&token.name)
    .bind(tool)
    .bind(arguments)
    .execute(pool)
    .await?;
    Ok(Pending { id: result.last_insert_id() as i64 })
}

/// Records how a call ended. A failure here is logged, not returned: the
/// tool has already run, and the row stays `pending`, which is the honest
/// record of what is known.
pub async fn finish(pool: &MySqlPool, pending: Pending, outcome: Outcome, detail: Option<&str>) {
    let detail = detail.map(|d| clip(d, MAX_DETAIL));
    if let Err(e) = sqlx::query("UPDATE mcp_audit SET outcome = ?, detail = ? WHERE id = ?")
        .bind(outcome.as_str())
        .bind(detail)
        .bind(pending.id)
        .execute(pool)
        .await
    {
        log::error!("mcp: could not record the outcome of audit {}: {e}", pending.id);
    }
}

/// An account's most recent calls, newest first.
pub async fn recent_for(pool: &MySqlPool, username: &str, limit: u32) -> Result<Vec<Entry>, sqlx::Error> {
    type Row = (i64, chrono::NaiveDateTime, i64, String, String, Option<String>, String, Option<String>);
    let rows: Vec<Row> = sqlx::query_as(
        "SELECT id, at, token_id, token_name, tool, arguments, outcome, detail
         FROM mcp_audit WHERE username = ? ORDER BY id DESC LIMIT ?",
    )
    .bind(username)
    .bind(limit.clamp(1, MAX_LIMIT))
    .fetch_all(pool)
    .await?;

    Ok(rows
        .into_iter()
        .map(|(id, at, token_id, token_name, tool, arguments, outcome, detail)| Entry {
            id,
            at: rfc3339(at),
            token_id,
            token_name,
            tool,
            arguments,
            outcome,
            detail,
        })
        .collect())
}

pub fn rfc3339(at: chrono::NaiveDateTime) -> String {
    at.and_utc().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_cuts_on_character_boundaries() {
        assert_eq!(clip("short", 10), "short");
        assert_eq!(clip("abcdef", 3), "abc…");
        assert_eq!(clip("ééééé", 2), "éé…");
        assert_eq!(clip("", 0), "");
    }
}
