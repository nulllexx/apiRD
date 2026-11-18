//! MCP access tokens: issuing them, and deciding whether one still counts.
//!
//! A token is `rdmcp_` followed by 43 base64url characters: 256 random bits.
//! The prefix makes a leaked token recognisable to secret scanners and to
//! whoever finds it in a config file. Only its SHA-256 is stored, so the
//! table is no use to someone who reads it; with that much entropy a plain
//! hash is enough, and it lets a token be found with one indexed lookup.
//!
//! [`authenticate`] accepts a token only when all of these hold, checked on
//! every request:
//!
//! * it exists and has not expired;
//! * the account's `session_version` has not moved on since it was issued
//!   (password changed or reset, or "Sign out everywhere");
//! * the account still holds `mcp.access`.
//!
//! What it may then do is the token's permission list intersected with what
//! the account holds right now.

use std::collections::BTreeSet;

use base64::Engine;
use chrono::NaiveDateTime;
use rand::RngCore;
use sha2::{Digest, Sha256};
use sqlx::MySqlPool;

use crate::access::{store, Permission};
use crate::middleware::permission::Principal;

pub const PREFIX: &str = "rdmcp_";
/// Base64url characters in 32 bytes, unpadded.
const SECRET_CHARS: usize = 43;
/// Characters after the prefix kept in the clear, to tell tokens apart.
const HINT_CHARS: usize = 4;

/// Longest a token may live. Short on purpose: a token sits in a config
/// file, outside anything that would notice it being copied.
pub const MAX_DAYS: u32 = 90;
pub const DEFAULT_DAYS: u32 = 30;
/// Tokens one account may hold at once, expired ones included until revoked.
pub const MAX_PER_ACCOUNT: i64 = 10;
pub const NAME_MAX: usize = 64;

/// A freshly made token. `token` is shown to its owner once and never stored.
pub struct Issued {
    pub token: String,
    pub hash: String,
    pub hint: String,
}

pub fn generate() -> Issued {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    let token = format!("{PREFIX}{secret}");
    Issued { hash: hash(&token), hint: format!("{PREFIX}{}", &secret[..HINT_CHARS]), token }
}

