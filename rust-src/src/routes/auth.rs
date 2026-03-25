use actix_multipart::Multipart;
use actix_web::cookie::time::Duration as CookieDuration;
use actix_web::cookie::{Cookie, SameSite};
use actix_web::{web, HttpRequest, HttpResponse};
use bcrypt::{hash, verify, DEFAULT_COST};
use chrono::Utc;
use futures_util::StreamExt;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::io::{Read as _, Write as _};
use std::path::Path;
use std::collections::HashMap;
use uuid::Uuid;

use crate::error::AppError;
use crate::middleware::api_key_auth::get_key_usage;
use crate::middleware::auth::{create_jwt, decode_jwt, AuthUser};
use crate::middleware::auth_failopen::OptionalAuthUser;
use crate::middleware::dashboard::{check_rd_admin_auth, check_status_dashboard_auth};
use crate::middleware::rate_limit::RateLimiter;
use crate::AppState;

// ─── Request / response structs ──────────────────────────────────────────────

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

#[derive(Debug, Deserialize)]
pub struct UpdateAdminStatusBody {
    pub username: String,
    #[serde(rename = "isAdmin")]
    pub is_admin: bool,
}

#[derive(Debug, Deserialize)]
pub struct AccStatusQuery {
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteUserBody {
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ModerateBody {
    pub username: Option<String>,
    #[serde(rename = "type")]
    pub ban_type: Option<String>,
    #[serde(rename = "modNote")]
    pub mod_note: Option<String>,
    pub incriminatory: Option<serde_json::Value>,
    #[serde(rename = "poisonHWID")]
    pub poison_hwid: Option<String>,
    #[serde(rename = "makeAdmin")]
    pub make_admin: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UnbanBody {
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListUsersQuery {
    pub search: Option<String>,
    pub page: Option<u32>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateMemberStatusBody {
    pub username: Option<String>,
    #[serde(rename = "isMember")]
    pub is_member: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateProjStatusBody {
    pub username: Option<String>,
    #[serde(rename = "isAllowed")]
    pub is_allowed: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct UpdatePlexStatusBody {
    pub username: Option<String>,
    #[serde(rename = "isAllowed")]
    pub is_allowed: Option<bool>,
}

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

#[derive(Debug, Deserialize)]
pub struct SkinDeleteBody {
    pub username: Option<String>,
    pub password: Option<String>,
    pub filename: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct UploadLogEntry {
    pub timestamp: String,
    pub username: String,
    pub file: String,
}

#[derive(Serialize)]
pub struct GameDetails {
    pub title: String,
    pub description: String,
    pub image: String,
    pub file: String,       // URL to the .zip file
    pub executable: String, // The exact name of the .exe inside the zip (e.g., "Game.exe")
}
// ─── Helpers ─────────────────────────────────────────────────────────────────

const SEVEN_DAYS_SECS: u64 = 7 * 24 * 60 * 60;

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

fn gen_custom_uuid() -> String {
    let mut rng = rand::thread_rng();
    let random_bytes: [u8; 16] = rng.gen();
    let random_hex: String = random_bytes.iter().map(|b| format!("{:02x}", b)).collect();

    let fingerprint = format!(
        "Rust/actix-web ({}; {})|en-US|1920x1080|{}|{}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        Utc::now().timestamp(),
        num_cpus_hint(),
    );

    let combined = format!("{}|{}", fingerprint, random_hex);
    let mut hasher = Sha256::new();
    hasher.update(combined.as_bytes());
    let result = hasher.finalize();
    let b64 = base64_encode(&result);
    b64.chars().take(32).collect()
}

fn num_cpus_hint() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut result = String::new();
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let triple = (b0 << 16) | (b1 << 8) | b2;
        result.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        result.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            result.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
        if chunk.len() > 2 {
            result.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            result.push('=');
        }
    }
    result
}

/// Read, update, and write the authedPlayers.json file with advisory locking.
fn update_authed_players_file<F>(path: &str, update_fn: F)
where
    F: FnOnce(&mut Vec<serde_json::Value>),
{
    use std::fs::OpenOptions;

    let file_result = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(path);

    let file = match file_result {
        Ok(f) => f,
        Err(e) => {
            log::error!("Error opening authedPlayers file: {}", e);
            return;
        }
    };

    if let Err(e) = fs2::FileExt::lock_exclusive(&file) {
        log::error!("File lock error: {}", e);
        return;
    }

    let mut contents = String::new();
    let mut reader = std::io::BufReader::new(&file);
    if let Err(e) = reader.read_to_string(&mut contents) {
        log::error!("Error reading authedPlayers file: {}", e);
        let _ = fs2::FileExt::unlock(&file);
        return;
    }

    let mut players: Vec<serde_json::Value> = match serde_json::from_str(&contents) {
        Ok(v) => v,
        Err(_) => {
            log::error!("Error parsing authedPlayers file, treating as empty array");
            let _ = fs2::FileExt::unlock(&file);
            return;
        }
    };

    update_fn(&mut players);

    // Truncate and write back
    if let Err(e) = file.set_len(0) {
        log::error!("Error truncating authedPlayers file: {}", e);
        let _ = fs2::FileExt::unlock(&file);
        return;
    }
    let mut writer = std::io::BufWriter::new(&file);
    if let Err(e) = writer.write_all(
        serde_json::to_string_pretty(&players)
            .unwrap_or_default()
            .as_bytes(),
    ) {
        log::error!("Error writing authedPlayers file: {}", e);
    }

    let _ = fs2::FileExt::unlock(&file);
}

fn get_rate_limiter_ip(req: &HttpRequest) -> std::net::IpAddr {
    req.peer_addr()
        .map(|a| a.ip())
        .unwrap_or_else(|| "127.0.0.1".parse().unwrap())
}

fn check_rate_limit(
    req: &HttpRequest,
    limiter: &web::Data<RateLimiter>,
) -> Result<(), AppError> {
    let ip = get_rate_limiter_ip(req);
    limiter.check(ip)
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// POST /register
async fn register(
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

    let token = create_jwt(username, &user_id, false, &state.config.jwt_secret, SEVEN_DAYS_SECS)
        .map_err(|e| AppError::Internal(format!("JWT error: {}", e)))?;

    let cookie = build_user_token_cookie(&token);

    Ok(HttpResponse::Ok()
        .cookie(cookie)
        .json(json!({ "message": "Registration successful", "token": token })))
}

/// POST /login
async fn login(
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
    }

    let user: Option<UserRow> =
        sqlx::query_as("SELECT id, password_hash, is_admin FROM users WHERE username = ?")
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
async fn validate_credentials(
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

/// POST /update-admin-status
async fn update_admin_status(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<UpdateAdminStatusBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    if !auth.is_admin {
        return Err(AppError::Forbidden("Access Denied".to_string()));
    }

    let target: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = ?")
            .bind(&body.username)
            .fetch_optional(&state.pool)
            .await?;

    let (user_id,) =
        target.ok_or_else(|| AppError::NotFound("User not found".to_string()))?;

    sqlx::query("UPDATE users SET is_admin = ? WHERE id = ?")
        .bind(body.is_admin)
        .bind(&user_id)
        .execute(&state.pool)
        .await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "Admin status updated" })))
}

/// GET /validate
async fn validate(
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

/// GET /account-status
async fn account_status(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    _opt_auth: OptionalAuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let token = match req.cookie("userToken") {
        Some(c) => c.value().to_string(),
        None => return Ok(HttpResponse::Ok().json(json!({ "accountStatus": null }))),
    };

    let claims = match decode_jwt(&token, &state.config.jwt_secret) {
        Ok(c) => c,
        Err(_) => return Ok(HttpResponse::Ok().json(json!({ "accountStatus": null }))),
    };

    let hwid_cookie = req.cookie("hwid").map(|c| c.value().to_string());

    // Lookup user
    let user: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = ?")
            .bind(&claims.username)
            .fetch_optional(&state.pool)
            .await?;

    let (user_id,) = match user {
        Some(u) => u,
        None => return Ok(HttpResponse::Ok().json(json!({ "accountStatus": null }))),
    };

    // Check moderation
    #[derive(sqlx::FromRow)]
    struct ModerationRow {
        #[sqlx(rename = "type")]
        mod_type: String,
        moderated_at: String,
        mod_note: Option<String>,
        incriminatory: Option<String>,
    }

    let moderation: Option<ModerationRow> = sqlx::query_as(
            "SELECT type, CAST(moderated_at AS CHAR) AS moderated_at, mod_note, incriminatory FROM user_moderation WHERE user_id = ? ORDER BY moderated_at DESC LIMIT 1",
    )
    .bind(&user_id)
    .fetch_optional(&state.pool)
    .await?;

    let moderation = match moderation {
        Some(m) => m,
        None => return Ok(HttpResponse::Ok().json(json!({ "accountStatus": "ok" }))),
    };

    // Need hwid cookie for poison check
    let hwid = match hwid_cookie {
        Some(h) => h,
        None => {
            return Ok(
                HttpResponse::BadRequest().json(json!({ "error": "Missing hwid param" }))
            );
        }
    };

    // If poison type, insert HWID if not already there
    if moderation.mod_type == "poison" {
        let poisoned: Option<(i64,)> =
            sqlx::query_as("SELECT id FROM poison_hwids WHERE hwid = ?")
                .bind(&hwid)
                .fetch_optional(&state.pool)
                .await?;
        if poisoned.is_none() {
            let _ = sqlx::query(
                "INSERT IGNORE INTO poison_hwids (hwid, user_id) VALUES (?, ?)",
            )
            .bind(&hwid)
            .bind(&user_id)
            .execute(&state.pool)
            .await;
        }
    }

    let incriminatory_val: Option<serde_json::Value> =
        moderation.incriminatory.as_deref().and_then(|s| {
            serde_json::from_str(s).ok()
        });

    Ok(HttpResponse::Forbidden().json(json!({
        "accountStatus": "moderated",
        "banInfo": {
            "type": moderation.mod_type,
            "moderatedTimePDT": moderation.moderated_at,
            "modNote": moderation.mod_note,
            "incriminatory": incriminatory_val
        }
    })))
}

/// GET /account-data
async fn account_data(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    #[derive(sqlx::FromRow)]
    struct UserRow {
        id: String,
        username: String,
        is_admin: bool,
        is_member: bool,
        created_at: String,
        #[sqlx(rename = "apiKeyId")]
        api_key_id: Option<String>,
    }

    let user: Option<UserRow> = sqlx::query_as(
            "SELECT id, username, is_admin, is_member, CAST(created_at AS CHAR) AS created_at, apiKeyId FROM users WHERE username = ?",
    )
    .bind(&auth.username)
    .fetch_optional(&state.pool)
    .await?;

    let user = user.ok_or_else(|| AppError::NotFound("User not found".to_string()))?;

    Ok(HttpResponse::Ok().json(json!({
        "id": user.id,
        "username": user.username,
        "isAdmin": user.is_admin,
        "isMember": user.is_member,
        "createdAt": user.created_at,
        "hasApiKey": user.api_key_id.is_some()
    })))
}

/// DELETE /delete-account
async fn delete_account(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let user: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = ?")
            .bind(&auth.username)
            .fetch_optional(&state.pool)
            .await?;

    let (user_id,) =
        user.ok_or_else(|| AppError::NotFound("User not found".to_string()))?;

    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(&user_id)
        .execute(&state.pool)
        .await?;

    sqlx::query("DELETE FROM user_moderation WHERE user_id = ?")
        .bind(&user_id)
        .execute(&state.pool)
        .await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "Account deleted successfully" })))
}

/// GET /accstatus-cuser
async fn accstatus_cuser(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    query: web::Query<AccStatusQuery>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let username = query
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing username param".to_string()))?;

    // Look up user
    let user: Option<(String, String)> =
        sqlx::query_as("SELECT id, username FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&state.pool)
            .await?;

    let (user_id, _username) = match user {
        Some(u) => u,
        None => return Ok(HttpResponse::Ok().json(serde_json::Value::Null)),
    };

    // Check moderation
    #[derive(sqlx::FromRow)]
    struct BanRow {
        #[sqlx(rename = "type")]
        ban_type: String,
        moderated_at: String,
        mod_note: Option<String>,
        incriminatory: Option<String>,
    }

    let ban: Option<BanRow> = sqlx::query_as(
            "SELECT type, CAST(moderated_at AS CHAR) AS moderated_at, mod_note, incriminatory FROM user_moderation WHERE user_id = ? ORDER BY moderated_at DESC LIMIT 1",
    )
    .bind(&user_id)
    .fetch_optional(&state.pool)
    .await?;

    let authed_path = state.config.authed_players_path.clone();
    let username_owned = username.to_string();

    match ban {
        None => {
            // No ban -> mark ok in file
            let un = username_owned.clone();
            let ap = authed_path.clone();
            tokio::task::spawn_blocking(move || {
                update_authed_players_file(&ap, |players| {
                    if let Some(player) = players
                        .iter_mut()
                        .find(|p| p.get("username").and_then(|v| v.as_str()) == Some(&un))
                    {
                        player["moderation"] = json!({ "accountStatus": "ok" });
                    }
                });
            })
            .await
            .ok();

            Ok(HttpResponse::Ok().json(json!({ "accountStatus": "ok" })))
        }
        Some(ban) => {
            // Format moderated time (simple ISO-like representation)
            let moderated_time_pdt = &ban.moderated_at;

            let incriminatory_val: Option<serde_json::Value> =
                ban.incriminatory.as_deref().and_then(|s| {
                    serde_json::from_str(s).ok()
                });

            let ban_info = json!({
                "type": ban.ban_type,
                "moderatedTimePDT": moderated_time_pdt,
                "modNote": ban.mod_note,
                "incriminatory": incriminatory_val
            });

            // Update file
            let bi = ban_info.clone();
            let un = username_owned.clone();
            let ap = authed_path.clone();
            tokio::task::spawn_blocking(move || {
                update_authed_players_file(&ap, |players| {
                    if let Some(player) = players
                        .iter_mut()
                        .find(|p| p.get("username").and_then(|v| v.as_str()) == Some(&un))
                    {
                        player["moderation"] = json!({
                            "accountStatus": "moderated",
                            "banInfo": bi
                        });
                    }
                });
            })
            .await
            .ok();

            Ok(HttpResponse::Forbidden().json(json!({
                "accountStatus": "moderated",
                "banInfo": ban_info
            })))
        }
    }
}

/// GET /get-key
async fn get_key(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    #[derive(sqlx::FromRow)]
    struct UserRow {
        id: String,
        username: String,
        #[sqlx(rename = "apiKeyId")]
        api_key_id: Option<String>,
    }

    let user: Option<UserRow> =
        sqlx::query_as("SELECT id, username, apiKeyId FROM users WHERE username = ?")
            .bind(&auth.username)
            .fetch_optional(&state.pool)
            .await?;

    let user = user.ok_or_else(|| AppError::NotFound("User not found".to_string()))?;

    // If user already has an API key, fetch and return it
    if let Some(ref key_id) = user.api_key_id {
        let existing: Option<(String,)> =
            sqlx::query_as("SELECT api_key FROM api_keys WHERE id = ?")
                .bind(key_id)
                .fetch_optional(&state.pool)
                .await?;

        if let Some((api_key,)) = existing {
            return Ok(HttpResponse::Ok().json(json!({
                "apiKey": api_key,
                "message": "Existing API key retrieved"
            })));
        }

        // Key ID exists in user but not in api_keys table - clear it
        sqlx::query("UPDATE users SET apiKeyId = NULL WHERE id = ?")
            .bind(&user.id)
            .execute(&state.pool)
            .await?;
    }

    // Generate new API key
    let id = Uuid::new_v4().to_string();
    let api_key = gen_custom_uuid();
    let name = format!("{}'s API Key", user.username);

    sqlx::query(
        "INSERT INTO api_keys (id, name, api_key, hourly_limit) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&name)
    .bind(&api_key)
    .bind(100i32)
    .execute(&state.pool)
    .await?;

    sqlx::query("UPDATE users SET apiKeyId = ? WHERE id = ?")
        .bind(&id)
        .bind(&user.id)
        .execute(&state.pool)
        .await?;

    Ok(HttpResponse::Ok().json(json!({
        "apiKey": api_key,
        "message": "New API key generated"
    })))
}

/// POST /profile
async fn profile(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let token = match req.cookie("userToken") {
        Some(c) => c.value().to_string(),
        None => return Ok(HttpResponse::Ok().json(json!({ "username": null }))),
    };

    match decode_jwt(&token, &state.config.jwt_secret) {
        Ok(claims) => Ok(HttpResponse::Ok().json(json!({ "username": claims.username }))),
        Err(_) => Ok(HttpResponse::Ok().json(json!({ "username": null }))),
    }
}

/// GET /api-usage
async fn api_usage(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    #[derive(sqlx::FromRow)]
    struct UserRow {
        id: String,
        #[sqlx(rename = "apiKeyId")]
        api_key_id: Option<String>,
    }

    let user: Option<UserRow> =
        sqlx::query_as("SELECT id, apiKeyId FROM users WHERE username = ?")
            .bind(&auth.username)
            .fetch_optional(&state.pool)
            .await?;

    let user = user.ok_or_else(|| AppError::NotFound("User not found".to_string()))?;

    let key_id = user
        .api_key_id
        .ok_or_else(|| AppError::NotFound("No API key found for this user".to_string()))?;

    let api_key_row: Option<(String,)> =
        sqlx::query_as("SELECT api_key FROM api_keys WHERE id = ?")
            .bind(&key_id)
            .fetch_optional(&state.pool)
            .await?;

    let (api_key,) = match api_key_row {
        Some(k) => k,
        None => {
            // Clean up invalid reference
            sqlx::query("UPDATE users SET apiKeyId = NULL WHERE id = ?")
                .bind(&user.id)
                .execute(&state.pool)
                .await?;
            return Err(AppError::NotFound("API key not found".to_string()));
        }
    };

    let usage_stats = get_key_usage(&state.pool, &api_key).await?;
    Ok(HttpResponse::Ok().json(usage_stats))
}

/// GET /logged-in
async fn logged_in(
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

/// GET /proj/allowed
async fn proj_allowed(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    opt_auth: OptionalAuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let user = match opt_auth.0 {
        Some(u) => u,
        None => {
            return Ok(
                HttpResponse::Unauthorized().json(json!({ "allowed": false }))
            );
        }
    };

    if user.username.is_empty() {
        return Ok(
            HttpResponse::Unauthorized().json(json!({ "allowed": false }))
        );
    }

    let row: Option<(bool,)> =
        sqlx::query_as("SELECT is_projallowed FROM users WHERE username = ?")
            .bind(&user.username)
            .fetch_optional(&state.pool)
            .await?;

    match row {
        Some((allowed,)) => {
            if allowed {
                Ok(HttpResponse::Ok().json(json!({ "allowed": true })))
            } else {
                Ok(HttpResponse::Forbidden().json(json!({ "allowed": false })))
            }
        }
        None => Ok(
            HttpResponse::Unauthorized().json(json!({ "allowed": false }))
        ),
    }
}

/// GET /plex/allowed
async fn plex_allowed(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    opt_auth: OptionalAuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let user = match opt_auth.0 {
        Some(u) => u,
        None => {
            return Ok(
                HttpResponse::Unauthorized().json(json!({ "allowed": false }))
            );
        }
    };

    if user.username.is_empty() {
        return Ok(
            HttpResponse::Unauthorized().json(json!({ "allowed": false }))
        );
    }

    let row: Option<(bool,)> =
        sqlx::query_as("SELECT is_plexallowed FROM users WHERE username = ?")
            .bind(&user.username)
            .fetch_optional(&state.pool)
            .await?;

    match row {
        Some((allowed,)) => {
            if allowed {
                Ok(HttpResponse::Ok().json(json!({ "allowed": true })))
            } else {
                Ok(HttpResponse::Forbidden().json(json!({ "allowed": false })))
            }
        }
        None => Ok(
            HttpResponse::Unauthorized().json(json!({ "allowed": false }))
        ),
    }
}

/// POST /refresh-token
async fn refresh_token(
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
async fn logout(
    req: HttpRequest,
    limiter: web::Data<RateLimiter>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    Ok(HttpResponse::Ok()
        .cookie(clear_cookie("userToken"))
        .json(json!({ "message": "Logged out successfully" })))
}

/// POST /purge-logout
async fn purge_logout(
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

/// POST /startserver
async fn start_server(
    req: HttpRequest,
    limiter: web::Data<RateLimiter>,
    _auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let output = tokio::process::Command::new("docker")
        .args(["compose", "up", "-d", "minecraft"])
        .output()
        .await
        .map_err(|e| AppError::Internal(format!("Error starting server: {}", e)))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        log::error!("Error starting server: {}", stderr);
        return Ok(HttpResponse::InternalServerError().json(json!({
            "message": "Error starting server",
            "error": stderr.to_string()
        })));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    log::info!("Server started successfully: {}", stdout);

    Ok(HttpResponse::Ok().json(json!({ "message": "Server started successfully" })))
}

/// GET /fetch-worlds
async fn fetch_worlds(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let worlds_dir = &state.config.seasons_path;

    let entries = std::fs::read_dir(worlds_dir)
        .map_err(|e| AppError::Internal(format!("Error reading worlds directory: {}", e)))?;

    let mut result: Vec<serde_json::Value> = Vec::new();
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().to_string();
        // Match Season_<number>.zip pattern
        if let Some(rest) = file_name.strip_prefix("Season_") {
            if let Some(num_str) = rest.strip_suffix(".zip") {
                if !num_str.is_empty() && num_str.chars().all(|c| c.is_ascii_digit()) {
                    result.push(json!({
                        "path": format!("raindrippy/content/{}", file_name),
                        "name": format!("Season {}", num_str)
                    }));
                }
            }
        }
    }

    Ok(HttpResponse::Ok().json(result))
}

/// POST /uploadskinfile — multipart upload
async fn upload_skin_file(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    mut payload: Multipart,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let mut username: Option<String> = None;
    let mut password: Option<String> = None;
    let mut file_data: Option<Vec<u8>> = None;
    let mut original_filename: Option<String> = None;

    while let Some(item) = payload.next().await {
        let mut field = item.map_err(|e| AppError::BadRequest(format!("Multipart error: {}", e)))?;
        let content_disposition = field.content_disposition().cloned();
        let cd = match content_disposition {
            Some(cd) => cd,
            None => {
                // Drain field with no content disposition
                while let Some(_chunk) = field.next().await {}
                continue;
            }
        };
        let field_name = cd.get_name().unwrap_or("").to_string();

        match field_name.as_str() {
            "username" => {
                let mut buf = Vec::new();
                while let Some(chunk) = field.next().await {
                    let data =
                        chunk.map_err(|e| AppError::BadRequest(format!("Read error: {}", e)))?;
                    buf.extend_from_slice(&data);
                }
                username = Some(String::from_utf8_lossy(&buf).to_string());
            }
            "password" => {
                let mut buf = Vec::new();
                while let Some(chunk) = field.next().await {
                    let data =
                        chunk.map_err(|e| AppError::BadRequest(format!("Read error: {}", e)))?;
                    buf.extend_from_slice(&data);
                }
                password = Some(String::from_utf8_lossy(&buf).to_string());
            }
            "file" => {
                let fname = cd.get_filename().unwrap_or("unknown").to_string();

                // Check file extension
                let ext = Path::new(&fname)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                if ext != "skin" && ext != "skinfile" && ext != "customskin" {
                    return Ok(HttpResponse::BadRequest()
                        .json(json!({ "error": "only .skin or .skinfile allowed" })));
                }

                original_filename = Some(fname);

                let mut buf = Vec::new();
                while let Some(chunk) = field.next().await {
                    let data =
                        chunk.map_err(|e| AppError::BadRequest(format!("Read error: {}", e)))?;
                    buf.extend_from_slice(&data);
                }
                file_data = Some(buf);
            }
            _ => {
                // Drain unknown fields
                while let Some(_chunk) = field.next().await {}
            }
        }
    }

    let username =
        username.ok_or_else(|| AppError::BadRequest("missing info or no file".to_string()))?;
    let password =
        password.ok_or_else(|| AppError::BadRequest("missing info or no file".to_string()))?;
    let file_data =
        file_data.ok_or_else(|| AppError::BadRequest("missing info or no file".to_string()))?;
    let orig_name = original_filename.unwrap_or_else(|| "file".to_string());

    // Verify credentials
    #[derive(sqlx::FromRow)]
    struct UserRow {
        password_hash: String,
    }

    let user: Option<UserRow> =
        sqlx::query_as("SELECT password_hash FROM users WHERE username = ?")
            .bind(&username)
            .fetch_optional(&state.pool)
            .await?;

    let user = match user {
        Some(u) => u,
        None => return Ok(HttpResponse::Unauthorized().json(json!({ "error": "Bad creds" }))),
    };

    let valid = verify(&password, &user.password_hash)?;
    if !valid {
        return Ok(HttpResponse::Unauthorized().json(json!({ "error": "Bad creds" })));
    }

    // Generate unique filename
    let now = chrono::Utc::now().timestamp_millis();
    let rand_part: u64 = rand::thread_rng().gen_range(0..1_000_000_000);
    let ext = Path::new(&orig_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("skin");
    let save_filename = format!("file-{}-{}.{}", now, rand_part, ext);

    // Save to skin path
    let skin_dir = &state.config.minecraft_skin_path;
    let save_path = Path::new(skin_dir).join(&save_filename);
    std::fs::write(&save_path, &file_data)
        .map_err(|e| AppError::Internal(format!("Error saving skin file: {}", e)))?;

    // Log to uploadlogs.json
    let log_path = &state.config.upload_logs_path;
    let mut logs: Vec<UploadLogEntry> = if Path::new(log_path).exists() {
        let content = std::fs::read_to_string(log_path).unwrap_or_else(|_| "[]".to_string());
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        Vec::new()
    };

    logs.push(UploadLogEntry {
        timestamp: Utc::now().to_rfc3339(),
        username: username.clone(),
        file: save_filename.clone(),
    });

    let _ = std::fs::write(
        log_path,
        serde_json::to_string_pretty(&logs).unwrap_or_default(),
    );

    Ok(HttpResponse::Ok().json(json!({
        "message": "skinfile uploaded",
        "file": save_filename
    })))
}

/// POST /delskin
async fn delete_skin(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    body: web::Json<SkinDeleteBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let username = body
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("missing fields vro".to_string()))?;
    let password = body
        .password
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("missing fields vro".to_string()))?;
    let filename = body
        .filename
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("missing fields vro".to_string()))?;

    #[derive(sqlx::FromRow)]
    struct UserRow {
        password_hash: String,
    }

    let user: Option<UserRow> =
        sqlx::query_as("SELECT password_hash FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&state.pool)
            .await?;

    let user = match user {
        Some(u) => u,
        None => {
            return Ok(
                HttpResponse::Unauthorized().json(json!({ "error": "invalid credentials" }))
            );
        }
    };

    let valid = verify(password, &user.password_hash)?;
    if !valid {
        return Ok(
            HttpResponse::Unauthorized().json(json!({ "error": "invalid credentials" }))
        );
    }

    // Read log
    let log_path = &state.config.upload_logs_path;
    let mut logs: Vec<UploadLogEntry> = if Path::new(log_path).exists() {
        let content = std::fs::read_to_string(log_path).unwrap_or_else(|_| "[]".to_string());
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        Vec::new()
    };

    // Check ownership
    let found = logs
        .iter()
        .any(|entry| entry.username == username && entry.file == filename);
    if !found {
        return Ok(
            HttpResponse::Forbidden().json(json!({ "error": "unauthorized file" }))
        );
    }

    // Delete file
    let skin_path =
        Path::new(&state.config.minecraft_skin_path).join(filename);
    if skin_path.exists() {
        std::fs::remove_file(&skin_path)
            .map_err(|e| AppError::Internal(format!("Error deleting file: {}", e)))?;
    }

    // Update log
    logs.retain(|entry| !(entry.username == username && entry.file == filename));
    let _ = std::fs::write(
        log_path,
        serde_json::to_string_pretty(&logs).unwrap_or_default(),
    );

    Ok(HttpResponse::Ok().json(json!({ "message": "skin file deleted" })))
}

/// GET /userskins/{username}
async fn user_skins(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let username = path.into_inner();
    if username.is_empty() {
        return Err(AppError::BadRequest("Username is required".to_string()));
    }

    let log_path = &state.config.upload_logs_path;
    let logs: Vec<UploadLogEntry> = if Path::new(log_path).exists() {
        let content = std::fs::read_to_string(log_path).unwrap_or_else(|_| "[]".to_string());
        serde_json::from_str(&content).unwrap_or_default()
    } else {
        Vec::new()
    };

    let files: Vec<&str> = logs
        .iter()
        .filter(|entry| entry.username == username)
        .map(|entry| entry.file.as_str())
        .collect();

    Ok(HttpResponse::Ok().json(json!({ "files": files })))
}

/// DELETE /admin/delete-user
async fn admin_delete_user(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<DeleteUserBody>,
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

    sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(&user_id)
        .execute(&state.pool)
        .await?;

    sqlx::query("DELETE FROM user_moderation WHERE user_id = ?")
        .bind(&user_id)
        .execute(&state.pool)
        .await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "User deleted successfully" })))
}

