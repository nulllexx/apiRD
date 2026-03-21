use actix_multipart::Multipart;
use actix_web::{web, HttpResponse};
use futures_util::StreamExt;
use serde_json::json;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::error::AppError;
use crate::middleware::auth::AuthUser;
use crate::models::project::{
    CreateProjectRequest, UpdateProjectRequest, MAX_BYTES_PER_USER, MAX_PROJECTS_PER_USER,
};
use crate::AppState;

// ---------------------------------------------------------------------------
// Helper: look up user row and enforce is_projallowed
// ---------------------------------------------------------------------------

#[derive(Debug, sqlx::FromRow)]
struct UserRow {
    id: String,
    username: String,
    is_projallowed: bool,
}

async fn get_user_by_req(
    pool: &sqlx::MySqlPool,
    auth: &AuthUser,
) -> Result<UserRow, AppError> {
    let row: Option<UserRow> = sqlx::query_as(
        "SELECT id, username, is_projallowed FROM users WHERE username = ?",
    )
    .bind(&auth.username)
    .fetch_optional(pool)
    .await?;

    row.ok_or_else(|| AppError::NotFound("User not found".to_string()))
}

// ---------------------------------------------------------------------------
// Helper: total bytes used by a user across all project files
// ---------------------------------------------------------------------------

async fn get_user_usage_bytes(pool: &sqlx::MySqlPool, user_id: &str) -> Result<i64, AppError> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COALESCE(SUM(f.size), 0) FROM project_files f JOIN projects p ON f.project_id = p.id WHERE p.user_id = ?",
    )
    .bind(user_id)
    .fetch_one(pool)
    .await?;

    Ok(row.0)
}

// ---------------------------------------------------------------------------
// Helper: scan user dir for the sub-directory whose metadata.json matches projectId
// ---------------------------------------------------------------------------

