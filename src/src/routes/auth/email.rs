//! Account email and the password flows built on it.
//!
//!   GET    /account/settings        what the settings page shows
//!   POST   /account/email           send a link to confirm a new address
//!   DELETE /account/email           remove the address
//!   POST   /account/email/verify    use that link
//!   POST   /change-password         change it, knowing the current one
//!   POST   /password/forgot         email a reset link
//!   POST   /password/reset          use that link
//!   POST   /account/sessions/revoke sign out everywhere
//!
//! Setting a password, by any route, ends every session the account has (see
//! `middleware::auth::revoke_sessions`). Changing it while signed in issues
//! this device a fresh session, so only the others are signed out.
//!
//! `users.email` only ever holds a verified address. An address that has
//! been typed in but not confirmed lives in `email_tokens` until its link is
//! used. That matters beyond password resets: a Google sign-in is linked to
//! an existing account by matching verified addresses, so an unverified one
//! in `users.email` would let whoever owns that inbox into the account.
//!
//! The admin-generated reset links (`/admin/gen-pwd-reset` and
//! `/forgot-password`, in password.rs) are untouched. They remain the way in
//! for accounts that never added an address.

use actix_web::{web, HttpRequest, HttpResponse};
use bcrypt::{hash, verify, DEFAULT_COST};
use serde::Deserialize;
use serde_json::json;

use crate::email::{self, PURPOSE_RESET, PURPOSE_VERIFY};
use crate::error::AppError;
use crate::middleware::auth::{create_jwt, revoke_sessions, AuthUser};
use crate::middleware::rate_limit::RateLimiter;
use crate::AppState;

use super::common::*;
use super::session::{build_user_token_cookie, clear_cookie};

const SEVEN_DAYS_SECS: u64 = 7 * 24 * 60 * 60;

/// One link of a kind per account per minute...
const COOLDOWN_SECS: i64 = 60;
/// ...and at most this many an hour.
const MAX_PER_HOUR: i64 = 5;

pub(crate) const MIN_PASSWORD_LEN: usize = 8;
/// bcrypt only reads the first 72 bytes; refusing longer input beats letting
/// someone believe the rest of their passphrase counts.
const MAX_PASSWORD_BYTES: usize = 72;

const FORGOT_RESPONSE: &str =
    "If that account has a verified email address, a reset link is on its way. It expires in 30 minutes.";