/// POST /admin/moderate
async fn admin_moderate(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<ModerateBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    if !auth.is_admin {
        return Err(AppError::Forbidden("Access denied".to_string()));
    }

    let username = body
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing required fields".to_string()))?;
    let ban_type = body
        .ban_type
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing required fields".to_string()))?;

    let user: Option<(String,)> =
        sqlx::query_as("SELECT id FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&state.pool)
            .await?;

    let (user_id,) =
        user.ok_or_else(|| AppError::NotFound("User not found".to_string()))?;

    let now = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
    let mod_note = body.mod_note.as_deref().unwrap_or("");
    let incriminatory_str: Option<String> = body
        .incriminatory
        .as_ref()
        .map(|v| serde_json::to_string(v).unwrap_or_default());

    // Compute expiration and message
    let (expiration, message): (Option<String>, String) = match ban_type {
        "poison" => (None, "Poison ban applied successfully".to_string()),
        "perm" => (
            None,
            "Permanent ban (termination) applied successfully".to_string(),
        ),
        "1d" | "3d" | "7d" | "14d" => {
            let days: i64 = match ban_type {
                "1d" => 1,
                "3d" => 3,
                "7d" => 7,
                "14d" => 14,
                _ => unreachable!(),
            };
            let exp_time = Utc::now() + chrono::Duration::days(days);
            let exp_str = exp_time.format("%Y-%m-%d %H:%M:%S").to_string();
            let msg = format!(
                "{} temporary ban applied successfully (expires: {})",
                ban_type, exp_str
            );
            (Some(exp_str), msg)
        }
        _ => {
            return Err(AppError::BadRequest(format!(
                "Invalid moderation type: {}",
                ban_type
            )));
        }
    };

    // Upsert moderation
    let existing: Option<(i64,)> =
        sqlx::query_as("SELECT id FROM user_moderation WHERE user_id = ?")
            .bind(&user_id)
            .fetch_optional(&state.pool)
            .await?;

    if existing.is_some() {
        sqlx::query(
            "UPDATE user_moderation SET type = ?, mod_note = ?, moderated_at = ?, expires_at = ?, created_by = ?, incriminatory = ? WHERE user_id = ?",
        )
        .bind(ban_type)
        .bind(mod_note)
        .bind(&now)
        .bind(&expiration)
        .bind(&auth.username)
        .bind(&incriminatory_str)
        .bind(&user_id)
        .execute(&state.pool)
        .await?;
    } else {
        sqlx::query(
            "INSERT INTO user_moderation (user_id, type, mod_note, moderated_at, expires_at, created_by, incriminatory) VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&user_id)
        .bind(ban_type)
        .bind(mod_note)
        .bind(&now)
        .bind(&expiration)
        .bind(&auth.username)
        .bind(&incriminatory_str)
        .execute(&state.pool)
        .await?;
    }

    // Update authedPlayers.json file
    let moderation_data = json!({
        "type": ban_type,
        "modNote": mod_note,
        "moderatedAt": now,
        "expiresAt": expiration,
        "incriminatory": incriminatory_str.as_deref().and_then(|s| serde_json::from_str::<serde_json::Value>(s).ok())
    });

    let un = username.to_string();
    let md = moderation_data.clone();
    let ap = state.config.authed_players_path.clone();
    tokio::task::spawn_blocking(move || {
        update_authed_players_file(&ap, |players| {
            if let Some(player) = players
                .iter_mut()
                .find(|p| p.get("username").and_then(|v| v.as_str()) == Some(&un))
            {
                player["moderation"] = md;
            }
        });
    })
    .await
    .ok();

    // Handle admin toggle
    if let Some(make_admin) = body.make_admin {
        sqlx::query("UPDATE users SET is_admin = ? WHERE id = ?")
            .bind(make_admin)
            .bind(&user_id)
            .execute(&state.pool)
            .await?;
        log::info!("Admin status changed for {}: {}", username, make_admin);
    }

    log::info!("Moderation applied: {}", message);
    Ok(HttpResponse::Ok().json(json!({ "message": message })))
}

/// POST /admin/unban
async fn admin_unban(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<UnbanBody>,
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

    sqlx::query("DELETE FROM user_moderation WHERE user_id = ? AND type != 'poison'")
        .bind(&user_id)
        .execute(&state.pool)
        .await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "User unbanned / moderation cleared" })))
}

