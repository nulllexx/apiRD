use actix_multipart::Multipart;
use bcrypt::verify;
use chrono::Utc;
use actix_web::{web, HttpRequest, HttpResponse};
use futures_util::StreamExt;
use rand::Rng;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::Path;

use crate::error::AppError;
use crate::AppState;
use crate::middleware::rate_limit::RateLimiter;

use super::common::*;

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

/// POST /uploadskinfile — multipart upload
pub(super) async fn upload_skin_file(
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
pub(super) async fn delete_skin(
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
pub(super) async fn user_skins(
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