async fn find_project_dir_for_user(
    storage_base: &str,
    username: &str,
    project_id: &str,
) -> Option<PathBuf> {
    let user_dir = Path::new(storage_base).join(username);
    let mut entries = match tokio::fs::read_dir(&user_dir).await {
        Ok(e) => e,
        Err(_) => return None,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let meta_path = path.join("metadata.json");
        if let Ok(raw) = tokio::fs::read_to_string(&meta_path).await {
            if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&raw) {
                if meta.get("projectId").and_then(|v| v.as_str()) == Some(project_id) {
                    return Some(path);
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Helper: find the next numbered sub-directory index for a user
// ---------------------------------------------------------------------------

async fn next_user_project_index(storage_base: &str, username: &str) -> u64 {
    let user_dir = Path::new(storage_base).join(username);
    let mut entries = match tokio::fs::read_dir(&user_dir).await {
        Ok(e) => e,
        Err(_) => return 1,
    };

    let mut max: u64 = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(ft) = entry.file_type().await {
            if ft.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Ok(n) = name.parse::<u64>() {
                        if n > max {
                            max = n;
                        }
                    }
                }
            }
        }
    }

    if max == 0 { 1 } else { max + 1 }
}

// ---------------------------------------------------------------------------
// Helper: move file with EXDEV fallback (cross-device copy + unlink)
// ---------------------------------------------------------------------------

async fn move_file(src: &Path, dest: &Path) -> Result<(), AppError> {
    if let Some(parent) = dest.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    match tokio::fs::rename(src, dest).await {
        Ok(()) => Ok(()),
        Err(e) => {
            // On cross-device or unsupported rename, fall back to copy+remove
            if e.raw_os_error() == Some(18) // EXDEV on Linux
                || e.kind() == std::io::ErrorKind::Other
                || e.kind() == std::io::ErrorKind::Unsupported
                || e.kind() == std::io::ErrorKind::PermissionDenied
            {
                tokio::fs::copy(src, dest).await?;
                let _ = tokio::fs::remove_file(src).await;
                Ok(())
            } else {
                Err(AppError::Internal(format!("Failed to move file: {}", e)))
            }
        }
    }
}

// ===========================================================================
// 1. POST /  — create project
// ===========================================================================

async fn create_project(
    state: web::Data<AppState>,
    auth: AuthUser,
    body: web::Json<CreateProjectRequest>,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    if !user.is_projallowed {
        return Err(AppError::Forbidden(
            "Project creation not allowed".to_string(),
        ));
    }

    if body.name.is_empty() {
        return Err(AppError::BadRequest("Missing project name".to_string()));
    }

    // Enforce project limit
    let (count,): (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM projects WHERE user_id = ?")
            .bind(&user.id)
            .fetch_one(&state.pool)
            .await?;

    if count >= MAX_PROJECTS_PER_USER {
        return Err(AppError::Forbidden(format!(
            "Max projects ({}) reached",
            MAX_PROJECTS_PER_USER
        )));
    }

    let project_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    sqlx::query(
        "INSERT INTO projects (id, user_id, name, description, created_at) VALUES (?, ?, ?, ?, ?)",
    )
    .bind(&project_id)
    .bind(&user.id)
    .bind(&body.name)
    .bind(&body.description)
    .bind(&now)
    .execute(&state.pool)
    .await?;

    // Create filesystem directory
    let user_dir = Path::new(&state.config.storage_base).join(&user.username);
    tokio::fs::create_dir_all(&user_dir).await?;

    let index = next_user_project_index(&state.config.storage_base, &user.username).await;
    let project_dir = user_dir.join(index.to_string());
    tokio::fs::create_dir_all(&project_dir).await?;

    // Write metadata.json
    let metadata = json!({
        "projectId": project_id,
        "name": body.name,
        "description": body.description,
        "createdAt": chrono::Utc::now().to_rfc3339(),
    });
    tokio::fs::write(
        project_dir.join("metadata.json"),
        serde_json::to_string_pretty(&metadata).unwrap_or_default(),
    )
    .await?;

    Ok(HttpResponse::Created().json(json!({
        "id": project_id,
        "storageIndex": index,
    })))
}

// ===========================================================================
// 2. GET /  — list user projects
// ===========================================================================

async fn list_projects(
    state: web::Data<AppState>,
    auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;

    let rows: Vec<crate::models::project::Project> = sqlx::query_as(
        "SELECT id, user_id, name, description, created_at FROM projects WHERE user_id = ? ORDER BY created_at DESC",
    )
    .bind(&user.id)
    .fetch_all(&state.pool)
    .await?;

    Ok(HttpResponse::Ok().json(rows))
}

// ===========================================================================
// 3. GET /quota  — user storage quota
// ===========================================================================

async fn get_quota(
    state: web::Data<AppState>,
    auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    let used = get_user_usage_bytes(&state.pool, &user.id).await?;

    Ok(HttpResponse::Ok().json(json!({
        "usedBytes": used,
        "maxBytes": MAX_BYTES_PER_USER,
        "remainingBytes": std::cmp::max(0, MAX_BYTES_PER_USER - used),
    })))
}

// ===========================================================================
// 4. GET /{id}  — project details + file list
// ===========================================================================

async fn get_project(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    let project_id = path.into_inner();

    let project: Option<crate::models::project::Project> = sqlx::query_as(
        "SELECT id, user_id, name, description, created_at FROM projects WHERE id = ? AND user_id = ?",
    )
    .bind(&project_id)
    .bind(&user.id)
    .fetch_optional(&state.pool)
    .await?;

    let project = project.ok_or_else(|| AppError::NotFound("Project not found".to_string()))?;

    let files: Vec<crate::models::project::ProjectFile> = sqlx::query_as(
        "SELECT id, project_id, filename, original_name, mime, size, path, uploaded_at FROM project_files WHERE project_id = ? ORDER BY uploaded_at DESC",
    )
    .bind(&project_id)
    .fetch_all(&state.pool)
    .await?;

    // Return only safe fields for files (exclude internal path)
    let file_infos: Vec<serde_json::Value> = files
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "original_name": f.original_name,
                "mime": f.mime,
                "size": f.size,
                "uploaded_at": f.uploaded_at,
            })
        })
        .collect();

    Ok(HttpResponse::Ok().json(json!({
        "project": project,
        "files": file_infos,
    })))
}

