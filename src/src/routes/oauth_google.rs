use actix_web::cookie::time::Duration as CookieDuration;
use actix_web::cookie::{Cookie, SameSite};
use actix_web::{web, HttpRequest, HttpResponse};
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use oauth2::basic::BasicClient;
use oauth2::reqwest::async_http_client;
use oauth2::{
    AuthUrl, AuthorizationCode, ClientId, ClientSecret, CsrfToken, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse, TokenUrl,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::middleware::auth::{authenticate_opt, create_jwt, AuthUser};
use crate::middleware::rate_limit::RateLimiter;
use crate::AppState;

const SEVEN_DAYS_SECS: u64 = 7 * 24 * 60 * 60;
const PENDING_TTL_SECS: i64 = 10 * 60;
const STATE_TTL_SECS: i64 = 10 * 60;
const PROVIDER: &str = "google";
const USERINFO_URL: &str = "https://openidconnect.googleapis.com/v1/userinfo";
const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

// ─── Cookie payloads (JWT-signed with JWT_SECRET) ────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
struct StateClaims {
    state_kind: String, // "oauth_state" — distinguishes from userToken / pending
    csrf: String,
    pkce_verifier: String,
    exp: i64,
    /// Set when a signed-in account started the flow to link Google to
    /// itself (POST /auth/oauth/google/link), rather than to sign in.
    #[serde(default)]
    link_user: Option<String>,
    /// Which settings page to return to after linking: "rd" or "bako".
    #[serde(default)]
    return_to: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PendingClaims {
    pending_kind: String, // "oauth_pending"
    pending_sub: String,
    pending_email: String,
    pending_email_verified: bool,
    exp: i64,
}

// ─── Request / response shapes ───────────────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub code: Option<String>,
    pub state: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CompleteBody {
    pub username: Option<String>,
    pub hwid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GoogleUserInfo {
    sub: String,
    email: Option<String>,
    #[serde(default)]
    email_verified: bool,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn build_oauth_client(state: &AppState) -> Result<BasicClient, AppError> {
    let cfg = &state.config;
    if cfg.google_client_id.is_empty()
        || cfg.google_client_secret.is_empty()
        || cfg.google_redirect_uri.is_empty()
    {
        return Err(AppError::Internal(
            "Google OAuth is not configured on this server".to_string(),
        ));
    }
    let client = BasicClient::new(
        ClientId::new(cfg.google_client_id.clone()),
        Some(ClientSecret::new(cfg.google_client_secret.clone())),
        AuthUrl::new(GOOGLE_AUTH_URL.to_string())
            .map_err(|e| AppError::Internal(format!("OAuth auth url: {}", e)))?,
        Some(
            TokenUrl::new(GOOGLE_TOKEN_URL.to_string())
                .map_err(|e| AppError::Internal(format!("OAuth token url: {}", e)))?,
        ),
    )
    .set_redirect_uri(
        RedirectUrl::new(cfg.google_redirect_uri.clone())
            .map_err(|e| AppError::Internal(format!("OAuth redirect url: {}", e)))?,
    );
    Ok(client)
}

fn sign_state_cookie(
    secret: &str,
    csrf: &str,
    pkce_verifier: &str,
    link: Option<(&str, &str)>,
) -> Result<String, AppError> {
    let claims = StateClaims {
        state_kind: "oauth_state".to_string(),
        csrf: csrf.to_string(),
        pkce_verifier: pkce_verifier.to_string(),
        exp: chrono::Utc::now().timestamp() + STATE_TTL_SECS,
        link_user: link.map(|(user, _)| user.to_string()),
        return_to: link.map(|(_, to)| to.to_string()),
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(format!("State JWT: {}", e)))
}

fn read_state_cookie(secret: &str, token: &str) -> Result<StateClaims, AppError> {
    let mut validation = Validation::default();
    validation.required_spec_claims.clear();
    let data = decode::<StateClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map_err(|_| AppError::BadRequest("Invalid OAuth state".to_string()))?;
    if data.claims.state_kind != "oauth_state" {
        return Err(AppError::BadRequest("Invalid OAuth state".to_string()));
    }
    if data.claims.exp < chrono::Utc::now().timestamp() {
        return Err(AppError::BadRequest("OAuth state expired".to_string()));
    }
    Ok(data.claims)
}

fn sign_pending_cookie(
    secret: &str,
    sub: &str,
    email: &str,
    email_verified: bool,
) -> Result<String, AppError> {
    let claims = PendingClaims {
        pending_kind: "oauth_pending".to_string(),
        pending_sub: sub.to_string(),
        pending_email: email.to_string(),
        pending_email_verified: email_verified,
        exp: chrono::Utc::now().timestamp() + PENDING_TTL_SECS,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|e| AppError::Internal(format!("Pending JWT: {}", e)))
}

fn read_pending_cookie(secret: &str, token: &str) -> Result<PendingClaims, AppError> {
    let mut validation = Validation::default();
    validation.required_spec_claims.clear();
    let data = decode::<PendingClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )
    .map_err(|_| AppError::Unauthorized("Invalid OAuth pending token".to_string()))?;
    if data.claims.pending_kind != "oauth_pending" {
        return Err(AppError::Unauthorized(
            "Invalid OAuth pending token".to_string(),
        ));
    }
    if data.claims.exp < chrono::Utc::now().timestamp() {
        return Err(AppError::Unauthorized(
            "OAuth signup session expired, please try again".to_string(),
        ));
    }
    Ok(data.claims)
}

/// Must match `session::build_user_token_cookie` attribute for attribute: this
/// is the same session cookie, just issued down the Google path. See there for
/// why it is Lax and not Strict.
fn build_user_token_cookie(token: &str) -> Cookie<'static> {
    Cookie::build("userToken", token.to_string())
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::days(7))
        .path("/")
        .finish()
}

fn build_state_cookie(token: &str) -> Cookie<'static> {
    // SameSite=Lax so the cookie survives Google's top-level redirect back to us.
    Cookie::build("oauth_state", token.to_string())
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::seconds(STATE_TTL_SECS))
        .path("/")
        .finish()
}

