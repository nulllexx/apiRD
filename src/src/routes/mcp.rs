//! HTTP side of the MCP server (see `crate::mcp` for the rules).
//!
//! * `POST /api/mcp` — the MCP endpoint (Streamable HTTP transport, JSON
//!   replies, no sessions). Authenticated by access token only.
//! * `GET` / `DELETE /api/mcp` — 405: there is no server-to-client stream
//!   and no session to end.
//! * `/api/account/mcp…` — the signed-in account's own tokens and activity,
//!   for the admin panel's AI Access tab. Authenticated by session cookie
//!   only.
//!
//! The two halves never accept each other's credential. A token cannot
//! manage tokens, so a leaked one cannot mint a replacement or outlive its
//! own revocation; a session cannot call tools, so a web page cannot use a
//! signed-in browser to reach them.

use actix_web::http::{header, StatusCode};
use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::access::{parse_permissions, store, Permission, PermissionSet};
use crate::error::AppError;
use crate::mcp::protocol::{self, RpcError};
use crate::mcp::token::{self, Refusal, TokenContext};
use crate::mcp::{self as mcp, audit, tools};
use crate::middleware::permission::{Principal, Require};
use crate::middleware::rate_limit::RateLimiter;
use crate::access::perm;
use crate::AppState;

const REALM: &str = "raindrippy-mcp";

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::resource("/mcp")
            .route(web::post().to(endpoint))
            .route(web::get().to(no_stream))
            .route(web::delete().to(no_stream)),
    )
    .service(
        web::scope("/account/mcp")
            .route("", web::get().to(overview))
            .route("/tokens", web::post().to(create_token))
            .route("/tokens/{id}", web::delete().to(revoke_token))
            .route("/activity", web::get().to(activity)),
    );
}

// --------------------------------------------------------------- endpoint

fn plain(status: StatusCode, message: &str) -> HttpResponse {
    HttpResponse::build(status)
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(json!({ "error": message }))
}

/// 401 with the challenge RFC 6750 asks for. `error` is set only when a
/// token was offered, so a client can tell "send one" from "that one is no
/// good".
fn unauthorized(offered: bool) -> HttpResponse {
    let challenge = if offered {
        format!("Bearer realm=\"{REALM}\", error=\"invalid_token\"")
    } else {
        format!("Bearer realm=\"{REALM}\"")
    };
    let message = if offered {
        "This access token is not valid: it may have expired or been revoked."
    } else {
        "An access token is required: send it as \"Authorization: Bearer <token>\"."
    };
    HttpResponse::Unauthorized()
        .insert_header((header::WWW_AUTHENTICATE, challenge))
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(json!({ "error": message }))
}

fn rpc(status: StatusCode, body: Value) -> HttpResponse {
    HttpResponse::build(status)
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(body)
}

/// Checks that need no database, in the order that refuses the most for the
/// least: switched off, a web page, a flood from one address.
fn gate(req: &HttpRequest, state: &AppState, limiter: &RateLimiter) -> Result<(), HttpResponse> {
    if !state.config.mcp_enabled {
        return Err(plain(StatusCode::NOT_FOUND, "Not found"));
    }
    let origin = req.headers().get(header::ORIGIN).map(|v| v.to_str().unwrap_or("null"));
    if !mcp::origin_allowed(origin, &state.config.mcp_allowed_origins) {
        log::warn!("mcp: refused a request from origin {:?}", origin);
        return Err(plain(StatusCode::FORBIDDEN, "Requests from web pages are not accepted"));
    }
    let ip = req.peer_addr().map(|a| a.ip()).unwrap_or_else(|| [127, 0, 0, 1].into());
    if limiter.check(ip).is_err() {
        return Err(plain(StatusCode::TOO_MANY_REQUESTS, "Too many requests, please try again later."));
    }
    Ok(())
}

/// GET and DELETE: the transport lets a server without a stream or sessions
/// answer 405.
async fn no_stream(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
) -> HttpResponse {
    if let Err(resp) = gate(&req, &state, &limiter) {
        return resp;
    }
    HttpResponse::MethodNotAllowed()
        .insert_header((header::ALLOW, "POST"))
        .json(json!({ "error": "This server answers POST only: it keeps no stream or session" }))
}

