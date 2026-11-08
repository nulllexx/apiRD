//! The chat prompt: a pop-up the AI group chat (ai_gc_app) shows each person
//! once, the first time they open it after it goes up. Admins write it in
//! rdadmin's "Chat Prompt" tab: a title, some text, and a few buttons that
//! each do one thing.
//!
//! Two surfaces, as with polls:
//!
//!   * `/api/admin/chat-prompt` — read and save it. Gated by
//!     `Require<perm::ChatPromptManage>`.
//!   * `/api/chat-prompt` — what the chat server calls on a person's behalf,
//!     forwarding their cookie: the prompt if they have not been shown it yet,
//!     and recording that they have. Not under `/api/chat/`, which belongs to
//!     the chat server (see `chat_allowed`).
//!
//! **"Once" is per version.** A plain save keeps the version, so people who
//! have already seen the prompt are not shown it again over a reworded
//! sentence. Saving with `showAgain` bumps it, and everyone sees it once more.

use actix_web::{web, HttpResponse};
use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::AppError;
use crate::access::perm;
use crate::middleware::permission::Require;
use crate::middleware::auth::AuthUser;
use crate::AppState;

/// Limits on what an admin can save. The columns would take more; these keep
/// the pop-up something a person reads rather than scrolls.
const MAX_TITLE: usize = 120;
const MAX_BODY: usize = 4000;
const MAX_BUTTONS: usize = 4;
const MAX_LABEL: usize = 40;
const MAX_URL: usize = 2000;
/// The chat's own limit on room names.
const MAX_ROOM: usize = 64;

// ---------------------------------------------------------------------------
// Buttons
// ---------------------------------------------------------------------------

/// What a button does when pressed. Every button also closes the prompt.
///
/// The spellings are stored (inside `chat_prompt.buttons`) and read by the
/// chat's web client, so renaming one breaks saved prompts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    /// Nothing else.
    Close,
    /// Opens `value` in a new tab: an http(s) URL, or a path on this site.
    Link,
    /// Switches to the room named `value`.
    Room,
    /// Asks the browser to allow desktop notifications for pings.
    Notifications,
}

