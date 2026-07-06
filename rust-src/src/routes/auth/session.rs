use actix_web::cookie::time::Duration as CookieDuration;
use actix_web::cookie::{Cookie, SameSite};
use actix_web::{web, HttpRequest, HttpResponse};
use bcrypt::{hash, verify, DEFAULT_COST};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::middleware::auth::{create_jwt, decode_jwt};
use crate::middleware::auth_failopen::OptionalAuthUser;
use crate::AppState;
use crate::middleware::rate_limit::RateLimiter;

use super::common::*;

const SEVEN_DAYS_SECS: u64 = 7 * 24 * 60 * 60;

#[derive(Debug, Deserialize)]
pub struct RegisterBody {
    pub username: Option<String>,
    pub password: Option<String>,
    pub hwid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginBody {
    pub username: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct VCredsBody {
    pub username: Option<String>,
    pub password: Option<String>,
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

fn clear_cookie(name: &str) -> Cookie<'static> {
    Cookie::build(name.to_string(), "")
        .http_only(true)
        .secure(true)
        .same_site(SameSite::Strict)
        .max_age(CookieDuration::seconds(0))
        .path("/")
        .finish()
}

/// POST /register
pub(super) async fn register(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    body: web::Json<RegisterBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let username = body
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing username or password".to_string()))?;
    let password = body
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing username or password".to_string()))?;

    // Check HWID
    let hwid = match &body.hwid {
        Some(h) if !h.is_empty() => h.clone(),
        _ => {
            return Ok(HttpResponse::Unauthorized()
                .json(json!({ "error": "Could not validate your device" })));
        }
    };

    // Check poison hwids
    let poisoned: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM poison_hwids WHERE hwid = ?")
            .bind(&hwid)
            .fetch_optional(&state.pool)
            .await?;
    if poisoned.is_some() {
        return Ok(
            HttpResponse::Forbidden().json(json!({ "error": "This device is banned." }))
        );
    }

    // Check username uniqueness
    let existing: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&state.pool)
            .await?;
    if existing.is_some() {
        return Ok(
            HttpResponse::Conflict().json(json!({ "error": "Username already taken" }))
        );
    }

    let hashed = hash(password, DEFAULT_COST)?;
    let user_id = Uuid::new_v4().to_string();

    sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, ?)")
        .bind(&user_id)
        .bind(username)
        .bind(&hashed)
        .execute(&state.pool)
        .await?;

    let token = create_jwt(username, &user_id, false, false, &state.config.jwt_secret, SEVEN_DAYS_SECS)
        .map_err(|e| AppError::Internal(format!("JWT error: {}", e)))?;

    let cookie = build_user_token_cookie(&token);

    Ok(HttpResponse::Ok()
        .cookie(cookie)
        .json(json!({ "message": "Registration successful", "token": token })))
}

