use std::sync::Arc;
use std::time::Duration;

use actix_web::{web, HttpResponse};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::console::control::{self, PowerAction};
use crate::console::inventory::{self, InventoryError};
use crate::console::textures::TextureError;
use crate::console::players::{self, PlayerAction};
use crate::console::stats::{self, STATS_MAX_AGE};
use crate::console::audit::{self, Kind, Outcome};
use crate::console::{sse_event, sse_frame, strip_ansi, LogSource};
use crate::error::AppError;
use crate::middleware::admin_auth::AdminUser;
use crate::AppState;

/// Idle gap after which a comment frame is emitted. Keeps intermediaries from
/// reaping a quiet connection, and lets the browser notice a dead socket.
const HEARTBEAT: Duration = Duration::from_secs(15);

fn sse_response() -> actix_web::HttpResponseBuilder {
    let mut builder = HttpResponse::Ok();
    builder
        .content_type("text/event-stream")
        .insert_header(("Cache-Control", "no-cache"))
        .insert_header(("X-Accel-Buffering", "no"));
    builder
}

/// The SSE event name attribution lines are delivered under.
///
/// A `&'static str` chosen here and nowhere else. See `console::audit` for why
/// that is the security property and not just a naming convention.
const AUDIT_EVENT: &str = "audit";

fn log_stream(
    backlog: Vec<Arc<str>>,
    rx: broadcast::Receiver<Arc<str>>,
    audit: Option<(Vec<Arc<str>>, broadcast::Receiver<Arc<str>>)>,
    heartbeat: Duration,
) -> impl futures_util::Stream<Item = Result<web::Bytes, actix_web::Error>> {
    // The whole replay buffer goes out as one frame so a reconnecting client
    // repaints in a single pass rather than a few hundred. Recent attribution
    // rides along in the same frame, named, so a console opened mid-session
    // starts with the actions it missed rather than only future ones.
    let (audit_backlog, audit_rx) = match audit {
        Some((backlog, rx)) => (backlog, Some(rx)),
        None => (Vec::new(), None),
    };

    let mut head = backlog.iter().map(|line| sse_frame(line)).collect::<String>();
    for line in &audit_backlog {
        head.push_str(&sse_event(AUDIT_EVENT, line));
    }
    let head = web::Bytes::from(head);

    let live = futures_util::stream::unfold(
        (rx, audit_rx),
        move |(mut rx, mut audit_rx)| async move {
            // A closed audit channel must not become a busy loop, so it is
            // dropped from the select rather than polled forever. `pending()`
            // is what lets one select arm be absent.
            let frame = tokio::select! {
                // `timeout` doubles as the heartbeat clock, which avoids pulling
                // in tokio-stream or hand-rolling a select! just for a keepalive.
                result = tokio::time::timeout(heartbeat, rx.recv()) => match result {
                    Ok(Ok(line)) => web::Bytes::from(sse_frame(&line)),
                    // Surfaced rather than swallowed, so a slow client can see
                    // that it missed lines instead of silently believing the log
                    // went quiet.
                    Ok(Err(RecvError::Lagged(n))) => web::Bytes::from(format!(": lagged {n}\n\n")),
                    Ok(Err(RecvError::Closed)) => return None,
                    Err(_elapsed) => web::Bytes::from_static(b": ping\n\n"),
                },
                result = async {
                    match audit_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => match result {
                    Ok(line) => web::Bytes::from(sse_event(AUDIT_EVENT, &line)),
                    Err(RecvError::Lagged(n)) => {
                        web::Bytes::from(format!(": audit lagged {n}\n\n"))
                    }
                    // The log stream outlives the audit channel rather than
                    // ending with it: losing attribution is bad, losing the
                    // console as well would be worse.
                    Err(RecvError::Closed) => {
                        audit_rx = None;
                        web::Bytes::from_static(b": audit closed\n\n")
                    }
                },
            };
            Some((Ok::<_, actix_web::Error>(frame), (rx, audit_rx)))
        },
    );

    futures_util::stream::once(async move { Ok::<_, actix_web::Error>(head) }).chain(live)
}

#[derive(Deserialize)]
struct StreamQuery {
    source: Option<String>,
}

/// GET /api/admin/console/stream?source=log|stdout — live log, backlog first.
async fn stream_log(
    state: web::Data<AppState>,
    _admin: AdminUser,
    query: web::Query<StreamQuery>,
) -> HttpResponse {
    let source = LogSource::parse(query.source.as_deref());
    let (backlog, rx) = state.console.get(source).subscribe();
    // Attribution rides along with whichever log the viewer chose, since it
    // belongs to neither and is relevant to both.
    let audit = state.console.audit.subscribe();
    sse_response().streaming(log_stream(backlog, rx, Some(audit), HEARTBEAT))
}

#[derive(Deserialize)]
struct CommandBody {
    command: String,
}

/// POST /api/admin/console/command — run one command over RCON.
async fn run_command(
    state: web::Data<AppState>,
    admin: AdminUser,
    body: web::Json<CommandBody>,
) -> Result<HttpResponse, AppError> {
    let command = validate_command(&body.command)?;

    // Who ran what. There is deliberately no denylist — the route is
    // admin-only and operators need the full command set — so attribution is
    // what makes that safe to offer rather than restriction.
    log::info!("console command by {}: {}", admin.username, command);
    let record = audit_or_refuse(&state, &admin, Kind::Command, &command, None).await?;

    let output = audit::settle(
        &state.pool,
        &state.console.audit,
        record,
        run_rcon(&state, &command).await,
    )
    .await?;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "command": command,
        "output": strip_ansi(output.trim()),
    })))
}

