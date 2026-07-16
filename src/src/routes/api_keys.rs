use actix_web::{web, HttpResponse};
use uuid::Uuid;

use crate::error::AppError;
use crate::middleware::admin_auth::AdminUser;
use crate::AppState;

#[derive(serde::Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: Option<String>,
    #[serde(rename = "hourlyLimit")]
    pub hourly_limit: Option<i32>,
}

/// POST /api/keys — Create new API key (admin only)
async fn create_api_key(
    state: web::Data<AppState>,
    _admin: AdminUser,
    body: web::Json<CreateApiKeyRequest>,
) -> Result<HttpResponse, AppError> {
    let name = body
        .name
        .as_deref()
        .filter(|n| !n.is_empty())
        .ok_or_else(|| AppError::BadRequest("Name is required".to_string()))?;

    let id = Uuid::new_v4().to_string();
    let api_key = Uuid::new_v4().to_string();
    let hourly_limit = body.hourly_limit.unwrap_or(100);

    sqlx::query(
        "INSERT INTO api_keys (id, name, api_key, hourly_limit) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(name)
    .bind(&api_key)
    .bind(hourly_limit)
    .execute(&state.pool)
    .await?;

    Ok(HttpResponse::Created().json(serde_json::json!({
        "id": id,
        "name": name,
        "apiKey": api_key,
        "hourlyLimit": hourly_limit
    })))
}

/// GET /api/keys — List all API keys (admin only)
async fn list_api_keys(
    state: web::Data<AppState>,
    _admin: AdminUser,
) -> Result<HttpResponse, AppError> {
    let keys: Vec<crate::models::api_key::ApiKey> = sqlx::query_as(
        "SELECT id, name, api_key, hourly_limit, created_at, last_reset, request_count FROM api_keys ORDER BY created_at DESC",
    )
    .fetch_all(&state.pool)
    .await?;

    Ok(HttpResponse::Ok().json(keys))
}

/// DELETE /api/keys/{id} — Delete API key (admin only)
async fn delete_api_key(
    state: web::Data<AppState>,
    _admin: AdminUser,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let id = path.into_inner();

    sqlx::query("DELETE FROM api_keys WHERE id = ?")
        .bind(&id)
        .execute(&state.pool)
        .await?;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "message": "API key deleted successfully"
    })))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/keys")
            .route("", web::post().to(create_api_key))
            .route("", web::get().to(list_api_keys))
            .route("/{id}", web::delete().to(delete_api_key)),
    );
}