pub fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether `token` has the shape of one this server issues. Checked before
/// any query, so junk in the header never reaches the database.
pub fn well_formed(token: &str) -> bool {
    token.strip_prefix(PREFIX).is_some_and(|secret| {
        secret.len() == SECRET_CHARS
            && secret.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

/// The credential in an `Authorization: Bearer …` header. The scheme is
/// case-insensitive, as RFC 7235 says.
pub fn bearer(header: &str) -> Option<&str> {
    let (scheme, value) = header.trim().split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| value.trim()).filter(|v| !v.is_empty())
}

/// A token's label: what the owner calls the assistant it is for. Narrow,
/// because it appears in the panel and the audit trail.
pub fn clean_name(raw: &str) -> Result<String, String> {
    let name = raw.trim();
    if name.is_empty() {
        return Err("Give the token a name, such as the assistant it is for".to_string());
    }
    if name.chars().count() > NAME_MAX {
        return Err(format!("The name can be at most {NAME_MAX} characters"));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || " -_.()".contains(c)) {
        return Err("The name can only use letters, digits, spaces and - _ . ( )".to_string());
    }
    Ok(name.to_string())
}

pub fn encode_permissions(set: &BTreeSet<Permission>) -> String {
    set.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(",")
}

/// Stored permission strings this build does not recognise are skipped, as
/// `PermissionSet::from_grants` does for roles: a token can only lose reach
/// that way, never gain it.
pub fn decode_permissions(raw: &str) -> BTreeSet<Permission> {
    raw.split(',').filter_map(|s| Permission::parse(s.trim())).collect()
}

/// An accepted token and the principal it acts as.
#[derive(Debug, Clone)]
pub struct TokenContext {
    pub id: i64,
    pub name: String,
    pub expires_at: NaiveDateTime,
    /// The token's owner, holding only what the token allows.
    pub principal: Principal,
}

/// Why a token was refused. The client is told only "invalid" or "no
/// access"; the distinction is for the log.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    Unknown,
    Expired,
    SignedOut,
    /// The token is fine but its account no longer holds `mcp.access`.
    NoAccess,
}

pub async fn authenticate(
    pool: &MySqlPool,
    token: &str,
) -> Result<Result<TokenContext, Refusal>, sqlx::Error> {
    if !well_formed(token) {
        return Ok(Err(Refusal::Unknown));
    }

    #[derive(sqlx::FromRow)]
    struct Row {
        id: i64,
        name: String,
        permissions: String,
        expires_at: NaiveDateTime,
        live: i64,
        token_session: u32,
        account_session: u32,
        user_id: String,
        username: String,
        is_og: Option<bool>,
    }

    let row: Option<Row> = sqlx::query_as(
        r#"SELECT t.id, t.name, t.permissions, t.expires_at,
                  CAST(t.expires_at > UTC_TIMESTAMP(3) AS SIGNED) AS live,
                  t.session_version AS token_session, u.session_version AS account_session,
                  u.id AS user_id, u.username, u.is_og
           FROM mcp_tokens t JOIN users u ON u.id = t.user_id
           WHERE t.token_hash = ?"#,
    )
    .bind(hash(token))
    .fetch_optional(pool)
    .await?;

    let Some(row) = row else { return Ok(Err(Refusal::Unknown)) };
    if row.live == 0 {
        return Ok(Err(Refusal::Expired));
    }
    if row.token_session != row.account_session {
        return Ok(Err(Refusal::SignedOut));
    }

    let (roles, held) = store::grants_for(pool, &row.user_id).await?;
    if !held.has(Permission::McpAccess) {
        return Ok(Err(Refusal::NoAccess));
    }

    let permissions = held.restricted_to(decode_permissions(&row.permissions));
    Ok(Ok(TokenContext {
        id: row.id,
        name: row.name,
        expires_at: row.expires_at,
        principal: Principal {
            id: row.user_id,
            username: row.username,
            roles,
            permissions,
            is_og: row.is_og.unwrap_or(false),
        },
    }))
}

/// Notes that a token was used, at most once a minute so an active assistant
/// is not a write per request. Best effort: a failure is only logged.
pub async fn touch(pool: &MySqlPool, id: i64) {
    let result = sqlx::query(
        "UPDATE mcp_tokens SET last_used_at = UTC_TIMESTAMP(3)
         WHERE id = ? AND (last_used_at IS NULL OR last_used_at < UTC_TIMESTAMP(3) - INTERVAL 1 MINUTE)",
    )
    .bind(id)
    .execute(pool)
    .await;
    if let Err(e) = result {
        log::warn!("mcp: could not record use of token {id}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_well_formed_distinct_and_hashed() {
        let a = generate();
        let b = generate();
        assert!(well_formed(&a.token), "{}", a.token);
        assert_ne!(a.token, b.token);
        assert_eq!(a.hash, hash(&a.token));
        assert_eq!(a.hash.len(), 64);
        assert!(a.token.starts_with(&a.hint));
        assert_eq!(a.hint.len(), PREFIX.len() + HINT_CHARS);
        assert!(!a.hash.contains(&a.token[PREFIX.len()..]));
    }

    #[test]
    fn only_tokens_of_the_issued_shape_pass() {
        let good = generate().token;
        assert!(well_formed(&good));
        for bad in [
            "".to_string(),
            PREFIX.to_string(),
            good[PREFIX.len()..].to_string(),
            format!("{good}A"),
            good[..good.len() - 1].to_string(),
            format!("rdmcp-{}", &good[PREFIX.len()..]),
            format!("{}{}", PREFIX, "!".repeat(SECRET_CHARS)),
            format!("{}{}", PREFIX, "é".repeat(SECRET_CHARS)),
        ] {
            assert!(!well_formed(&bad), "{bad:?} should be refused");
        }
    }

    #[test]
    fn the_bearer_scheme_is_case_insensitive_and_required() {
        assert_eq!(bearer("Bearer abc"), Some("abc"));
        assert_eq!(bearer("bearer   abc  "), Some("abc"));
        assert_eq!(bearer("BEARER abc"), Some("abc"));
        assert_eq!(bearer("Basic abc"), None);
        assert_eq!(bearer("Bearer"), None);
        assert_eq!(bearer("Bearer   "), None);
        assert_eq!(bearer("abc"), None);
    }

    #[test]
    fn names_are_narrow() {
        assert_eq!(clean_name("  Claude Code (laptop) ").unwrap(), "Claude Code (laptop)");
        for bad in ["", "   ", "<script>", "a\nb", "naïve", &"a".repeat(NAME_MAX + 1)] {
            assert!(clean_name(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(clean_name(&"a".repeat(NAME_MAX)).is_ok());
    }

    #[test]
    fn permissions_round_trip_and_unknown_ones_are_dropped() {
        let set: BTreeSet<_> = [Permission::ConsoleView, Permission::PollsManage].into();
        assert_eq!(decode_permissions(&encode_permissions(&set)), set);
        assert_eq!(
            decode_permissions("console.view, launch.missiles,,"),
            [Permission::ConsoleView].into()
        );
        assert!(decode_permissions("").is_empty());
    }
}
