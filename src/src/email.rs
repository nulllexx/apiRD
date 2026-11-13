//! Outgoing email, through Resend (https://resend.com/docs/api-reference/emails/send-email).
//!
//! Three things live here:
//!
//! * [`send`], the one place a message leaves the server.
//! * The messages themselves. Each is a function that returns a [`Message`],
//!   so the wording sits in one file and every user-supplied value in it goes
//!   through [`escape`].
//! * The helpers the account routes share: address validation, single-use
//!   tokens, and the global send budget.
//!
//! Email is off unless both `RESEND_API_KEY` and `EMAIL_FROM` are set (see
//! [`AppConfig::email_enabled`]). The routes check that before doing anything
//! that would leave a user waiting for a message that never comes.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::LazyLock;
use std::time::Duration;

use rand::Rng;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::config::AppConfig;
use crate::middleware::rate_limit::RateLimiter;

/// How long a link to confirm an address stays valid.
pub const VERIFY_TTL_MINUTES: i64 = 24 * 60;
/// How long a password reset link stays valid.
pub const RESET_TTL_MINUTES: i64 = 30;

/// Token purposes, as stored in `email_tokens.purpose`.
pub const PURPOSE_VERIFY: &str = "verify";
pub const PURPOSE_RESET: &str = "reset";

static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("build the email HTTP client")
});

/// A cap on emails sent by the whole server, whoever asks.
///
/// The per-account limits in the routes stop one inbox being flooded, but
/// someone cycling through many usernames would get past them. Behind nginx
/// every request arrives from the same peer address, so a per-IP limit would
/// be a global one anyway; this just says so. 40 per 10 minutes is far above
/// what real users need and well inside Resend's quotas.
static SEND_BUDGET: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::new(40, 600));

/// Takes one send from the global budget. False means the budget is spent.
pub fn take_send_budget() -> bool {
    SEND_BUDGET
        .check(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        .is_ok()
}

pub struct Message {
    pub subject: String,
    pub html: String,
    pub text: String,
}

/// Sends one message. The error is for the log; it may name the provider's
/// response, so it is not meant for the person who asked.
pub async fn send(config: &AppConfig, to: &str, message: &Message) -> Result<(), String> {
    if !config.email_enabled() {
        return Err("email is not configured (RESEND_API_KEY / EMAIL_FROM)".to_string());
    }

    let response = CLIENT
        .post(&config.resend_api_url)
        .bearer_auth(&config.resend_api_key)
        .json(&json!({
            "from": config.email_from,
            "to": [to],
            "subject": message.subject,
            "html": message.html,
            "text": message.text,
        }))
        .send()
        .await
        .map_err(|e| format!("request to Resend failed: {e}"))?;

    if response.status().is_success() {
        return Ok(());
    }

    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    Err(format!("Resend answered {status}: {}", body.chars().take(300).collect::<String>()))
}

/// Sends in the background and logs a failure.
///
/// For messages whose outcome the caller must not reveal (a reset request for
/// an account that may not exist) or does not need to wait on (a notice after
/// the change already happened).
pub fn send_in_background(config: &AppConfig, to: String, message: Message) {
    let config = config.clone();
    tokio::spawn(async move {
        if let Err(e) = send(&config, &to, &message).await {
            log::error!("Could not send \"{}\": {e}", message.subject);
        }
    });
}

// ─── Addresses ───────────────────────────────────────────────────────────────

/// Checks an address for shape and returns the form it is stored in.
///
/// Deliberately loose: it rejects what is clearly not an address, and the
/// verification email is what proves the rest. Lowercased so the same inbox
/// cannot be attached twice under different capitalisation.
pub fn normalize_address(raw: &str) -> Option<String> {
    let address = raw.trim();
    if address.is_empty() || address.len() > 254 {
        return None;
    }
    if address
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || "<>()[]\\,;:\"".contains(c))
    {
        return None;
    }

    let (local, domain) = address.split_once('@')?;
    if local.is_empty() || local.len() > 64 || domain.contains('@') {
        return None;
    }
    if !domain.contains('.')
        || domain.starts_with('.')
        || domain.ends_with('.')
        || domain.starts_with('-')
        || domain.contains("..")
    {
        return None;
    }

    Some(address.to_lowercase())
}

/// `alice@example.com` becomes `a••••@example.com`, for the places that show
/// an address to someone who may not own it.
pub fn mask_address(address: &str) -> String {
    match address.split_once('@') {
        Some((local, domain)) => {
            let first: String = local.chars().take(1).collect();
            format!("{first}\u{2022}\u{2022}\u{2022}\u{2022}@{domain}")
        }
        None => "\u{2022}\u{2022}\u{2022}\u{2022}".to_string(),
    }
}