/// GET /api/moderation-list
async fn moderation_list(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    if !auth.is_admin {
        return Err(AppError::Forbidden("Access denied".to_string()));
    }

    #[derive(sqlx::FromRow, Serialize)]
    struct ModerationListRow {
        username: String,
        #[sqlx(rename = "type")]
        #[serde(rename = "type")]
        mod_type: String,
        mod_note: Option<String>,
        moderated_at: String,
        created_by: String,
    }

    let rows: Vec<ModerationListRow> = sqlx::query_as(
        r#"SELECT
            u.username,
            m.type,
            m.mod_note,
                CAST(m.moderated_at AS CHAR) AS moderated_at,
            m.created_by
        FROM user_moderation m
        JOIN users u ON m.user_id = u.id
        ORDER BY m.moderated_at DESC
        LIMIT 100"#,
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

/// GET /admin/list-users
async fn admin_list_users(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    query: web::Query<ListUsersQuery>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    if !auth.is_admin {
        return Err(AppError::Forbidden("Access denied".to_string()));
    }

    let search = match &query.search {
        Some(s) if !s.is_empty() => format!("%{}%", s),
        _ => "%".to_string(),
    };
    let page = query.page.unwrap_or(1).max(1);
    let limit: u32 = 10;
    let offset = (page - 1) * limit;

    #[derive(sqlx::FromRow)]
    struct UserListRow {
        username: String,
        is_admin: bool,
        is_member: bool,
        is_projallowed: bool,
        is_plexallowed: bool,
        moderation_type: Option<String>,
    }

    let users: Vec<UserListRow> = sqlx::query_as(
        r#"SELECT u.id, u.username, u.is_admin, u.is_member, u.is_projallowed, u.is_plexallowed,
            m.type AS moderation_type
        FROM users u
        LEFT JOIN (
            SELECT user_id, type
            FROM user_moderation
            WHERE moderated_at = (
                SELECT MAX(moderated_at)
                FROM user_moderation m2
                WHERE m2.user_id = user_moderation.user_id
            )
        ) m ON u.id = m.user_id
        WHERE u.username LIKE ?
        ORDER BY u.username ASC
        LIMIT ? OFFSET ?"#,
    )
    .bind(&search)
    .bind(limit)
    .bind(offset)
    .fetch_all(&state.pool)
    .await?;

    let result: Vec<serde_json::Value> = users
        .iter()
        .map(|u| {
            json!({
                "username": u.username,
                "accountStatus": if u.moderation_type.is_some() { "moderated" } else { "ok" },
                "isAdmin": u.is_admin,
                "isMember": u.is_member,
                "isProjAllowed": u.is_projallowed,
                "isPlexAllowed": u.is_plexallowed
            })
        })
        .collect();

    Ok(HttpResponse::Ok().json(result))
}

/// PATCH /admin/update-member-status
async fn admin_update_member_status(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<UpdateMemberStatusBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    if !auth.is_admin {
        return Err(AppError::Forbidden("Access denied".to_string()));
    }

    let username = body
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(
                "Invalid request body. Username and isMember are required.".to_string(),
            )
        })?;
    let is_member = body.is_member.ok_or_else(|| {
        AppError::BadRequest(
            "Invalid request body. Username and isMember are required.".to_string(),
        )
    })?;

    let result = sqlx::query("UPDATE users SET is_member = ? WHERE username = ?")
        .bind(is_member)
        .bind(username)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("User not found".to_string()));
    }

    Ok(HttpResponse::Ok().json(json!({ "message": "User member status updated successfully" })))
}

