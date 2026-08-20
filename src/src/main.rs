use actix_cors::Cors;
use actix_web::{web, App, HttpServer, middleware as actix_middleware};
use sqlx::MySqlPool;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use api_rd::config::AppConfig;
use api_rd::{configure_api, console, db, middleware, AppState};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    // Load .env file
    dotenvy::dotenv().ok();
    env_logger::init();

    let config = AppConfig::from_env();
    let port = config.port;

    // `Files` refuses to mount a directory that does not exist, and it does so
    // once per worker thread — the eight identical "Specified path is not a
    // directory" errors at boot. Creating it here is what the Dockerfile's
    // `mkdir` was for; that never worked, because the path it made is under
    // /home/useradmin/api, which docker-compose bind-mounts over from the host.
    //
    // Not fatal if it fails: a read-only or unwritable mount leaves /content
    // answering 404s exactly as it does today, which beats refusing to boot the
    // whole API over a static file service.
    if let Err(e) = std::fs::create_dir_all(&config.content_path) {
        log::error!(
            "Could not create the content directory at {}: {e}",
            config.content_path
        );
    }

    // Create database pool
    let pool = db::create_pool(&config)
        .await
        .expect("Failed to create database pool");

    // Initialize database tables, migrations, and seed data
    db::init_database(&pool)
        .await
        .expect("Failed to initialize database");

    let app_state = AppState {
        pool: pool.clone(),
        config: config.clone(),
        player_count: Arc::new(RwLock::new(0)),
        max_players: Arc::new(RwLock::new(20)),
        console: console::Consoles::new(config.console_backlog_lines),
        // One connection reused across commands; see RconClient for why.
        rcon: api_rd::rcon::RconClient::new(
            config.rcon_address.clone(),
            config.rcon_password.clone(),
        ),
        snapshot: console::stats::SnapshotCache::new(console::stats::SNAPSHOT_TTL),
        presence: console::presence::Presence::new(),
        textures: console::textures::TextureCache::new(
            config.item_texture_dir.clone(),
            config.minecraft_assets_version.clone(),
            config.item_texture_base_url.clone(),
            config.mods_dir.clone(),
        ),
    };

    // Spawn auto-unban background task
    let unban_pool = pool.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(60));
        loop {
            interval.tick().await;
            log::info!("Running auto-unban check...");
            if let Err(e) = run_auto_unban(&unban_pool).await {
                log::error!("Error auto-unbanning users: {}", e);
            }
        }
    });

    // Spawn file watcher for player count
    let pc_state = app_state.clone();
    tokio::spawn(async move {
        watch_player_count(pc_state).await;
    });

    // Spawn the console tailers — one reader per source process-wide, fanned
    // out to every admin viewing the console over SSE.
    //
    // The stdout capture only exists if `start.sh` tees the container's output
    // to it; until then that tailer simply idles waiting for the file, and the
    // console's stdout tab stays empty.
    for (label, path, hub) in [
        (
            "latest.log",
            config.minecraft_log_path.clone(),
            Arc::clone(&app_state.console.server_log),
        ),
        (
            "stdout capture",
            config.minecraft_stdout_path.clone(),
            Arc::clone(&app_state.console.stdout),
        ),
    ] {
        log::info!("Console tailing {} at {}", label, path);
        tokio::spawn(async move {
            console::tail::tail_log(hub, path).await;
        });
    }

    // Track who is online off the back of the log tail. The hub is already
    // fanning these lines out to the browser, so following them here costs one
    // more receiver and no I/O at all — the alternative is polling the game
    // server, which writes a line to that same log for every command.
    //
    // The backlog is deliberately not replayed: it can span a restart, so the
    // first HTTP request reconciles against the server instead of trusting it.
    let presence = Arc::clone(&app_state.presence);
    let presence_hub = Arc::clone(&app_state.console.server_log);
    tokio::spawn(async move {
        let (_backlog, mut rx) = presence_hub.subscribe();
        loop {
            match rx.recv().await {
                Ok(line) => {
                    presence.apply(&line);
                }
                // A burst that outran this task cannot be reconstructed, so let
                // the periodic resync clean up rather than guessing.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!("presence tracker fell {n} lines behind");
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    log::info!("Server is running on port {}", port);

    let rate_limiter = web::Data::new(middleware::rate_limit::RateLimiter::new(500, 60));

    HttpServer::new(move || {
        let cors = Cors::default()
            .allowed_origin(&config.cors_origin)
            .supports_credentials()
            .allowed_methods(vec!["GET", "HEAD", "PUT", "PATCH", "POST", "DELETE"])
            .allowed_headers(vec!["Content-Type", "Authorization"])
            .expose_headers(vec!["Set-Cookie"]);

        App::new()
            .app_data(web::Data::new(app_state.clone()))
            .app_data(rate_limiter.clone())
            .wrap(cors)
            .wrap(actix_middleware::NormalizePath::trim())
            .configure(configure_api)
            .service(
                actix_files::Files::new("/content", &config.content_path)
                    .show_files_listing(),
            )
    })
    .bind(format!("0.0.0.0:{}", port))?
    .run()
    .await
}

async fn run_auto_unban(pool: &MySqlPool) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();

    let expired: Vec<(String,)> = sqlx::query_as(
        "SELECT user_id FROM user_moderation WHERE expires_at IS NOT NULL AND expires_at <= ?",
    )
    .bind(&now)
    .fetch_all(pool)
    .await?;

    log::info!("Found {} users to unban.", expired.len());

    for (user_id,) in &expired {
        sqlx::query("DELETE FROM user_moderation WHERE user_id = ? AND type != 'poison'")
            .bind(user_id)
            .execute(pool)
            .await?;
        log::info!("Auto-unbanned user ID: {}", user_id);
    }

    Ok(())
}

async fn watch_player_count(state: AppState) {
    use notify::{Event, EventKind, RecursiveMode, Watcher};
    use std::path::Path;

    let pc_path = state.config.player_count_path.clone();
    let sp_path = state.config.server_properties_path.clone();

    // Initial read
    read_player_count(&pc_path, &state);
    read_max_players(&sp_path, &state);

    let state_clone = state.clone();
    let pc_path_clone = pc_path.clone();
    let sp_path_clone = sp_path.clone();

    let mut watcher = match notify::recommended_watcher(move |res: Result<Event, notify::Error>| {
        if let Ok(event) = res {
            if matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_)) {
                for path in &event.paths {
                    if path.to_string_lossy().contains("plrCount") {
                        read_player_count(&pc_path_clone, &state_clone);
                    }
                    if path.to_string_lossy().contains("server.properties") {
                        read_max_players(&sp_path_clone, &state_clone);
                    }
                }
            }
        }
    }) {
        Ok(w) => w,
        Err(e) => {
            log::error!("Failed to create file watcher: {}", e);
            return;
        }
    };

    // Watch the parent directories
    if let Some(parent) = Path::new(&pc_path).parent() {
        let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
    }
    if let Some(parent) = Path::new(&sp_path).parent() {
        if parent != Path::new(&pc_path).parent().unwrap_or(Path::new("")) {
            let _ = watcher.watch(parent, RecursiveMode::NonRecursive);
        }
    }

    // Keep the watcher alive
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}

fn read_player_count(path: &str, state: &AppState) {
    if let Ok(content) = std::fs::read_to_string(path) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(count) = json.get("players").and_then(|v| v.as_u64()) {
                if let Ok(mut pc) = state.player_count.write() {
                    *pc = count as u32;
                }
            }
        }
    }
}

fn read_max_players(path: &str, state: &AppState) {
    if let Ok(content) = std::fs::read_to_string(path) {
        for line in content.lines() {
            if let Some(val) = line.strip_prefix("max-players=") {
                if let Ok(max) = val.trim().parse::<u32>() {
                    if let Ok(mut mp) = state.max_players.write() {
                        *mp = max;
                    }
                }
            }
        }
    }
}