impl Action {
    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "close" => Some(Self::Close),
            "link" => Some(Self::Link),
            "room" => Some(Self::Room),
            "notifications" => Some(Self::Notifications),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Button {
    label: String,
    action: Action,
    /// The link or room name; empty for actions that take none.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    value: String,
    /// Drawn as the filled, main button rather than an outlined one.
    #[serde(default)]
    primary: bool,
}

/// An absolute http(s) URL, or a path on this site (but not `//host`, which
/// browsers read as another site).
fn link_ok(url: &str) -> bool {
    if url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"));
    match rest {
        Some(host) => !host.is_empty() && !host.starts_with('/'),
        None => url.starts_with('/') && !url.starts_with("//") && !url.starts_with("/\\"),
    }
}

// ---------------------------------------------------------------------------
// Saving
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct ButtonBody {
    label: Option<String>,
    action: Option<String>,
    value: Option<String>,
    #[serde(default)]
    primary: bool,
}

#[derive(Debug, Deserialize)]
pub struct SavePromptBody {
    #[serde(default)]
    enabled: bool,
    title: Option<String>,
    body: Option<String>,
    #[serde(default)]
    buttons: Vec<ButtonBody>,
    #[serde(rename = "showAgain", default)]
    show_again: bool,
}

/// A save request that has been checked over.
#[derive(Debug, PartialEq, Eq)]
struct NewPrompt {
    enabled: bool,
    title: String,
    body: String,
    buttons: Vec<Button>,
}

fn validate(body: &SavePromptBody) -> Result<NewPrompt, String> {
    let title = body.title.as_deref().unwrap_or("").trim();
    if title.is_empty() {
        return Err("A title is required".to_string());
    }
    if title.chars().count() > MAX_TITLE {
        return Err(format!("The title must be {MAX_TITLE} characters or fewer"));
    }

    // Trimmed at the end only: leading spaces can be Markdown indentation.
    let text = body.body.as_deref().unwrap_or("").trim_end();
    if text.chars().count() > MAX_BODY {
        return Err(format!("The text must be {MAX_BODY} characters or fewer"));
    }

    if body.buttons.len() > MAX_BUTTONS {
        return Err(format!("A prompt can have at most {MAX_BUTTONS} buttons"));
    }
    let mut buttons = Vec::with_capacity(body.buttons.len());
    for (i, b) in body.buttons.iter().enumerate() {
        let n = i + 1;
        let label = b.label.as_deref().unwrap_or("").trim();
        // Not skipped, for the reason blank poll options are not: the admin
        // would publish something other than what they were looking at.
        if label.is_empty() {
            return Err(format!("Button {n} needs a label"));
        }
        if label.chars().count() > MAX_LABEL {
            return Err(format!("Button labels must be {MAX_LABEL} characters or fewer"));
        }
        let action = b
            .action
            .as_deref()
            .and_then(|a| Action::parse(a.trim()))
            .ok_or_else(|| {
                format!("Button {n}: action must be one of: close, link, room, notifications")
            })?;

        let raw = b.value.as_deref().unwrap_or("").trim();
        let value = match action {
            Action::Close | Action::Notifications => String::new(),
            Action::Link => {
                if raw.is_empty() {
                    return Err(format!("Button {n} needs a link to open"));
                }
                if raw.len() > MAX_URL || !link_ok(raw) {
                    return Err(format!(
                        "Button {n}: the link must start with https://, http:// or /"
                    ));
                }
                raw.to_string()
            }
            Action::Room => {
                let room = raw.trim_start_matches('#').trim();
                if room.is_empty() {
                    return Err(format!("Button {n} needs a room name"));
                }
                if room.chars().count() > MAX_ROOM {
                    return Err(format!(
                        "Button {n}: room names are {MAX_ROOM} characters or fewer"
                    ));
                }
                room.to_string()
            }
        };

        buttons.push(Button {
            label: label.to_string(),
            action,
            value,
            primary: b.primary,
        });
    }

    Ok(NewPrompt {
        enabled: body.enabled,
        title: title.to_string(),
        body: text.to_string(),
        buttons,
    })
}

// ---------------------------------------------------------------------------
// Admin
// ---------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
struct PromptRow {
    enabled: bool,
    title: String,
    body: String,
    buttons: String,
    version: u32,
    updated_at: NaiveDateTime,
    updated_by: String,
}

/// A stored button list. One that no longer parses (hand-edited, or from a
/// build with an action since removed) reads as no buttons, which the chat
/// shows as a plain "Got it", rather than taking the prompt down.
fn stored_buttons(raw: &str) -> Vec<Button> {
    serde_json::from_str(raw).unwrap_or_else(|e| {
        log::warn!("chat_prompt.buttons does not parse: {e}");
        Vec::new()
    })
}

/// The prompt as the admin tab shows it, with how many people have seen the
/// current version and how many could.
async fn admin_view(pool: &sqlx::MySqlPool) -> Result<serde_json::Value, AppError> {
    let row: Option<PromptRow> = sqlx::query_as(
        "SELECT enabled, title, body, buttons, version, updated_at, updated_by
         FROM chat_prompt WHERE id = 1",
    )
    .fetch_optional(pool)
    .await?;

    let (chat_users,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM users WHERE is_chatallowed = 1")
            .fetch_one(pool)
            .await?;

    let Some(row) = row else {
        return Ok(json!({ "prompt": null, "seen": 0, "chatUsers": chat_users }));
    };

    let (seen,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM chat_prompt_seen WHERE version >= ?")
            .bind(row.version)
            .fetch_one(pool)
            .await?;

    Ok(json!({
        "prompt": {
            "enabled": row.enabled,
            "title": row.title,
            "body": row.body,
            "buttons": stored_buttons(&row.buttons),
            "version": row.version,
            "updatedAt": row.updated_at.and_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            "updatedBy": row.updated_by,
        },
        "seen": seen,
        "chatUsers": chat_users,
    }))
}

/// GET /api/admin/chat-prompt
async fn get_prompt_admin(
    state: web::Data<AppState>,
    _admin: Require<perm::ChatPromptManage>,
) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(admin_view(&state.pool).await?))
}

/// PUT /api/admin/chat-prompt — replace the prompt (admin only)
async fn save_prompt(
    state: web::Data<AppState>,
    admin: Require<perm::ChatPromptManage>,
    body: web::Json<SavePromptBody>,
) -> Result<HttpResponse, AppError> {
    let new = validate(&body).map_err(AppError::BadRequest)?;
    let buttons = serde_json::to_string(&new.buttons)
        .map_err(|e| AppError::Internal(format!("Could not store buttons: {e}")))?;

    // The first save is version 1, which nobody has seen; after that the
    // version only moves when asked to.
    sqlx::query(
        "INSERT INTO chat_prompt
            (id, enabled, title, body, buttons, version, updated_at, updated_by)
         VALUES (1, ?, ?, ?, ?, 1, ?, ?)
         ON DUPLICATE KEY UPDATE
            enabled = VALUES(enabled),
            title = VALUES(title),
            body = VALUES(body),
            buttons = VALUES(buttons),
            version = version + ?,
            updated_at = VALUES(updated_at),
            updated_by = VALUES(updated_by)",
    )
    .bind(new.enabled)
    .bind(&new.title)
    .bind(&new.body)
    .bind(&buttons)
    .bind(chrono::Utc::now().naive_utc())
    .bind(&admin.username)
    .bind(u32::from(body.show_again))
    .execute(&state.pool)
    .await?;

    Ok(HttpResponse::Ok().json(admin_view(&state.pool).await?))
}

// ---------------------------------------------------------------------------
// The chat
// ---------------------------------------------------------------------------

/// GET /api/chat-prompt — the prompt, if it is up and this account has not
/// been shown this version of it; otherwise `{"prompt": null}`.
async fn my_prompt(
    state: web::Data<AppState>,
    user: AuthUser,
) -> Result<HttpResponse, AppError> {
    let row: Option<(u32, String, String, String)> = sqlx::query_as(
        "SELECT p.version, p.title, p.body, p.buttons
         FROM chat_prompt p
         WHERE p.id = 1 AND p.enabled
           AND NOT EXISTS (
               SELECT 1 FROM chat_prompt_seen s
               JOIN users u ON u.id = s.user_id
               WHERE u.username = ? AND s.version >= p.version
           )",
    )
    .bind(&user.username)
    .fetch_optional(&state.pool)
    .await?;

    let prompt = row.map(|(version, title, body, buttons)| {
        json!({
            "version": version,
            "title": title,
            "body": body,
            "buttons": stored_buttons(&buttons),
        })
    });
    Ok(HttpResponse::Ok().json(json!({ "prompt": prompt })))
}

#[derive(Debug, Deserialize)]
pub struct SeenBody {
    version: Option<u32>,
}

/// POST /api/chat-prompt/seen — this account has been shown `version`.
///
/// Capped at the current version, so a made-up number cannot hide prompts
/// that have not been written yet. A no-op when there is no prompt or the
/// account is gone.
async fn mark_seen(
    state: web::Data<AppState>,
    user: AuthUser,
    body: web::Json<SeenBody>,
) -> Result<HttpResponse, AppError> {
    let version = body
        .version
        .ok_or_else(|| AppError::BadRequest("version is required".to_string()))?;

    sqlx::query(
        "INSERT INTO chat_prompt_seen (user_id, version)
         SELECT u.id, LEAST(?, p.version)
         FROM users u JOIN chat_prompt p ON p.id = 1
         WHERE u.username = ?
         ON DUPLICATE KEY UPDATE version = GREATEST(chat_prompt_seen.version, VALUES(version))",
    )
    .bind(version)
    .bind(&user.username)
    .execute(&state.pool)
    .await?;

    Ok(HttpResponse::Ok().json(json!({ "ok": true })))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/admin/chat-prompt")
            .route("", web::get().to(get_prompt_admin))
            .route("", web::put().to(save_prompt)),
    )
    .service(
        web::scope("/chat-prompt")
            .route("", web::get().to(my_prompt))
            .route("/seen", web::post().to(mark_seen)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn button(label: &str, action: &str, value: Option<&str>) -> ButtonBody {
        ButtonBody {
            label: Some(label.to_string()),
            action: Some(action.to_string()),
            value: value.map(str::to_string),
            primary: false,
        }
    }

    fn body(buttons: Vec<ButtonBody>) -> SavePromptBody {
        SavePromptBody {
            enabled: true,
            title: Some("Welcome to the chat".to_string()),
            body: Some("Say **@ai hello** to start.".to_string()),
            buttons,
            show_again: false,
        }
    }

    #[test]
    fn accepts_every_action() {
        let new = validate(&body(vec![
            button("Rules", "link", Some("https://example.com/rules")),
            button("Go to #general", "room", Some("#general")),
            button("Notify me", "notifications", Some("ignored")),
            button("Got it", "close", None),
        ]))
        .expect("valid");
        assert_eq!(new.buttons.len(), 4);
        // The # is how people write a room, not part of its name.
        assert_eq!(new.buttons[1].value, "general");
        // Actions that take no value do not keep one.
        assert_eq!(new.buttons[2].value, "");
    }

    #[test]
    fn needs_a_title() {
        let mut b = body(Vec::new());
        b.title = Some("   ".to_string());
        assert!(validate(&b).is_err());
    }

    #[test]
    fn no_buttons_is_allowed() {
        assert!(validate(&body(Vec::new())).is_ok());
    }

    #[test]
    fn caps_the_number_of_buttons() {
        let many = (0..=MAX_BUTTONS).map(|_| button("Ok", "close", None)).collect();
        assert!(validate(&body(many)).is_err());
    }

    #[test]
    fn refuses_blank_labels_and_unknown_actions() {
        assert!(validate(&body(vec![button(" ", "close", None)])).is_err());
        assert!(validate(&body(vec![button("Run", "javascript", None)])).is_err());
    }

    #[test]
    fn links_must_be_web_or_on_site() {
        for ok in ["https://example.com", "http://example.com/a?b=c", "/raindrippy/index.html", "HTTPS://Example.com"] {
            assert!(link_ok(ok), "{ok} should be accepted");
        }
        for bad in ["javascript:alert(1)", "data:text/html,x", "//evil.example", "/\\evil.example", "https://", "https:///x", "https://a b", "example.com", ""] {
            assert!(!link_ok(bad), "{bad} should be refused");
        }
        assert!(validate(&body(vec![button("Go", "link", None)])).is_err());
        assert!(validate(&body(vec![button("Go", "link", Some("javascript:alert(1)"))])).is_err());
    }

    #[test]
    fn rooms_need_a_name() {
        assert!(validate(&body(vec![button("Go", "room", Some("#"))])).is_err());
    }

    /// The chat's web client switches on these strings.
    #[test]
    fn stored_spellings_are_stable() {
        let b = Button { label: "x".into(), action: Action::Notifications, value: String::new(), primary: true };
        assert_eq!(
            serde_json::to_value(&b).unwrap(),
            json!({ "label": "x", "action": "notifications", "primary": true })
        );
        assert_eq!(stored_buttons("not json"), Vec::new());
    }
}
