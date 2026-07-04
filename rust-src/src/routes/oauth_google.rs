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
use crate::middleware::auth::create_jwt;
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

fn sign_state_cookie(secret: &str, csrf: &str, pkce_verifier: &str) -> Result<String, AppError> {
    let claims = StateClaims {
        state_kind: "oauth_state".to_string(),
        csrf: csrf.to_string(),
        pkce_verifier: pkce_verifier.to_string(),
        exp: chrono::Utc::now().timestamp() + STATE_TTL_SECS,
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

fn build_user_token_cookie(token: &str) -> Cookie<'static> {
    Cookie::build("userToken", token.to_string())
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Strict)
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
    is_admin: bool,
    is_og: bool,
) -> Result<Cookie<'static>, AppError> {
    let token = create_jwt(
        username,
        user_id,
        is_admin,
        is_og,
        &state.config.jwt_secret,
        SEVEN_DAYS_SECS,
    )
    .map_err(|e| AppError::Internal(format!("JWT: {}", e)))?;
    Ok(build_user_token_cookie(&token))
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// GET /auth/oauth/google — kick off the OAuth dance.
async fn initiate(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let client = build_oauth_client(&state)?;
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
    )?;

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
    let by_sub: Option<(String, String, bool, bool)> = sqlx::query_as(
        "SELECT id, username, is_admin, is_og FROM users
         WHERE oauth_provider = ? AND oauth_subject = ?",
    )
    .bind(PROVIDER)
    .bind(&userinfo.sub)
    .fetch_optional(&state.pool)
    .await?;

    if let Some((user_id, username, is_admin, is_og)) = by_sub {
        let session = issue_session(&state, &user_id, &username, is_admin, is_og).await?;
        return Ok(HttpResponse::Found()
            .append_header(("Location", state.config.oauth_success_redirect.as_str()))
            .cookie(session)
            .cookie(clear_cookie("oauth_state"))
            .finish());
    }

    // 2) Auto-link by verified email.
    if userinfo.email_verified {
        let by_email: Option<(String, String, bool, bool)> = sqlx::query_as(
            "SELECT id, username, is_admin, is_og FROM users WHERE email = ?",
        )
        .bind(&email)
        .fetch_optional(&state.pool)
        .await?;

        if let Some((user_id, username, is_admin, is_og)) = by_email {
            sqlx::query(
                "UPDATE users SET oauth_provider = ?, oauth_subject = ? WHERE id = ?",
            )
            .bind(PROVIDER)
            .bind(&userinfo.sub)
            .bind(&user_id)
            .execute(&state.pool)
            .await?;

            let session = issue_session(&state, &user_id, &username, is_admin, is_og).await?;
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
    let by_sub: Option<(String, String, bool, bool)> = sqlx::query_as(
        "SELECT id, username, is_admin, is_og FROM users
         WHERE oauth_provider = ? AND oauth_subject = ?",
    )
    .bind(PROVIDER)
    .bind(&pending.pending_sub)
    .fetch_optional(&state.pool)
    .await?;
    if let Some((user_id, existing_username, is_admin, is_og)) = by_sub {
        let session = issue_session(&state, &user_id, &existing_username, is_admin, is_og).await?;
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
            .bind(&pending.pending_email)
            .fetch_optional(&state.pool)
            .await?;
    if email_taken.is_some() {
        return Err(AppError::Conflict(
            "An account with this email already exists. Please sign in with your password and link Google from settings.".to_string(),
        ));
    }

    let user_id = Uuid::new_v4().to_string();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, email, oauth_provider, oauth_subject)
         VALUES (?, ?, '', ?, ?, ?)",
    )
    .bind(&user_id)
    .bind(username)
    .bind(&pending.pending_email)
    .bind(PROVIDER)
    .bind(&pending.pending_sub)
    .execute(&state.pool)
    .await?;

    let session = issue_session(&state, &user_id, username, false, false).await?;
    Ok(HttpResponse::Ok()
        .cookie(session)
        .cookie(clear_cookie("oauth_pending"))
        .json(json!({ "ok": true, "username": username })))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.route("/auth/oauth/google", web::get().to(initiate))
        .route("/auth/oauth/google/callback", web::get().to(callback))
        .route("/auth/oauth/google/complete", web::post().to(complete));
}
