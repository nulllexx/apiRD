//! An MCP (Model Context Protocol) server, so AI assistants such as Claude
//! can read what the admin panel shows.
//!
//! The transport is in `routes::mcp`: one endpoint, `POST /api/mcp`, speaking
//! JSON-RPC over MCP's Streamable HTTP transport, answered with plain JSON
//! and no server-side sessions. This module holds what the endpoint decides.
//!
//! # Who can use it
//!
//! * **Off by default.** Nothing answers unless `MCP_ENABLED` is set
//!   (`AppConfig::mcp_enabled`).
//! * **Personal access tokens, not sessions.** An assistant authenticates
//!   with `Authorization: Bearer rdmcp_…`, a token its owner created in the
//!   admin panel ([`token`]). The session cookie is never read here, so a
//!   web page cannot make a signed-in browser call this endpoint, and a token
//!   cannot be used anywhere else in the API.
//! * **A token holds a ceiling, not a grant.** It is issued for a list of
//!   permissions its owner holds, and every request intersects that list
//!   with what the account holds *at that moment*
//!   (`PermissionSet::restricted_to`). Taking a role away narrows every
//!   token at once; taking away `mcp.access` ends them all. A token is never
//!   a superuser, even an administrator's.
//! * **Tied to the account's sessions.** Changing the password or "Sign out
//!   everywhere" ends the account's tokens as well as its browser sessions.
//! * **Short-lived and stored hashed.** Tokens expire in at most
//!   [`token::MAX_DAYS`] days, only their SHA-256 is kept, and the token is
//!   shown once, when it is created.
//! * **Refuses web pages.** A request with an `Origin` header is refused
//!   unless that origin is listed in `MCP_ALLOWED_ORIGINS`: MCP clients are
//!   programs, and refusing pages is what stops DNS rebinding.
//!
//! # What it can do
//!
//! Read, and only read ([`tools`]). Every tool calls the same route handler
//! the admin panel does, through the same `Require<P>` permission check, so a
//! tool cannot show more than the panel would show the token's owner — and
//! nothing here can run a command, change a setting or touch an account.
//!
//! That is deliberate rather than unfinished. Some of what the tools return
//! was written by players (the console log carries chat). An assistant that
//! reads it is reading text an attacker controls, and an assistant with a
//! tool that acts on the server could be talked into using it. With no such
//! tool, the worst a hostile chat line can do is mislead a summary.
//!
//! # Accountability
//!
//! Every tool call is written to `mcp_audit` before it runs ([`audit`]), and
//! creating or revoking a token is recorded in the access audit trail the
//! Roles tab shows.

use std::collections::VecDeque;
use std::hash::Hash;
use std::time::{Duration, Instant};

use dashmap::DashMap;

pub mod audit;
pub mod protocol;
pub mod token;
pub mod tools;

/// The largest JSON-RPC message the endpoint reads. Tool arguments here are
/// a few short fields; anything near this is not a real client.
pub const MAX_BODY_BYTES: usize = 64 * 1024;

/// The largest tool result returned. Every tool is bounded well below this;
/// it exists so a surprise (a huge roster, a runaway log line) is an error
/// the assistant can see rather than a response that floods its context.
pub const MAX_RESULT_BYTES: usize = 256 * 1024;

/// Requests per token per minute.
pub const CALLS_PER_MINUTE: usize = 120;

/// A sliding-window counter keyed by anything hashable.
///
/// `middleware::rate_limit::RateLimiter` does the same for IP addresses; this
/// one keys on token ids, so one busy assistant cannot starve another behind
/// the same proxy address.
pub struct KeyedLimiter<K: Hash + Eq> {
    hits: DashMap<K, VecDeque<Instant>>,
    max: usize,
    window: Duration,
}

impl<K: Hash + Eq> KeyedLimiter<K> {
    pub fn new(max: usize, window: Duration) -> Self {
        KeyedLimiter { hits: DashMap::new(), max, window }
    }

    /// Records a hit for `key` and says whether it is within the limit.
    pub fn allow(&self, key: K) -> bool {
        self.allow_at(key, Instant::now())
    }

    fn allow_at(&self, key: K, now: Instant) -> bool {
        let mut entry = self.hits.entry(key).or_default();
        let hits = entry.value_mut();
        while hits.front().is_some_and(|t| now.duration_since(*t) >= self.window) {
            hits.pop_front();
        }
        if hits.len() >= self.max {
            return false;
        }
        hits.push_back(now);
        true
    }
}

/// Whether a request's `Origin` header is acceptable: absent, which is how
/// MCP clients call, or one of the configured origins. Everything else is a
/// web page, and is refused -- `null` included.
pub fn origin_allowed(origin: Option<&str>, allowed: &[String]) -> bool {
    match origin {
        None => true,
        Some(o) => {
            let o = o.trim().trim_end_matches('/');
            allowed.iter().any(|a| a == o)
        }
    }
}

/// The process-wide per-token limiter.
pub fn token_limiter() -> &'static KeyedLimiter<i64> {
    static LIMITER: std::sync::OnceLock<KeyedLimiter<i64>> = std::sync::OnceLock::new();
    LIMITER.get_or_init(|| KeyedLimiter::new(CALLS_PER_MINUTE, Duration::from_secs(60)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_listed_origins_and_originless_requests_pass() {
        let none: Vec<String> = Vec::new();
        assert!(origin_allowed(None, &none));
        assert!(!origin_allowed(Some("https://evil.example"), &none));
        assert!(!origin_allowed(Some("null"), &none));

        let listed = vec!["https://panel.example".to_string()];
        assert!(origin_allowed(Some("https://panel.example"), &listed));
        assert!(origin_allowed(Some("https://panel.example/"), &listed));
        assert!(!origin_allowed(Some("https://panel.example.evil"), &listed));
        assert!(!origin_allowed(Some("http://panel.example"), &listed));
    }

    #[test]
    fn the_limiter_counts_per_key_and_forgets_old_hits() {
        let limiter = KeyedLimiter::new(2, Duration::from_secs(60));
        let t0 = Instant::now();
        assert!(limiter.allow_at(1, t0));
        assert!(limiter.allow_at(1, t0));
        assert!(!limiter.allow_at(1, t0), "third hit in the window is refused");
        assert!(limiter.allow_at(2, t0), "another key has its own budget");
        assert!(limiter.allow_at(1, t0 + Duration::from_secs(60)), "the window slides");
    }
}