/// PATCH /admin/update-proj-status
async fn admin_update_proj_status(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<UpdateProjStatusBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    if !auth.is_admin {
        return Err(AppError::Forbidden("Access denied".to_string()));
    }

    let username = body
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(
                "Invalid request body. Username and isAllowed are required.".to_string(),
            )
        })?;
    let is_allowed = body.is_allowed.ok_or_else(|| {
        AppError::BadRequest(
            "Invalid request body. Username and isAllowed are required.".to_string(),
        )
    })?;

    let result = sqlx::query("UPDATE users SET is_projallowed = ? WHERE username = ?")
        .bind(is_allowed)
        .bind(username)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("User not found".to_string()));
    }

    Ok(HttpResponse::Ok().json(json!({ "message": "User project status updated successfully" })))
}

/// PATCH /admin/update-plex-status
async fn admin_update_plex_status(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<UpdatePlexStatusBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    if !auth.is_admin {
        return Err(AppError::Forbidden("Access denied".to_string()));
    }

    let username = body
        .username
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(
                "Invalid request body. Username and isAllowed are required.".to_string(),
            )
        })?;
    let is_allowed = body.is_allowed.ok_or_else(|| {
        AppError::BadRequest(
            "Invalid request body. Username and isAllowed are required.".to_string(),
        )
    })?;

    let result = sqlx::query("UPDATE users SET is_plexallowed = ? WHERE username = ?")
        .bind(is_allowed)
        .bind(username)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("User not found".to_string()));
    }

    Ok(HttpResponse::Ok().json(json!({ "message": "User Plex status updated successfully" })))
}