// ===========================================================================
// 5. PATCH /{id}  — update project name/description
// ===========================================================================

async fn update_project(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<String>,
    body: web::Json<UpdateProjectRequest>,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    let project_id = path.into_inner();

    if body.name.is_none() && body.description.is_none() {
        return Err(AppError::BadRequest("Nothing to update".to_string()));
    }

    let proj: Option<(String,)> =
        sqlx::query_as("SELECT id FROM projects WHERE id = ? AND user_id = ?")
            .bind(&project_id)
            .bind(&user.id)
            .fetch_optional(&state.pool)
            .await?;

    if proj.is_none() {
        return Err(AppError::NotFound("Project not found".to_string()));
    }

    sqlx::query(
        "UPDATE projects SET name = COALESCE(?, name), description = COALESCE(?, description) WHERE id = ?",
    )
    .bind(&body.name)
    .bind(&body.description)
    .bind(&project_id)
    .execute(&state.pool)
    .await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "Project updated" })))
}

// ===========================================================================
// 6. DELETE /{id}  — delete project + filesystem
// ===========================================================================

async fn delete_project(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    let project_id = path.into_inner();

    let proj: Option<(String,)> =
        sqlx::query_as("SELECT id FROM projects WHERE id = ? AND user_id = ?")
            .bind(&project_id)
            .bind(&user.id)
            .fetch_optional(&state.pool)
            .await?;

    if proj.is_none() {
        return Err(AppError::NotFound("Project not found".to_string()));
    }

    // Remove filesystem folder if present
    if let Some(project_dir) =
        find_project_dir_for_user(&state.config.storage_base, &user.username, &project_id).await
    {
        let _ = tokio::fs::remove_dir_all(&project_dir).await;
    }

    // Delete DB rows (files cascade or delete manually)
    sqlx::query("DELETE FROM project_files WHERE project_id = ?")
        .bind(&project_id)
        .execute(&state.pool)
        .await?;

    sqlx::query("DELETE FROM projects WHERE id = ?")
        .bind(&project_id)
        .execute(&state.pool)
        .await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "Project deleted" })))
}

// ===========================================================================
// 7. POST /{id}/files  — multipart file upload
// ===========================================================================