fn build_pending_cookie(token: &str) -> Cookie<'static> {
    Cookie::build("oauth_pending", token.to_string())
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::seconds(PENDING_TTL_SECS))
        .path("/")
        .finish()
}

fn clear_cookie(name: &str) -> Cookie<'static> {
    Cookie::build(name.to_string(), "")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Lax)
        .max_age(CookieDuration::seconds(0))
        .path("/")
        .finish()
}

fn check_rate_limit(
    req: &HttpRequest,
    limiter: &web::Data<RateLimiter>,
) -> Result<(), AppError> {
    let ip = req
        .connection_info()
        .realip_remote_addr()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| "127.0.0.1".parse().unwrap());
    limiter.check(ip)
}

async fn fetch_userinfo(access_token: &str) -> Result<GoogleUserInfo, AppError> {
    let resp = reqwest::Client::new()
        .get(USERINFO_URL)
        .bearer_auth(access_token)
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("userinfo request: {}", e)))?;
    if !resp.status().is_success() {
        return Err(AppError::Unauthorized(format!(
            "Google userinfo returned {}",
            resp.status()
        )));
    }
    resp.json::<GoogleUserInfo>()
        .await
        .map_err(|e| AppError::Internal(format!("userinfo decode: {}", e)))
}

async fn issue_session(
    state: &AppState,
    user_id: &str,
    username: &str,
    is_og: bool,
) -> Result<Cookie<'static>, AppError> {
    let version = crate::middleware::auth::current_session_version(&state.pool, user_id).await?;
    let token = create_jwt(
        username,
        user_id,
        is_og,
        version,
        &state.config.jwt_secret,
        SEVEN_DAYS_SECS,
    )
    .map_err(|e| AppError::Internal(format!("JWT: {}", e)))?;
    Ok(build_user_token_cookie(&token))
}