#[derive(Debug, Deserialize)]
pub struct SetEmailBody {
    pub email: Option<String>,
    /// Required when the account has a password. Without it, a stolen
    /// session could swap in the thief's address and reset the password.
    pub password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct RemoveEmailBody {
    pub password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TokenBody {
    pub token: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ChangePasswordBody {
    #[serde(rename = "currentPassword")]
    pub current_password: Option<String>,
    #[serde(rename = "newPassword")]
    pub new_password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ForgotBody {
    /// A username or an email address.
    pub identifier: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResetBody {
    pub token: Option<String>,
    #[serde(rename = "newPassword")]
    pub new_password: Option<String>,
}

#[derive(sqlx::FromRow)]
struct AccountRow {
    id: String,
    username: String,
    password_hash: String,
    email: Option<String>,
    email_verified: bool,
}

impl AccountRow {
    fn has_password(&self) -> bool {
        !self.password_hash.is_empty()
    }

    /// The address links can be sent to, if there is one.
    fn verified_email(&self) -> Option<&str> {
        self.email.as_deref().filter(|_| self.email_verified)
    }
}

const ACCOUNT_COLUMNS: &str = "id, username, password_hash, email, email_verified";

async fn account_by_id(state: &AppState, id: &str) -> Result<Option<AccountRow>, AppError> {
    Ok(sqlx::query_as(&format!("SELECT {ACCOUNT_COLUMNS} FROM users WHERE id = ?"))
        .bind(id)
        .fetch_optional(&state.pool)
        .await?)
}

/// The session's account. A token for a deleted account is a logged-out one.
async fn current_account(state: &AppState, auth: &AuthUser) -> Result<AccountRow, AppError> {
    account_by_id(state, &auth.id)
        .await?
        .ok_or_else(|| AppError::Unauthorized("Unauthorized".to_string()))
}

fn required<'a>(value: &'a Option<String>, message: &str) -> Result<&'a str, AppError> {
    value
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest(message.to_string()))
}

pub(crate) fn check_new_password(password: &str) -> Result<(), AppError> {
    if password.chars().count() < MIN_PASSWORD_LEN {
        return Err(AppError::BadRequest(format!(
            "Use at least {MIN_PASSWORD_LEN} characters."
        )));
    }
    if password.len() > MAX_PASSWORD_BYTES {
        return Err(AppError::BadRequest(
            "That password is too long (72 bytes at most).".to_string(),
        ));
    }
    Ok(())
}

/// Confirms the caller knows the account's password, when it has one.
///
/// An account made through Google has none, and its session is all there is
/// to check.
fn confirm_password(account: &AccountRow, given: Option<&str>) -> Result<(), AppError> {
    if !account.has_password() {
        return Ok(());
    }
    let given = given
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Enter your current password to confirm.".to_string()))?;
    if verify(given, &account.password_hash)? {
        Ok(())
    } else {
        Err(AppError::Unauthorized("That password is not right.".to_string()))
    }
}

fn require_email_enabled(state: &AppState) -> Result<(), AppError> {
    if state.config.email_enabled() {
        Ok(())
    } else {
        Err(AppError::ServiceUnavailable(
            "Email is not set up on this server yet. Contact support.".to_string(),
        ))
    }
}

/// Whether this account may be sent another link of this kind now.
async fn within_send_limits(state: &AppState, user_id: &str, purpose: &str) -> Result<bool, AppError> {
    // SUM() of a comparison is DECIMAL in MariaDB, which does not decode into
    // an i64; hence the cast.
    let (in_last_hour, recent): (i64, i64) = sqlx::query_as(
        "SELECT COUNT(*),
                CAST(COALESCE(SUM(created_at > UTC_TIMESTAMP() - INTERVAL ? SECOND), 0) AS SIGNED)
         FROM email_tokens
         WHERE user_id = ? AND purpose = ? AND created_at > UTC_TIMESTAMP() - INTERVAL 1 HOUR",
    )
    .bind(COOLDOWN_SECS)
    .bind(user_id)
    .bind(purpose)
    .fetch_one(&state.pool)
    .await?;
    Ok(recent == 0 && in_last_hour < MAX_PER_HOUR)
}

/// Stores a new token and returns it. Long-expired rows are swept on the way.
async fn issue_token(
    state: &AppState,
    user_id: &str,
    purpose: &str,
    address: &str,
    ttl_minutes: i64,
) -> Result<String, AppError> {
    sqlx::query("DELETE FROM email_tokens WHERE expires_at < UTC_TIMESTAMP() - INTERVAL 1 DAY")
        .execute(&state.pool)
        .await?;

    let token = email::new_token();
    sqlx::query(
        "INSERT INTO email_tokens (user_id, purpose, token_hash, email, created_at, expires_at)
         VALUES (?, ?, ?, ?, UTC_TIMESTAMP(), UTC_TIMESTAMP() + INTERVAL ? MINUTE)",
    )
    .bind(user_id)
    .bind(purpose)
    .bind(email::hash_token(&token))
    .bind(address)
    .bind(ttl_minutes)
    .execute(&state.pool)
    .await?;
    Ok(token)
}

/// Looks up a live token. Returns its owner and the address it was sent to.
async fn redeem_lookup(
    state: &AppState,
    token: &str,
    purpose: &str,
) -> Result<Option<(String, String)>, AppError> {
    Ok(sqlx::query_as(
        "SELECT user_id, email FROM email_tokens
         WHERE token_hash = ? AND purpose = ? AND expires_at > UTC_TIMESTAMP()",
    )
    .bind(email::hash_token(token))
    .bind(purpose)
    .fetch_optional(&state.pool)
    .await?)
}

async fn clear_tokens(state: &AppState, user_id: &str, purpose: &str) -> Result<(), AppError> {
    sqlx::query("DELETE FROM email_tokens WHERE user_id = ? AND purpose = ?")
        .bind(user_id)
        .bind(purpose)
        .execute(&state.pool)
        .await?;
    Ok(())
}

/// After a password changes, no outstanding way to change it again should
/// survive: neither emailed links nor admin-generated ones.
async fn clear_reset_links(state: &AppState, user_id: &str) -> Result<(), AppError> {
    clear_tokens(state, user_id, PURPOSE_RESET).await?;
    sqlx::query("DELETE FROM password_reset_sessions WHERE username = ?")
        .bind(user_id)
        .execute(&state.pool)
        .await?;
    Ok(())
}

/// Sets the password and signs the account out everywhere. Returns the new
/// session version.
async fn set_password(state: &AppState, user_id: &str, password: &str) -> Result<u32, AppError> {
    let hashed = hash(password, DEFAULT_COST)?;
    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(&hashed)
        .bind(user_id)
        .execute(&state.pool)
        .await?;
    clear_reset_links(state, user_id).await?;
    Ok(revoke_sessions(&state.pool, user_id).await?)
}

fn is_unique_violation(err: &sqlx::Error) -> bool {
    err.as_database_error()
        .map(|e| e.is_unique_violation())
        .unwrap_or(false)
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// GET /account/settings
pub(super) async fn account_settings(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    #[derive(sqlx::FromRow)]
    struct Row {
        username: String,
        created_at: String,
        password_hash: String,
        email: Option<String>,
        email_verified: bool,
        oauth_provider: Option<String>,
    }
    let row: Row = sqlx::query_as(
        "SELECT username, CAST(created_at AS CHAR) AS created_at, password_hash, email,
                email_verified, oauth_provider
         FROM users WHERE id = ?",
    )
    .bind(&auth.id)
    .fetch_optional(&state.pool)
    .await?
    .ok_or_else(|| AppError::Unauthorized("Unauthorized".to_string()))?;

    // The newest address waiting on its link, if any.
    let pending: Option<(String,)> = sqlx::query_as(
        "SELECT email FROM email_tokens
         WHERE user_id = ? AND purpose = ? AND expires_at > UTC_TIMESTAMP()
         ORDER BY created_at DESC, id DESC LIMIT 1",
    )
    .bind(&auth.id)
    .bind(PURPOSE_VERIFY)
    .fetch_optional(&state.pool)
    .await?;

    let verified = row.email.is_some() && row.email_verified;
    Ok(HttpResponse::Ok().json(json!({
        "username": row.username,
        "createdAt": row.created_at,
        "email": row.email,
        "emailVerified": verified,
        // The verify step refuses an address another account holds, so one
        // that matches the current address is not "pending" anything.
        "pendingEmail": pending.map(|(e,)| e).filter(|e| !(verified && row.email.as_deref() == Some(e))),
        "hasPassword": !row.password_hash.is_empty(),
        "googleLinked": row.oauth_provider.as_deref() == Some("google"),
        "emailEnabled": state.config.email_enabled(),
    })))
}

/// POST /account/email
pub(super) async fn set_email(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<SetEmailBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;
    require_email_enabled(&state)?;

    let address = email::normalize_address(required(&body.email, "Enter an email address.")?)
        .ok_or_else(|| AppError::BadRequest("That does not look like an email address.".to_string()))?;

    let account = current_account(&state, &auth).await?;
    confirm_password(&account, body.password.as_deref())?;

    if account.verified_email() == Some(address.as_str()) {
        return Err(AppError::BadRequest("That is already your email address.".to_string()));
    }

    if !within_send_limits(&state, &account.id, PURPOSE_VERIFY).await? || !email::take_send_budget() {
        return Err(AppError::TooManyRequests(
            "Too many emails sent recently. Wait a few minutes and try again.".to_string(),
        ));
    }

    // Recorded either way, so the limits above count this request.
    let token = issue_token(&state, &account.id, PURPOSE_VERIFY, &address, email::VERIFY_TTL_MINUTES).await?;

    // Someone else's address gets a note instead of a link, and the reply
    // here is the same either way: whether an address is registered is the
    // inbox owner's business, not whoever typed it in.
    let taken: Option<(String,)> = sqlx::query_as("SELECT id FROM users WHERE email = ? AND id <> ?")
        .bind(&address)
        .bind(&account.id)
        .fetch_optional(&state.pool)
        .await?;

    let message = if taken.is_some() {
        email::address_in_use_message(&state.config, &account.username)
    } else {
        email::verify_message(&state.config, &account.username, &token)
    };

    if let Err(e) = email::send(&state.config, &address, &message).await {
        log::error!("Could not send the address confirmation for {}: {e}", account.username);
        sqlx::query("DELETE FROM email_tokens WHERE token_hash = ?")
            .bind(email::hash_token(&token))
            .execute(&state.pool)
            .await?;
        return Err(AppError::ServiceUnavailable(
            "The email could not be sent. Try again in a few minutes.".to_string(),
        ));
    }

    Ok(HttpResponse::Ok().json(json!({
        "message": format!("We sent a link to {address}. Open it to confirm the address."),
        "pendingEmail": address,
    })))
}

/// DELETE /account/email
pub(super) async fn remove_email(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: Option<web::Json<RemoveEmailBody>>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let account = current_account(&state, &auth).await?;
    confirm_password(&account, body.as_ref().and_then(|b| b.password.as_deref()))?;

    sqlx::query("UPDATE users SET email = NULL, email_verified = FALSE WHERE id = ?")
        .bind(&account.id)
        .execute(&state.pool)
        .await?;
    clear_tokens(&state, &account.id, PURPOSE_VERIFY).await?;
    // A reset link already in someone's inbox should not outlive the address.
    clear_tokens(&state, &account.id, PURPOSE_RESET).await?;

    if let Some(old) = account.verified_email() {
        email::send_in_background(
            &state.config,
            old.to_string(),
            email::address_changed_message(&account.username, None),
        );
    }

    Ok(HttpResponse::Ok().json(json!({ "message": "Email address removed." })))
}

/// POST /account/email/verify
///
/// No session needed: the link is often opened on a different device (a
/// phone's mail app) from the one that asked for it. The token is the proof.
pub(super) async fn verify_email(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    body: web::Json<TokenBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let token = required(&body.token, "Missing token.")?;
    let (user_id, address) = redeem_lookup(&state, token, PURPOSE_VERIFY)
        .await?
        .ok_or_else(|| {
            AppError::BadRequest("This link has expired or was already used. Ask for a new one from your settings.".to_string())
        })?;

    let account = account_by_id(&state, &user_id)
        .await?
        .ok_or_else(|| AppError::NotFound("Account not found.".to_string()))?;

    let conflict = || {
        AppError::Conflict("That address is already linked to a different account.".to_string())
    };
    let taken: Option<(String,)> = sqlx::query_as("SELECT id FROM users WHERE email = ? AND id <> ?")
        .bind(&address)
        .bind(&user_id)
        .fetch_optional(&state.pool)
        .await?;
    if taken.is_some() {
        return Err(conflict());
    }

    // Changing the address invalidates reset links sent to the old one.
    let mut tx = state.pool.begin().await?;
    match sqlx::query("UPDATE users SET email = ?, email_verified = TRUE WHERE id = ?")
        .bind(&address)
        .bind(&user_id)
        .execute(&mut *tx)
        .await
    {
        Ok(_) => {}
        // Another account confirmed it between the check and here.
        Err(e) if is_unique_violation(&e) => return Err(conflict()),
        Err(e) => return Err(e.into()),
    }
    sqlx::query("DELETE FROM email_tokens WHERE user_id = ? AND (purpose = ? OR (purpose = ? AND email <> ?))")
        .bind(&user_id)
        .bind(PURPOSE_VERIFY)
        .bind(PURPOSE_RESET)
        .bind(&address)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    if let Some(old) = account.verified_email().filter(|old| *old != address) {
        email::send_in_background(
            &state.config,
            old.to_string(),
            email::address_changed_message(&account.username, Some(&address)),
        );
    }

    Ok(HttpResponse::Ok().json(json!({
        "message": "Email address confirmed.",
        "email": address,
        "username": account.username,
    })))
}

/// POST /change-password
pub(super) async fn change_password(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<ChangePasswordBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let account = current_account(&state, &auth).await?;
    if !account.has_password() {
        return Err(AppError::BadRequest(
            "This account signs in with Google and has no password yet. Use \"Set a password\" to get a link by email.".to_string(),
        ));
    }

    let current = required(&body.current_password, "Enter your current password.")?;
    let new_password = required(&body.new_password, "Enter a new password.")?;
    if !verify(current, &account.password_hash)? {
        return Err(AppError::Unauthorized("Your current password is not right.".to_string()));
    }
    check_new_password(new_password)?;

    let version = set_password(&state, &account.id, new_password).await?;

    if let Some(address) = account.verified_email() {
        email::send_in_background(
            &state.config,
            address.to_string(),
            email::password_changed_message(&state.config, &account.username),
        );
    }

    // Every other session just ended; this device gets a new one so the
    // person who changed it is not signed out too.
    let token = create_jwt(
        &account.username,
        &account.id,
        auth.is_og,
        version,
        &state.config.jwt_secret,
        SEVEN_DAYS_SECS,
    )
    .map_err(|e| AppError::Internal(format!("JWT error: {e}")))?;

    Ok(HttpResponse::Ok()
        .cookie(build_user_token_cookie(&token))
        .json(json!({ "message": "Password changed. Every other device has been signed out." })))
}

/// POST /account/sessions/revoke
///
/// Signs the account out everywhere, this device included.
pub(super) async fn revoke_all_sessions(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;
    revoke_sessions(&state.pool, &auth.id).await?;
    Ok(HttpResponse::Ok()
        .cookie(clear_cookie("userToken"))
        .json(json!({ "message": "Signed out everywhere." })))
}

/// POST /password/forgot
///
/// Answers the same way whether or not the account exists, has an address,
/// or has hit a limit, and sends in the background so the response time does
/// not say either.
pub(super) async fn forgot_password_email(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    body: web::Json<ForgotBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;
    require_email_enabled(&state)?;

    let identifier = required(&body.identifier, "Enter your username or email address.")?.trim();
    if identifier.is_empty() || identifier.len() > 254 {
        return Err(AppError::BadRequest("Enter your username or email address.".to_string()));
    }

    let account: Option<AccountRow> = match email::normalize_address(identifier) {
        Some(address) if identifier.contains('@') => {
            sqlx::query_as(&format!(
                "SELECT {ACCOUNT_COLUMNS} FROM users WHERE email = ? AND email_verified = TRUE"
            ))
            .bind(address)
            .fetch_optional(&state.pool)
            .await?
        }
        _ => {
            sqlx::query_as(&format!("SELECT {ACCOUNT_COLUMNS} FROM users WHERE username = ?"))
                .bind(identifier)
                .fetch_optional(&state.pool)
                .await?
        }
    };

    let generic = HttpResponse::Ok().json(json!({ "message": FORGOT_RESPONSE }));

    let Some(account) = account else { return Ok(generic) };
    let Some(address) = account.verified_email().map(str::to_string) else {
        return Ok(generic);
    };
    if !within_send_limits(&state, &account.id, PURPOSE_RESET).await? {
        log::info!("Reset link for {} not sent: per-account limit", account.username);
        return Ok(generic);
    }
    if !email::take_send_budget() {
        log::warn!("Reset link for {} not sent: global email budget spent", account.username);
        return Ok(generic);
    }

    let token = issue_token(&state, &account.id, PURPOSE_RESET, &address, email::RESET_TTL_MINUTES).await?;
    email::send_in_background(
        &state.config,
        address,
        email::reset_message(&state.config, &account.username, &token),
    );

    Ok(generic)
}

/// POST /password/reset
pub(super) async fn reset_password_email(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    body: web::Json<ResetBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let token = required(&body.token, "Missing token.")?;
    let new_password = required(&body.new_password, "Enter a new password.")?;

    let expired = || {
        AppError::BadRequest("This link has expired or was already used. Ask for a new one.".to_string())
    };
    let (user_id, sent_to) = redeem_lookup(&state, token, PURPOSE_RESET).await?.ok_or_else(expired)?;
    let account = account_by_id(&state, &user_id).await?.ok_or_else(expired)?;

    // The link was sent to an address the account no longer has.
    if account.verified_email() != Some(sent_to.as_str()) {
        return Err(expired());
    }

    check_new_password(new_password)?;
    set_password(&state, &account.id, new_password).await?;

    email::send_in_background(
        &state.config,
        sent_to,
        email::password_changed_message(&state.config, &account.username),
    );

    Ok(HttpResponse::Ok().json(json!({
        "message": "Password reset. Every device that was signed in has been signed out; log in with the new password.",
        "username": account.username,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_passwords_have_a_floor_and_a_bcrypt_sized_ceiling() {
        assert!(check_new_password("short").is_err());
        assert!(check_new_password("long enough").is_ok());
        assert!(check_new_password(&"x".repeat(72)).is_ok());
        assert!(check_new_password(&"x".repeat(73)).is_err());
        // Counted in characters for the floor, so a short non-ASCII password
        // is not let through on its byte length.
        assert!(check_new_password("ééé").is_err());
    }

    fn account(password_hash: &str) -> AccountRow {
        AccountRow {
            id: "id".into(),
            username: "u".into(),
            password_hash: password_hash.into(),
            email: Some("a@example.com".into()),
            email_verified: false,
        }
    }

    #[test]
    fn an_unverified_address_is_never_used_for_links() {
        let mut row = account("");
        assert_eq!(row.verified_email(), None);
        row.email_verified = true;
        assert_eq!(row.verified_email(), Some("a@example.com"));
    }

    #[test]
    fn changing_the_address_needs_the_password_when_there_is_one() {
        let hashed = bcrypt::hash("correct horse", 4).unwrap();
        let row = account(&hashed);
        assert!(confirm_password(&row, None).is_err());
        assert!(confirm_password(&row, Some("wrong")).is_err());
        assert!(confirm_password(&row, Some("correct horse")).is_ok());
        // A Google-only account has nothing to check.
        assert!(confirm_password(&account(""), None).is_ok());
    }
}