async fn upload_file(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<String>,
    mut payload: Multipart,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    let project_id = path.into_inner();

    // Verify project belongs to user
    let proj: Option<(String,)> =
        sqlx::query_as("SELECT id FROM projects WHERE id = ? AND user_id = ?")
            .bind(&project_id)
            .bind(&user.id)
            .fetch_optional(&state.pool)
            .await?;

    if proj.is_none() {
        return Err(AppError::NotFound("Project not found".to_string()));
    }

    let project_dir =
        find_project_dir_for_user(&state.config.storage_base, &user.username, &project_id).await;
    let project_dir = project_dir.ok_or_else(|| {
        AppError::NotFound("Project storage folder not found".to_string())
    })?;

    // Read the first file field from multipart
    let mut original_name = String::new();
    let mut mime_type: Option<String> = None;
    let mut temp_path: Option<PathBuf> = None;

    while let Some(Ok(mut field)) = payload.next().await {
        let filename = field
            .content_disposition()
            .and_then(|cd| cd.get_filename().map(|s| s.to_string()))
            .unwrap_or_else(|| "upload".to_string());

        original_name = filename.clone();
        mime_type = field.content_type().map(|m| m.to_string());

        // Save to temp directory
        let tmp_dir = Path::new(&state.config.upload_tmp);
        tokio::fs::create_dir_all(tmp_dir).await?;

        let tmp_name = format!("{}-{}", Uuid::new_v4(), sanitize_filename::sanitize(&filename));
        let tmp_file = tmp_dir.join(&tmp_name);

        let mut bytes_written: usize = 0;
        let mut file_data = Vec::new();
        while let Some(chunk) = field.next().await {
            let chunk = chunk.map_err(|e| {
                AppError::Internal(format!("Multipart read error: {}", e))
            })?;
            bytes_written += chunk.len();
            file_data.extend_from_slice(&chunk);
        }

        tokio::fs::write(&tmp_file, &file_data).await?;
        temp_path = Some(tmp_file);

        // Only process the first file
        let _ = bytes_written;
        break;
    }

    let temp_path = match temp_path {
        Some(p) => p,
        None => return Err(AppError::BadRequest("No file uploaded".to_string())),
    };

    let file_size = tokio::fs::metadata(&temp_path).await?.len() as i64;

    // Quota check
    let used = get_user_usage_bytes(&state.pool, &user.id).await?;
    if used + file_size > MAX_BYTES_PER_USER {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(AppError::PayloadTooLarge(
            "Upload would exceed your 1GB quota".to_string(),
        ));
    }

    // Move to project directory
    let safe_name = sanitize_filename::sanitize(&original_name);
    let stored_name = format!(
        "{}-{}-{}",
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u32>(),
        safe_name
    );
    let dest_path = project_dir.join(&stored_name);
    move_file(&temp_path, &dest_path).await?;

    // Insert DB row
    let file_id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    sqlx::query(
        "INSERT INTO project_files (id, project_id, filename, original_name, mime, size, path, uploaded_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&file_id)
    .bind(&project_id)
    .bind(&stored_name)
    .bind(&original_name)
    .bind(&mime_type)
    .bind(file_size)
    .bind(dest_path.to_string_lossy().as_ref())
    .bind(&now)
    .execute(&state.pool)
    .await?;

    Ok(HttpResponse::Ok().json(json!({
        "id": file_id,
        "originalName": original_name,
        "mime": mime_type,
        "size": file_size,
        "uploadedAt": chrono::Utc::now().to_rfc3339(),
    })))
}

// ===========================================================================
// 8. GET /{id}/files  — list project files
// ===========================================================================

async fn list_files(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    let project_id = path.into_inner();

    let proj: Option<(String,)> =
        sqlx::query_as("SELECT id FROM projects WHERE id = ? AND user_id = ?")
            .bind(&project_id)
            .bind(&user.id)
            .fetch_optional(&state.pool)
            .await?;

    if proj.is_none() {
        return Err(AppError::NotFound("Project not found".to_string()));
    }

    let files: Vec<crate::models::project::ProjectFile> = sqlx::query_as(
        "SELECT id, project_id, filename, original_name, mime, size, path, uploaded_at FROM project_files WHERE project_id = ? ORDER BY uploaded_at DESC",
    )
    .bind(&project_id)
    .fetch_all(&state.pool)
    .await?;

    let file_infos: Vec<serde_json::Value> = files
        .iter()
        .map(|f| {
            json!({
                "id": f.id,
                "original_name": f.original_name,
                "mime": f.mime,
                "size": f.size,
                "uploaded_at": f.uploaded_at,
            })
        })
        .collect();

    Ok(HttpResponse::Ok().json(file_infos))
}

// ===========================================================================
// 9. GET /{id}/files/{file_id}  — download file
// ===========================================================================

async fn download_file(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<actix_files::NamedFile, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    let (_project_id, file_id) = path.into_inner();

    let file: Option<crate::models::project::ProjectFile> = sqlx::query_as(
        "SELECT f.id, f.project_id, f.filename, f.original_name, f.mime, f.size, f.path, f.uploaded_at FROM project_files f JOIN projects p ON f.project_id = p.id WHERE f.id = ? AND p.user_id = ?",
    )
    .bind(&file_id)
    .bind(&user.id)
    .fetch_optional(&state.pool)
    .await?;

    let file = file.ok_or_else(|| AppError::NotFound("File not found".to_string()))?;

    let file_path = Path::new(&file.path);
    if !file_path.exists() {
        return Err(AppError::NotFound("File missing on server".to_string()));
    }

    let named_file = actix_files::NamedFile::open(file_path)
        .map_err(|e| AppError::Internal(format!("Cannot open file: {}", e)))?
        .set_content_disposition(actix_web::http::header::ContentDisposition {
            disposition: actix_web::http::header::DispositionType::Attachment,
            parameters: vec![actix_web::http::header::DispositionParam::Filename(
                file.original_name.clone(),
            )],
        });

    Ok(named_file)
}

// ===========================================================================
// 10. PATCH /{id}/files/{file_id}  — replace file
// ===========================================================================

async fn replace_file(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<(String, String)>,
    mut payload: Multipart,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    let (project_id, file_id) = path.into_inner();

    // Verify file belongs to user's project
    #[derive(Debug, sqlx::FromRow)]
    struct FileRow {
        fid: String,
        old_path: String,
        old_size: i64,
    }

    let row: Option<FileRow> = sqlx::query_as(
        "SELECT f.id AS fid, f.path AS old_path, f.size AS old_size FROM project_files f JOIN projects p ON f.project_id = p.id WHERE f.id = ? AND p.id = ? AND p.user_id = ?",
    )
    .bind(&file_id)
    .bind(&project_id)
    .bind(&user.id)
    .fetch_optional(&state.pool)
    .await?;

    let row = match row {
        Some(r) => r,
        None => return Err(AppError::NotFound("File not found".to_string())),
    };

    // Read upload from multipart
    let mut original_name = String::new();
    let mut mime_type: Option<String> = None;
    let mut temp_path: Option<PathBuf> = None;

    while let Some(Ok(mut field)) = payload.next().await {
        let filename = field
            .content_disposition()
            .and_then(|cd| cd.get_filename().map(|s| s.to_string()))
            .unwrap_or_else(|| "upload".to_string());

        original_name = filename.clone();
        mime_type = field.content_type().map(|m| m.to_string());

        let tmp_dir = Path::new(&state.config.upload_tmp);
        tokio::fs::create_dir_all(tmp_dir).await?;

        let tmp_name = format!("{}-{}", Uuid::new_v4(), sanitize_filename::sanitize(&filename));
        let tmp_file = tmp_dir.join(&tmp_name);

        let mut file_data = Vec::new();
        while let Some(chunk) = field.next().await {
            let chunk = chunk.map_err(|e| {
                AppError::Internal(format!("Multipart read error: {}", e))
            })?;
            file_data.extend_from_slice(&chunk);
        }

        tokio::fs::write(&tmp_file, &file_data).await?;
        temp_path = Some(tmp_file);
        break;
    }

    let temp_path = match temp_path {
        Some(p) => p,
        None => return Err(AppError::BadRequest("No file uploaded".to_string())),
    };

    let new_size = tokio::fs::metadata(&temp_path).await?.len() as i64;

    // Quota check: used - old + new <= max
    let used = get_user_usage_bytes(&state.pool, &user.id).await?;
    if used - row.old_size + new_size > MAX_BYTES_PER_USER {
        let _ = tokio::fs::remove_file(&temp_path).await;
        return Err(AppError::PayloadTooLarge(
            "Replace would exceed your 1GB quota".to_string(),
        ));
    }

    // Destination: same directory as old file
    let old_path = Path::new(&row.old_path);
    let dest_dir = old_path
        .parent()
        .unwrap_or_else(|| Path::new("."));

    let safe_name = sanitize_filename::sanitize(&original_name);
    let stored_name = format!(
        "{}-{}-{}",
        chrono::Utc::now().timestamp_millis(),
        rand::random::<u32>(),
        safe_name
    );
    let dest_path = dest_dir.join(&stored_name);

    move_file(&temp_path, &dest_path).await?;

    // Remove old file from disk
    let _ = tokio::fs::remove_file(old_path).await;

    // Update DB
    let now = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    sqlx::query(
        "UPDATE project_files SET filename = ?, original_name = ?, mime = ?, size = ?, path = ?, uploaded_at = ? WHERE id = ?",
    )
    .bind(&stored_name)
    .bind(&original_name)
    .bind(&mime_type)
    .bind(new_size)
    .bind(dest_path.to_string_lossy().as_ref())
    .bind(&now)
    .bind(&file_id)
    .execute(&state.pool)
    .await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "File replaced", "id": file_id })))
}