/// Stored the way `crate::email` stores typed-in addresses, so the two meet
/// in the unique index instead of passing each other by capitalisation.
fn normalize_google_email(email: &str) -> String {
    crate::email::normalize_address(email).unwrap_or_else(|| email.trim().to_lowercase())
}

/// Takes the address Google vouches for on a returning sign-in, if the
/// account has no confirmed one of its own.
///
/// This is how accounts made through Google before `email_verified` existed
/// get their address confirmed, and how one whose address was removed gets
/// it back. An address the user confirmed themselves is never replaced: they
/// may have chosen a different inbox on purpose.
async fn adopt_google_email(
    state: &AppState,
    user_id: &str,
    email: &str,
    google_verified: bool,
) -> Result<(), AppError> {
    if !google_verified {
        return Ok(());
    }
    let email = normalize_google_email(email);
    let taken: Option<(String,)> = sqlx::query_as("SELECT id FROM users WHERE email = ? AND id <> ?")
        .bind(&email)
        .bind(user_id)
        .fetch_optional(&state.pool)
        .await?;
    if taken.is_some() {
        return Ok(());
    }
    let result = sqlx::query(
        "UPDATE users SET email = ?, email_verified = TRUE
         WHERE id = ? AND NOT (email_verified = TRUE AND email IS NOT NULL)",
    )
    .bind(&email)
    .bind(user_id)
    .execute(&state.pool)
    .await;
    match result {
        Ok(_) => Ok(()),
        // Lost a race with another account confirming the same address. The
        // sign-in still goes ahead; the address just is not adopted.
        Err(e) if e.as_database_error().map(|d| d.is_unique_violation()).unwrap_or(false) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Google's consent URL and the signed state cookie that goes with it.
fn authorization(state: &AppState, link: Option<(&str, &str)>) -> Result<(String, String), AppError> {
    let client = build_oauth_client(state)?;
    let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();

    let (auth_url, csrf_token) = client
        .authorize_url(CsrfToken::new_random)
        .add_scope(Scope::new("openid".to_string()))
        .add_scope(Scope::new("email".to_string()))
        .add_scope(Scope::new("profile".to_string()))
        .set_pkce_challenge(pkce_challenge)
        .url();

    let state_jwt = sign_state_cookie(
        &state.config.jwt_secret,
        csrf_token.secret(),
        pkce_verifier.secret(),
        link,
    )?;
    Ok((auth_url.to_string(), state_jwt))
}

// ─── Linking an existing account ─────────────────────────────────────────────

/// Where a link attempt ends up. `outcome` is one of the fixed strings
/// [`finish_link`] returns, and `return_to` only ever picks between two
/// built-in pages, so nothing a caller sends can steer the redirect elsewhere.
fn link_return_url(site: &str, return_to: Option<&str>, outcome: &str) -> String {
    match return_to {
        Some("rd") => format!("{site}/raindrippy/profile?google={outcome}#settings"),
        _ => format!("{site}/settings.html?google={outcome}#google"),
    }
}

/// Confirms the caller knows the account's password, when it has one.
fn confirm_password(password_hash: &str, given: Option<&str>) -> Result<(), AppError> {
    if password_hash.is_empty() {
        return Ok(());
    }
    let given = given
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Enter your current password to confirm.".to_string()))?;
    if bcrypt::verify(given, password_hash)? {
        Ok(())
    } else {
        Err(AppError::Unauthorized("That password is not right.".to_string()))
    }
}

#[derive(Debug, Deserialize)]
pub struct LinkBody {
    /// Required when the account has one. Linking adds a way in, so a stolen
    /// session must not be enough to add the thief's Google account.
    pub password: Option<String>,
    /// "rd" to come back to the RainDrippy profile, anything else for the
    /// BakoSMP settings page.
    #[serde(rename = "returnTo")]
    pub return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UnlinkBody {
    pub password: Option<String>,
}

/// POST /auth/oauth/google/link — start linking Google to the signed-in
/// account. Answers with the URL to send the browser to.
///
/// A POST rather than a link: the session cookie is SameSite=Lax, so another
/// site cannot start this on someone's behalf.
async fn link_start(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: Option<web::Json<LinkBody>>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let (password_hash, oauth_subject): (String, Option<String>) =
        sqlx::query_as("SELECT password_hash, oauth_subject FROM users WHERE id = ?")
            .bind(&auth.id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| AppError::Unauthorized("Unauthorized".to_string()))?;

    if oauth_subject.is_some() {
        return Err(AppError::Conflict("A Google account is already linked.".to_string()));
    }
    confirm_password(&password_hash, body.as_ref().and_then(|b| b.password.as_deref()))?;

    let return_to = match body.as_ref().and_then(|b| b.return_to.as_deref()) {
        Some("rd") => "rd",
        _ => "bako",
    };
    let (url, state_jwt) = authorization(&state, Some((&auth.id, return_to)))?;

    Ok(HttpResponse::Ok()
        .cookie(build_state_cookie(&state_jwt))
        .json(json!({ "url": url })))
}

/// The link half of the callback. Returns the outcome for the redirect.
async fn finish_link(
    req: &HttpRequest,
    state: &AppState,
    query: &CallbackQuery,
    stored: StateClaims,
    link_user: &str,
) -> &'static str {
    if query.error.is_some() {
        return "cancelled";
    }
    let (Some(code), Some(received_state)) = (query.code.as_deref(), query.state.as_deref()) else {
        return "error";
    };
    if stored.csrf != received_state {
        return "error";
    }

    // The browser finishing the flow must still be signed in as the account
    // that started it. Otherwise a link begun on one account could land on
    // whichever account the browser has switched to since.
    let session = match req.cookie("userToken") {
        Some(c) => authenticate_opt(state, c.value()).await,
        None => None,
    };
    if session.map(|c| c.id).as_deref() != Some(link_user) {
        return "session";
    }

    let client = match build_oauth_client(state) {
        Ok(c) => c,
        Err(_) => return "error",
    };
    let token_resp = match client
        .exchange_code(AuthorizationCode::new(code.to_string()))
        .set_pkce_verifier(PkceCodeVerifier::new(stored.pkce_verifier))
        .request_async(async_http_client)
        .await
    {
        Ok(t) => t,
        Err(e) => {
            log::error!("OAuth token exchange failed while linking: {e}");
            return "error";
        }
    };
    let userinfo = match fetch_userinfo(token_resp.access_token().secret()).await {
        Ok(u) => u,
        Err(e) => {
            log::error!("Google userinfo failed while linking: {e}");
            return "error";
        }
    };

    match link_google(state, link_user, &userinfo).await {
        Ok(outcome) => outcome,
        Err(e) => {
            log::error!("Linking Google failed: {e}");
            "error"
        }
    }
}

async fn link_google(state: &AppState, user_id: &str, userinfo: &GoogleUserInfo) -> Result<&'static str, AppError> {
    let owner: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE oauth_provider = ? AND oauth_subject = ?")
            .bind(PROVIDER)
            .bind(&userinfo.sub)
            .fetch_optional(&state.pool)
            .await?;
    match owner {
        Some((id,)) if id == user_id => return Ok("linked"),
        Some(_) => return Ok("taken"),
        None => {}
    }

    let result = sqlx::query(
        "UPDATE users SET oauth_provider = ?, oauth_subject = ?
         WHERE id = ? AND oauth_subject IS NULL",
    )
    .bind(PROVIDER)
    .bind(&userinfo.sub)
    .bind(user_id)
    .execute(&state.pool)
    .await;
    match result {
        Ok(r) if r.rows_affected() == 1 => {}
        // Linked to something else in the meantime, from another tab.
        Ok(_) => return Ok("already"),
        Err(e) if e.as_database_error().map(|d| d.is_unique_violation()).unwrap_or(false) => {
            return Ok("taken")
        }
        Err(e) => return Err(e.into()),
    }

    if let Some(email) = &userinfo.email {
        adopt_google_email(state, user_id, email, userinfo.email_verified).await?;
    }
    Ok("linked")
}

/// DELETE /account/google — unlink. Only for an account with a password,
/// which is then its only way in; a Google-only account would be locked out.
async fn unlink(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: Option<web::Json<UnlinkBody>>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let (password_hash, oauth_subject): (String, Option<String>) =
        sqlx::query_as("SELECT password_hash, oauth_subject FROM users WHERE id = ?")
            .bind(&auth.id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| AppError::Unauthorized("Unauthorized".to_string()))?;

    if oauth_subject.is_none() {
        return Err(AppError::BadRequest("No Google account is linked.".to_string()));
    }
    if password_hash.is_empty() {
        return Err(AppError::BadRequest(
            "Google is the only way into this account. Set a password first, then unlink it.".to_string(),
        ));
    }
    confirm_password(&password_hash, body.as_ref().and_then(|b| b.password.as_deref()))?;

    sqlx::query("UPDATE users SET oauth_provider = NULL, oauth_subject = NULL WHERE id = ?")
        .bind(&auth.id)
        .execute(&state.pool)
        .await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "Google account unlinked." })))
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// GET /auth/oauth/google — kick off the OAuth dance.
async fn initiate(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let (auth_url, state_jwt) = authorization(&state, None)?;

    Ok(HttpResponse::Found()
        .append_header(("Location", auth_url.to_string()))
        .cookie(build_state_cookie(&state_jwt))
        .finish())
}

