use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::middleware::api_key_auth::get_key_usage;
use crate::middleware::auth::{decode_jwt, AuthUser};
use crate::middleware::auth_failopen::OptionalAuthUser;
use crate::AppState;
use crate::middleware::rate_limit::RateLimiter;

use super::common::*;

#[derive(Debug, Deserialize)]
pub struct AccStatusQuery {
    pub username: Option<String>,
}

/// GET /account-status
pub(super) async fn account_status(
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
    let user: Option<(String, bool, bool)> =
        sqlx::query_as("SELECT id, is_og, is_admin FROM users WHERE username = ?")
            .bind(&claims.username)
            .fetch_optional(&state.pool)
            .await?;

    let (user_id, is_og, is_admin) = match user {
        Some(u) => u,
        None => return Ok(HttpResponse::Ok().json(json!({ "accountStatus": null, "is_og": false, "is_admin": false }))),
    };

    // Check moderation
    #[derive(sqlx::FromRow)]
    struct ModerationRow {
        #[sqlx(rename = "type")]
        mod_type: String,
        moderated_at: String,
        mod_note: Option<String>,
        incriminatory: Option<serde_json::Value>,
    }

    let moderation: Option<ModerationRow> = sqlx::query_as(
            "SELECT type, CAST(moderated_at AS CHAR) AS moderated_at, mod_note, incriminatory FROM user_moderation WHERE user_id = ? ORDER BY moderated_at DESC LIMIT 1",
    )
    .bind(&user_id)
    .fetch_optional(&state.pool)
    .await?;

    let moderation = match moderation {
        Some(m) => m,
        None => return Ok(HttpResponse::Ok().json(json!({ "accountStatus": "ok", "is_og": is_og, "is_admin": is_admin }))),
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

    let incriminatory_val = moderation.incriminatory;

    Ok(HttpResponse::Forbidden().json(json!({
        "accountStatus": "moderated",
        "banInfo": {
            "type": moderation.mod_type,
            "moderatedTimePDT": moderation.moderated_at,
            "modNote": moderation.mod_note,
            "incriminatory": incriminatory_val,
            "is_og": is_og,
            "is_admin": is_admin
        }
    })))
}

/// GET /account-data
pub(super) async fn account_data(
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
pub(super) async fn delete_account(
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
pub(super) async fn accstatus_cuser(
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
        incriminatory: Option<serde_json::Value>,
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

            let incriminatory_val = ban.incriminatory;

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
pub(super) async fn get_key(
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
pub(super) async fn profile(
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
pub(super) async fn api_usage(
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

