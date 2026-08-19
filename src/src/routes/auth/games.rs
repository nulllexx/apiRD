use actix_web::{web, HttpRequest, HttpResponse};
use serde::Serialize;
use serde_json::json;
use std::collections::HashMap;

use crate::console::control;
use crate::error::AppError;
use crate::middleware::auth::AuthUser;
use crate::AppState;
use crate::middleware::rate_limit::RateLimiter;

use super::common::*;

#[derive(Serialize)]
pub struct GameDetails {
    pub title: String,
    pub description: String,
    pub image: String,
    pub file: String,       // URL to the .zip file
    pub executable: String, // The exact name of the .exe inside the zip (e.g., "Game.exe")
}

/// POST /startserver
pub(super) async fn start_server(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    _auth: AuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    // Queued for the `mc-control` sidecar. This container holds no Docker
    // socket, so it cannot (and must not) drive the daemon itself.
    control::request(&state.config.control_dir, control::PowerAction::Start)
        .await
        .map_err(|e| {
            log::error!("Error queueing start for the sidecar: {}", e);
            AppError::Internal("Error starting server".to_string())
        })?;

    log::info!("Server start queued.");
    Ok(HttpResponse::Accepted().json(json!({ "message": "Server start queued" })))
}

/// GET /fetch-worlds
pub(super) async fn fetch_worlds(
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

/// GET /getGames
/// GET /getGames
pub(super) async fn get_games(
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

