use actix_web::{web, HttpRequest, HttpResponse};

use crate::console::control;
use crate::error::AppError;
use crate::middleware::api_key_auth;
use crate::middleware::auth::AuthUser;
use crate::AppState;

/// Validate API key from X-API-Key header (called at the start of each handler)
async fn require_api_key(req: &HttpRequest, state: &web::Data<AppState>) -> Result<(), AppError> {
    let api_key = req
        .headers()
        .get("X-API-Key")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| AppError::Unauthorized("API key is required".to_string()))?;

    let result = api_key_auth::validate_and_track_usage(&state.pool, api_key).await?;

    if !result.valid {
        return Err(AppError::Unauthorized(
            result.error.unwrap_or_else(|| "Invalid API key".to_string()),
        ));
    }

    Ok(())
}

/// GET /api/v1/playercount
async fn player_count(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> Result<HttpResponse, AppError> {
    require_api_key(&req, &state).await?;

    let pc = state.player_count.read().map(|v| *v).unwrap_or(0);
    let mp = state.max_players.read().map(|v| *v).unwrap_or(20);

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "players": pc,
        "maxPlayers": mp
    })))
}

/// POST /api/v1/hash
async fn hash_password(
    req: HttpRequest,
    state: web::Data<AppState>,
    body: web::Json<serde_json::Value>,
) -> Result<HttpResponse, AppError> {
    require_api_key(&req, &state).await?;

    let password = body
        .get("password")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::BadRequest("Password is required".to_string()))?;

    let hashed = bcrypt::hash(password, 10)?;

    Ok(HttpResponse::Ok().json(serde_json::json!({ "output": hashed })))
}

/// POST /api/v1/files — upload file (requires auth + API key)
async fn upload_file(
    req: HttpRequest,
    state: web::Data<AppState>,
    _auth: AuthUser,
    mut payload: actix_multipart::Multipart,
) -> Result<HttpResponse, AppError> {
    require_api_key(&req, &state).await?;

    use futures_util::StreamExt;

    let media_path = &state.config.media_path;
    tokio::fs::create_dir_all(media_path).await?;

    if let Some(item) = payload.next().await {
        let mut field = item.map_err(|e| AppError::BadRequest(e.to_string()))?;

        let filename = field
            .content_disposition()
            .and_then(|cd| cd.get_filename().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("upload-{}", uuid::Uuid::new_v4()));

        let filepath = format!("{}/{}", media_path, sanitize_filename::sanitize(&filename));
        let mut file = tokio::fs::File::create(&filepath).await?;

        while let Some(chunk) = field.next().await {
            let data = chunk.map_err(|e| AppError::Internal(e.to_string()))?;
            tokio::io::AsyncWriteExt::write_all(&mut file, &data).await?;
        }

        return Ok(HttpResponse::Ok().json(serde_json::json!({
            "message": "File uploaded successfully",
            "file": filename
        })));
    }

    Err(AppError::BadRequest("No file uploaded".to_string()))
}

/// GET /api/v1/files — list files in media directory
async fn get_files(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> Result<HttpResponse, AppError> {
    require_api_key(&req, &state).await?;

    let media_path = &state.config.media_path;
    let mut files = Vec::new();

    match tokio::fs::read_dir(media_path).await {
        Ok(mut entries) => {
            while let Ok(Some(entry)) = entries.next_entry().await {
                if let Some(name) = entry.file_name().to_str() {
                    files.push(name.to_string());
                }
            }
        }
        Err(_) => {
            return Err(AppError::Internal("Unable to scan files".to_string()));
        }
    }

    Ok(HttpResponse::Ok().json(serde_json::json!({ "files": files })))
}

/// GET /api/v1/serverrunning
async fn server_running(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> Result<HttpResponse, AppError> {
    require_api_key(&req, &state).await?;

    // Read the state the `mc-control` sidecar last observed. This container has
    // no Docker socket by design, so it cannot ask the daemon itself.
    let server_state = control::status(&state.config.control_dir).await;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "running": server_state == control::ServerState::Running,
        "state": server_state.as_str(),
    })))
}

/// POST /api/v1/restart — requires auth + API key
async fn restart_server(
    req: HttpRequest,
    state: web::Data<AppState>,
    _auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    require_api_key(&req, &state).await?;

    // Queued for the `mc-control` sidecar rather than run here: the API holds
    // no Docker socket, so an RCE in this process cannot reach the daemon.
    control::request(&state.config.control_dir, control::PowerAction::Restart)
        .await
        .map_err(|e| {
            log::error!("Error queueing restart for the sidecar: {}", e);
            AppError::Internal("Failed to restart Minecraft server.".to_string())
        })?;

    log::info!("Server restart queued.");
    Ok(HttpResponse::Accepted().json(serde_json::json!({
        "message": "Minecraft server restart queued."
    })))
}

/// POST /api/v1/startserver — requires auth + API key
async fn start_server(
    req: HttpRequest,
    state: web::Data<AppState>,
    _auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    require_api_key(&req, &state).await?;

    control::request(&state.config.control_dir, control::PowerAction::Start)
        .await
        .map_err(|e| {
            log::error!("Error queueing start for the sidecar: {}", e);
            AppError::Internal("Failed to start Minecraft server.".to_string())
        })?;

    log::info!("Server start queued.");
    Ok(HttpResponse::Accepted().json(serde_json::json!({
        "message": "Server start queued"
    })))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/v1")
            .route("/playercount", web::get().to(player_count))
            .route("/hash", web::post().to(hash_password))
            .route("/files", web::post().to(upload_file))
            .route("/files", web::get().to(get_files))
            .route("/serverrunning", web::get().to(server_running))
            .route("/restart", web::post().to(restart_server))
            .route("/startserver", web::post().to(start_server)),
    );
}