/// Open an audit record, or refuse the action.
///
/// Every mutating console route goes through here first. The refusal is the
/// point: an action nobody can be shown to have taken is exactly what this
/// exists to prevent, so failing to record one means not doing it. That costs
/// nothing in practice — `AdminUser` is itself resolved by a database query, so
/// a database that cannot be written to has already turned every one of these
/// routes into a 401.
async fn audit_or_refuse(
    state: &AppState,
    admin: &AdminUser,
    kind: Kind,
    command: &str,
    target: Option<&str>,
) -> Result<audit::Record, AppError> {
    audit::begin(&state.pool, &admin.username, kind, command, target)
        .await
        .map_err(|e| {
            log::error!("console: refusing to run `{command}` unrecorded: {e}");
            AppError::Internal(
                "The action was not run: it could not be recorded in the audit log".to_string(),
            )
        })
}

/// Run a command that has already been validated, mapping any failure onto the
/// message an operator should see.
async fn run_rcon(state: &AppState, command: &str) -> Result<String, AppError> {
    state.rcon.execute(command).await.map_err(|e| {
        log::error!("console: RCON command failed: {e}");
        AppError::Internal(e.user_message())
    })
}

/// Reject commands that are empty, oversized, or carry control characters.
///
/// Shape only — the caller is already an authenticated admin, so this guards
/// against mangled input rather than against the operator.
pub fn validate_command(raw: &str) -> Result<String, AppError> {
    let command = raw.trim();

    if command.is_empty() {
        return Err(AppError::BadRequest("Command cannot be empty".to_string()));
    }
    if command.chars().count() > 512 {
        return Err(AppError::BadRequest("Command is too long".to_string()));
    }
    if command.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "Command contains control characters".to_string(),
        ));
    }

    // Operators type Minecraft commands with the leading slash out of habit;
    // RCON wants them without.
    Ok(command.trim_start_matches('/').trim().to_string())
}

/// POST /api/admin/console/power/{action} — start, stop or restart.
async fn power(
    state: web::Data<AppState>,
    admin: AdminUser,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let action = PowerAction::parse(&path).ok_or_else(|| {
        AppError::BadRequest("Unknown action; expected start, stop or restart".to_string())
    })?;

    log::info!("console power action by {}: {}", admin.username, action.as_str());
    let record = audit_or_refuse(&state, &admin, Kind::Power, action.as_str(), None).await?;

    // Recorded as queued rather than done, which is all this route can honestly
    // claim: the sidecar acts on it afterwards, and a stop that succeeds here
    // takes the server down before anything could report back.
    audit::settle(
        &state.pool,
        &state.console.audit,
        record,
        control::request(&state.config.control_dir, action)
            .await
            .map_err(|e| {
                log::error!("console: cannot queue {} for the sidecar: {}", action.as_str(), e);
                AppError::Internal("Server control is unavailable".to_string())
            }),
    )
    .await?;

    // The sidecar polls the spool directory, so this is an acknowledgement that
    // the request was queued, not that the container has finished acting on it.
    Ok(HttpResponse::Accepted().json(serde_json::json!({
        "queued": action.as_str(),
    })))
}

/// GET /api/admin/console/power/status — what the sidecar last observed.
async fn power_status(
    state: web::Data<AppState>,
    _admin: AdminUser,
) -> Result<HttpResponse, AppError> {
    let state_now = control::status(&state.config.control_dir).await;
    Ok(HttpResponse::Ok().json(serde_json::json!({
        "state": state_now.as_str(),
        "running": state_now == control::ServerState::Running,
    })))
}

/// Who is online, reconciling against the server itself only when the live set
/// has gone stale.
///
/// Presence is maintained from the log stream, so the usual cost of this is
/// nothing at all. The occasional `list` exists because the log reports
/// *changes* — it can say Steve left, but never that Steve was there to begin
/// with.
async fn online_now(state: &AppState) -> (Vec<String>, Option<String>) {
    if !state.presence.needs_sync() {
        return (state.presence.names(), None);
    }

    let online = state.snapshot.online(&state.rcon).await;

    // An empty name list is only believed when the server said zero — see
    // `Online::is_readable`. `list` replies are rewritten freely by plugins,
    // and one that yields no names may mean nobody is on *or* that the wording
    // is one the parser cannot read. This server's reply is the second kind, so
    // trusting it blanked the roster every reconcile: the log put a player
    // online the instant they joined, and five minutes later this wiped them
    // again. They reappeared only by rejoining, which is a fresh log line.
    //
    // Same principle as the RCON error — an unreachable server is not an empty
    // one, and neither is an unreadable answer.
    match &online.rcon_error {
        None if online.is_readable() => state.presence.replace(&online.names),
        // Keeps the last known set rather than blanking it.
        _ => state.presence.record_failure(),
    }

    (state.presence.names(), online.rcon_error.clone())
}