/// POST /login
pub(super) async fn login(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    body: web::Json<LoginBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let username = body
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing username or password".to_string()))?;
    let password = body
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing username or password".to_string()))?;

    #[derive(sqlx::FromRow)]
    struct UserRow {
        id: String,
        password_hash: String,
        is_admin: bool,
        is_og: bool,
    }

    let user: Option<UserRow> =
        sqlx::query_as("SELECT id, password_hash, is_admin, is_og FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&state.pool)
            .await?;

    let user = user.ok_or_else(|| AppError::Unauthorized("Invalid credentials".to_string()))?;

    let valid = verify(password, &user.password_hash)?;
    if !valid {
        return Err(AppError::Unauthorized("Invalid credentials".to_string()));
    }

    let token = create_jwt(
        username,
        &user.id,
        user.is_admin,
        user.is_og,
        &state.config.jwt_secret,
        SEVEN_DAYS_SECS,
    )
    .map_err(|e| AppError::Internal(format!("JWT error: {}", e)))?;

    let cookie = build_user_token_cookie(&token);

    Ok(HttpResponse::Ok().cookie(cookie).json(json!({
        "message": "Login successful",
        "token": token,
        "isAdmin": user.is_admin
    })))
}

/// POST /v-creds
pub(super) async fn validate_credentials(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    body: web::Json<VCredsBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let username = body
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing username or password".to_string()))?;
    let password = body
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing username or password".to_string()))?;

    #[derive(sqlx::FromRow)]
    struct UserRow {
        password_hash: String,
        is_admin: bool,
        is_member: bool,
    }

    let user: Option<UserRow> = sqlx::query_as(
        "SELECT password_hash, is_admin, is_member FROM users WHERE username = ?",
    )
    .bind(username)
    .fetch_optional(&state.pool)
    .await?;

    let user = user.ok_or_else(|| AppError::Unauthorized("Invalid credentials".to_string()))?;

    let valid = verify(password, &user.password_hash)?;
    if !valid {
        return Err(AppError::Unauthorized("Invalid credentials".to_string()));
    }

    Ok(HttpResponse::Ok().json(json!({
        "message": "Credentials valid",
        "isAdmin": user.is_admin,
        "isMember": user.is_member
    })))
}

/// GET /validate
pub(super) async fn validate(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let token = req
        .cookie("userToken")
        .map(|c| c.value().to_string())
        .ok_or_else(|| AppError::Unauthorized("Not logged in".to_string()))?;

    let claims = decode_jwt(&token, &state.config.jwt_secret)
        .map_err(|_| AppError::Unauthorized("Invalid token".to_string()))?;

    #[derive(sqlx::FromRow)]
    struct UserRow {
        username: String,
        is_admin: bool,
    }

    let user: Option<UserRow> =
        sqlx::query_as("SELECT username, is_admin FROM users WHERE id = ?")
            .bind(&claims.id)
            .fetch_optional(&state.pool)
            .await?;

    let user = user.ok_or_else(|| AppError::Unauthorized("Invalid user".to_string()))?;

    Ok(HttpResponse::Ok().json(json!({
        "username": user.username,
        "isAdmin": user.is_admin
    })))
}

/// POST /refresh-token
pub(super) async fn refresh_token(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let token = req
        .cookie("refreshToken")
        .map(|c| c.value().to_string())
        .ok_or_else(|| AppError::Unauthorized("No refresh token".to_string()))?;

    let decoded = decode_jwt(&token, &state.config.jwt_secret)
        .map_err(|_| AppError::Forbidden("Invalid refresh token".to_string()))?;

    let new_access_token = create_jwt(
        &decoded.username,
        &decoded.id,
        decoded.is_admin,
        decoded.is_og,
        &state.config.jwt_secret,
        15 * 60, // 15 minutes
    )
    .map_err(|e| AppError::Internal(format!("JWT error: {}", e)))?;

    let cookie = Cookie::build("authToken", new_access_token.clone())
        .http_only(false)
        .secure(true)
        .same_site(SameSite::None)
        .max_age(CookieDuration::seconds(900))
        .path("/")
        .finish();

    Ok(HttpResponse::Ok()
        .cookie(cookie)
        .json(json!({ "accessToken": new_access_token })))
}

/// POST /logout
pub(super) async fn logout(
    req: HttpRequest,
    limiter: web::Data<RateLimiter>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    Ok(HttpResponse::Ok()
        .cookie(clear_cookie("userToken"))
        .json(json!({ "message": "Logged out successfully" })))
}

/// POST /purge-logout
pub(super) async fn purge_logout(
    req: HttpRequest,
    limiter: web::Data<RateLimiter>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    Ok(HttpResponse::Ok()
        .cookie(clear_cookie("userToken"))
        .cookie(clear_cookie("authToken"))
        .cookie(clear_cookie("refreshToken"))
        .json(json!({ "message": "Logged out successfully" })))
}

/// GET /logged-in
pub(super) async fn logged_in(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    opt_auth: OptionalAuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let user = match opt_auth.0 {
        Some(u) => u,
        None => return Ok(HttpResponse::Ok().json(json!({ "loggedIn": false }))),
    };

    if user.username.is_empty() {
        return Ok(HttpResponse::Ok().json(json!({ "loggedIn": false })));
    }

    let exists: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = ?")
            .bind(&user.username)
            .fetch_optional(&state.pool)
            .await?;

    match exists {
        Some(_) => Ok(HttpResponse::Ok().json(json!({ "loggedIn": true }))),
        None => Ok(HttpResponse::Ok().json(json!({ "loggedIn": false }))),
    }
}