/// POST /api/mcp — one JSON-RPC message in, one JSON reply out.
///
/// The token is checked before the body is read, so an anonymous caller
/// never gets as far as the parser.
async fn endpoint(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    payload: web::Payload,
) -> HttpResponse {
    if let Err(resp) = gate(&req, &state, &limiter) {
        return resp;
    }

    let offered = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    let Some(raw) = offered.and_then(token::bearer) else {
        return unauthorized(offered.is_some());
    };

    let ctx = match token::authenticate(&state.pool, raw).await {
        Ok(Ok(ctx)) => ctx,
        Ok(Err(Refusal::NoAccess)) => {
            return plain(StatusCode::FORBIDDEN, "This account no longer has the mcp.access permission");
        }
        Ok(Err(why)) => {
            log::info!("mcp: refused a token ({why:?})");
            return unauthorized(true);
        }
        Err(e) => {
            log::error!("mcp: token lookup failed: {e}");
            return plain(StatusCode::SERVICE_UNAVAILABLE, "Server error, try again later");
        }
    };

    if !mcp::token_limiter().allow(ctx.id) {
        return HttpResponse::TooManyRequests()
            .insert_header((header::RETRY_AFTER, "60"))
            .json(json!({ "error": format!("At most {} requests a minute per token", mcp::CALLS_PER_MINUTE) }));
    }

    if let Some(version) = req.headers().get("mcp-protocol-version") {
        let version = version.to_str().unwrap_or("");
        if !protocol::version_supported(version) {
            return plain(
                StatusCode::BAD_REQUEST,
                &format!("Unsupported MCP-Protocol-Version; this server speaks {}", protocol::SUPPORTED_VERSIONS.join(", ")),
            );
        }
    }

    let body = match payload.to_bytes_limited(mcp::MAX_BODY_BYTES).await {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => return plain(StatusCode::BAD_REQUEST, "The request body could not be read"),
        Err(_) => return plain(StatusCode::PAYLOAD_TOO_LARGE, "The message is too large"),
    };

    token::touch(&state.pool, ctx.id).await;

    match protocol::parse(&body) {
        Err(e) => rpc(StatusCode::BAD_REQUEST, protocol::failure(&Value::Null, &e)),
        Ok(protocol::Message::Notification { .. }) | Ok(protocol::Message::Response) => {
            HttpResponse::Accepted().finish()
        }
        Ok(protocol::Message::Request { id, method, params }) => {
            let reply = dispatch(&req, &state, &limiter, &ctx, &id, &method, params).await;
            rpc(StatusCode::OK, reply)
        }
    }
}

async fn dispatch(
    req: &HttpRequest,
    state: &web::Data<AppState>,
    limiter: &web::Data<RateLimiter>,
    ctx: &TokenContext,
    id: &Value,
    method: &str,
    params: Value,
) -> Value {
    let fail = |code, message: String| protocol::failure(id, &RpcError::new(code, message));
    match method {
        "initialize" => protocol::success(
            id,
            protocol::initialize_result(params.get("protocolVersion").and_then(Value::as_str)),
        ),
        "ping" => protocol::success(id, json!({})),
        // Small enough to need no pagination; a cursor is ignored.
        "tools/list" => {
            protocol::success(id, json!({ "tools": tools::list(&ctx.principal.permissions) }))
        }
        "tools/call" => {
            let Some(name) = params.get("name").and_then(Value::as_str) else {
                return fail(protocol::INVALID_PARAMS, "params.name is required".to_string());
            };
            let arguments = match params.get("arguments") {
                None | Some(Value::Null) => json!({}),
                Some(a @ Value::Object(_)) => a.clone(),
                Some(_) => return fail(protocol::INVALID_PARAMS, "arguments must be an object".to_string()),
            };
            let call_ctx = tools::Ctx { req, state, limiter, token: ctx };
            match tools::call(&call_ctx, name, arguments).await {
                Ok(outcome) => protocol::success(id, protocol::tool_result(outcome, mcp::MAX_RESULT_BYTES)),
                Err(tools::UnknownTool) => {
                    fail(protocol::INVALID_PARAMS, format!("Unknown tool: {}", audit::clip(name, 64)))
                }
            }
        }
        _ => fail(protocol::METHOD_NOT_FOUND, format!("Method not found: {}", audit::clip(method, 64))),
    }
}

// ------------------------------------------------------- token management

