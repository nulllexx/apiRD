//! Library crate for apiRD.
//!
//! The HTTP routing and shared state live here (rather than in `main.rs`) so
//! integration tests in `tests/` can mount the *exact same* routes the
//! production server serves. `main.rs` is a thin binary that wires up the
//! database, background tasks, and the HTTP server around this crate.

use actix_web::web;
use sqlx::MySqlPool;
use std::sync::{Arc, RwLock};

pub mod config;
pub mod console;
pub mod db;
pub mod error;
pub mod middleware;
pub mod models;
pub mod polls;
pub mod rcon;
pub mod routes;

use config::AppConfig;
use console::presence::Presence;
use console::stats::SnapshotCache;
use console::textures::TextureCache;
use console::Consoles;
use rcon::RconClient;

#[derive(Clone)]
pub struct AppState {
    pub pool: MySqlPool,
    pub config: AppConfig,
    pub player_count: Arc<RwLock<u32>>,
    pub max_players: Arc<RwLock<u32>>,
    /// Fan-out points for the two live log streams, fed by the tail tasks.
    pub console: Arc<Consoles>,
    /// Long-lived RCON connection shared by every console command.
    pub rcon: Arc<RconClient>,
    /// Short-lived cache of what the game server reports about itself, so a
    /// dashboard left open does not turn into a stream of RCON chatter in the
    /// log it is displaying.
    pub snapshot: Arc<SnapshotCache>,
    /// Who is online, maintained from the log stream rather than by polling
    /// the game server.
    pub presence: Arc<Presence>,
    /// Item artwork for the inventory viewer, fetched once and then local.
    pub textures: Arc<TextureCache>,
}

/// Registers every HTTP route the server exposes (the `/api` scope plus the
/// dashboard pages) onto an actix `ServiceConfig`.
///
/// Keeping this separate from `main` means both the production server and the
/// integration tests build their routing from one source of truth — a route
/// that stops compiling or gets dropped here fails the tests before it ships.
///
/// Note: the static `/content` file service is intentionally NOT registered
/// here because it needs a runtime-resolved filesystem path; `main.rs` mounts
/// it directly.
pub fn configure_api(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/api")
            .configure(routes::auth::configure)
            .configure(routes::util::configure_util)
            .configure(routes::status::configure)
            .configure(routes::projects::configure)
            .configure(routes::api_keys::configure)
            .configure(routes::api::configure)
            .configure(routes::console::configure)
            .configure(routes::polls::configure)
            .configure(routes::oauth_google::configure),
    )
    .route("/dashboard.html", web::get().to(routes::auth::serve_dashboard))
    .route("/dashboard", web::get().to(routes::auth::serve_dashboard))
    .route("/rdadmin.html", web::get().to(routes::auth::serve_rdadmin))
    .route("/rdadmin", web::get().to(routes::auth::serve_rdadmin))
    .route("/polls.html", web::get().to(routes::auth::serve_polls))
    .route("/polls", web::get().to(routes::auth::serve_polls));
}