/// GET /auth/oauth/google/callback — Google redirects here after consent.
async fn callback(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    query: web::Query<CallbackQuery>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    // A link started from the settings page comes back here too. It always
    // ends in a redirect to that page, success or not, rather than an error
    // page out of context.
    let stored_state = req
        .cookie("oauth_state")
        .and_then(|c| read_state_cookie(&state.config.jwt_secret, c.value()).ok());
    if let Some(stored) = stored_state {
        if let Some(link_user) = stored.link_user.clone() {
            let return_to = stored.return_to.clone();
            let outcome = finish_link(&req, &state, &query, stored, &link_user).await;
            return Ok(HttpResponse::Found()
                .append_header((
                    "Location",
                    link_return_url(&state.config.public_site_url, return_to.as_deref(), outcome),
                ))
                .cookie(clear_cookie("oauth_state"))
                .finish());
        }
    }

    if let Some(err) = &query.error {
        return Err(AppError::BadRequest(format!("Google OAuth error: {}", err)));
    }

    let code = query
        .code
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("Missing authorization code".to_string()))?;
    let received_state = query
        .state
        .as_deref()
        .ok_or_else(|| AppError::BadRequest("Missing OAuth state".to_string()))?;

    // Verify CSRF state + recover PKCE verifier from the signed cookie.
    let cookie_token = req
        .cookie("oauth_state")
        .map(|c| c.value().to_string())
        .ok_or_else(|| AppError::BadRequest("Missing OAuth state cookie".to_string()))?;
    let stored = read_state_cookie(&state.config.jwt_secret, &cookie_token)?;
    if stored.csrf != received_state {
        return Err(AppError::BadRequest(
            "OAuth state mismatch (possible CSRF)".to_string(),
        ));
    }

    // Exchange the code for tokens.
    let client = build_oauth_client(&state)?;
    let token_resp = client
        .exchange_code(AuthorizationCode::new(code.to_string()))
        .set_pkce_verifier(PkceCodeVerifier::new(stored.pkce_verifier))
        .request_async(async_http_client)
        .await
        .map_err(|e| {
            log::error!("OAuth token exchange failed: {}", e);
            AppError::Unauthorized("OAuth token exchange failed".to_string())
        })?;

    let userinfo = fetch_userinfo(token_resp.access_token().secret()).await?;
    let email = userinfo
        .email
        .clone()
        .ok_or_else(|| AppError::Unauthorized("Google did not return an email".to_string()))?;

    // 1) Returning Google user — looked up by (provider, sub).
    let by_sub: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT id, username, is_og FROM users
         WHERE oauth_provider = ? AND oauth_subject = ?",
    )
    .bind(PROVIDER)
    .bind(&userinfo.sub)
    .fetch_optional(&state.pool)
    .await?;

    if let Some((user_id, username, is_og)) = by_sub {
        adopt_google_email(&state, &user_id, &email, userinfo.email_verified).await?;
        let session = issue_session(&state, &user_id, &username, is_og).await?;
        return Ok(HttpResponse::Found()
            .append_header(("Location", state.config.oauth_success_redirect.as_str()))
            .cookie(session)
            .cookie(clear_cookie("oauth_state"))
            .finish());
    }

    // 2) Auto-link by verified email -- verified on both sides. Our side
    // matters as much as Google's: an address nobody confirmed on this site
    // says nothing about who owns the account.
    if userinfo.email_verified {
        let by_email: Option<(String, String, bool)> = sqlx::query_as(
            "SELECT id, username, is_og FROM users WHERE email = ? AND email_verified = TRUE",
        )
        .bind(normalize_google_email(&email))
        .fetch_optional(&state.pool)
        .await?;

        if let Some((user_id, username, is_og)) = by_email {
            sqlx::query(
                "UPDATE users SET oauth_provider = ?, oauth_subject = ? WHERE id = ?",
            )
            .bind(PROVIDER)
            .bind(&userinfo.sub)
            .bind(&user_id)
            .execute(&state.pool)
            .await?;

            let session = issue_session(&state, &user_id, &username, is_og).await?;
            return Ok(HttpResponse::Found()
                .append_header(("Location", state.config.oauth_success_redirect.as_str()))
                .cookie(session)
                .cookie(clear_cookie("oauth_state"))
                .finish());
        }
    }

    // 3) New user — stash claims in oauth_pending and bounce to signup page.
    let pending = sign_pending_cookie(
        &state.config.jwt_secret,
        &userinfo.sub,
        &email,
        userinfo.email_verified,
    )?;

    Ok(HttpResponse::Found()
        .append_header(("Location", state.config.oauth_signup_redirect.as_str()))
        .cookie(build_pending_cookie(&pending))
        .cookie(clear_cookie("oauth_state"))
        .finish())
}

