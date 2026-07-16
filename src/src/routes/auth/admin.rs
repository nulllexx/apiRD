use actix_web::{web, HttpRequest, HttpResponse};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::error::AppError;
use crate::middleware::auth::AuthUser;
use crate::AppState;
use crate::middleware::rate_limit::RateLimiter;

use super::common::*;

#[derive(Debug, Deserialize)]
pub struct DeleteUserBody {
    pub username: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAdminStatusBody {
    pub username: String,
    #[serde(rename = "isAdmin")]
    pub is_admin: bool,
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
pub struct UpdateOgStatusBody {
    pub username: Option<String>,
    #[serde(rename = "isOG")]
    pub is_og: Option<bool>,
}

/// POST /update-admin-status
pub(super) async fn update_admin_status(
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

/// DELETE /admin/delete-user
pub(super) async fn admin_delete_user(
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
pub(super) async fn admin_moderate(
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
pub(super) async fn admin_unban(
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
pub(super) async fn moderation_list(
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
pub(super) async fn admin_list_users(
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
        is_og: bool,
        moderation_type: Option<String>,
    }

    let users: Vec<UserListRow> = sqlx::query_as(
        r#"SELECT u.id, u.username, u.is_admin, u.is_member, u.is_projallowed, u.is_plexallowed, u.is_og,
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
                "isPlexAllowed": u.is_plexallowed,
                "isOG": u.is_og
            })
        })
        .collect();

    Ok(HttpResponse::Ok().json(result))
}

/// PATCH /admin/update-member-status
pub(super) async fn admin_update_member_status(
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
pub(super) async fn admin_update_proj_status(
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
pub(super) async fn admin_update_plex_status(
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

/// PATCH /admin/update-og-status
pub(super) async fn admin_update_og_status(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<UpdateOgStatusBody>,
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
                "Invalid request body. Username and isOG are required.".to_string(),
            )
        })?;
    let is_og = body.is_og.ok_or_else(|| {
        AppError::BadRequest(
            "Invalid request body. Username and isOG are required.".to_string(),
        )
    })?;

    let result = sqlx::query("UPDATE users SET is_og = ? WHERE username = ?")
        .bind(is_og)
        .bind(username)
        .execute(&state.pool)
        .await?;

    if result.rows_affected() == 0 {
        return Err(AppError::NotFound("User not found".to_string()));
    }

    Ok(HttpResponse::Ok().json(json!({ "message": "User OG status updated successfully" })))
}