/// GET /api/admin/console/online — who is on the server right now.
///
/// Split out from `/stats` precisely because it is cheap: the UI polls this
/// often enough to feel live, which would be unaffordable for anything that
/// costs an RCON command.
async fn online_players(
    state: web::Data<AppState>,
    _admin: AdminUser,
) -> Result<HttpResponse, AppError> {
    let (names, rcon_error) = online_now(&state).await;
    let max_players = state.max_players.read().map(|m| *m).unwrap_or(0);

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "count": names.len(),
        "max": max_players,
        "names": names,
        "rconError": rcon_error,
    })))
}

/// GET /api/admin/console/stats — health of the server at a glance.
///
/// Three sources merged into one payload so the panel is a single poll: the
/// sidecar's container stats, the sidecar's power state, and a cached probe of
/// the game server itself.
async fn server_stats(
    state: web::Data<AppState>,
    _admin: AdminUser,
) -> Result<HttpResponse, AppError> {
    let health = state.snapshot.health(&state.rcon).await;
    let container = stats::read_container_stats(&state.config.control_dir, STATS_MAX_AGE).await;
    let power = control::status(&state.config.control_dir).await;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "state": power.as_str(),
        // Null rather than zeroes wherever the number is genuinely unknown, so
        // the UI can say "unavailable" instead of implying a reading of zero.
        "tps": health.tps,
        "heap": health.heap,
        "container": container,
        // No player figures here on purpose: /online owns them, and two
        // endpoints reporting the same count on different schedules would
        // visibly disagree with each other.
        "rconError": health.rcon_error,
    })))
}

/// GET /api/admin/console/players — everyone who has ever joined.
async fn list_players(
    state: web::Data<AppState>,
    _admin: AdminUser,
) -> Result<HttpResponse, AppError> {
    let (online, rcon_error) = online_now(&state).await;
    let roster = players::load_roster(&state.config.server_properties_path, &online).await;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "players": roster,
        // The roster itself comes off disk and so is always available; only the
        // online flags depend on the presence set. Reporting the failure
        // separately lets the UI show the list and note that live status is
        // missing.
        "rconError": rcon_error,
    })))
}

#[derive(Deserialize)]
struct InventoryQuery {
    /// `?live=<name>` asks the server itself for this player's inventory, using
    /// the name because `/data get` selects by name rather than by UUID. The
    /// UUID in the path still identifies whose file to fall back to.
    live: Option<String>,
}

/// Ask the running server for one player's live inventory.
///
/// Returns `Ok(None)` when the server answered but had nothing usable — the
/// player logged off between the roster load and this request being the normal
/// way that happens — so the caller can fall back to the saved file rather than
/// report a failure the operator can do nothing about.
async fn live_inventory(
    state: &AppState,
    player: &str,
) -> Result<Option<inventory::PlayerSnapshot>, String> {
    if !state.rcon.is_configured() {
        return Err("RCON is not configured on this server".to_string());
    }

    let mut replies: Vec<(&str, String)> = Vec::new();
    for path in inventory::LIVE_PATHS {
        let Some(command) = inventory::live_command(player, path) else {
            return Err("That is not a valid Minecraft username".to_string());
        };

        match state.rcon.execute(&command).await {
            Ok(output) => replies.push((path, output)),
            // One failed path is survivable; a dead connection is not, and
            // retrying the remaining three would just wait out three timeouts.
            Err(e) => return Err(e.user_message()),
        }
    }

    let root = inventory::live_root(&replies);
    if !inventory::live_root_is_usable(&root) {
        return Ok(None);
    }

    Ok(Some(inventory::snapshot_from_root(&root)))
}