// ─── Tokens ──────────────────────────────────────────────────────────────────

/// A fresh single-use token: 32 random bytes, hex. Only the email carries
/// it; the database keeps [`hash_token`] of it, so a leaked table hands out no
/// working links.
pub fn new_token() -> String {
    let bytes: [u8; 32] = rand::thread_rng().gen();
    hex(&bytes)
}

pub fn hash_token(token: &str) -> String {
    hex(&Sha256::digest(token.trim().as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ─── Messages ────────────────────────────────────────────────────────────────

/// HTML-escapes a value going into a message. Usernames are not restricted
/// at registration, so every one of them goes through here.
pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Wraps paragraphs (already escaped) and an optional button in the shared
/// layout.
fn layout(heading: &str, paragraphs: &[String], button: Option<(&str, &str)>, footer: &str) -> String {
    let body: String = paragraphs
        .iter()
        .map(|p| format!(r#"<p style="margin:0 0 16px;font-size:15px;line-height:1.55;color:#1a1f2b">{p}</p>"#))
        .collect();
    let button = match button {
        Some((label, url)) => format!(
            r#"<p style="margin:24px 0"><a href="{url}" style="display:inline-block;padding:12px 22px;background:#0891b2;color:#ffffff;text-decoration:none;border-radius:6px;font-weight:600;font-size:15px">{label}</a></p>
<p style="margin:0 0 16px;font-size:13px;line-height:1.5;color:#5b6478">If the button does not work, copy this address into your browser:<br><span style="word-break:break-all">{url}</span></p>"#,
            url = escape(url),
            label = escape(label),
        ),
        None => String::new(),
    };
    format!(
        r#"<!DOCTYPE html><html><body style="margin:0;padding:24px;background:#f4f6f9;font-family:Inter,Segoe UI,Arial,sans-serif">
<div style="max-width:520px;margin:0 auto;background:#ffffff;border:1px solid #dde2ea;border-radius:10px;padding:28px">
<h1 style="margin:0 0 18px;font-size:20px;color:#1a1f2b">{heading}</h1>
{body}{button}
<p style="margin:24px 0 0;padding-top:16px;border-top:1px solid #e9ecf2;font-size:12px;line-height:1.5;color:#8891a3">{footer}</p>
</div></body></html>"#,
        heading = escape(heading),
    )
}

const SIGNATURE: &str = "RainDrippy \u{00b7} BakoSMP";

pub fn verify_message(config: &AppConfig, username: &str, token: &str) -> Message {
    let url = format!("{}/verify-email?token={token}", config.public_site_url);
    let hours = VERIFY_TTL_MINUTES / 60;
    Message {
        subject: "Confirm your email address".to_string(),
        html: layout(
            "Confirm your email address",
            &[
                format!(
                    "Someone signed in as <b>{}</b> asked to use this address for their RainDrippy / BakoSMP account. Confirm it and it becomes the address password reset links are sent to.",
                    escape(username)
                ),
                format!("The link works once and expires in {hours} hours."),
            ],
            Some(("Confirm this address", &url)),
            &format!("Not you? Ignore this email and the address will not be added. {SIGNATURE}"),
        ),
        text: format!(
            "Someone signed in as {username} asked to use this address for their RainDrippy / BakoSMP account.\n\nConfirm it here (works once, expires in {hours} hours):\n{url}\n\nNot you? Ignore this email and the address will not be added.\n"
        ),
    }
}

/// Sent instead of a verification link when the address already belongs to a
/// different account, so the person asking learns nothing they could not
/// learn by owning the inbox.
pub fn address_in_use_message(config: &AppConfig, requested_by: &str) -> Message {
    let url = format!("{}/forgot-password", config.public_site_url);
    Message {
        subject: "This address is already in use".to_string(),
        html: layout(
            "This address is already in use",
            &[
                format!(
                    "Someone signed in as <b>{}</b> asked to add this address to their account. It is already linked to a different account, so nothing was changed.",
                    escape(requested_by)
                ),
                "If you have lost access to the account it is linked to, you can reset its password.".to_string(),
            ],
            Some(("Reset a password", &url)),
            &format!("Not you? You can ignore this email. {SIGNATURE}"),
        ),
        text: format!(
            "Someone signed in as {requested_by} asked to add this address to their account. It is already linked to a different account, so nothing was changed.\n\nIf you have lost access to that account, reset its password here:\n{url}\n"
        ),
    }
}

pub fn reset_message(config: &AppConfig, username: &str, token: &str) -> Message {
    let url = format!("{}/reset-pwd?token={token}", config.public_site_url);
    Message {
        subject: "Reset your password".to_string(),
        html: layout(
            "Reset your password",
            &[
                format!(
                    "Someone asked to reset the password for <b>{}</b>. Use the button below to choose a new one.",
                    escape(username)
                ),
                format!("The link works once and expires in {RESET_TTL_MINUTES} minutes."),
            ],
            Some(("Choose a new password", &url)),
            &format!("Did not ask for this? Ignore this email; your password stays as it is. {SIGNATURE}"),
        ),
        text: format!(
            "Someone asked to reset the password for {username}.\n\nChoose a new one here (works once, expires in {RESET_TTL_MINUTES} minutes):\n{url}\n\nDid not ask for this? Ignore this email; your password stays as it is.\n"
        ),
    }
}

pub fn password_changed_message(config: &AppConfig, username: &str) -> Message {
    let url = format!("{}/forgot-password", config.public_site_url);
    Message {
        subject: "Your password was changed".to_string(),
        html: layout(
            "Your password was changed",
            &[format!(
                "The password for <b>{}</b> was just changed. If that was you, there is nothing else to do.",
                escape(username)
            )],
            Some(("It was not me \u{2014} reset it", &url)),
            SIGNATURE,
        ),
        text: format!(
            "The password for {username} was just changed. If that was you, there is nothing else to do.\n\nIf it was not you, reset it now:\n{url}\n"
        ),
    }
}

/// Sent to the old address when the account's address changes or is removed.
pub fn address_changed_message(username: &str, new_address: Option<&str>) -> Message {
    let what = match new_address {
        Some(new) => format!(
            "The email address for <b>{}</b> was changed to {}. Password reset links now go there.",
            escape(username),
            escape(&mask_address(new))
        ),
        None => format!(
            "The email address for <b>{}</b> was removed. Password reset links can no longer be sent to it.",
            escape(username)
        ),
    };
    let what_text = match new_address {
        Some(new) => format!(
            "The email address for {username} was changed to {}. Password reset links now go there.",
            mask_address(new)
        ),
        None => format!("The email address for {username} was removed."),
    };
    Message {
        subject: "Your account's email address changed".to_string(),
        html: layout(
            "Your email address changed",
            &[
                what,
                "If you did not do this, contact support straight away: someone may have access to your account.".to_string(),
            ],
            None,
            &format!("Support: raindrippy.contact@protonmail.com \u{00b7} {SIGNATURE}"),
        ),
        text: format!(
            "{what_text}\n\nIf you did not do this, contact support straight away (raindrippy.contact@protonmail.com): someone may have access to your account.\n"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plausible_addresses_are_accepted_and_lowercased() {
        assert_eq!(normalize_address("  Alice@Example.COM "), Some("alice@example.com".into()));
        assert_eq!(normalize_address("a.b+tag@sub.example.co.uk"), Some("a.b+tag@sub.example.co.uk".into()));
    }

    #[test]
    fn things_that_are_not_addresses_are_refused() {
        for bad in [
            "",
            "alice",
            "alice@",
            "@example.com",
            "alice@example",
            "alice@@example.com",
            "alice@example..com",
            "alice@.example.com",
            "alice@example.com.",
            "al ice@example.com",
            "alice@example.com\nBcc: victim@example.com",
            "Alice <alice@example.com>",
            "a,b@example.com",
        ] {
            assert_eq!(normalize_address(bad), None, "{bad:?} should be refused");
        }
        let long_local = format!("{}@example.com", "a".repeat(65));
        assert_eq!(normalize_address(&long_local), None);
    }

    #[test]
    fn masking_keeps_only_the_first_letter_and_domain() {
        assert_eq!(mask_address("alice@example.com"), "a\u{2022}\u{2022}\u{2022}\u{2022}@example.com");
    }

    #[test]
    fn tokens_are_random_and_only_their_hash_is_stored() {
        let a = new_token();
        let b = new_token();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert_ne!(hash_token(&a), a);
        assert_eq!(hash_token(&a), hash_token(&format!(" {a} ")));
    }

    #[test]
    fn usernames_cannot_inject_markup_into_a_message() {
        let cfg = AppConfig::build(|k| match k {
            "DB_HOST" | "DB_USER" | "DB_PASSWORD" | "DB_NAME" | "JWT_SECRET" => Some("x".into()),
            _ => None,
        })
        .unwrap();
        let message = reset_message(&cfg, "<img src=x onerror=alert(1)>", "tok");
        assert!(!message.html.contains("<img"));
        assert!(message.html.contains("&lt;img"));
        assert!(message.html.contains("https://bakosmp.go.ro/reset-pwd?token=tok"));
    }
}