// ===========================================================================
// 11. DELETE /{id}/files/{file_id}  — delete file
// ===========================================================================

async fn delete_file(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    let (_project_id, file_id) = path.into_inner();

    #[derive(Debug, sqlx::FromRow)]
    struct FilePath {
        id: String,
        path: String,
    }

    let file: Option<FilePath> = sqlx::query_as(
        "SELECT f.id, f.path FROM project_files f JOIN projects p ON f.project_id = p.id WHERE f.id = ? AND p.user_id = ?",
    )
    .bind(&file_id)
    .bind(&user.id)
    .fetch_optional(&state.pool)
    .await?;

    let file = file.ok_or_else(|| AppError::NotFound("File not found".to_string()))?;

    let _ = tokio::fs::remove_file(&file.path).await;

    sqlx::query("DELETE FROM project_files WHERE id = ?")
        .bind(&file.id)
        .execute(&state.pool)
        .await?;

    Ok(HttpResponse::Ok().json(json!({ "message": "File deleted" })))
}

// ===========================================================================
// 12. GET /{id}/public  — public URL for project
// ===========================================================================

async fn get_public_project(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    let project_id = path.into_inner();

    let project_dir =
        find_project_dir_for_user(&state.config.storage_base, &user.username, &project_id).await;

    let project_dir = project_dir.ok_or_else(|| {
        AppError::NotFound("Project storage folder not found".to_string())
    })?;

    let index = project_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("0");

    let url = format!(
        "{}/{}/{}/",
        state.config.public_url_base,
        urlencoding::encode(&user.username),
        urlencoding::encode(index),
    );

    Ok(HttpResponse::Ok().json(json!({ "url": url })))
}