/// GET /api/admin/console/players/{uuid}/inventory — what a player is carrying.
///
/// Two sources, and the response says which one answered. `?live=<name>` reads
/// the player out of the running server over RCON, which is exact but only
/// possible while they are online. Everything else — and any live read that
/// does not come back — falls back to their `playerdata` file, which exists for
/// everyone who has ever joined but is only as fresh as the last autosave.
async fn player_inventory(
    state: web::Data<AppState>,
    _admin: AdminUser,
    path: web::Path<String>,
    query: web::Query<InventoryQuery>,
) -> Result<HttpResponse, AppError> {
    let uuid = path.into_inner().trim().to_ascii_lowercase();

    // The UUID becomes a filename, so a value that could not name a player file
    // is refused outright rather than cleaned up into one that could.
    if !inventory::is_canonical_uuid(&uuid) {
        return Err(AppError::BadRequest(
            "That is not a valid player UUID".to_string(),
        ));
    }

    let mut live_error: Option<String> = None;
    let mut live_snapshot: Option<inventory::PlayerSnapshot> = None;

    if let Some(name) = query.live.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        match live_inventory(&state, name).await {
            Ok(Some(snapshot)) => live_snapshot = Some(snapshot),
            Ok(None) => {
                live_error = Some("The server had no live data for that player".to_string())
            }
            Err(why) => {
                log::warn!("console: live inventory for {name} failed: {why}");
                live_error = Some(why);
            }
        }
    }

    // The file is read even in live mode: it is the only source for `savedAt`,
    // and its vitals fill the header when the live paths do not carry them.
    let saved = inventory::load_snapshot(&state.config.server_properties_path, &uuid).await;

    // Whether any part of the header had to come off disk. Only meaningful on
    // the live path; a saved read is stale by definition and says so already.
    let mut vitals_borrowed = false;

    let (snapshot, source, saved_at) = match (live_snapshot, saved) {
        (Some(mut live), saved) => {
            let saved_at = match saved {
                Ok((from_file, saved_at)) => {
                    // The live paths carry vitals now, so the file is a
                    // backstop rather than the source: it covers any field the
                    // server did not answer for and leaves the rest alone.
                    vitals_borrowed = live.vitals.fill_gaps_from(&from_file.vitals);
                    saved_at
                }
                Err(_) => None,
            };
            (live, "live", saved_at)
        }
        (None, Ok((from_file, saved_at))) => (from_file, "saved", saved_at),
        (None, Err(e)) => {
            return Err(match e {
                InventoryError::Missing => AppError::NotFound(
                    "This player has no saved data on the server yet".to_string(),
                ),
                InventoryError::Unreadable(why) => {
                    log::error!("console: cannot read playerdata for {uuid}: {why}");
                    AppError::Internal("Could not read this player's saved data".to_string())
                }
            })
        }
    };

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "uuid": uuid,
        "source": source,
        // Separate from `source` because the two can disagree: a live inventory
        // whose health reply did not parse is "live" with "mixed" vitals, and
        // the bars are the one place where that difference matters.
        "vitalsSource": match (source, vitals_borrowed) {
            ("live", false) => "live",
            ("live", true) => "mixed",
            _ => "saved",
        },
        "savedAt": saved_at,
        "liveError": live_error,
        "items": snapshot.item_count(),
        "inventory": snapshot,
    })))
}

/// GET /api/admin/console/item-texture/{namespace}/{name} — item artwork.
///
/// Cached on disk after the first request, so this is a file read in the steady
/// state. A miss is a 404 rather than a placeholder image: the panel already
/// draws its own fallback tile, and a real 404 lets the browser's cache
/// remember the gap.
///
/// Note the deliberately absent `.png`. The content type is what identifies the
/// payload, and an extension in the path only invites a reverse proxy to treat
/// the URL as a static asset — a `location ~* \.png$` rule in nginx takes
/// precedence over a `location /api/` prefix, which would quietly answer every
/// one of these from the web root instead of forwarding it here. A trailing
/// extension is still accepted so an older cached page keeps working.
async fn item_texture(
    state: web::Data<AppState>,
    _admin: AdminUser,
    path: web::Path<(String, String)>,
) -> Result<HttpResponse, AppError> {
    let (namespace, file) = path.into_inner();
    let name = file.trim_end_matches(".png");

    match state.textures.get(&namespace, name).await {
        Ok(bytes) => Ok(HttpResponse::Ok()
            .content_type("image/png")
            // Private because the route is behind an admin session, so a shared
            // cache must not hold it. Immutable because a texture for a given
            // Minecraft version never changes — which is what keeps an
            // inventory full of images from costing an auth query per slot on
            // every open.
            .insert_header(("Cache-Control", "private, max-age=2592000, immutable"))
            .body(bytes)),
        Err(TextureError::BadId) => Err(AppError::BadRequest("Not a valid item id".to_string())),
        Err(TextureError::Missing) => {
            Err(AppError::NotFound("No texture for that item".to_string()))
        }
        // Deliberately not a 404. The panel falls back to its initials tile
        // either way, but a wall of 404s reads as "these items have no art"
        // while a wall of 500s carrying this message reads as "this server
        // cannot reach the mirror" — and only one of those is something an
        // operator can act on.
        Err(TextureError::Unreachable(why)) => Err(AppError::Internal(format!(
            "Could not reach the texture mirror: {why}"
        ))),
        Err(TextureError::Cache(why)) => {
            log::error!("textures: cache is not writable: {why}");
            Err(AppError::Internal("Texture cache is unavailable".to_string()))
        }
    }
}