/// A token as its owner sees it in the panel.
fn token_json(row: &TokenRow, account: &PermissionSet) -> Value {
    let stored = token::decode_permissions(&row.permissions);
    let effective = account.restricted_to(stored.iter().copied());
    let status = if row.live == 0 {
        "expired"
    } else if row.token_session != row.account_session {
        "signed_out"
    } else if !account.has(Permission::McpAccess) {
        "no_access"
    } else {
        "active"
    };
    json!({
        "id": row.id,
        "name": row.name,
        "hint": row.hint,
        "permissions": stored.iter().map(|p| p.as_str()).collect::<Vec<_>>(),
        "effective": crate::access::permission_names(&effective),
        "createdAt": audit::rfc3339(row.created_at),
        "expiresAt": audit::rfc3339(row.expires_at),
        "lastUsedAt": row.last_used_at.map(audit::rfc3339),
        "status": status,
    })
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    id: i64,
    name: String,
    hint: String,
    permissions: String,
    created_at: chrono::NaiveDateTime,
    expires_at: chrono::NaiveDateTime,
    last_used_at: Option<chrono::NaiveDateTime>,
    live: i64,
    token_session: u32,
    account_session: u32,
}

async fn tokens_of(pool: &sqlx::MySqlPool, user_id: &str) -> Result<Vec<TokenRow>, sqlx::Error> {
    sqlx::query_as(
        r#"SELECT t.id, t.name, t.hint, t.permissions, t.created_at, t.expires_at, t.last_used_at,
                  CAST(t.expires_at > UTC_TIMESTAMP(3) AS SIGNED) AS live,
                  t.session_version AS token_session, u.session_version AS account_session
           FROM mcp_tokens t JOIN users u ON u.id = t.user_id
           WHERE t.user_id = ? ORDER BY t.id DESC"#,
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// GET /api/account/mcp — the caller's tokens, what a new one could be given,
/// and whether the endpoint is on.
async fn overview(state: web::Data<AppState>, principal: Principal) -> Result<HttpResponse, AppError> {
    let tokens: Vec<Value> = tokens_of(&state.pool, &principal.id)
        .await?
        .iter()
        .map(|row| token_json(row, &principal.permissions))
        .collect();

    let grantable: Vec<Value> = tools::grantable()
        .into_iter()
        .filter(|p| principal.has(*p))
        .map(|p| json!({ "permission": p.as_str(), "description": p.description(), "tools": tools::unlocked_by(p) }))
        .collect();
    let always: Vec<&str> = tools::TOOLS.iter().filter(|t| t.permission.is_none()).map(|t| t.name).collect();

    Ok(HttpResponse::Ok().insert_header((header::CACHE_CONTROL, "no-store")).json(json!({
        "enabled": state.config.mcp_enabled,
        "canCreate": principal.has(Permission::McpAccess),
        "endpoint": "/api/mcp",
        "grantable": grantable,
        "alwaysAvailable": always,
        "tokens": tokens,
        "limits": {
            "maxTokens": token::MAX_PER_ACCOUNT,
            "maxDays": token::MAX_DAYS,
            "defaultDays": token::DEFAULT_DAYS,
        },
    })))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateBody {
    name: String,
    #[serde(default)]
    permissions: Vec<String>,
    #[serde(rename = "expiresInDays")]
    expires_in_days: Option<u32>,
    /// Required when the account has one: a token adds a way in, so a stolen
    /// session alone must not be enough to make one -- the rule linking
    /// Google follows too.
    password: Option<String>,
}

/// POST /api/account/mcp/tokens — make a token. Answers with it once.
async fn create_token(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    actor: Require<perm::McpAccess>,
    body: web::Json<CreateBody>,
) -> Result<HttpResponse, AppError> {
    crate::routes::console::require_panel_header(&req, "Access tokens can only be created from the admin panel")?;
    let ip = req.peer_addr().map(|a| a.ip()).unwrap_or_else(|| [127, 0, 0, 1].into());
    limiter.check(ip)?;

    if !state.config.mcp_enabled {
        return Err(AppError::ServiceUnavailable(
            "MCP is turned off on this server (MCP_ENABLED), so no tokens can be made".to_string(),
        ));
    }

    let body = body.into_inner();
    let name = token::clean_name(&body.name).map_err(AppError::BadRequest)?;
    let wanted = parse_permissions(&body.permissions).map_err(AppError::BadRequest)?;
    let grantable = tools::grantable();
    if let Some(p) = wanted.iter().find(|p| !grantable.contains(p)) {
        return Err(AppError::BadRequest(format!("{p} does not unlock any MCP tool")));
    }
    let wanted_set = PermissionSet::from_permissions(wanted.iter().copied());
    if !actor.permissions.covers(&wanted_set) {
        let missing: Vec<&str> =
            actor.permissions.missing_from(&wanted_set).into_iter().map(Permission::as_str).collect();
        return Err(AppError::Forbidden(format!(
            "A token can only hold permissions you hold yourself; you lack {}",
            missing.join(", ")
        )));
    }
    let days = body.expires_in_days.unwrap_or(token::DEFAULT_DAYS);
    if !(1..=token::MAX_DAYS).contains(&days) {
        return Err(AppError::BadRequest(format!("A token can last 1 to {} days", token::MAX_DAYS)));
    }

    let (password_hash, session_version): (String, u32) =
        sqlx::query_as("SELECT password_hash, session_version FROM users WHERE id = ?")
            .bind(&actor.id)
            .fetch_optional(&state.pool)
            .await?
            .ok_or_else(|| AppError::Unauthorized("Unknown account".to_string()))?;
    crate::routes::oauth_google::confirm_password(&password_hash, body.password.as_deref())?;

    let (held,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM mcp_tokens WHERE user_id = ?")
        .bind(&actor.id)
        .fetch_one(&state.pool)
        .await?;
    if held >= token::MAX_PER_ACCOUNT {
        return Err(AppError::Conflict(format!(
            "You already have {} tokens; revoke one first",
            token::MAX_PER_ACCOUNT
        )));
    }

    let issued = token::generate();
    let permissions = token::encode_permissions(&wanted);
    let id = sqlx::query(
        r#"INSERT INTO mcp_tokens
               (user_id, name, token_hash, hint, permissions, session_version, created_at, expires_at)
           VALUES (?, ?, ?, ?, ?, ?, UTC_TIMESTAMP(3), DATE_ADD(UTC_TIMESTAMP(3), INTERVAL ? DAY))"#,
    )
    .bind(&actor.id)
    .bind(&name)
    .bind(&issued.hash)
    .bind(&issued.hint)
    .bind(&permissions)
    .bind(session_version)
    .bind(days)
    .execute(&state.pool)
    .await?
    .last_insert_id() as i64;

    let shown = if permissions.is_empty() { "no permissions".to_string() } else { permissions.clone() };
    store::record(
        &state.pool,
        &actor.username,
        "mcp.token.create",
        &actor.username,
        Some(&format!("\"{name}\" ({}…), {shown}, {days} days", issued.hint)),
    )
    .await?;
    log::info!("mcp: {} created token {id} \"{name}\" ({shown}, {days} days)", actor.username);

    let row = tokens_of(&state.pool, &actor.id)
        .await?
        .into_iter()
        .find(|r| r.id == id)
        .ok_or_else(|| AppError::Internal("The new token could not be read back".to_string()))?;
    let mut out = token_json(&row, &actor.permissions);
    out["token"] = json!(issued.token);

    Ok(HttpResponse::Created().insert_header((header::CACHE_CONTROL, "no-store")).json(out))
}

/// DELETE /api/account/mcp/tokens/{id} — revoke one of the caller's tokens.
///
/// Needs only a session, not `mcp.access`: someone whose access was taken
/// away should still be able to clear out what they left behind.
async fn revoke_token(
    req: HttpRequest,
    state: web::Data<AppState>,
    principal: Principal,
    path: web::Path<i64>,
) -> Result<HttpResponse, AppError> {
    crate::routes::console::require_panel_header(&req, "Access tokens can only be revoked from the admin panel")?;
    let id = path.into_inner();

    let found: Option<(String, String)> =
        sqlx::query_as("SELECT name, hint FROM mcp_tokens WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(&principal.id)
            .fetch_optional(&state.pool)
            .await?;
    let (name, hint) = found.ok_or_else(|| AppError::NotFound("No such token".to_string()))?;

    sqlx::query("DELETE FROM mcp_tokens WHERE id = ? AND user_id = ?")
        .bind(id)
        .bind(&principal.id)
        .execute(&state.pool)
        .await?;
    store::record(
        &state.pool,
        &principal.username,
        "mcp.token.revoke",
        &principal.username,
        Some(&format!("\"{name}\" ({hint}…)")),
    )
    .await?;
    log::info!("mcp: {} revoked token {id} \"{name}\"", principal.username);

    Ok(HttpResponse::Ok().json(json!({ "revoked": id })))
}

#[derive(Debug, Deserialize)]
struct ActivityQuery {
    limit: Option<u32>,
}

/// GET /api/account/mcp/activity — the tools the caller's tokens called.
async fn activity(
    state: web::Data<AppState>,
    principal: Principal,
    query: web::Query<ActivityQuery>,
) -> Result<HttpResponse, AppError> {
    let entries = audit::recent_for(&state.pool, &principal.username, query.limit.unwrap_or(50)).await?;
    Ok(HttpResponse::Ok()
        .insert_header((header::CACHE_CONTROL, "no-store"))
        .json(json!({ "entries": entries })))
}