// ===========================================================================
// 13. GET /{id}/files/{file_id}/public  — public URL for file
// ===========================================================================

async fn get_public_file(
    state: web::Data<AppState>,
    auth: AuthUser,
    path: web::Path<(String, String)>,
) -> Result<HttpResponse, AppError> {
    let user = get_user_by_req(&state.pool, &auth).await?;
    let (project_id, file_id) = path.into_inner();

    #[derive(Debug, sqlx::FromRow)]
    struct FileInfo {
        filename: String,
    }

    let file_row: Option<FileInfo> = sqlx::query_as(
        "SELECT f.filename FROM project_files f JOIN projects p ON f.project_id = p.id WHERE f.id = ? AND p.id = ? AND p.user_id = ?",
    )
    .bind(&file_id)
    .bind(&project_id)
    .bind(&user.id)
    .fetch_optional(&state.pool)
    .await?;

    let file_row =
        file_row.ok_or_else(|| AppError::NotFound("File not found".to_string()))?;

    let project_dir =
        find_project_dir_for_user(&state.config.storage_base, &user.username, &project_id).await;

    let project_dir = project_dir.ok_or_else(|| {
        AppError::NotFound("Project storage folder not found".to_string())
    })?;

    let index = project_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("0");

    let url = format!(
        "{}/{}/{}/{}",
        state.config.public_url_base,
        urlencoding::encode(&user.username),
        urlencoding::encode(index),
        urlencoding::encode(&file_row.filename),
    );

    Ok(HttpResponse::Ok().json(json!({ "url": url })))
}

// ===========================================================================
// Route configuration
// ===========================================================================

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/projects")
            .route("", web::post().to(create_project))
            .route("", web::get().to(list_projects))
            .route("/quota", web::get().to(get_quota))
            .route("/{id}", web::get().to(get_project))
            .route("/{id}", web::patch().to(update_project))
            .route("/{id}", web::delete().to(delete_project))
            .route("/{id}/files", web::post().to(upload_file))
            .route("/{id}/files", web::get().to(list_files))
            .route("/{id}/files/{file_id}", web::get().to(download_file))
            .route("/{id}/files/{file_id}", web::patch().to(replace_file))
            .route("/{id}/files/{file_id}", web::delete().to(delete_file))
            .route("/{id}/public", web::get().to(get_public_project))
            .route(
                "/{id}/files/{file_id}/public",
                web::get().to(get_public_file),
            ),
    );
}