#[derive(Deserialize)]
struct PlayerActionBody {
    player: String,
    #[serde(default)]
    reason: Option<String>,
}

/// POST /api/admin/console/players/{action} — op, kick, ban and friends.
async fn player_action(
    state: web::Data<AppState>,
    admin: AdminUser,
    path: web::Path<String>,
    body: web::Json<PlayerActionBody>,
) -> Result<HttpResponse, AppError> {
    let action = PlayerAction::parse(&path).ok_or_else(|| {
        AppError::BadRequest(format!(
            "Unknown player action; expected one of {}",
            PlayerAction::NAMES.join(", ")
        ))
    })?;

    let player = body.player.trim();
    if !players::is_valid_name(player) {
        return Err(AppError::BadRequest(
            "That is not a valid Minecraft username".to_string(),
        ));
    }

    let command = players::command_for(action, player, body.reason.as_deref());

    // Who did what to whom.
    log::info!("console player action by {}: {}", admin.username, command);
    let record =
        audit_or_refuse(&state, &admin, Kind::Player, &command, Some(player)).await?;

    let output = audit::settle(
        &state.pool,
        &state.console.audit,
        record,
        run_rcon(&state, &command).await,
    )
    .await?;

    // The roster is about to be out of date by exactly the change just made.
    state.snapshot.invalidate().await;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "command": command,
        "output": strip_ansi(output.trim()),
    })))
}

#[derive(Deserialize)]
struct OfflineVitalBody {
    /// "heal", "feed" or "starve". Not "kill" — see below.
    action: String,
}

/// POST /api/admin/console/players/{uuid}/vitals — heal, feed or starve someone
/// who is not online.
///
/// The online buttons go through RCON, and cannot work here: every Minecraft
/// command that touches health or hunger selects a loaded entity, and an
/// offline player is not one. So this edits their `playerdata` file directly,
/// which is the same file the Overview already reads — for anyone offline, that
/// file *is* the player.
///
/// Kill has no offline form and is deliberately absent. Heal, feed and starve
/// describe a state to leave someone in; killing describes something that
/// happens to a player who is there to have it happen to them.
async fn offline_vitals(
    state: web::Data<AppState>,
    admin: AdminUser,
    path: web::Path<String>,
    body: web::Json<OfflineVitalBody>,
) -> Result<HttpResponse, AppError> {
    let uuid = path.into_inner().trim().to_ascii_lowercase();

    // Becomes a filename, so anything that could not name a player file is
    // refused outright rather than cleaned up into one that could.
    if !inventory::is_canonical_uuid(&uuid) {
        return Err(AppError::BadRequest(
            "That is not a valid player UUID".to_string(),
        ));
    }

    let vital = inventory::OfflineVital::parse(&body.action).ok_or_else(|| {
        AppError::BadRequest(
            "Unknown action; expected one of heal, feed, starve".to_string(),
        )
    })?;

    // Refused rather than attempted for an online player. The server holds
    // their state in memory and writes the file at the next autosave, so the
    // edit would apply, disappear minutes later, and look like a bug in this
    // endpoint rather than the misuse it is.
    let (online, _) = online_now(&state).await;
    let roster = players::load_roster(&state.config.server_properties_path, &online).await;
    if roster
        .iter()
        .any(|player| player.uuid.as_deref() == Some(uuid.as_str()) && player.online)
    {
        return Err(AppError::BadRequest(
            "That player is online — use the buttons on a running server, which take effect \
             immediately. Editing their saved file would be overwritten at the next autosave."
                .to_string(),
        ));
    }

    log::info!(
        "console offline vitals by {}: {} {}",
        admin.username,
        vital.as_str(),
        uuid
    );
    // The command column holds what was done, not a command string — there is
    // no command here, which is the point of recording it the same way.
    let record = audit_or_refuse(
        &state,
        &admin,
        Kind::Vitals,
        &format!("{} (offline, saved file)", vital.as_str()),
        Some(&uuid),
    )
    .await?;

    let edit = inventory::apply_offline_vital(
        &state.config.server_properties_path,
        &uuid,
        vital,
    )
    .await
    .map_err(|e| match e {
        InventoryError::Missing => {
            AppError::NotFound("This player has no saved data on the server yet".to_string())
        }
        InventoryError::Unreadable(why) => {
            log::error!("console: offline vitals for {uuid} failed: {why}");
            // The message is written for an operator to act on, so it is
            // passed through rather than replaced with a generic failure.
            AppError::BadRequest(why)
        }
    });

    let snapshot = audit::settle(&state.pool, &state.console.audit, record, edit).await?;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "uuid": uuid,
        "action": vital.as_str(),
        "vitals": snapshot.vitals,
    })))
}

#[derive(Deserialize)]
struct AuditQuery {
    limit: Option<u32>,
    /// Narrow to one operator, which is the shape the question gets asked in.
    user: Option<String>,
}

