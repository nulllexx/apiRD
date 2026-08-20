use std::sync::Arc;
use std::time::Duration;

use actix_web::{web, HttpResponse};
use futures_util::StreamExt;
use serde::Deserialize;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;

use crate::console::control::{self, PowerAction};
use crate::console::players::{self, PlayerAction};
use crate::console::stats::{self, STATS_MAX_AGE};
use crate::console::{sse_frame, strip_ansi, LogSource};
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

fn log_stream(
    backlog: Vec<Arc<str>>,
    rx: broadcast::Receiver<Arc<str>>,
    heartbeat: Duration,
) -> impl futures_util::Stream<Item = Result<web::Bytes, actix_web::Error>> {
    // The whole replay buffer goes out as one frame so a reconnecting client
    // repaints in a single pass rather than a few hundred.
    let head = web::Bytes::from(backlog.iter().map(|line| sse_frame(line)).collect::<String>());

    let live = futures_util::stream::unfold(rx, move |mut rx| async move {
        // `timeout` doubles as the heartbeat clock, which avoids pulling in
        // tokio-stream or hand-rolling a select! just for a keepalive.
        let frame = match tokio::time::timeout(heartbeat, rx.recv()).await {
            Ok(Ok(line)) => web::Bytes::from(sse_frame(&line)),
            // Surfaced rather than swallowed, so a slow client can see that it
            // missed lines instead of silently believing the log went quiet.
            Ok(Err(RecvError::Lagged(n))) => web::Bytes::from(format!(": lagged {n}\n\n")),
            Ok(Err(RecvError::Closed)) => return None,
            Err(_elapsed) => web::Bytes::from_static(b": ping\n\n"),
        };
        Some((Ok::<_, actix_web::Error>(frame), rx))
    });

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
    sse_response().streaming(log_stream(backlog, rx, HEARTBEAT))
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

    // Audit trail: who ran what. There is deliberately no denylist — the route
    // is admin-only, and operators need the full command set.
    log::info!("console command by {}: {}", admin.username, command);

    let output = run_rcon(&state, &command).await?;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "command": command,
        "output": strip_ansi(output.trim()),
    })))
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

    control::request(&state.config.control_dir, action)
        .await
        .map_err(|e| {
            log::error!("console: cannot queue {} for the sidecar: {}", action.as_str(), e);
            AppError::Internal("Server control is unavailable".to_string())
        })?;

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
    match &online.rcon_error {
        None => state.presence.replace(&online.names),
        // Keeps the last known set rather than blanking it; an unreachable
        // server is not an empty one.
        Some(_) => state.presence.record_failure(),
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

    // Audit trail: who did what to whom.
    log::info!("console player action by {}: {}", admin.username, command);

    let output = run_rcon(&state, &command).await?;

    // The roster is about to be out of date by exactly the change just made.
    state.snapshot.invalidate().await;

    Ok(HttpResponse::Ok().json(serde_json::json!({
        "command": command,
        "output": strip_ansi(output.trim()),
    })))
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
            .route("/online", web::get().to(online_players))
            .route("/players", web::get().to(list_players))
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

    #[tokio::test]
    async fn stream_opens_with_the_backlog_then_follows_live_lines() {
        let hub = LogHub::new(10);
        hub.push("[12:00:00] [Server thread/INFO]: Done");
        hub.push("[12:00:01] [Server thread/WARN]: Slow tick");

        let (backlog, rx) = hub.subscribe();
        let mut stream = Box::pin(log_stream(backlog, rx, Duration::from_secs(30)));

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
        let mut stream = Box::pin(log_stream(backlog, rx, Duration::from_millis(50)));

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

        let mut stream = Box::pin(log_stream(backlog, rx, Duration::from_secs(30)));
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
        let mut stream = Box::pin(log_stream(backlog, rx, Duration::from_secs(30)));

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
                    sse_response().streaming(log_stream(backlog, rx, Duration::from_secs(30)))
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
        let mut stream = Box::pin(log_stream(backlog, rx, Duration::from_secs(30)));
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
