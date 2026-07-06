use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;
use crate::middleware::auth::AuthUser;
use crate::middleware::rate_limit::RateLimiter;
use crate::AppState;

use super::common::*;

#[derive(Debug, Deserialize)]
pub struct HistoryWikiEditBody {
    pub slug: Option<String>,
    pub edited_content: Option<String>,
}

/// PATCH /history/wiki/edit
pub(super) async fn history_wiki_edit(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    auth: AuthUser,
    body: web::Json<HistoryWikiEditBody>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;
    // Check if the user is an OG / admin
    if !auth.is_og && !auth.is_admin {
        return Err(AppError::Forbidden("Access denied".to_string()));
    }
    // Check if it contains the edited_content field
    let edited_content = body
        .edited_content
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("Missing edited_content field".to_string()))?;
    // Update the content in the db
    sqlx::query("UPDATE history_wiki SET content = ? WHERE slug = 'main'")
        .bind(edited_content)
        .execute(&state.pool)
        .await?;
    Ok(HttpResponse::Ok().body("Content updated successfully"))
}

/// GET /history/wiki/view
pub(super) async fn history_wiki_view(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;
    // Get the content from the db
    let row: Option<(String, chrono::DateTime<chrono::Utc>)> = sqlx::query_as("SELECT content, updated_at FROM history_wiki WHERE slug = 'main'")
        .fetch_optional(&state.pool)
        .await?;
    let (content, updated_at) = row.ok_or_else(|| AppError::NotFound("Content not found".to_string()))?;
    
    let iso = updated_at.to_rfc3339();
    let response = json!({
        "content": content,
        "updated_at": iso
    });
    Ok(HttpResponse::Ok().json(response))
}