/// GET /api/admin/console/audit — who ran what, newest first.
///
/// The live stream carries attribution as it happens; this is the same record
/// afterwards, which is when it is actually needed. Read-only and admin-only:
/// there is deliberately no route that edits or deletes an entry, because a
/// record an operator can revise is not one that settles an argument about an
/// operator.
async fn audit_history(
    state: web::Data<AppState>,
    _admin: AdminUser,
    query: web::Query<AuditQuery>,
) -> Result<HttpResponse, AppError> {
    let user = query
        .user
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty());

    let entries = audit::recent(&state.pool, query.limit, user)
        .await
        .map_err(|e| {
            log::error!("console: cannot read the audit log: {e}");
            AppError::Internal("Could not read the audit log".to_string())
        })?;

    Ok(HttpResponse::Ok().json(serde_json::json!({ "entries": entries })))
}

/// GET /api/admin/console/download — the raw `latest.log`.
async fn download_log(
    req: actix_web::HttpRequest,
    state: web::Data<AppState>,
    _admin: AdminUser,
) -> Result<HttpResponse, AppError> {
    let path = &state.config.minecraft_log_path;

    // `NamedFile` streams the file rather than buffering it. On a busy server
    // `latest.log` reaches hundreds of megabytes, so reading it into a Vec
    // would spike memory by that much per concurrent download.
    let file = actix_files::NamedFile::open_async(path).await.map_err(|e| {
        log::error!("console: cannot open {}: {}", path, e);
        AppError::NotFound("Log file is not available".to_string())
    })?;

    Ok(file
        // `mime` reached through mime_guess, which already ships it, rather
        // than adding a direct dependency for one constant.
        .set_content_type(mime_guess::mime::TEXT_PLAIN_UTF_8)
        .set_content_disposition(actix_web::http::header::ContentDisposition {
            disposition: actix_web::http::header::DispositionType::Attachment,
            parameters: vec![actix_web::http::header::DispositionParam::Filename(
                "latest.log".to_string(),
            )],
        })
        .into_response(&req))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/admin/console")
            .route("/stream", web::get().to(stream_log))
            .route("/command", web::post().to(run_command))
            .route("/download", web::get().to(download_log))
            .route("/stats", web::get().to(server_stats))
            .route("/audit", web::get().to(audit_history))
            .route("/online", web::get().to(online_players))
            .route("/players", web::get().to(list_players))
            // Three segments, so it cannot be swallowed by the two-segment
            // `{action}` route below.
            .route("/players/{uuid}/inventory", web::get().to(player_inventory))
            // Before `/players/{action}` so a UUID cannot be read as an action.
            .route("/players/{uuid}/vitals", web::post().to(offline_vitals))
            .route(
                "/item-texture/{namespace}/{name}",
                web::get().to(item_texture),
            )
            .route("/players/{action}", web::post().to(player_action))
            // Registered before the `{action}` catch-all so "status" is not
            // swallowed by it.
            .route("/power/status", web::get().to(power_status))
            .route("/power/{action}", web::post().to(power)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::LogHub;

    fn err_message(e: AppError) -> String {
        e.to_string()
    }

    /// Pull the next frame, failing rather than hanging if none arrives.
    async fn next_frame<S>(stream: &mut std::pin::Pin<Box<S>>) -> String
    where
        S: futures_util::Stream<Item = Result<web::Bytes, actix_web::Error>> + ?Sized,
    {
        let frame = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("a frame within 2s")
            .expect("stream not exhausted")
            .expect("frame is not an error");
        String::from_utf8(frame.to_vec()).expect("frames are utf-8")
    }

    #[test]
    fn accepts_a_plain_command() {
        assert_eq!(validate_command("say hello").unwrap(), "say hello");
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(validate_command("  list  ").unwrap(), "list");
    }

    #[test]
    fn strips_the_leading_slash() {
        // Operators type "/list" out of habit; RCON wants "list".
        assert_eq!(validate_command("/list").unwrap(), "list");
        assert_eq!(validate_command("  /say hi ").unwrap(), "say hi");
    }

    #[test]
    fn rejects_empty_and_whitespace_only() {
        assert!(matches!(validate_command(""), Err(AppError::BadRequest(_))));
        assert!(matches!(
            validate_command("   "),
            Err(AppError::BadRequest(_))
        ));
    }

    #[test]
    fn rejects_control_characters() {
        // A newline would let one request smuggle a second command through.
        let err = validate_command("say hi\nop attacker").unwrap_err();
        assert!(matches!(err, AppError::BadRequest(_)));
        assert!(err_message(err).contains("control characters"));
    }

    #[test]
    fn rejects_oversized_commands() {
        let long = "a".repeat(513);
        assert!(matches!(
            validate_command(&long),
            Err(AppError::BadRequest(_))
        ));
        // The boundary itself is allowed.
        assert!(validate_command(&"a".repeat(512)).is_ok());
    }

    #[test]
    fn length_is_measured_in_characters_not_bytes() {
        // 300 multi-byte characters is well under the limit, but would exceed
        // it if the check counted bytes.
        let multibyte = "é".repeat(300);
        assert!(validate_command(&multibyte).is_ok());
    }

    /* ------------------------------------------------ attribution channel */

    /// The property the whole audit design rests on: an operator's identity
    /// arrives on a channel that server output cannot reach.
    ///
    /// Anyone who can make the server print a line — `/say`, or just typing in
    /// chat — can print one that *reads* like an attribution. On a single
    /// channel that would be indistinguishable from a real one, and forging an
    /// accusation would be a chat message. Here, a log line that impersonates
    /// an audit entry word for word still comes out as an unnamed frame.
    #[tokio::test]
    async fn a_forged_log_line_cannot_arrive_as_an_audit_entry() {
        let log = LogHub::new(10);
        let audit = LogHub::new(10);

        let (backlog, rx) = log.subscribe();
        let mut stream = Box::pin(log_stream(
            backlog,
            rx,
            Some(audit.subscribe()),
            Duration::from_secs(30),
        ));
        let _head = next_frame(&mut stream).await;

        // Someone in chat doing their best impression of an audit entry.
        let forgery = r#"{"username":"someone-else","command":"ban Steve"}"#;
        log.push(format!("[12:00:00] [Server thread/INFO]: <Mallory> {forgery}"));

        let frame = next_frame(&mut stream).await;
        assert!(
            !frame.starts_with("event: "),
            "server output must stay unnamed: {frame}"
        );
        assert!(!frame.contains("event: audit"), "and must not be an audit frame");
        assert!(frame.contains(forgery), "but is still shown as the log line it is");
    }

    #[tokio::test]
    async fn an_audit_entry_arrives_on_its_own_named_event() {
        let log = LogHub::new(10);
        let audit = LogHub::new(10);

        let (backlog, rx) = log.subscribe();
        let mut stream = Box::pin(log_stream(
            backlog,
            rx,
            Some(audit.subscribe()),
            Duration::from_secs(30),
        ));
        let _head = next_frame(&mut stream).await;

        audit.push(r#"{"username":"bs","command":"ban Steve"}"#);

        let frame = next_frame(&mut stream).await;
        assert_eq!(
            frame,
            "event: audit\ndata: {\"username\":\"bs\",\"command\":\"ban Steve\"}\n\n"
        );
    }

    /// A console opened after the fact still shows what it missed, so an
    /// operator joining mid-argument sees the actions rather than only the
    /// next one.
    #[tokio::test]
    async fn the_opening_frame_replays_attribution_alongside_the_log() {
        let log = LogHub::new(10);
        let audit = LogHub::new(10);

        log.push("[12:00:00] [Server thread/INFO]: Done");
        audit.push(r#"{"username":"bs","command":"ban Steve"}"#);

        let (backlog, rx) = log.subscribe();
        let mut stream = Box::pin(log_stream(
            backlog,
            rx,
            Some(audit.subscribe()),
            Duration::from_secs(30),
        ));

        let head = next_frame(&mut stream).await;
        assert!(head.contains("data: [12:00:00] [Server thread/INFO]: Done\n\n"));
        assert!(head.contains("event: audit\ndata: {\"username\":\"bs\""));
    }

    /// Both channels feed one stream, and neither starves the other.
    #[tokio::test]
    async fn log_and_audit_lines_interleave() {
        let log = LogHub::new(10);
        let audit = LogHub::new(10);

        let (backlog, rx) = log.subscribe();
        let mut stream = Box::pin(log_stream(
            backlog,
            rx,
            Some(audit.subscribe()),
            Duration::from_secs(30),
        ));
        let _head = next_frame(&mut stream).await;

        audit.push(r#"{"id":1}"#);
        assert!(next_frame(&mut stream).await.starts_with("event: audit\n"));

        log.push("[12:00:01] [Server thread/INFO]: still here");
        let frame = next_frame(&mut stream).await;
        assert!(!frame.starts_with("event: "), "{frame}");

        audit.push(r#"{"id":2}"#);
        assert!(next_frame(&mut stream).await.starts_with("event: audit\n"));
    }

    /// Losing attribution must not take the console with it. Dropping the hub
    /// closes the channel; the log stream carries on.
    #[tokio::test]
    async fn the_log_survives_the_audit_channel_closing() {
        let log = LogHub::new(10);
        let audit = LogHub::new(10);

        let (backlog, rx) = log.subscribe();
        let subscription = audit.subscribe();
        let mut stream = Box::pin(log_stream(
            backlog,
            rx,
            Some(subscription),
            Duration::from_secs(30),
        ));
        let _head = next_frame(&mut stream).await;

        drop(audit);

        // One comment frame noting the loss...
        assert_eq!(next_frame(&mut stream).await, ": audit closed\n\n");

        // ...and then the log continues, rather than the stream ending or
        // spinning on a channel that will never produce anything again.
        log.push("[12:00:02] [Server thread/INFO]: still here");
        assert_eq!(
            next_frame(&mut stream).await,
            "data: [12:00:02] [Server thread/INFO]: still here\n\n"
        );
    }

    #[tokio::test]
    async fn stream_opens_with_the_backlog_then_follows_live_lines() {
        let hub = LogHub::new(10);
        hub.push("[12:00:00] [Server thread/INFO]: Done");
        hub.push("[12:00:01] [Server thread/WARN]: Slow tick");

        let (backlog, rx) = hub.subscribe();
        let mut stream = Box::pin(log_stream(backlog, rx, None, Duration::from_secs(30)));

        // The whole replay buffer arrives as one opening frame.
        let head = next_frame(&mut stream).await;
        assert_eq!(
            head,
            "data: [12:00:00] [Server thread/INFO]: Done\n\n\
             data: [12:00:01] [Server thread/WARN]: Slow tick\n\n"
        );

        hub.push("[12:00:02] [Server thread/INFO]: Player joined");
        assert_eq!(
            next_frame(&mut stream).await,
            "data: [12:00:02] [Server thread/INFO]: Player joined\n\n"
        );
    }

    #[tokio::test]
    async fn stream_emits_a_heartbeat_while_the_log_is_quiet() {
        let hub = LogHub::new(10);
        let (backlog, rx) = hub.subscribe();
        let mut stream = Box::pin(log_stream(backlog, rx, None, Duration::from_millis(50)));

        // Empty backlog still produces an opening frame, just an empty one.
        assert_eq!(next_frame(&mut stream).await, "");
        // Then keepalives, which are comments and so ignored by EventSource.
        assert_eq!(next_frame(&mut stream).await, ": ping\n\n");
        assert_eq!(next_frame(&mut stream).await, ": ping\n\n");
    }

    #[tokio::test]
    async fn stream_reports_when_a_slow_client_falls_behind() {
        let hub = LogHub::new(4);
        let (backlog, rx) = hub.subscribe();

        // Overrun the broadcast buffer before the stream is ever polled.
        for i in 0..2000 {
            hub.push(format!("line {i}"));
        }

        let mut stream = Box::pin(log_stream(backlog, rx, None, Duration::from_secs(30)));
        assert_eq!(next_frame(&mut stream).await, "");

        let frame = next_frame(&mut stream).await;
        assert!(
            frame.starts_with(": lagged "),
            "a lagging client must be told, got {frame:?}"
        );
    }

    #[tokio::test]
    async fn stream_ends_when_the_hub_is_dropped() {
        let hub = LogHub::new(10);
        let (backlog, rx) = hub.subscribe();
        let mut stream = Box::pin(log_stream(backlog, rx, None, Duration::from_secs(30)));

        assert_eq!(next_frame(&mut stream).await, "");
        drop(hub);

        let ended = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("must not hang once the sender is gone");
        assert!(ended.is_none(), "stream should terminate, not stall");
    }

    /// Drives a real actix response so the SSE headers are pinned down. Getting
    /// these wrong fails silently in production: without `X-Accel-Buffering`,
    /// nginx buffers the stream and the console simply never updates.
    #[actix_web::test]
    async fn sse_response_carries_the_headers_proxies_need() {
        use actix_web::{test as actix_test, App};

        let hub = LogHub::new(10);
        hub.push("hello");

        let app = actix_test::init_service(App::new().route(
            "/s",
            // `Arc<LogHub>` is Clone, which an actix handler closure must be;
            // a broadcast Receiver is not, so subscribe per request.
            web::get().to(move || {
                let hub = Arc::clone(&hub);
                async move {
                    let (backlog, rx) = hub.subscribe();
                    sse_response().streaming(log_stream(backlog, rx, None, Duration::from_secs(30)))
                }
            }),
        ))
        .await;

        let resp = actix_test::call_service(
            &app,
            actix_test::TestRequest::get().uri("/s").to_request(),
        )
        .await;

        assert_eq!(resp.status(), actix_web::http::StatusCode::OK);
        let headers = resp.headers();
        assert_eq!(headers.get("content-type").unwrap(), "text/event-stream");
        assert_eq!(headers.get("cache-control").unwrap(), "no-cache");
        assert_eq!(headers.get("x-accel-buffering").unwrap(), "no");
    }

    #[tokio::test]
    async fn stream_escapes_newlines_embedded_in_a_log_line() {
        let hub = LogHub::new(10);
        let (backlog, rx) = hub.subscribe();
        let mut stream = Box::pin(log_stream(backlog, rx, None, Duration::from_secs(30)));
        assert_eq!(next_frame(&mut stream).await, "");

        // A crafted chat message must not be able to terminate the event early
        // and inject a frame of its own.
        hub.push("<player> hi\n\ndata: injected");
        assert_eq!(
            next_frame(&mut stream).await,
            "data: <player> hi\ndata: \ndata: data: injected\n\n"
        );
    }
}
