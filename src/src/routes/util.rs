use actix_web::{web, HttpResponse};
use std::collections::HashMap;
use std::path::Path;

use crate::error::AppError;
use crate::AppState;

#[derive(serde::Deserialize)]
pub struct CheckUpdatesRequest {
    pub version: Option<String>,
}

/// POST /api/util/check-updates
async fn check_updates(
    body: web::Json<CheckUpdatesRequest>,
) -> Result<HttpResponse, AppError> {
    let client_version = body
        .version
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest("No version supplied in request body".to_string())
        })?;

    let version_file = "/home/useradmin/api/mainapi/src/content/Highway63/Highway63.version";

    let disk_version = tokio::fs::read_to_string(version_file)
        .await
        .map_err(|e| AppError::Internal(format!("Failed to read version file: {}", e)))?;
    let disk_version = disk_version.trim();

    if client_version == disk_version {
        Ok(HttpResponse::Ok().body("Latest version installed"))
    } else {
        Ok(HttpResponse::Ok().json(serde_json::json!({
            "update": "available",
            "url": "https://bakosmp.go.ro/content/Highway63.exe"
        })))
    }
}

/// GET /api/util/games
async fn get_games() -> Result<HttpResponse, AppError> {
    let data_folder = "/home/useradmin/api/mainapi/src/data/";

    let mut entries = tokio::fs::read_dir(data_folder)
        .await
        .map_err(|e| AppError::Internal(format!("Unable to read data folder: {}", e)))?;

    // Group files by name (stem before extension)
    let mut game_files: HashMap<String, HashMap<String, String>> = HashMap::new();

    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Some(filename) = entry.file_name().to_str() {
            let path = Path::new(filename);
            if let (Some(stem), Some(ext)) = (path.file_stem(), path.extension()) {
                let stem = stem.to_string_lossy().to_string();
                let ext = ext.to_string_lossy().to_string();
                game_files
                    .entry(stem)
                    .or_default()
                    .insert(ext.clone(), filename.to_string());
            }
        }
    }

    let mut games = Vec::new();

    for (name, file_types) in &game_files {
        let title = if let Some(title_file) = file_types.get("title") {
            tokio::fs::read_to_string(format!("{}{}", data_folder, title_file))
                .await
                .unwrap_or_default()
                .replace('\n', "")
        } else {
            continue;
        };

        let description = if let Some(desc_file) = file_types.get("description") {
            tokio::fs::read_to_string(format!("{}{}", data_folder, desc_file))
                .await
                .unwrap_or_default()
                .replace('\n', "")
        } else {
            String::new()
        };

        let image = file_types
            .get("jpg")
            .map(|f| format!("https://bakosmp.go.ro/data/{}", f))
            .unwrap_or_default();

        let file = file_types
            .get("exe")
            .map(|f| format!("https://bakosmp.go.ro/data/{}", f))
            .unwrap_or_default();

        games.push(serde_json::json!({
            name: {
                "title": title,
                "description": description,
                "image": image,
                "file": file
            }
        }));
    }

    Ok(HttpResponse::Ok().json(games))
}

/// GET /api/util/router/perf
async fn router_perf(
    _state: web::Data<AppState>,
) -> Result<HttpResponse, AppError> {
    let router_password = std::env::var("ROUTER_PASSWORD").unwrap_or_else(|_| {
        "81328db1ef19a4e17b19ecff256840fce17d0cf24240b58193e1933020f4a5430bf44decd10bbc02017317de09c98f42d2fa3620098b04d80348c57a7bc84720f4a55ff1934297119355a0bf74c1366598bd8562b0ae1dc8f0b79a572daca5e40f01f6938583ca885fa1b952308a360fc4fe4c8db83d4e9c2d94deb5f05f789e".to_string()
    });

    if router_password.is_empty() {
        return Err(AppError::Internal("No router password".to_string()));
    }

    // Login to router
    let client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .build()
        .map_err(|e| AppError::Internal(format!("HTTP client error: {}", e)))?;

    let login_url = format!(
        "https://192.168.0.1/cgi-bin/luci/;stok=/login?form=login&operation=login&password={}",
        router_password
    );

    let login_resp = client
        .get(&login_url)
        .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36")
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Router login failed: {}", e)))?;

    let login_json: serde_json::Value = login_resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Router login parse failed: {}", e)))?;

    let stok = login_json
        .get("data")
        .and_then(|d| d.get("stok"))
        .and_then(|s| s.as_str())
        .ok_or_else(|| AppError::Internal("Router login failed: no stok".to_string()))?;

    // Fetch performance data
    let perf_url = format!(
        "https://192.168.0.1/cgi-bin/luci/;stok={}/admin/status",
        stok
    );

    let perf_resp = client
        .get(&perf_url)
        .header("User-Agent", "PostmanRuntime/7.45.0")
        .send()
        .await
        .map_err(|e| AppError::Internal(format!("Router perf fetch failed: {}", e)))?;

    let perf_json: serde_json::Value = perf_resp
        .json()
        .await
        .map_err(|e| AppError::Internal(format!("Router perf parse failed: {}", e)))?;

    Ok(HttpResponse::Ok().json(perf_json))
}

pub fn configure_util(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/util")
            .route("/check-updates", web::post().to(check_updates))
            .route("/games", web::get().to(get_games))
            .route("/router/perf", web::get().to(router_perf)),
    );
}
