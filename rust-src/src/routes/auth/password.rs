use actix_web::{web, HttpRequest, HttpResponse};
use bcrypt::{hash, verify, DEFAULT_COST};
use chrono::Utc;
use rand::Rng;
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;
use crate::middleware::auth::AuthUser;
use crate::AppState;
use crate::middleware::rate_limit::RateLimiter;

use super::common::*;

#[derive(Debug, Deserialize)]
pub struct GenPwdResetBody {
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResetPasswordBody {
    pub username: Option<String>,
    #[serde(rename = "oldPassword")]
    pub old_password: Option<String>,
    #[serde(rename = "newPassword")]
    pub new_password: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ForgotPasswordBody {
    #[serde(rename = "newPassword")]
    pub new_password: Option<String>,
    #[serde(rename = "resetSession")]
    pub reset_session: Option<String>,
}

/// POST /reset-password
pub(super) async fn reset_password(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    body: web::Json<ResetPasswordBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let username = body
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing fields".to_string()))?;
    let old_password = body
        .old_password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing fields".to_string()))?;
    let new_password = body
        .new_password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing fields".to_string()))?;

    #[derive(sqlx::FromRow)]
    struct UserRow {
        id: String,
        password_hash: String,
    }

    let user: Option<UserRow> =
        sqlx::query_as("SELECT id, password_hash FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&state.pool)
            .await?;

    let user = user.ok_or_else(|| AppError::NotFound("User not found".to_string()))?;

    let valid = verify(old_password, &user.password_hash)?;
    if !valid {
        return Err(AppError::Unauthorized("Invalid credentials".to_string()));
    }

    let hashed = hash(new_password, DEFAULT_COST)?;

    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(&hashed)
        .bind(&user.id)
        .execute(&state.pool)
        .await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "Password reset successfully" })))
}

/// POST /forgot-password
pub(super) async fn forgot_password(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    body: web::Json<ForgotPasswordBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let new_password = body
        .new_password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing fields".to_string()))?;
    let reset_session = body
        .reset_session
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing fields".to_string()))?;

    // Look up session
    #[derive(sqlx::FromRow)]
    struct SessionRow {
        username: String, // actually stores user_id per the JS code
        expires_at: String,
    }

    let session: Option<SessionRow> = sqlx::query_as(
            "SELECT username, CAST(expires_at AS CHAR) AS expires_at FROM password_reset_sessions WHERE session_token = ?",
    )
    .bind(reset_session)
    .fetch_optional(&state.pool)
    .await?;

    let session =
        session.ok_or_else(|| AppError::BadRequest("Invalid session token".to_string()))?;

    // Check expiration
    let expires_at = chrono::NaiveDateTime::parse_from_str(&session.expires_at, "%Y-%m-%d %H:%M:%S")
        .map(|dt| dt.and_utc())
        .unwrap_or_else(|_| Utc::now());

    if expires_at < Utc::now() {
        sqlx::query("DELETE FROM password_reset_sessions WHERE session_token = ?")
            .bind(reset_session)
            .execute(&state.pool)
            .await?;
        return Err(AppError::BadRequest("Session token expired".to_string()));
    }

    // The `username` field in password_reset_sessions actually stores the user ID
    let user: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE id = ?")
            .bind(&session.username)
            .fetch_optional(&state.pool)
            .await?;

    let (user_id,) =
        user.ok_or_else(|| AppError::NotFound("User not found".to_string()))?;

    let hashed = hash(new_password, DEFAULT_COST)?;

    sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(&hashed)
        .bind(&user_id)
        .execute(&state.pool)
        .await?;

    sqlx::query("DELETE FROM password_reset_sessions WHERE session_token = ?")
        .bind(reset_session)
        .execute(&state.pool)
        .await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "Password changed successfully" })))
}

/// POST /admin/gen-pwd-reset
pub(super) async fn admin_gen_pwd_reset(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<GenPwdResetBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    if !auth.is_admin {
        return Err(AppError::Forbidden("Access denied".to_string()));
    }

    let username = body
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing username".to_string()))?;

    let user: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&state.pool)
            .await?;

    let (user_id,) =
        user.ok_or_else(|| AppError::NotFound("User not found".to_string()))?;

    // Generate session token: 32 random bytes -> hex
    let mut rng = rand::thread_rng();
    let random_bytes: [u8; 32] = rng.gen();
    let session_token: String = random_bytes.iter().map(|b| format!("{:02x}", b)).collect();

    let expires_at = (Utc::now() + chrono::Duration::minutes(15))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    sqlx::query(
        "INSERT INTO password_reset_sessions (username, session_token, expires_at) VALUES (?, ?, ?)",
    )
    .bind(&user_id)
    .bind(&session_token)
    .bind(&expires_at)
    .execute(&state.pool)
    .await?;

    Ok(HttpResponse::Ok().json(json!({
        "message": "Password reset session created",
        "resetSession": session_token,
        "expiresAt": expires_at
    })))
}