/// POST /admin/gen-pwd-reset
async fn admin_gen_pwd_reset(
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

/// POST /reset-password
async fn reset_password(
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

/// GET /getGames
/// GET /getGames
async fn get_games(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let mut game_map = HashMap::new();
    
    // Get paths from environment variables (fallback to common defaults)
    let local_games_dir = std::env::var("GAMES_DIR")
        .unwrap_or_else(|_| "/home/useradmin/api/mainapi/games".to_string());
    
    let nginx_base_url = std::env::var("GAMES_URL")
        .unwrap_or_else(|_| "https://bakosmp.go.ro/games".to_string());

    // Iterate through the games directory
    if let Ok(entries) = std::fs::read_dir(&local_games_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            
            if path.is_dir() {
                if let Some(folder_name) = path.file_name().and_then(|n| n.to_str()) {
                    
                    // 1. Read DESCRIPTION.txt
                    let desc_path = path.join("DESCRIPTION.txt");
                    let description = std::fs::read_to_string(desc_path)
                        .unwrap_or_else(|_| "No description provided.".to_string());
                    
                    // 2. Detect the .zip file
                    let mut zip_filename = format!("{}.zip", folder_name); 
                    if let Ok(dir_files) = std::fs::read_dir(&path) {
                        for file in dir_files.flatten() {
                            let fname = file.file_name().to_string_lossy().to_string();
                            if fname.to_lowercase().ends_with(".zip") {
                                zip_filename = fname;
                                break; 
                            }
                        }
                    }

                    // 3. Metadata Heuristics
                    let title = folder_name.replace("_", " ");
                    let executable = format!("{}.exe", folder_name); 
                    
                    // Check for icon.png, otherwise default to a root-level default
                    let icon_name = if path.join("icon.png").exists() {
                        "icon.png"
                    } else {
                        "default.png"
                    };

                    // 4. Build the GameDetails object
                    game_map.insert(
                        folder_name.to_string(),
                        GameDetails {
                            title,
                            description: description.trim().to_string(),
                            image: format!("{}/{}/{}", nginx_base_url, folder_name, icon_name),
                            file: format!("{}/{}/{}", nginx_base_url, folder_name, zip_filename),
                            executable,
                        },
                    );
                }
            }
        }
    } else {
        log::error!("Could not read games directory at: {}", local_games_dir);
    }

    // Returns [ { "game_id": { ...details } } ]
    Ok(HttpResponse::Ok().json(vec![game_map]))
}

/// POST /forgot-password
async fn forgot_password(
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

// ─── Dashboard handlers ──────────────────────────────────────────────────────

pub async fn serve_dashboard(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> HttpResponse {
    if let Err(resp) = check_status_dashboard_auth(&req, &state).await {
        return resp;
    }

    let path = format!("{}/dashboard.html", state.config.private_dir);
    match std::fs::read_to_string(&path) {
        Ok(content) => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(content),
        Err(e) => {
            log::error!("Error reading dashboard.html: {}", e);
            HttpResponse::InternalServerError().body("Error loading dashboard")
        }
    }
}

pub async fn serve_rdadmin(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> HttpResponse {
    if let Err(resp) = check_rd_admin_auth(&req, &state).await {
        return resp;
    }

    let path = format!("{}/rdadmin.html", state.config.private_dir);
    match std::fs::read_to_string(&path) {
        Ok(content) => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(content),
        Err(e) => {
            log::error!("Error reading rdadmin.html: {}", e);
            HttpResponse::InternalServerError().body("Error loading admin panel")
        }
    }
}

// ─── Route configuration ─────────────────────────────────────────────────────

pub fn configure(cfg: &mut web::ServiceConfig) {
    // Auth routes are registered directly (no sub-scope) because in the
    // original Node.js they are mounted at /api (the parent scope).
    cfg.route("/register", web::post().to(register))
        .route("/login", web::post().to(login))
        .route("/v-creds", web::post().to(validate_credentials))
        .route("/update-admin-status", web::post().to(update_admin_status))
        .route("/validate", web::get().to(validate))
        .route("/account-status", web::get().to(account_status))
        .route("/account-data", web::get().to(account_data))
        .route("/delete-account", web::delete().to(delete_account))
        .route("/accstatus-cuser", web::get().to(accstatus_cuser))
        .route("/get-key", web::get().to(get_key))
        .route("/profile", web::post().to(profile))
        .route("/api-usage", web::get().to(api_usage))
        .route("/logged-in", web::get().to(logged_in))
        .route("/proj/allowed", web::get().to(proj_allowed))
        .route("/getGames", web::get().to(get_games))
        .route("/plex/allowed", web::get().to(plex_allowed))
        .route("/refresh-token", web::post().to(refresh_token))
        .route("/logout", web::post().to(logout))
        .route("/purge-logout", web::post().to(purge_logout))
        .route("/startserver", web::post().to(start_server))
        .route("/fetch-worlds", web::get().to(fetch_worlds))
        .route("/uploadskinfile", web::post().to(upload_skin_file))
        .route("/delskin", web::post().to(delete_skin))
        .route("/userskins/{username}", web::get().to(user_skins))
        .route("/admin/delete-user", web::delete().to(admin_delete_user))
        .route("/admin/moderate", web::post().to(admin_moderate))
        .route("/admin/unban", web::post().to(admin_unban))
        .route("/api/moderation-list", web::get().to(moderation_list))
        .route("/admin/list-users", web::get().to(admin_list_users))
        .route(
            "/admin/update-member-status",
            web::patch().to(admin_update_member_status),
        )
        .route(
            "/admin/update-proj-status",
            web::patch().to(admin_update_proj_status),
        )
        .route(
            "/admin/update-plex-status",
            web::patch().to(admin_update_plex_status),
        )
        .route("/admin/gen-pwd-reset", web::post().to(admin_gen_pwd_reset))
        .route("/reset-password", web::post().to(reset_password))
        .route("/forgot-password", web::post().to(forgot_password));
}