/// POST /auth/oauth/google/complete — first-time signup: pick username + HWID.
async fn complete(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    body: web::Json<CompleteBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let pending_token = req
        .cookie("oauth_pending")
        .map(|c| c.value().to_string())
        .ok_or_else(|| AppError::Unauthorized("No pending OAuth session".to_string()))?;
    let pending = read_pending_cookie(&state.config.jwt_secret, &pending_token)?;

    let username = body
        .username
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing username".to_string()))?;

    let hwid = body
        .hwid
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Unauthorized("Could not validate your device".to_string())
        })?;

    // HWID poison check — same query as /register.
    let poisoned: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM poison_hwids WHERE hwid = ?")
            .bind(hwid)
            .fetch_optional(&state.pool)
            .await?;
    if poisoned.is_some() {
        return Err(AppError::Forbidden("This device is banned.".to_string()));
    }

    // Race protection: another tab/session may have already linked this sub.
    let by_sub: Option<(String, String, bool)> = sqlx::query_as(
        "SELECT id, username, is_og FROM users
         WHERE oauth_provider = ? AND oauth_subject = ?",
    )
    .bind(PROVIDER)
    .bind(&pending.pending_sub)
    .fetch_optional(&state.pool)
    .await?;
    if let Some((user_id, existing_username, is_og)) = by_sub {
        let session = issue_session(&state, &user_id, &existing_username, is_og).await?;
        return Ok(HttpResponse::Ok()
            .cookie(session)
            .cookie(clear_cookie("oauth_pending"))
            .json(json!({ "ok": true, "username": existing_username })));
    }

    // Username availability.
    let taken: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&state.pool)
            .await?;
    if taken.is_some() {
        return Err(AppError::Conflict("Username already taken".to_string()));
    }

    // Email collision: if a row already has this email but a different oauth_subject
    // (or verified flag is false so we couldn't auto-link earlier), refuse rather
    // than silently shadow it.
    let email_taken: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE email = ?")
            .bind(normalize_google_email(&pending.pending_email))
            .fetch_optional(&state.pool)
            .await?;
    if email_taken.is_some() {
        return Err(AppError::Conflict(
            "An account with this email already exists. Please sign in with your password and link Google from settings.".to_string(),
        ));
    }

    let user_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, email, email_verified, oauth_provider, oauth_subject)
         VALUES (?, ?, '', ?, ?, ?, ?)",
    )
    .bind(&user_id)
    .bind(username)
    // Only an address Google has verified is kept. An unverified one would
    // hold its place in the unique index and stop the inbox's real owner from
    // ever confirming it on their own account.
    .bind(pending.pending_email_verified.then(|| normalize_google_email(&pending.pending_email)))
    .bind(pending.pending_email_verified)
    .bind(PROVIDER)
    .bind(&pending.pending_sub)
    .execute(&state.pool)
    .await?;

    let session = issue_session(&state, &user_id, username, false).await?;
    Ok(HttpResponse::Ok()
        .cookie(session)
        .cookie(clear_cookie("oauth_pending"))
        .json(json!({ "ok": true, "username": username })))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/auth/oauth/google", web::get().to(initiate))
        .route("/auth/oauth/google/callback", web::get().to(callback))
        .route("/auth/oauth/google/complete", web::post().to(complete))
        .route("/auth/oauth/google/link", web::post().to(link_start))
        .route("/account/google", web::delete().to(unlink));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_only_ever_returns_to_one_of_the_two_settings_pages() {
        let site = "https://site.example";
        assert_eq!(
            link_return_url(site, Some("rd"), "linked"),
            "https://site.example/raindrippy/profile?google=linked#settings"
        );
        assert_eq!(
            link_return_url(site, Some("https://evil.example"), "taken"),
            "https://site.example/settings.html?google=taken#google"
        );
        assert_eq!(
            link_return_url(site, None, "error"),
            "https://site.example/settings.html?google=error#google"
        );
    }

    #[test]
    fn a_link_state_round_trips_through_its_cookie() {
        let token = sign_state_cookie("s", "csrf", "verifier", Some(("user-1", "rd"))).unwrap();
        let claims = read_state_cookie("s", &token).unwrap();
        assert_eq!(claims.link_user.as_deref(), Some("user-1"));
        assert_eq!(claims.return_to.as_deref(), Some("rd"));

        // A plain sign-in carries no link, and old cookies without the
        // fields still read.
        let token = sign_state_cookie("s", "csrf", "verifier", None).unwrap();
        assert_eq!(read_state_cookie("s", &token).unwrap().link_user, None);
    }

    #[test]
    fn the_google_path_issues_the_same_session_cookie() {
        let cookie = build_user_token_cookie("token");
        assert_eq!(cookie.same_site(), Some(SameSite::Lax));
        assert_eq!(cookie.path(), Some("/"));
        assert!(cookie.http_only().unwrap_or(false));
        assert!(cookie.secure().unwrap_or(false));
    }
}
