//! Integration tests that mount the real production routing (`configure_api`)
//! and drive it through actix's in-process test service.
//!
//! Two tiers:
//!   * **DB-free** tests always run. They prove routing, request parsing, and
//!     error mapping stay wired together — catching the "a route silently
//!     disappeared / stopped compiling" class of regression that a rename or
//!     refactor can introduce.
//!   * **DB-backed** tests run only when `TEST_DATABASE_URL` points at a
//!     throwaway MariaDB (CI provides one; locally they skip with a notice).
//!     They prove the public read endpoints still return their documented
//!     JSON shape after `db::init_database` runs.

use actix_web::{http::StatusCode, test, web, App};
use api_rd::{config::AppConfig, configure_api, AppState};

// ---------------------------------------------------------------------------
// DB-free tests (always run)
// ---------------------------------------------------------------------------

#[actix_web::test]
async fn check_updates_rejects_missing_version() {
    let app = test::init_service(App::new().configure(configure_api)).await;
    let req = test::TestRequest::post()
        .uri("/api/util/check-updates")
        .set_json(serde_json::json!({}))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}

#[actix_web::test]
async fn unknown_route_returns_404() {
    let app = test::init_service(App::new().configure(configure_api)).await;
    let req = test::TestRequest::get()
        .uri("/api/definitely/not/a/real/route")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

/// An `AppState` whose pool is never actually connected.
///
/// `AdminUser` rejects a request with no `userToken` cookie before it issues
/// any query, so the console's auth gate is exercisable without a database.
/// The short acquire timeout matters: if a refactor ever moves a query *ahead*
/// of the cookie check, these tests fail quickly instead of hanging.
fn lazy_state() -> AppState {
    let config = AppConfig::build(|key| match key {
        "DB_HOST" => Some("127.0.0.1".to_string()),
        // Port 1 has nothing on it. In CI, 127.0.0.1:3306 is the real MariaDB
        // service, so a pool aimed there could quietly consume connections from
        // the shared server if a regression ever made these routes query.
        "DB_PORT" => Some("1".to_string()),
        "DB_USER" => Some("test".to_string()),
        "DB_PASSWORD" => Some("test".to_string()),
        "DB_NAME" => Some("apird_test".to_string()),
        "JWT_SECRET" => Some("test-secret".to_string()),
        _ => None,
    })
    .expect("build test config");

    let pool = MySqlPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_secs(2))
        .connect_lazy(&config.database_url())
        .expect("lazy pool never dials on construction");

    AppState {
        pool,
        config,
        player_count: Arc::new(RwLock::new(0)),
        max_players: Arc::new(RwLock::new(20)),
        console: api_rd::console::Consoles::new(64),
        // Points at a dead port; the console routes under test are
        // rejected at the auth layer long before RCON is reached.
        rcon: api_rd::rcon::RconClient::new("127.0.0.1:1".to_string(), String::new()),
        snapshot: api_rd::console::stats::SnapshotCache::new(
            api_rd::console::stats::SNAPSHOT_TTL,
        ),
        presence: api_rd::console::presence::Presence::new(),
        // Fetching disabled and the cache pointed at a path that does not
        // exist: these tests must never reach the network or write a file.
        textures: api_rd::console::textures::TextureCache::new(
            String::from("target/test-texture-cache"),
            String::from("1.21.4"),
            String::new(),
            String::from("target/test-mods"),
        ),
        power_jobs: api_rd::console::control::PowerJobs::new(),
    }
}

/// The console exposes the server's log and an arbitrary-command channel, so
/// the property most worth pinning down is that none of it answers without an
/// admin session.
#[actix_web::test]
async fn console_routes_reject_anonymous_callers() {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(lazy_state()))
            .configure(configure_api),
    )
    .await;

    for uri in [
        "/api/admin/console/stream",
        "/api/admin/console/stream?source=stdout",
        "/api/admin/console/download",
        "/api/admin/console/power/status",
        "/api/admin/console/power/result/069a79f4-44e9-4726-a5be-fca90e38aaf5",
        "/api/admin/console/stats",
        "/api/admin/console/online",
        "/api/admin/console/players",
        // Reads a file off the server's disk, so it must be behind the
        // admin gate exactly like everything else here.
        "/api/admin/console/players/069a79f4-44e9-4726-a5be-fca90e38aaf5/inventory",
        // Fetches from a mirror and writes to disk on a miss, so an
        // anonymous caller must not be able to drive it either. Both the
        // extensionless form the page requests and the legacy .png form
        // an already-cached page may still ask for.
        "/api/admin/console/item-texture/minecraft/diamond_sword",
        "/api/admin/console/item-texture/minecraft/diamond_sword.png",
        // The file browser reads anything under the server directory.
        "/api/admin/console/files",
        "/api/admin/console/files/view?path=server.properties",
        "/api/admin/console/files/download?path=server.properties",
    ] {
        let req = test::TestRequest::get().uri(uri).to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "GET {uri} must require an admin session"
        );
    }

    let req = test::TestRequest::post()
        .uri("/api/admin/console/command")
        .set_json(serde_json::json!({ "command": "list" }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "POST /api/admin/console/command must require an admin session"
    );

    // Writes to the server's files.
    let req = test::TestRequest::put()
        .uri("/api/admin/console/files/content")
        .insert_header(("x-rd-panel", "1"))
        .set_json(serde_json::json!({ "path": "server.properties", "content": "", "version": "" }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "PUT files/content needs a session");

    let req = test::TestRequest::post()
        .uri("/api/admin/console/files/upload?path=config")
        .insert_header(("x-rd-panel", "1"))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "POST files/upload needs a session");

    // Ending a season writes to the public website.
    for (method, uri) in [
        ("GET", "/api/admin/season/current"),
        ("POST", "/api/admin/season/end"),
        ("GET", "/api/admin/season/end/status"),
        ("GET", "/api/admin/season/archives"),
        ("GET", "/api/admin/season/archives/9"),
        ("PUT", "/api/admin/season/archives/9"),
    ] {
        let req = match method {
            "GET" => test::TestRequest::get(),
            "POST" => test::TestRequest::post(),
            _ => test::TestRequest::put(),
        }
        .uri(uri)
        .insert_header(("x-rd-panel", "1"))
        .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {uri} needs a session");
    }

    // Power control reaches the sidecar, so an unauthenticated caller must not
    // be able to queue anything — including an unrecognised verb.
    for action in ["start", "stop", "restart", "bogus"] {
        let req = test::TestRequest::post()
            .uri(&format!("/api/admin/console/power/{action}"))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "POST /api/admin/console/power/{action} must require an admin session"
        );
    }

    // Player actions run privileged commands against the game server. The
    // unrecognised verb is in the list on purpose: rejecting it as *bad input*
    // would confirm the route exists and answers to anonymous callers.
    for action in ["op", "deop", "kick", "ban", "pardon", "clear", "bogus"] {
        let req = test::TestRequest::post()
            .uri(&format!("/api/admin/console/players/{action}"))
            .set_json(serde_json::json!({ "player": "Steve" }))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "POST /api/admin/console/players/{action} must require an admin session"
        );
    }
}

/// Every poll route requires a session, and the two admin ones require an
/// admin session.
///
/// `lazy_state()` points the pool at a dead port, so any route that reached a
/// query before checking the cookie would hang rather than answer — which is
/// exactly the regression this catches. The unknown poll id is deliberate: a
/// 404 here would confirm to an anonymous caller which polls exist.
#[actix_web::test]
async fn poll_routes_reject_anonymous_callers() {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(lazy_state()))
            .configure(configure_api),
    )
    .await;

    for uri in [
        "/api/polls",
        "/api/polls/1",
        "/api/polls/1/access",
        "/api/admin/polls",
        "/api/admin/polls?status=past",
    ] {
        let req = test::TestRequest::get().uri(uri).to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "GET {uri} must require a session"
        );
    }

    // Casting a vote and opening a poll both write. Auth is checked before the
    // body is looked at, so a hostile payload never reaches validation.
    let req = test::TestRequest::post()
        .uri("/api/polls/1/vote")
        .set_json(serde_json::json!({ "optionIds": [1] }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    for body in [
        serde_json::json!({ "title": "x", "duration": "1d",
                            "options": ["a", "b"], "audiences": ["everyone"] }),
        serde_json::json!({ "wrong_field": true }),
    ] {
        let req = test::TestRequest::post()
            .uri("/api/admin/polls")
            .set_json(body)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "POST /api/admin/polls must require an admin session"
        );
    }

    // Ending a poll early is the one destructive verb here.
    let req = test::TestRequest::post()
        .uri("/api/admin/polls/1/end")
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "POST /api/admin/polls/{{id}}/end must require an admin session"
    );
}

/// Neither the chat prompt's admin side nor the chat's side answers without a
/// session, and saving checks auth before it looks at the body.
#[actix_web::test]
async fn chat_prompt_routes_reject_anonymous_callers() {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(lazy_state()))
            .configure(configure_api),
    )
    .await;

    for uri in ["/api/chat-prompt", "/api/admin/chat-prompt"] {
        let req = test::TestRequest::get().uri(uri).to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "GET {uri} must require a session"
        );
    }

    let req = test::TestRequest::post()
        .uri("/api/chat-prompt/seen")
        .set_json(serde_json::json!({ "version": 1 }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let req = test::TestRequest::put()
        .uri("/api/admin/chat-prompt")
        .set_json(serde_json::json!({ "title": "x", "buttons": [{ "label": "y", "action": "nope" }] }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "PUT /api/admin/chat-prompt must require an admin session"
    );
}

/// The player-facing polls page sends anonymous visitors to sign in.
///
/// A redirect rather than a 401 because this is a page a person opened, not a
/// call some script made — and to the ordinary sign-in, not the staff login,
/// since any account may vote.
#[actix_web::test]
async fn the_polls_page_sends_anonymous_visitors_to_sign_in() {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(lazy_state()))
            .configure(configure_api),
    )
    .await;

    for uri in ["/polls", "/polls.html"] {
        let req = test::TestRequest::get().uri(uri).to_request();
        let resp = test::call_service(&app, req).await;

        assert_eq!(
            resp.status(),
            StatusCode::FOUND,
            "GET {uri} must redirect a signed-out visitor"
        );

        let location = resp
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(
            location.contains("/auth.html"),
            "GET {uri} should point at the ordinary sign-in, not {location}"
        );
    }
}

/// Auth is checked before the body is parsed, so a malformed or hostile
/// payload never reaches command validation on an unauthenticated request.
#[actix_web::test]
async fn console_command_checks_auth_before_validating_input() {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(lazy_state()))
            .configure(configure_api),
    )
    .await;

    for body in [
        serde_json::json!({ "command": "" }),
        serde_json::json!({ "wrong_field": "list" }),
    ] {
        let req = test::TestRequest::post()
            .uri("/api/admin/console/command")
            .set_json(body)
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "auth must be decided before the body is considered"
        );
    }
}

/// The role editor, and every account-admin route that used to check the
/// token's `isAdmin` claim, answer nothing without a session — and decide that
/// before reading the body or the database.
#[actix_web::test]
async fn access_routes_reject_anonymous_callers() {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(lazy_state()))
            .app_data(web::Data::new(api_rd::middleware::rate_limit::RateLimiter::new(500, 60)))
            .configure(configure_api),
    )
    .await;

    let hostile = serde_json::json!({ "roles": ["admin"], "permissions": ["roles.manage"],
                                      "name": "pwn", "username": "x", "isAdmin": true });

    let cases: [(&str, &str); 22] = [
        ("GET", "/api/access/me"),
        ("GET", "/api/admin/access/permissions"),
        ("GET", "/api/admin/access/roles"),
        ("POST", "/api/admin/access/roles"),
        ("PATCH", "/api/admin/access/roles/moderator"),
        ("DELETE", "/api/admin/access/roles/moderator"),
        ("GET", "/api/admin/access/roles/admin/members"),
        ("GET", "/api/admin/access/users/someone"),
        ("PUT", "/api/admin/access/users/someone/roles"),
        ("GET", "/api/admin/access/audit"),
        // Formerly gated on the token's own say-so.
        ("POST", "/api/update-admin-status"),
        ("GET", "/api/admin/list-users"),
        ("POST", "/api/admin/moderate"),
        ("POST", "/api/admin/unban"),
        ("DELETE", "/api/admin/delete-user"),
        // Registered as "/api/moderation-list" inside the /api scope.
        ("GET", "/api/api/moderation-list"),
        ("POST", "/api/admin/gen-pwd-reset"),
        ("PATCH", "/api/admin/update-member-status"),
        ("PATCH", "/api/admin/update-proj-status"),
        ("PATCH", "/api/admin/update-chat-status"),
        ("PATCH", "/api/admin/update-og-status"),
        ("PATCH", "/api/history/wiki/edit"),
    ];

    for (method, uri) in cases {
        let req = match method {
            "GET" => test::TestRequest::get(),
            "POST" => test::TestRequest::post(),
            "PATCH" => test::TestRequest::patch(),
            "PUT" => test::TestRequest::put(),
            "DELETE" => test::TestRequest::delete(),
            _ => unreachable!(),
        }
        .uri(uri)
        .set_json(&hostile)
        .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must require a session"
        );
    }
}

/// A token that still says `isAdmin: true` — as every session issued before
/// roles does — is not taken at its word. With no database reachable the
/// request fails rather than being let through on the claim.
#[actix_web::test]
async fn a_legacy_is_admin_claim_grants_nothing_by_itself() {
    use jsonwebtoken::{encode, EncodingKey, Header};

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(lazy_state()))
            .app_data(web::Data::new(api_rd::middleware::rate_limit::RateLimiter::new(500, 60)))
            .configure(configure_api),
    )
    .await;

    let token = encode(
        &Header::default(),
        &serde_json::json!({ "username": "old-admin", "id": "old-admin-id",
                             "isAdmin": true, "isOG": false }),
        &EncodingKey::from_secret(b"test-secret"),
    )
    .unwrap();

    // One route: each request waits out the lazy pool's acquire timeout.
    for uri in ["/api/admin/list-users"] {
        let req = test::TestRequest::get()
            .uri(uri)
            .cookie(actix_web::cookie::Cookie::new("userToken", token.clone()))
            .to_request();
        let resp = test::call_service(&app, req).await;
        assert!(
            !resp.status().is_success(),
            "GET {uri} must not succeed on the token's isAdmin claim alone (got {})",
            resp.status()
        );
    }
}

// ---------------------------------------------------------------------------
// DB-backed tests (run when TEST_DATABASE_URL is set)
// ---------------------------------------------------------------------------

use sqlx::mysql::MySqlPoolOptions;
use sqlx::MySqlPool;
use std::sync::{Arc, RwLock};
use tokio::sync::OnceCell;

/// Guards `init_database` so the schema is created exactly once per test
/// binary, preventing concurrent `CREATE UNIQUE INDEX` races.
static SCHEMA_READY: OnceCell<bool> = OnceCell::const_new();

/// Serializes the DB-backed tests.
///
/// They share a single database and the status endpoint writes to `meta`, so
/// ordering them removes contention as a source of flakiness. There are only
/// two, so the cost is negligible.
static DB_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Prepare the schema once, then hand this test its **own** pool.
///
/// The pool deliberately is not shared between tests. `#[actix_web::test]`
/// builds a fresh runtime per test, and a sqlx pool binds each connection --
/// and the I/O driver polling its socket -- to the runtime that opened it. Once
/// that runtime is dropped at the end of its test, any connection it left in
/// the pool is inert: a later test acquires it, waits on a socket nobody is
/// polling, and blocks until `acquire_timeout` expires. The handler then maps
/// that to a generic 500, which is what made this look like a database fault
/// rather than a harness one. Symptom to recognise: the pool reports idle
/// connections available while `acquire()` times out anyway.
async fn shared_pool() -> Option<MySqlPool> {
    let url = std::env::var("TEST_DATABASE_URL").ok()?;

    // Uses a throwaway pool that is closed before returning, so no connection
    // from this runtime is ever left behind for another test to pick up.
    let ready = SCHEMA_READY
        .get_or_init(|| async {
            let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
                return false;
            };
            let pool = MySqlPoolOptions::new()
                .max_connections(1)
                .acquire_timeout(std::time::Duration::from_secs(10))
                .connect(&url)
                .await
                .expect("connect to TEST_DATABASE_URL");
            api_rd::db::init_database(&pool)
                .await
                .expect("initialize test database");
            pool.close().await;
            true
        })
        .await;

    if !ready {
        return None;
    }

    let pool = MySqlPoolOptions::new()
        .max_connections(5)
        // Well under sqlx's 30s default: a pool that cannot hand out a
        // connection should fail quickly and say so, not stall the suite.
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(&url)
        .await
        .expect("connect to TEST_DATABASE_URL");

    Some(pool)
}

/// Re-run each query `GET /api/status/status` performs, reporting which one
/// fails and why.
///
/// `AppError` flattens every `sqlx::Error` into a generic 500 `{"error":
/// "Internal server error"}` and logs the detail through `log::error!`, which
/// tests never see because no logger is installed. Probing the queries directly
/// is the only way to turn that back into an actionable message.
///
/// Acquiring a connection is timed separately from running the query. Without
/// that split, a pool that cannot hand out a connection is indistinguishable
/// from a slow or blocked query -- and they have completely different causes.
async fn diagnose_status_queries(pool: &MySqlPool) -> String {
    use std::time::{Duration, Instant};

    let probes: [(&str, &str); 4] = [
        (
            "SELECT components",
            "SELECT id, name, status, CAST(last_updated AS CHAR) AS last_updated FROM components",
        ),
        (
            "SELECT incidents",
            "SELECT id, title, impact, status, status_text, CAST(status_updated_at AS CHAR) AS status_updated_at,              CAST(started_at AS CHAR) AS started_at, CAST(ended_at AS CHAR) AS ended_at, created_by, CAST(created_at AS CHAR) AS created_at              FROM incidents WHERE status != 'resolved' ORDER BY started_at DESC",
        ),
        (
            "SELECT incident_status_history",
            "SELECT id, incident_id, status, status_text, CAST(status_updated_at AS CHAR) AS status_updated_at              FROM incident_status_history ORDER BY status_updated_at ASC",
        ),
        (
            // Deliberately the same row the handler upserts. Probing a
            // different key would take a different row lock and so miss the
            // lock-wait case entirely -- which is the whole point of the probe.
            "UPSERT meta (write path -- the only one unique to this endpoint)",
            "INSERT INTO meta (`key`, `value`) VALUES ('generated_at', '__diag__') ON DUPLICATE KEY UPDATE `value` = '__diag__'",
        ),
    ];

    let mut report = format!(
        "
Pool: size={} idle={} max={}

Query-by-query probe:
",
        pool.size(),
        pool.num_idle(),
        pool.options().get_max_connections(),
    );

    for (label, sql) in probes {
        let began = Instant::now();
        let conn = tokio::time::timeout(Duration::from_secs(10), pool.acquire()).await;
        let acquired_in = began.elapsed();

        let mut conn = match conn {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                report.push_str(&format!(
                    "  {label}: COULD NOT ACQUIRE a connection after {acquired_in:.1?} -- {e}
"
                ));
                continue;
            }
            Err(_) => {
                report.push_str(&format!(
                    "  {label}: ACQUIRE TIMED OUT after 10s (pool exhausted, or the server                      is not accepting connections -- not a problem with the query)
"
                ));
                continue;
            }
        };

        let began_query = Instant::now();
        let ran = tokio::time::timeout(
            Duration::from_secs(10),
            sqlx::query(sql).execute(&mut *conn),
        )
        .await;
        let ran_in = began_query.elapsed();

        let outcome = match ran {
            Ok(Ok(_)) => format!("ok (acquire {acquired_in:.1?}, query {ran_in:.1?})"),
            Ok(Err(e)) => format!("FAILED after {ran_in:.1?} -- {e}"),
            Err(_) => format!(
                "QUERY TIMED OUT after 10s (acquire took {acquired_in:.1?}) -- a lock or a                  genuinely slow query"
            ),
        };
        report.push_str(&format!("  {label}: {outcome}
"));
    }

    for table in ["components", "meta"] {
        report.push_str(&format!("
Columns in `{table}`:
"));
        let cols: Result<Vec<(String, String)>, _> = sqlx::query_as(
            "SELECT COLUMN_NAME, COLUMN_TYPE FROM information_schema.COLUMNS              WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
        )
        .bind(table)
        .fetch_all(pool)
        .await;

        match cols {
            Ok(cols) if cols.is_empty() => report.push_str("  (table does not exist)
"),
            Ok(cols) => {
                for (name, ty) in cols {
                    report.push_str(&format!("  {name} {ty}
"));
                }
            }
            Err(e) => report.push_str(&format!("  (could not read schema: {e})
")),
        }
    }

    report
}

/// A signed session cookie for a test account. Says nothing about privilege:
/// access comes from `user_roles`, which tests set with [`grant_role`].
/// "test-secret" is the JWT secret test_state's config carries.
fn session_cookie(username: &str, id: &str) -> actix_web::cookie::Cookie<'static> {
    // Session version 0, the column's starting value.
    let token = api_rd::middleware::auth::create_jwt(username, id, false, 0, "test-secret", 3600)
        .expect("sign a test session");
    actix_web::cookie::Cookie::new("userToken", token)
}

/// Gives a test account a role by name.
async fn grant_role(pool: &MySqlPool, user_id: &str, role: &str) {
    sqlx::query(
        "INSERT IGNORE INTO user_roles (user_id, role_id, granted_by)
         SELECT ?, id, 'test' FROM roles WHERE name = ?",
    )
    .bind(user_id)
    .bind(role)
    .execute(pool)
    .await
    .expect("grant test role");
}

/// A minimal `AppState` for tests: the pool is real, the config carries dummy
/// (but valid, non-empty) required values since the endpoints under test only
/// touch the pool.
fn test_state(pool: MySqlPool) -> AppState {
    let config = AppConfig::build(|key| match key {
        "DB_HOST" => Some("localhost".to_string()),
        "DB_USER" => Some("test".to_string()),
        "DB_PASSWORD" => Some("test".to_string()),
        "DB_NAME" => Some("apird_test".to_string()),
        "JWT_SECRET" => Some("test-secret".to_string()),
        _ => None,
    })
    .expect("build test config");

    AppState {
        pool,
        config,
        player_count: Arc::new(RwLock::new(0)),
        max_players: Arc::new(RwLock::new(20)),
        // No tailer is spawned in tests, so this stays empty — the console
        // routes under test are rejected at the auth layer before reaching it.
        console: api_rd::console::Consoles::new(64),
        // Points at a dead port; the console routes under test are
        // rejected at the auth layer long before RCON is reached.
        rcon: api_rd::rcon::RconClient::new("127.0.0.1:1".to_string(), String::new()),
        snapshot: api_rd::console::stats::SnapshotCache::new(
            api_rd::console::stats::SNAPSHOT_TTL,
        ),
        presence: api_rd::console::presence::Presence::new(),
        // Fetching disabled and the cache pointed at a path that does not
        // exist: these tests must never reach the network or write a file.
        textures: api_rd::console::textures::TextureCache::new(
            String::from("target/test-texture-cache"),
            String::from("1.21.4"),
            String::new(),
            String::from("target/test-mods"),
        ),
        power_jobs: api_rd::console::control::PowerJobs::new(),
    }
}

#[actix_web::test]
async fn status_endpoint_returns_seeded_components() {
    let Some(pool) = shared_pool().await else {
        eprintln!("skipping status_endpoint_returns_seeded_components: TEST_DATABASE_URL not set");
        return;
    };

    // Serialized against the other DB-backed test; see DB_TEST_LOCK.
    let _serialized = DB_TEST_LOCK.lock().await;

    // Kept aside so the failure path can probe the database directly; the
    // handler's own pool is moved into the app.
    let probe_pool = pool.clone();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(test_state(pool)))
            .configure(configure_api),
    )
    .await;

    let req = test::TestRequest::get()
        .uri("/api/status/status")
        .to_request();
    let resp = test::call_service(&app, req).await;

    // Assert on the status code first and print the body when it is not 2xx.
    // Every query in this handler maps failure to a generic 500 {"error": ...},
    // so checking only for a missing "components" key reports a schema mismatch
    // in the test database as an inscrutable "key not present".
    let status = resp.status();
    let body: serde_json::Value = test::read_body_json(resp).await;
    if !status.is_success() {
        let probe = diagnose_status_queries(&probe_pool).await;
        panic!(
            "GET /api/status/status returned {status}; body: {body}{probe}
A FAILED line means the query itself was rejected -- usually an older schema in
the test database. init_database uses CREATE TABLE IF NOT EXISTS and will not
reshape an existing table, so drop and recreate it.
An ACQUIRE line means the problem is the connection pool, not the query. If the
pool reports idle connections while acquire still times out, those connections
belong to a runtime that has already been dropped: #[actix_web::test] gives each
test its own runtime, so a pool must never be shared across tests.
A QUERY TIMED OUT line means a genuine lock. Look for a stuck transaction with:
SELECT * FROM information_schema.INNODB_TRX;"
        );
    }

    let components = body
        .get("components")
        .and_then(|c| c.as_array())
        .unwrap_or_else(|| panic!("no components array in status response; body: {body}"));
    // init_database seeds 6 baseline components.
    assert!(
        components.len() >= 6,
        "expected at least the 6 seeded components, got {}",
        components.len()
    );
    assert!(body.get("incidents").is_some(), "status response missing incidents");
    assert!(body.get("meta").is_some(), "status response missing meta");
}

#[actix_web::test]
async fn incidents_endpoint_returns_json_array() {
    let Some(pool) = shared_pool().await else {
        eprintln!("skipping incidents_endpoint_returns_json_array: TEST_DATABASE_URL not set");
        return;
    };

    // Serialized against the other DB-backed test; see DB_TEST_LOCK.
    let _serialized = DB_TEST_LOCK.lock().await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(test_state(pool)))
            .configure(configure_api),
    )
    .await;

    let req = test::TestRequest::get()
        .uri("/api/status/incidents")
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "GET /api/status/incidents returned {status}; body: {body}"
    );
    assert!(body.is_array(), "incidents endpoint should return a JSON array");
}


/// The full life of a poll, against a real database: open it, vote, change
/// that vote, be refused when excluded, end it early, and be refused after.
///
/// This is the first test here that *writes*. The read-only tests above prove
/// a response shape; a poll's whole risk surface is on the write side — a
/// changed vote that adds instead of replaces, an excluded account that votes
/// anyway, a closed poll that still accepts answers. None of that is visible
/// from a GET.
#[actix_web::test]
async fn a_poll_can_be_opened_voted_on_changed_and_ended() {
    let Some(pool) = shared_pool().await else {
        eprintln!(
            "skipping a_poll_can_be_opened_voted_on_changed_and_ended: TEST_DATABASE_URL not set"
        );
        return;
    };

    // Serialized against the other DB-backed tests; see DB_TEST_LOCK.
    let _serialized = DB_TEST_LOCK.lock().await;

    // Polls have no foreign key to `users` on `created_by`, so they outlive
    // their author and must be removed by name rather than by cascade. Every
    // other poll table does cascade, so this takes options, audiences,
    // exclusions and votes with it.
    let clean = |pool: MySqlPool| async move {
        sqlx::query("DELETE FROM polls WHERE created_by = 'polltest_admin'")
            .execute(&pool)
            .await
            .expect("clear test polls");
        sqlx::query(r"DELETE FROM users WHERE username LIKE 'polltest\_%'")
            .execute(&pool)
            .await
            .expect("clear test accounts");
    };
    clean(pool.clone()).await;

    for (id, username, is_admin, is_member) in [
        ("polltest-admin-id", "polltest_admin", 1, 0),
        ("polltest-member-id", "polltest_member", 0, 1),
        ("polltest-excluded-id", "polltest_excluded", 0, 1),
        ("polltest-outsider-id", "polltest_outsider", 0, 0),
    ] {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, is_member, is_og)
             VALUES (?, ?, 'x', ?, 0)",
        )
        .bind(id)
        .bind(username)
        .bind(is_member)
        .execute(&pool)
        .await
        .expect("create test account");
        if is_admin == 1 {
            grant_role(&pool, id, "admin").await;
        }
    }

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(test_state(pool.clone())))
            .configure(configure_api),
    )
    .await;

    // "test-secret" is the JWT secret test_state's config carries.
    let session = |username: &str, id: &str, _is_admin: bool| session_cookie(username, id);
    let admin = session("polltest_admin", "polltest-admin-id", true);
    let member = session("polltest_member", "polltest-member-id", false);
    let excluded = session("polltest_excluded", "polltest-excluded-id", false);
    let outsider = session("polltest_outsider", "polltest-outsider-id", false);

    // --------------------------------------------------------------- open one
    let req = test::TestRequest::post()
        .uri("/api/admin/polls")
        .cookie(admin.clone())
        .set_json(serde_json::json!({
            "title": "New spawn build?",
            "description": "Concept art is in #builds.",
            "duration": "7d",
            "allowMultiple": false,
            "options": ["Medieval", "Modern", "Keep current"],
            "audiences": ["members"],
            "exclusions": ["polltest_excluded"],
        }))
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(status, StatusCode::CREATED, "creating a poll failed: {body}");

    let poll_id = body["id"].as_i64().expect("new poll id");
    let options = body["poll"]["options"].as_array().expect("options").clone();
    assert_eq!(options.len(), 3);
    assert_eq!(body["poll"]["excludedCount"], 1);
    assert_eq!(body["poll"]["live"], true);

    let medieval = options[0]["id"].as_i64().unwrap();
    let modern = options[1]["id"].as_i64().unwrap();

    // A typo'd exclusion is refused rather than dropped -- silently ignoring
    // one is how somebody ends up voting who was meant to be kept out.
    let req = test::TestRequest::post()
        .uri("/api/admin/polls")
        .cookie(admin.clone())
        .set_json(serde_json::json!({
            "title": "Doomed", "duration": "1d",
            "options": ["A", "B"], "audiences": ["everyone"],
            "exclusions": ["polltest_nobody"],
        }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::BAD_REQUEST,
        "an unknown exclusion must not be silently dropped"
    );

    // ---------------------------------------------------------- who may vote
    let access = |cookie: actix_web::cookie::Cookie<'static>| {
        test::TestRequest::get()
            .uri(&format!("/api/polls/{poll_id}/access"))
            .cookie(cookie)
            .to_request()
    };

    let resp = test::call_service(&app, access(member.clone())).await;
    let status = resp.status();
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "an eligible member was refused: {body}");
    assert_eq!(body["allowed"], true);
    assert_eq!(body["hasVoted"], false);

    let resp = test::call_service(&app, access(excluded.clone())).await;
    let status = resp.status();
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["reason"], "excluded");
    assert_eq!(body["eligible"], false);

    // Not a member, so not in the audience -- a different refusal from being
    // named on the exclusion list, and the frontend says something different.
    let resp = test::call_service(&app, access(outsider.clone())).await;
    let status = resp.status();
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["reason"], "not_eligible");

    // -------------------------------------------------------------- vote
    let vote = |cookie: actix_web::cookie::Cookie<'static>, option: i64| {
        test::TestRequest::post()
            .uri(&format!("/api/polls/{poll_id}/vote"))
            .cookie(cookie)
            .set_json(serde_json::json!({ "optionIds": [option] }))
            .to_request()
    };

    let resp = test::call_service(&app, vote(member.clone(), medieval)).await;
    let status = resp.status();
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "casting a vote failed: {body}");
    assert_eq!(body["voters"], 1);
    assert_eq!(body["options"][0]["votes"], 1);
    assert_eq!(body["options"][0]["percent"], 100.0);
    assert_eq!(body["selected"], serde_json::json!([medieval]));

    // The guarantee the whole design is for: a poll body never carries who
    // voted, for any caller.
    let rendered = body.to_string();
    for trace in ["polltest-member-id", "polltest-excluded-id", "polltest_member"] {
        assert!(
            !rendered.contains(trace),
            "a poll response disclosed a voter ({trace}): {rendered}"
        );
    }

    // Changing an answer must move the vote, not add a second one.
    let resp = test::call_service(&app, vote(member.clone(), modern)).await;
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["voters"], 1, "changing a vote must not add a voter");
    assert_eq!(body["totalVotes"], 1, "changing a vote must not add a vote");
    assert_eq!(body["options"][0]["votes"], 0, "the old choice must be released");
    assert_eq!(body["options"][1]["votes"], 1);
    assert_eq!(body["selected"], serde_json::json!([modern]));

    // An excluded account is refused at the vote itself, not only at /access.
    assert_eq!(
        test::call_service(&app, vote(excluded.clone(), modern))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );

    // Single-answer poll: two different options is a client bug, not something
    // to resolve by keeping whichever came first.
    let req = test::TestRequest::post()
        .uri(&format!("/api/polls/{poll_id}/vote"))
        .cookie(member.clone())
        .set_json(serde_json::json!({ "optionIds": [medieval, modern] }))
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::BAD_REQUEST
    );

    // ------------------------------------------------------------- listings
    let req = test::TestRequest::get()
        .uri("/api/admin/polls?status=live")
        .cookie(admin.clone())
        .to_request();
    let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert!(
        body["polls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == poll_id),
        "the new poll should be live: {body}"
    );
    assert_eq!(body["page"], 1);

    // ------------------------------------------------------------ end early
    let req = test::TestRequest::post()
        .uri(&format!("/api/admin/polls/{poll_id}/end"))
        .cookie(admin.clone())
        .to_request();
    let resp = test::call_service(&app, req).await;
    let status = resp.status();
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(status, StatusCode::OK, "ending a poll failed: {body}");
    assert_eq!(body["live"], false);
    assert_eq!(body["endedEarly"], true, "it had days left, so this was early");
    assert_eq!(body["options"][1]["votes"], 1, "results survive the close");

    // Ending twice is a conflict, not a silent no-op that overwrites the first
    // admin's closing time.
    let req = test::TestRequest::post()
        .uri(&format!("/api/admin/polls/{poll_id}/end"))
        .cookie(admin.clone())
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::CONFLICT
    );

    // A closed poll takes no more votes.
    assert_eq!(
        test::call_service(&app, vote(member.clone(), medieval))
            .await
            .status(),
        StatusCode::CONFLICT,
        "a closed poll must not accept a vote"
    );

    // ...but stays readable to the people it was for, which is the point of
    // keeping past results at all.
    let req = test::TestRequest::get()
        .uri(&format!("/api/polls/{poll_id}"))
        .cookie(member.clone())
        .to_request();
    let resp = test::call_service(&app, req).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(body["options"][1]["votes"], 1);
    assert_eq!(body["options"][1]["leading"], true);

    // And it has moved to the past listing.
    let req = test::TestRequest::get()
        .uri("/api/admin/polls?status=past")
        .cookie(admin.clone())
        .to_request();
    let body: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert!(
        body["polls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == poll_id),
        "an ended poll belongs in the past listing: {body}"
    );

    // ------------------------------------------------- what a player is shown
    //
    // The polls page asks for each half separately. A closed poll must leave
    // the live half and appear in the past one, and eligibility has to govern
    // both -- a poll you could never have answered stays hidden once it ends.
    let mine = |cookie: actix_web::cookie::Cookie<'static>, status: &str| {
        test::TestRequest::get()
            .uri(&format!("/api/polls?status={status}"))
            .cookie(cookie)
            .to_request()
    };

    let body: serde_json::Value =
        test::call_and_read_body_json(&app, mine(member.clone(), "live")).await;
    let live = body["polls"].as_array().unwrap();
    assert!(
        !live.iter().any(|p| p["id"] == poll_id),
        "an ended poll must not still be offered for voting: {body}"
    );
    assert_eq!(body["you"], "polltest_member", "the page needs to name the session");

    let body: serde_json::Value =
        test::call_and_read_body_json(&app, mine(member.clone(), "past")).await;
    let past = body["polls"].as_array().unwrap();
    let seen = past
        .iter()
        .find(|p| p["id"] == poll_id)
        .unwrap_or_else(|| panic!("a voter should see the result of a poll they answered: {body}"));
    assert_eq!(seen["options"][1]["votes"], 1, "with the result intact");
    assert_eq!(
        seen["selected"],
        serde_json::json!([modern]),
        "and their own answer still marked"
    );

    // The outsider was never in the audience, so the closed poll is not theirs
    // to read either.
    let body: serde_json::Value =
        test::call_and_read_body_json(&app, mine(outsider.clone(), "past")).await;
    assert!(
        !body["polls"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["id"] == poll_id),
        "closing a poll must not widen who can see it: {body}"
    );

    // A status nobody recognises is refused rather than quietly treated as one
    // of the two, which would show the wrong half without saying so.
    let req = test::TestRequest::get()
        .uri("/api/polls?status=everything")
        .cookie(member.clone())
        .to_request();
    assert_eq!(
        test::call_service(&app, req).await.status(),
        StatusCode::BAD_REQUEST
    );

    clean(pool).await;
}

/// One exclusion hides one poll — not every poll that account could answer.
///
/// The exclusion list names a person on a single poll, so it has to be read
/// per poll. A check that forgot to correlate on the poll id would still look
/// right in every place an admin looks: the poll they meant to bar someone
/// from *is* barred, and the panel's own listing does not filter by account at
/// all. The only place it would show is here, in what the barred player is
/// handed — which is why this asks for their listing rather than trusting
/// `/access` on one poll.
#[actix_web::test]
async fn an_exclusion_hides_one_poll_not_every_poll() {
    let Some(pool) = shared_pool().await else {
        eprintln!("skipping an_exclusion_hides_one_poll_not_every_poll: TEST_DATABASE_URL not set");
        return;
    };

    let _serialized = DB_TEST_LOCK.lock().await;

    let clean = |pool: MySqlPool| async move {
        sqlx::query("DELETE FROM polls WHERE created_by = 'excltest_admin'")
            .execute(&pool)
            .await
            .expect("clear test polls");
        sqlx::query(r"DELETE FROM users WHERE username LIKE 'excltest\_%'")
            .execute(&pool)
            .await
            .expect("clear test accounts");
    };
    clean(pool.clone()).await;

    for (id, username, is_admin, is_member) in [
        ("excltest-admin-id", "excltest_admin", 1, 0),
        ("excltest-member-id", "excltest_member", 0, 1),
        ("excltest-barred-id", "excltest_barred", 0, 1),
    ] {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, is_member, is_og)
             VALUES (?, ?, 'x', ?, 0)",
        )
        .bind(id)
        .bind(username)
        .bind(is_member)
        .execute(&pool)
        .await
        .expect("create test account");
        if is_admin == 1 {
            grant_role(&pool, id, "admin").await;
        }
    }

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(test_state(pool.clone())))
            .configure(configure_api),
    )
    .await;

    let session = |username: &str, id: &str, _is_admin: bool| session_cookie(username, id);
    let admin = session("excltest_admin", "excltest-admin-id", true);
    let member = session("excltest_member", "excltest-member-id", false);
    let barred = session("excltest_barred", "excltest-barred-id", false);

    // Three polls the barred account is in the audience of, and it is named on
    // the exclusion list of exactly one.
    let open_poll = |title: &str, audience: &str, exclusions: Vec<&str>| {
        test::TestRequest::post()
            .uri("/api/admin/polls")
            .cookie(admin.clone())
            .set_json(serde_json::json!({
                "title": title,
                "duration": "7d",
                "options": ["Yes", "No"],
                "audiences": [audience],
                "exclusions": exclusions,
            }))
            .to_request()
    };

    let mut ids = Vec::new();
    for (title, audience, exclusions) in [
        ("Barred from this one", "members", vec!["excltest_barred"]),
        ("Members only", "members", vec![]),
        ("Open to everyone", "everyone", vec![]),
    ] {
        let resp = test::call_service(&app, open_poll(title, audience, exclusions)).await;
        let status = resp.status();
        let body: serde_json::Value = test::read_body_json(resp).await;
        assert_eq!(status, StatusCode::CREATED, "opening \"{title}\" failed: {body}");
        ids.push(body["id"].as_i64().expect("new poll id"));
    }
    let (barred_poll, members_poll, everyone_poll) = (ids[0], ids[1], ids[2]);

    let listing = |cookie: actix_web::cookie::Cookie<'static>| {
        test::TestRequest::get()
            .uri("/api/polls?status=live")
            .cookie(cookie)
            .to_request()
    };

    let body: serde_json::Value =
        test::call_and_read_body_json(&app, listing(barred.clone())).await;
    let seen: Vec<i64> = body["polls"]
        .as_array()
        .expect("polls array")
        .iter()
        .filter_map(|p| p["id"].as_i64())
        .collect();

    assert!(
        !seen.contains(&barred_poll),
        "the poll they were barred from must be hidden: {body}"
    );
    assert!(
        seen.contains(&members_poll),
        "being barred from one poll must not hide the others they can answer: {body}"
    );
    assert!(
        seen.contains(&everyone_poll),
        "an everyone poll stays visible to a barred account: {body}"
    );

    // The same three, from an account nobody barred: the exclusion is the only
    // difference between these two listings.
    let body: serde_json::Value =
        test::call_and_read_body_json(&app, listing(member.clone())).await;
    let seen: Vec<i64> = body["polls"]
        .as_array()
        .expect("polls array")
        .iter()
        .filter_map(|p| p["id"].as_i64())
        .collect();
    for id in [barred_poll, members_poll, everyone_poll] {
        assert!(seen.contains(&id), "poll {id} should be offered to a member: {body}");
    }

    // And the refusal is still per poll where voting is decided, not account-wide.
    let access = |cookie: actix_web::cookie::Cookie<'static>, id: i64| {
        test::TestRequest::get()
            .uri(&format!("/api/polls/{id}/access"))
            .cookie(cookie)
            .to_request()
    };

    let resp = test::call_service(&app, access(barred.clone(), barred_poll)).await;
    let status = resp.status();
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["reason"], "excluded");

    let resp = test::call_service(&app, access(barred.clone(), members_poll)).await;
    let status = resp.status();
    let body: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "barred from one poll, still able to answer another: {body}"
    );
    assert_eq!(body["allowed"], true);

    // A session must resolve to the account it was issued for even when its
    // username no longer matches a row -- a rename, or a second row created by
    // the Google sign-in path under a different name.
    //
    // This is what actually emptied a member's poll list: the flags were read
    // with `WHERE username = ?` while every poll table keys on `users.id`, so
    // a name that landed on no row came back unauthorised, and a name that
    // landed on the *wrong* row came back with `is_member` false -- the
    // audience filter fell back to "everyone" alone and every members-only
    // poll disappeared at once. That is indistinguishable from being excluded
    // from all of them, which is why it was reported as an exclusion bug. The
    // id in the token is the fix and the thing under test; a username that
    // differs only in case would not catch it, because MySQL's default
    // collation matches those anyway.
    let renamed = session("excltest_member_renamed", "excltest-member-id", false);
    let body: serde_json::Value =
        test::call_and_read_body_json(&app, listing(renamed)).await;
    let seen: Vec<i64> = body["polls"]
        .as_array()
        .expect("polls array")
        .iter()
        .filter_map(|p| p["id"].as_i64())
        .collect();
    assert!(
        seen.contains(&members_poll),
        "a member must keep their member polls when the token's username no \
         longer matches the stored row: {body}"
    );

    // An account in no audience is told that polls exist rather than that
    // nothing is happening -- the empty list that hid this for so long.
    sqlx::query("UPDATE users SET is_member = 0 WHERE id = 'excltest-barred-id'")
        .execute(&pool)
        .await
        .expect("drop the barred account's membership");

    let body: serde_json::Value =
        test::call_and_read_body_json(&app, listing(barred.clone())).await;
    let seen: Vec<i64> = body["polls"]
        .as_array()
        .expect("polls array")
        .iter()
        .filter_map(|p| p["id"].as_i64())
        .collect();
    assert!(
        seen.contains(&everyone_poll) && !seen.contains(&members_poll),
        "dropping their membership leaves only the everyone poll: {body}"
    );

    sqlx::query("INSERT INTO poll_exclusions (poll_id, user_id) VALUES (?, 'excltest-barred-id')")
        .bind(everyone_poll)
        .execute(&pool)
        .await
        .expect("bar them from the everyone poll too");

    let body: serde_json::Value =
        test::call_and_read_body_json(&app, listing(barred.clone())).await;
    assert_eq!(
        body["polls"].as_array().map(|a| a.len()),
        Some(0),
        "nothing left to show: {body}"
    );
    // At least the three opened here. Not an equality: this counts every live
    // poll in the database, and the test database is shared.
    assert!(
        body["hiddenCount"].as_i64().unwrap_or(0) >= 3,
        "an empty list must say how many polls it was filtered out of: {body}"
    );

    clean(pool).await;
}

/// Roles end to end: permissions take effect and lapse on the very next
/// request, and nobody can use `roles.manage` to climb above their own access.
#[actix_web::test]
async fn roles_grant_live_access_and_cannot_be_used_to_escalate() {
    let Some(pool) = shared_pool().await else {
        eprintln!("skipping roles_grant_live_access_and_cannot_be_used_to_escalate: TEST_DATABASE_URL not set");
        return;
    };
    let _serialized = DB_TEST_LOCK.lock().await;

    let clean = |pool: MySqlPool| async move {
        sqlx::query(r"DELETE FROM users WHERE username LIKE 'rbactest\_%'")
            .execute(&pool)
            .await
            .expect("clear test accounts");
        sqlx::query(r"DELETE FROM roles WHERE name LIKE 'rbactest-%'")
            .execute(&pool)
            .await
            .expect("clear test roles");
    };
    clean(pool.clone()).await;

    for (id, username) in [
        ("rbactest-admin-id", "rbactest_admin"),
        ("rbactest-mgr-id", "rbactest_mgr"),
        ("rbactest-plain-id", "rbactest_plain"),
    ] {
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
            .bind(id)
            .bind(username)
            .execute(&pool)
            .await
            .expect("create test account");
    }
    grant_role(&pool, "rbactest-admin-id", "admin").await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(test_state(pool.clone())))
            .app_data(web::Data::new(api_rd::middleware::rate_limit::RateLimiter::new(10_000, 60)))
            .configure(configure_api),
    )
    .await;

    let admin = session_cookie("rbactest_admin", "rbactest-admin-id");
    let mgr = session_cookie("rbactest_mgr", "rbactest-mgr-id");
    let plain = session_cookie("rbactest_plain", "rbactest-plain-id");

    macro_rules! call {
        ($req:expr, $who:expr) => {{
            let req = $req.cookie($who.clone()).to_request();
            test::call_service(&app, req).await.status()
        }};
    }
    let list_users = || test::TestRequest::get().uri("/api/admin/list-users");
    let create = |name: &str, perms: &[&str]| {
        test::TestRequest::post()
            .uri("/api/admin/access/roles")
            .set_json(serde_json::json!({ "name": name, "permissions": perms }))
    };
    let set_roles = |user: &str, roles: &[&str]| {
        test::TestRequest::put()
            .uri(&format!("/api/admin/access/users/{user}/roles"))
            .set_json(serde_json::json!({ "roles": roles }))
    };

    // An account with no roles is refused.
    assert_eq!(call!(list_users(), plain), StatusCode::FORBIDDEN);

    // The admin sets up a delegated role manager.
    assert_eq!(
        call!(create("rbactest-managers", &["roles.manage", "users.view", "panel.access"]), admin),
        StatusCode::CREATED
    );
    assert_eq!(
        call!(create("rbactest-mods", &["users.view", "users.moderate"]), admin),
        StatusCode::CREATED
    );
    assert_eq!(call!(set_roles("rbactest_mgr", &["rbactest-managers"]), admin), StatusCode::OK);

    // Same token as before the grant: access is read per request.
    assert_eq!(call!(list_users(), mgr), StatusCode::OK);

    // A manager can make roles out of what they hold...
    assert_eq!(call!(create("rbactest-viewers", &["users.view"]), mgr), StatusCode::CREATED);
    // ...but not out of what they do not.
    assert_eq!(call!(create("rbactest-sneaky", &["console.command"]), mgr), StatusCode::FORBIDDEN);
    // Nor widen an existing role beyond themselves.
    assert_eq!(
        call!(
            test::TestRequest::patch()
                .uri("/api/admin/access/roles/rbactest-viewers")
                .set_json(serde_json::json!({ "permissions": ["users.view", "users.delete"] })),
            mgr
        ),
        StatusCode::FORBIDDEN
    );
    // Nor grant themselves the admin role, or a role holding more than they do.
    assert_eq!(
        call!(set_roles("rbactest_mgr", &["rbactest-managers", "admin"]), mgr),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call!(set_roles("rbactest_plain", &["rbactest-mods"]), mgr),
        StatusCode::FORBIDDEN
    );
    // Nor touch the roles of someone who outranks them.
    assert_eq!(call!(set_roles("rbactest_admin", &["admin"]), mgr), StatusCode::FORBIDDEN);
    // The legacy switch is admin-only too.
    assert_eq!(
        call!(
            test::TestRequest::post()
                .uri("/api/update-admin-status")
                .set_json(serde_json::json!({ "username": "rbactest_mgr", "isAdmin": true })),
            mgr
        ),
        StatusCode::FORBIDDEN
    );

    // What they can grant, they can grant, and it works at once.
    assert_eq!(call!(set_roles("rbactest_plain", &["rbactest-viewers"]), mgr), StatusCode::OK);
    assert_eq!(call!(list_users(), plain), StatusCode::OK);

    // Revocation is just as immediate: no re-login, no waiting for expiry.
    assert_eq!(call!(set_roles("rbactest_plain", &[]), admin), StatusCode::OK);
    assert_eq!(call!(list_users(), plain), StatusCode::FORBIDDEN);

    // The built-in role is fixed.
    assert_eq!(
        call!(
            test::TestRequest::patch()
                .uri("/api/admin/access/roles/admin")
                .set_json(serde_json::json!({ "permissions": [] })),
            admin
        ),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call!(test::TestRequest::delete().uri("/api/admin/access/roles/admin"), admin),
        StatusCode::FORBIDDEN
    );

    // Password resets cannot be aimed upward.
    assert_eq!(
        call!(
            test::TestRequest::post()
                .uri("/api/admin/gen-pwd-reset")
                .set_json(serde_json::json!({ "username": "rbactest_admin" })),
            admin
        ),
        StatusCode::OK
    );
    grant_role(&pool, "rbactest-mgr-id", "rbactest-mods").await;
    sqlx::query(
        "INSERT INTO role_permissions (role_id, permission)
         SELECT id, 'users.reset_password' FROM roles WHERE name = 'rbactest-managers'",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        call!(
            test::TestRequest::post()
                .uri("/api/admin/gen-pwd-reset")
                .set_json(serde_json::json!({ "username": "rbactest_admin" })),
            mgr
        ),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call!(
            test::TestRequest::post()
                .uri("/api/admin/gen-pwd-reset")
                .set_json(serde_json::json!({ "username": "rbactest_plain" })),
            mgr
        ),
        StatusCode::OK
    );

    // /access/me reports what the caller holds.
    let req = test::TestRequest::get().uri("/api/access/me").cookie(mgr.clone()).to_request();
    let me: serde_json::Value = test::call_and_read_body_json(&app, req).await;
    assert_eq!(me["superuser"], false);
    let perms: Vec<&str> = me["permissions"].as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
    assert!(perms.contains(&"roles.manage") && !perms.contains(&"console.command"), "{me}");

    // Every change above left a line in the audit trail.
    let (count,): (i64,) = sqlx::query_as(
        r"SELECT COUNT(*) FROM access_audit WHERE target LIKE 'rbactest%'",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(count >= 6, "expected audit entries, found {count}");

    clean(pool.clone()).await;
    pool.close().await;
}

/// `users.is_admin` is carried into the admin role once. An admin demoted after
/// that is not promoted again by the stale column on the next start-up.
#[actix_web::test]
async fn the_is_admin_migration_runs_once() {
    let Some(pool) = shared_pool().await else {
        eprintln!("skipping the_is_admin_migration_runs_once: TEST_DATABASE_URL not set");
        return;
    };
    let _serialized = DB_TEST_LOCK.lock().await;

    sqlx::query(r"DELETE FROM users WHERE username LIKE 'migtest\_%'")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, is_admin) VALUES ('migtest-id', 'migtest_old', 'x', 1)",
    )
    .execute(&pool)
    .await
    .unwrap();

    let holds_admin = |pool: MySqlPool| async move {
        let (_, perms) = api_rd::access::store::grants_for(&pool, "migtest-id").await.unwrap();
        perms.is_superuser()
    };

    // As if this were the first start-up on the new version.
    sqlx::query("DELETE FROM meta WHERE `key` = 'rbac_migrated_from_is_admin'")
        .execute(&pool)
        .await
        .unwrap();
    api_rd::access::store::migrate(&pool).await.unwrap();
    assert!(holds_admin(pool.clone()).await, "an is_admin account must become an admin");

    // Demoted afterwards; the column still says 1.
    sqlx::query("DELETE FROM user_roles WHERE user_id = 'migtest-id'")
        .execute(&pool)
        .await
        .unwrap();
    api_rd::access::store::migrate(&pool).await.unwrap();
    assert!(!holds_admin(pool.clone()).await, "a later start-up must not re-promote");

    sqlx::query(r"DELETE FROM users WHERE username LIKE 'migtest\_%'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

/// The file browser reads the server directory, which holds the RCON password
/// and every player's credentials. Pins down, end to end: it needs its own
/// permission (`console.view` is not enough), credential stores are refused,
/// secrets are masked in both the viewer and downloads, downloads can never
/// render on this origin, and every read is attributed.
#[actix_web::test]
async fn server_files_need_their_own_permission_and_never_leak_secrets() {
    let Some(pool) = shared_pool().await else {
        eprintln!("skipping server_files_need_their_own_permission_and_never_leak_secrets: TEST_DATABASE_URL not set");
        return;
    };
    let _serialized = DB_TEST_LOCK.lock().await;

    let clean = |pool: MySqlPool| async move {
        sqlx::query(r"DELETE FROM users WHERE username LIKE 'filestest\_%'")
            .execute(&pool)
            .await
            .expect("clear test accounts");
        sqlx::query(r"DELETE FROM roles WHERE name LIKE 'filestest-%'")
            .execute(&pool)
            .await
            .expect("clear test roles");
        sqlx::query(r"DELETE FROM console_audit WHERE username LIKE 'filestest\_%'")
            .execute(&pool)
            .await
            .expect("clear test audit rows");
    };
    clean(pool.clone()).await;

    for (id, username, role, permission) in [
        ("filestest-viewer-id", "filestest_viewer", "filestest-viewer", "console.view"),
        ("filestest-files-id", "filestest_files", "filestest-files", "console.files"),
    ] {
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
            .bind(id)
            .bind(username)
            .execute(&pool)
            .await
            .expect("create test account");
        sqlx::query("INSERT INTO roles (name, description, is_system) VALUES (?, '', FALSE)")
            .bind(role)
            .execute(&pool)
            .await
            .expect("create test role");
        sqlx::query(
            "INSERT INTO role_permissions (role_id, permission)
             SELECT id, ? FROM roles WHERE name = ?",
        )
        .bind(permission)
        .bind(role)
        .execute(&pool)
        .await
        .expect("grant test permission");
        grant_role(&pool, id, role).await;
    }

    let dir = std::env::temp_dir().join(format!("apird-files-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("logs")).unwrap();
    std::fs::write(dir.join("server.properties"), "rcon.port=29875\nrcon.password=hunter2\n").unwrap();
    std::fs::write(dir.join("authedPlayers.json"), "{\"uuid\":{\"password\":\"x\"}}").unwrap();
    std::fs::write(dir.join("logs/page.html"), "<script>alert(1)</script>").unwrap();

    let mut state = test_state(pool.clone());
    state.config.minecraft_server_dir = dir.to_string_lossy().into_owned();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(configure_api),
    )
    .await;

    let viewer = session_cookie("filestest_viewer", "filestest-viewer-id");
    let reader = session_cookie("filestest_files", "filestest-files-id");
    let get = |uri: &str, who: &actix_web::cookie::Cookie<'static>| {
        test::TestRequest::get().uri(uri).cookie(who.clone()).to_request()
    };

    // console.view alone opens none of it.
    for uri in [
        "/api/admin/console/files",
        "/api/admin/console/files/view?path=server.properties",
        "/api/admin/console/files/download?path=server.properties",
    ] {
        let resp = test::call_service(&app, get(uri, &viewer)).await;
        assert_eq!(resp.status(), StatusCode::FORBIDDEN, "GET {uri} with console.view only");
    }

    let resp = test::call_service(&app, get("/api/admin/console/files", &reader)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let listing: serde_json::Value = test::read_body_json(resp).await;
    let entries = listing["entries"].as_array().unwrap();
    let creds = entries.iter().find(|e| e["name"] == "authedPlayers.json").unwrap();
    assert!(creds["restricted"].is_string() && creds.get("size").is_none(), "{creds}");

    let resp = test::call_service(&app, get("/api/admin/console/files/view?path=server.properties", &reader)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store");
    let preview: serde_json::Value = test::read_body_json(resp).await;
    let text = preview["text"].as_str().unwrap();
    assert!(!text.contains("hunter2") && text.contains("rcon.password=********"), "{text}");
    assert_eq!(preview["masked"], 1);

    for (uri, status) in [
        ("/api/admin/console/files/view?path=authedPlayers.json", StatusCode::FORBIDDEN),
        ("/api/admin/console/files/download?path=authedPlayers.json", StatusCode::FORBIDDEN),
        ("/api/admin/console/files/view?path=../etc/passwd", StatusCode::BAD_REQUEST),
        ("/api/admin/console/files/download?path=%2E%2E/%2E%2E/etc/passwd", StatusCode::BAD_REQUEST),
        ("/api/admin/console/files/view?path=nope.txt", StatusCode::NOT_FOUND),
    ] {
        let resp = test::call_service(&app, get(uri, &reader)).await;
        assert_eq!(resp.status(), status, "GET {uri}");
    }

    let resp = test::call_service(&app, get("/api/admin/console/files/download?path=server.properties", &reader)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let body = test::read_body(resp).await;
    assert_eq!(&body[..], b"rcon.port=29875\nrcon.password=********\n");

    let resp = test::call_service(&app, get("/api/admin/console/files/download?path=logs/page.html", &reader)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();
    assert_eq!(h.get("content-type").unwrap(), "application/octet-stream");
    assert!(h.get("content-disposition").unwrap().to_str().unwrap().starts_with("attachment"));
    assert_eq!(h.get("x-content-type-options").unwrap(), "nosniff");
    assert!(h.get("content-security-policy").unwrap().to_str().unwrap().contains("sandbox"));

    // One view and two downloads, all attributed; the refusals are not.
    let reads: Vec<(String, String)> = sqlx::query_as(
        "SELECT command, outcome FROM console_audit
         WHERE username = 'filestest_files' AND kind = 'file' ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        reads,
        [
            ("view server.properties".to_string(), "ok".to_string()),
            ("download server.properties".to_string(), "ok".to_string()),
            ("download logs/page.html".to_string(), "ok".to_string()),
        ]
    );

    clean(pool).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A multipart body carrying one file in the `file` field.
fn multipart_file(name: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----apirdtestboundary";
    let mut body = format!(
        "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\n\
         Content-Type: application/octet-stream\r\n\r\n"
    )
    .into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Editing and uploading, end to end: each needs its own permission, only
/// the panel's own requests are accepted, masked secrets survive a save,
/// locked keys and existing secrets cannot change, stale saves are refused,
/// code cannot be uploaded, and every attempt -- refused ones included -- is
/// attributed.
#[actix_web::test]
async fn server_file_writes_are_gated_attributed_and_keep_secrets() {
    let Some(pool) = shared_pool().await else {
        eprintln!("skipping server_file_writes_are_gated_attributed_and_keep_secrets: TEST_DATABASE_URL not set");
        return;
    };
    let _serialized = DB_TEST_LOCK.lock().await;

    let clean = |pool: MySqlPool| async move {
        sqlx::query(r"DELETE FROM users WHERE username LIKE 'fileswtest\_%'")
            .execute(&pool)
            .await
            .expect("clear test accounts");
        sqlx::query(r"DELETE FROM roles WHERE name LIKE 'fileswtest-%'")
            .execute(&pool)
            .await
            .expect("clear test roles");
        sqlx::query(r"DELETE FROM console_audit WHERE username LIKE 'fileswtest\_%'")
            .execute(&pool)
            .await
            .expect("clear test audit rows");
    };
    clean(pool.clone()).await;

    for (id, username, role, permissions) in [
        ("fileswtest-reader-id", "fileswtest_reader", "fileswtest-reader", &["console.files"][..]),
        ("fileswtest-editor-id", "fileswtest_editor", "fileswtest-editor", &["console.files", "console.edit"][..]),
        ("fileswtest-upload-id", "fileswtest_upload", "fileswtest-upload", &["console.files", "console.upload"][..]),
    ] {
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
            .bind(id)
            .bind(username)
            .execute(&pool)
            .await
            .expect("create test account");
        sqlx::query("INSERT INTO roles (name, description, is_system) VALUES (?, '', FALSE)")
            .bind(role)
            .execute(&pool)
            .await
            .expect("create test role");
        for permission in permissions {
            sqlx::query(
                "INSERT INTO role_permissions (role_id, permission)
                 SELECT id, ? FROM roles WHERE name = ?",
            )
            .bind(permission)
            .bind(role)
            .execute(&pool)
            .await
            .expect("grant test permission");
        }
        grant_role(&pool, id, role).await;
    }

    let base = std::env::temp_dir().join(format!("apird-filesw-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let dir = base.join("server");
    std::fs::create_dir_all(dir.join("config")).unwrap();
    let props = "motd=Hello\nrcon.password=hunter2\nonline-mode=true\n";
    std::fs::write(dir.join("server.properties"), props).unwrap();

    let mut state = test_state(pool.clone());
    state.config.minecraft_server_dir = dir.to_string_lossy().into_owned();
    state.config.server_files_backup_dir = base.join("backups").to_string_lossy().into_owned();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .configure(configure_api),
    )
    .await;

    let reader = session_cookie("fileswtest_reader", "fileswtest-reader-id");
    let editor = session_cookie("fileswtest_editor", "fileswtest-editor-id");
    let uploader = session_cookie("fileswtest_upload", "fileswtest-upload-id");

    let req = test::TestRequest::get()
        .uri("/api/admin/console/files/view?path=server.properties")
        .cookie(editor.clone())
        .to_request();
    let preview: serde_json::Value = test::read_body_json(test::call_service(&app, req).await).await;
    assert_eq!(preview["editable"], true, "{preview}");
    let version = preview["version"].as_str().unwrap().to_string();
    let masked = preview["text"].as_str().unwrap().to_string();
    assert!(masked.contains("rcon.password=********"));

    let save = |who: &actix_web::cookie::Cookie<'static>, content: &str, version: &str| {
        test::TestRequest::put()
            .uri("/api/admin/console/files/content")
            .cookie(who.clone())
            .insert_header(("x-rd-panel", "1"))
            .set_json(serde_json::json!({ "path": "server.properties", "content": content, "version": version }))
    };
    let edited = masked.replace("motd=Hello", "motd=Welcome");

    // Reading is not editing.
    let resp = test::call_service(&app, save(&reader, &edited, &version).to_request()).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);

    // Not from the panel: no header, or a cross-site fetch carrying one.
    let req = test::TestRequest::put()
        .uri("/api/admin/console/files/content")
        .cookie(editor.clone())
        .set_json(serde_json::json!({ "path": "server.properties", "content": edited, "version": version }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::FORBIDDEN);
    let req = save(&editor, &edited, &version).insert_header(("sec-fetch-site", "same-site")).to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::FORBIDDEN);

    // A locked key, then a changed secret: refused, file untouched.
    for bad in [
        masked.replace("online-mode=true", "online-mode=false"),
        masked.replace("rcon.password=********", "rcon.password=mine"),
    ] {
        let resp = test::call_service(&app, save(&editor, &bad, &version).to_request()).await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
    assert_eq!(std::fs::read_to_string(dir.join("server.properties")).unwrap(), props);

    // The real save keeps the password it never saw.
    let resp = test::call_service(&app, save(&editor, &edited, &version).to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        std::fs::read_to_string(dir.join("server.properties")).unwrap(),
        props.replace("motd=Hello", "motd=Welcome")
    );

    // The old version is now stale.
    let resp = test::call_service(&app, save(&editor, &masked, &version).to_request()).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT);

    let upload = |who: &actix_web::cookie::Cookie<'static>, uri: &str, name: &str, bytes: &[u8]| {
        let (content_type, body) = multipart_file(name, bytes);
        test::TestRequest::post()
            .uri(uri)
            .cookie(who.clone())
            .insert_header(("x-rd-panel", "1"))
            .insert_header(("content-type", content_type))
            .set_payload(body)
            .to_request()
    };
    let uri = "/api/admin/console/files/upload?path=config";
    let resp = test::call_service(&app, upload(&editor, uri, "a.toml", b"x = 1\n")).await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "editing is not uploading");
    let resp = test::call_service(&app, upload(&uploader, uri, "a.toml", b"x = 1\n")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(std::fs::read_to_string(dir.join("config/a.toml")).unwrap(), "x = 1\n");
    let resp = test::call_service(&app, upload(&uploader, uri, "a.toml", b"x = 2\n")).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "never replaced unasked");
    let resp = test::call_service(
        &app,
        upload(&uploader, "/api/admin/console/files/upload?path=config&overwrite=true", "a.toml", b"x = 2\n"),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN, "replacing needs console.edit");
    let resp = test::call_service(&app, upload(&uploader, uri, "evil.jar", b"PK\x03\x04")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(!dir.join("config/evil.jar").exists());

    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT username, command, outcome FROM console_audit
         WHERE username LIKE 'fileswtest\\_%' AND kind = 'file' AND command NOT LIKE 'view %'
         ORDER BY id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let rows: Vec<(&str, &str, &str)> =
        rows.iter().map(|(u, c, o)| (u.as_str(), c.as_str(), o.as_str())).collect();
    assert_eq!(
        rows,
        [
            ("fileswtest_editor", "edit server.properties", "failed"),
            ("fileswtest_editor", "edit server.properties", "failed"),
            ("fileswtest_editor", "edit server.properties", "ok"),
            ("fileswtest_editor", "edit server.properties", "failed"),
            ("fileswtest_upload", "upload config/a.toml", "ok"),
            ("fileswtest_upload", "upload config/a.toml", "failed"),
            ("fileswtest_upload", "upload config/evil.jar", "failed"),
        ]
    );

    clean(pool).await;
    let _ = std::fs::remove_dir_all(&base);
}

/// Ending a season end to end, against a scratch server the sidecar reports
/// as stopped (so no RCON is needed): the zip and record appear where the
/// website serves them, a second end of the same season is refused, and the
/// archive editor is gated on its own permission.
#[actix_web::test]
async fn ending_a_season_publishes_the_zip_and_record() {
    let Some(pool) = shared_pool().await else {
        eprintln!("skipping ending_a_season_publishes_the_zip_and_record: TEST_DATABASE_URL not set");
        return;
    };
    let _serialized = DB_TEST_LOCK.lock().await;

    let clean = |pool: MySqlPool| async move {
        sqlx::query(r"DELETE FROM users WHERE username LIKE 'seasontest\_%'").execute(&pool).await.unwrap();
        sqlx::query(r"DELETE FROM roles WHERE name LIKE 'seasontest-%'").execute(&pool).await.unwrap();
        sqlx::query(r"DELETE FROM console_audit WHERE username LIKE 'seasontest\_%'").execute(&pool).await.unwrap();
    };
    clean(pool.clone()).await;
    for (id, username, role, permission) in [
        ("seasontest-ender-id", "seasontest_ender", "seasontest-ender", "season.end"),
        ("seasontest-editor-id", "seasontest_editor", "seasontest-editor", "season.archives"),
    ] {
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, 'x')")
            .bind(id).bind(username).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO roles (name, description, is_system) VALUES (?, '', FALSE)")
            .bind(role).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO role_permissions (role_id, permission) SELECT id, ? FROM roles WHERE name = ?")
            .bind(permission).bind(role).execute(&pool).await.unwrap();
        grant_role(&pool, id, role).await;
    }

    let base = std::env::temp_dir().join(format!("apird-season-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for dir in ["server/world/region", "server/mods", "server/plugins/ServerSideUtils", "content", "control"] {
        std::fs::create_dir_all(base.join(dir)).unwrap();
    }
    std::fs::write(
        base.join("server/plugins/ServerSideUtils/season.json"),
        r#"{"season":9,"theme":"S8 reboot","minecraft":"1.21.1","world":"world"}"#,
    )
    .unwrap();
    std::fs::write(base.join("server/world/level.dat"), b"level").unwrap();
    std::fs::write(base.join("server/world/region/r.0.0.mca"), vec![1u8; 4096]).unwrap();
    std::fs::write(base.join("server/mods/thing-1.21.1-2.3.4.jar"), b"jar").unwrap();
    std::fs::write(base.join("control/status"), "false\n").unwrap();

    let (sidecar, _) = fake_sidecar(base.join("control"));
    let mut state = test_state(pool.clone());
    let path = |rel: &str| base.join(rel).to_string_lossy().into_owned();
    state.config.season_override_path = path("server/plugins/ServerSideUtils/season-override.json");
    state.config.minecraft_server_dir = path("server");
    state.config.mods_dir = path("server/mods");
    state.config.season_info_path = path("server/plugins/ServerSideUtils/season.json");
    state.config.server_properties_path = path("server/server.properties");
    state.config.seasons_path = path("content");
    state.config.archives_dir = path("archives");
    state.config.control_dir = path("control");
    let app = test::init_service(App::new().app_data(web::Data::new(state)).configure(configure_api)).await;

    let ender = session_cookie("seasontest_ender", "seasontest-ender-id");
    let editor = session_cookie("seasontest_editor", "seasontest-editor-id");
    let get = |uri: &str, who: &actix_web::cookie::Cookie<'static>| {
        test::TestRequest::get().uri(uri).cookie(who.clone()).to_request()
    };

    let resp = test::call_service(&app, get("/api/admin/season/current", &ender)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let plan: serde_json::Value = test::read_body_json(resp).await;
    assert_eq!((plan["season"].as_u64(), plan["theme"].as_str()), (Some(9), Some("S8 reboot")));
    assert_eq!(plan["mods"][0]["version"], "2.3.4");

    let end = |who: &actix_web::cookie::Cookie<'static>, season: u32| {
        test::TestRequest::post()
            .uri("/api/admin/season/end")
            .cookie(who.clone())
            .insert_header(("x-rd-panel", "1"))
            .set_json(serde_json::json!({
                "season": season, "started": "2026-08-01", "ended": "2026-11-12", "additional_info": "GG",
                "next_season": season + 1, "next_theme": "Glacier", "seed": "42"
            }))
            .to_request()
    };
    assert_eq!(test::call_service(&app, end(&editor, 9)).await.status(), StatusCode::FORBIDDEN, "editing is not ending");
    assert_eq!(test::call_service(&app, end(&ender, 9)).await.status(), StatusCode::ACCEPTED);

    let mut job = serde_json::Value::Null;
    for _ in 0..100 {
        let resp = test::call_service(&app, get("/api/admin/season/end/status", &ender)).await;
        let body: serde_json::Value = test::read_body_json(resp).await;
        job = body["job"].clone();
        if job["state"] != "running" {
            break;
        }
        actix_web::rt::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(job["state"], "done", "{job}");
    assert_eq!(job["percent"].as_f64(), Some(100.0));
    assert!(base.join("content/Season_9.zip").is_file());
    let record: serde_json::Value =
        serde_json::from_slice(&std::fs::read(base.join("archives/Season_9.json")).unwrap()).unwrap();
    assert_eq!(record["theme"], "S8 reboot");
    assert_eq!(record["additional_info"], "GG");
    assert_eq!(record["mods"], serde_json::json!([{ "name": "thing", "version": "2.3.4" }]));
    assert_eq!(record["world"]["url"], "/raindrippy/content/Season_9.zip");
    assert!(!base.join("server/world").exists(), "the old world is gone");
    assert!(std::fs::read_to_string(base.join("server/server.properties")).unwrap().contains("level-seed=42"));

    // Ending it again (the plugin not yet restarted, a world present) finds
    // the zip and record already there and does nothing.
    std::fs::create_dir_all(base.join("server/world")).unwrap();
    std::fs::write(base.join("server/world/level.dat"), b"new").unwrap();
    assert_eq!(test::call_service(&app, end(&ender, 9)).await.status(), StatusCode::ACCEPTED);
    for _ in 0..50 {
        let resp = test::call_service(&app, get("/api/admin/season/end/status", &ender)).await;
        let body: serde_json::Value = test::read_body_json(resp).await;
        job = body["job"].clone();
        if job["state"] != "running" { break; }
        actix_web::rt::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert_eq!(job["state"], "failed");
    assert!(job["error"].as_str().unwrap().contains("already exists"), "{job}");

    // The editor: the ender can't, the editor can, with the version.
    assert_eq!(test::call_service(&app, get("/api/admin/season/archives", &ender)).await.status(), StatusCode::FORBIDDEN);
    let resp = test::call_service(&app, get("/api/admin/season/archives/9", &editor)).await;
    let loaded: serde_json::Value = test::read_body_json(resp).await;
    let version = loaded["version"].as_str().unwrap().to_string();
    let mut input = serde_json::json!({
        "season": 9, "theme": "S8 Reboot", "started": "2026-08-01", "ended": "2026-11-12",
        "minecraft_version": "1.21.1", "has_world": true,
        "mods": [{ "name": "thing", "version": "2.3.4" }], "additional_info": "GG"
    });
    let put = |body: serde_json::Value, uri: &str| {
        test::TestRequest::put().uri(uri).cookie(editor.clone()).insert_header(("x-rd-panel", "1")).set_json(body).to_request()
    };
    let resp = test::call_service(&app, put(serde_json::json!({ "archive": input, "version": version }), "/api/admin/season/archives/9")).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = test::call_service(&app, put(serde_json::json!({ "archive": input, "version": version }), "/api/admin/season/archives/9")).await;
    assert_eq!(resp.status(), StatusCode::CONFLICT, "the old version is stale");
    input["season"] = serde_json::json!(4);
    input["has_world"] = serde_json::json!(false);
    let resp = test::call_service(&app, put(serde_json::json!({ "archive": input }), "/api/admin/season/archives/4?create=true")).await;
    assert_eq!(resp.status(), StatusCode::OK, "an old season can be added by hand");
    input["extra"] = serde_json::json!("x");
    let resp = test::call_service(&app, put(serde_json::json!({ "archive": input }), "/api/admin/season/archives/5?create=true")).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "unknown fields are refused");

    sidecar.abort();
    clean(pool).await;
    let _ = std::fs::remove_dir_all(&base);
}

/// A stand-in for the mc-control sidecar: answers every command queued in
/// `control`, keeps the status file in step, and records the order.
fn fake_sidecar(control: std::path::PathBuf) -> (tokio::task::JoinHandle<()>, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    let task = tokio::spawn(async move {
        loop {
            if let Ok(entries) = std::fs::read_dir(control.join("queue")) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "cmd") {
                        let cmd = std::fs::read_to_string(&path).unwrap();
                        std::fs::remove_file(&path).unwrap();
                        let id = path.file_stem().unwrap().to_string_lossy().into_owned();
                        std::fs::create_dir_all(control.join("result")).unwrap();
                        let status = if cmd == "stop" { "stopped" } else { "running" };
                        std::fs::write(control.join("status"), status).unwrap();
                        std::fs::write(control.join("result").join(id), "0\nok\n").unwrap();
                        log.lock().unwrap().push(cmd);
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    });
    (task, seen)
}

/// The End-season pipeline itself -- check, stop, zip, record, publish,
/// configure the next season, reset the world, start -- run directly, with
/// no database.
#[actix_web::test]
async fn the_end_season_job_runs_without_a_database() {
    let base = std::env::temp_dir().join(format!("apird-season-job-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    for dir in ["server/world/region", "server/world/playerdata", "server/world_the_end", "server/eventhub/region", "server/testing", "server/mods", "server/plugins/ServerSideUtils", "content", "control"] {
        std::fs::create_dir_all(base.join(dir)).unwrap();
    }
    std::fs::write(base.join("server/plugins/ServerSideUtils/season.json"), r#"{"season":12,"theme":"Ocean","minecraft":"1.21.1"}"#).unwrap();
    std::fs::write(base.join("server/server.properties"), "level-name=world\nlevel-seed=111\nrcon.password=hunter2\n").unwrap();
    std::fs::write(base.join("server/world/level.dat"), b"level").unwrap();
    std::fs::write(base.join("server/world/region/r.0.0.mca"), vec![3u8; 100_000]).unwrap();
    std::fs::write(base.join("server/world_the_end/level.dat"), b"end").unwrap();
    std::fs::write(base.join("server/eventhub/level.dat"), b"hub").unwrap();
    std::fs::write(base.join("server/eventhub/region/r.0.0.mca"), b"hub region").unwrap();
    std::fs::write(base.join("server/testing/level.dat"), b"test").unwrap();
    std::fs::write(base.join("server/mods/create-1.21.1-6.0.4.jar"), b"jar").unwrap();
    std::fs::write(base.join("control/status"), "running\n").unwrap();

    let (sidecar, seen) = fake_sidecar(base.join("control"));

    let mut state = lazy_state();
    let path = |rel: &str| base.join(rel).to_string_lossy().into_owned();
    state.config.minecraft_server_dir = path("server");
    state.config.mods_dir = path("server/mods");
    state.config.season_info_path = path("server/plugins/ServerSideUtils/season.json");
    state.config.season_override_path = path("server/plugins/ServerSideUtils/season-override.json");
    state.config.server_properties_path = path("server/server.properties");
    state.config.seasons_path = path("content");
    state.config.archives_dir = path("archives");
    state.config.control_dir = path("control");
    let state = web::Data::new(state);

    let request: api_rd::season::end::EndRequest = serde_json::from_value(serde_json::json!({
        "season": 12, "started": null, "ended": "2026-11-12", "additional_info": "Thanks all",
        "next_season": 13, "next_theme": "Glacier", "seed": "-4172144997902289642"
    }))
    .unwrap();
    let job = api_rd::season::end::begin(12, "tester").expect("no job running");
    assert!(api_rd::season::end::begin(12, "other").is_none(), "one job at a time");
    assert!(api_rd::season::end::is_running());
    api_rd::season::end::run(state.clone(), job.clone(), request, "tester".into()).await;
    sidecar.abort();

    let p = job.lock().unwrap().clone();
    assert_eq!(p.state, "done", "{:?} {:?}", p.error, p.notes);
    assert!(!api_rd::season::end::is_running());
    assert_eq!(p.percent, 100.0);
    assert_eq!(p.files_done, 4);
    assert_eq!(*seen.lock().unwrap(), ["stop", "start"], "stopped before zipping, started after");
    assert!(std::fs::read_dir(base.join("content")).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with('.')), "no partial file left");

    let mut zip = zip::ZipArchive::new(std::fs::File::open(base.join("content/Season_12.zip")).unwrap()).unwrap();
    let mut names: Vec<String> = (0..zip.len()).map(|i| zip.by_index(i).unwrap().name().to_string()).collect();
    names.sort();
    assert_eq!(names, [
        "Season_12/mods/create-1.21.1-6.0.4.jar",
        "Season_12/world/level.dat",
        "Season_12/world/region/r.0.0.mca",
        "Season_12/world_the_end/level.dat",
    ], "the event hub and test world are not archived");

    let text = std::fs::read_to_string(base.join("archives/Season_12.json")).unwrap();
    let record: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(record["season"], 12);
    assert_eq!(record["theme"], "Ocean");
    assert_eq!(record["started"], serde_json::Value::Null);
    assert_eq!(record["ended"], "2026-11-12");
    assert_eq!(record["minecraft_version"], "1.21.1");
    assert_eq!(record["mods"], serde_json::json!([{ "name": "create", "version": "6.0.4" }]));
    assert_eq!(record["world"]["file"], "Season_12.zip");
    assert!(record["world"]["size"].as_u64().unwrap() > 0);
    assert_eq!(record["created_by"], "tester");

    // The next season: new seed, nothing else in server.properties changed,
    // the plugin's override written, and only the main world gone.
    assert_eq!(
        std::fs::read_to_string(base.join("server/server.properties")).unwrap(),
        "level-name=world\nrcon.password=hunter2\nlevel-seed=-4172144997902289642\n"
    );
    let over: serde_json::Value = serde_json::from_slice(&std::fs::read(base.join("server/plugins/ServerSideUtils/season-override.json")).unwrap()).unwrap();
    assert_eq!(over, serde_json::json!({ "season": 13, "theme": "Glacier" }));
    assert!(!base.join("server/world").exists() && !base.join("server/world_the_end").exists());
    assert_eq!(std::fs::read(base.join("server/eventhub/region/r.0.0.mca")).unwrap(), b"hub region");
    assert!(base.join("server/testing/level.dat").is_file());
    let leftovers: Vec<String> = std::fs::read_dir(base.join("server")).unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with('.'))
        .collect();
    assert!(leftovers.is_empty(), "the old world is deleted, not left aside: {leftovers:?}");

    // Ending the same season again is refused before the server is touched.
    let again: api_rd::season::end::EndRequest = serde_json::from_value(serde_json::json!({
        "season": 12, "started": null, "ended": null, "additional_info": "",
        "next_season": 13, "next_theme": "Glacier"
    }))
    .unwrap();
    let job = api_rd::season::end::begin(12, "tester").unwrap();
    api_rd::season::end::run(state.clone(), job.clone(), again, "tester".into()).await;
    let p = job.lock().unwrap().clone();
    assert_eq!(p.state, "failed");
    assert_eq!(seen.lock().unwrap().len(), 2, "no further stop or start");

    let _ = std::fs::remove_dir_all(&base);
}

// ---------------------------------------------------------------------------
// Account email and password resets
// ---------------------------------------------------------------------------

/// Everything about an account's address and password needs that account's
/// session -- except using an emailed link, which is the proof by itself.
#[actix_web::test]
async fn account_email_routes_reject_anonymous_callers() {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(lazy_state()))
            .app_data(web::Data::new(api_rd::middleware::rate_limit::RateLimiter::new(500, 60)))
            .configure(configure_api),
    )
    .await;

    let body = serde_json::json!({ "email": "a@example.com", "password": "x",
                                   "currentPassword": "x", "newPassword": "long enough" });
    for (method, uri) in [
        ("GET", "/api/account/settings"),
        ("POST", "/api/account/email"),
        ("DELETE", "/api/account/email"),
        ("POST", "/api/change-password"),
        ("POST", "/api/account/sessions/revoke"),
        ("POST", "/api/auth/oauth/google/link"),
        ("DELETE", "/api/account/google"),
    ] {
        let req = match method {
            "GET" => test::TestRequest::get(),
            "POST" => test::TestRequest::post(),
            _ => test::TestRequest::delete(),
        }
        .uri(uri)
        .set_json(&body)
        .to_request();
        let resp = test::call_service(&app, req).await;
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{method} {uri} must require a session");
    }
}

/// With no Resend key the server says email is unavailable rather than
/// claiming a link is on its way -- and it says so before looking anyone up.
#[actix_web::test]
async fn password_reset_by_email_says_when_email_is_off() {
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(lazy_state()))
            .app_data(web::Data::new(api_rd::middleware::rate_limit::RateLimiter::new(500, 60)))
            .configure(configure_api),
    )
    .await;

    let req = test::TestRequest::post()
        .uri("/api/password/forgot")
        .set_json(serde_json::json!({ "identifier": "someone" }))
        .to_request();
    assert_eq!(test::call_service(&app, req).await.status(), StatusCode::SERVICE_UNAVAILABLE);

    // Missing input is refused before any lookup, too.
    for uri in ["/api/password/reset", "/api/account/email/verify"] {
        let req = test::TestRequest::post().uri(uri).set_json(serde_json::json!({})).to_request();
        assert_eq!(test::call_service(&app, req).await.status(), StatusCode::BAD_REQUEST, "{uri}");
    }
}

/// A stand-in for Resend's send endpoint. Answers every request with success
/// and keeps the JSON body, so a test can read the link out of the message.
async fn fake_resend() -> (String, Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/emails", listener.local_addr().unwrap());
    let outbox = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = Arc::clone(&outbox);

    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let sink = Arc::clone(&sink);
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Headers, then exactly Content-Length bytes of body.
                let body_start = loop {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 { return; }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") { break i + 4; }
                };
                let head = String::from_utf8_lossy(&buf[..body_start]).to_lowercase();
                let length: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                while buf.len() < body_start + length {
                    let n = socket.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 { break; }
                    buf.extend_from_slice(&chunk[..n]);
                }
                if let Ok(json) = serde_json::from_slice(&buf[body_start..]) {
                    sink.lock().unwrap().push(json);
                }
                let reply = "{\"id\":\"test\"}";
                let _ = socket
                    .write_all(format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                        reply.len()
                    ).as_bytes())
                    .await;
            });
        }
    });

    (url, outbox)
}

/// Waits for the outbox to reach `count` messages. Resets are sent in the
/// background, so they can land just after the response.
async fn wait_for_mail(outbox: &std::sync::Mutex<Vec<serde_json::Value>>, count: usize) -> Vec<serde_json::Value> {
    for _ in 0..100 {
        {
            let mail = outbox.lock().unwrap();
            if mail.len() >= count {
                return mail.clone();
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("expected {count} emails, got {:?}", outbox.lock().unwrap());
}

/// Sends a request and returns its status and the `userToken` cookie it set,
/// if any.
macro_rules! call_for_session {
    ($app:expr, $req:expr) => {{
        let resp = test::call_service($app, $req.to_request()).await;
        let cookie = resp
            .response()
            .cookies()
            .find(|c| c.name() == "userToken" && !c.value().is_empty())
            .map(|c| c.into_owned());
        (resp.status(), cookie)
    }};
}

fn link_token(message: &serde_json::Value) -> String {
    let text = message["text"].as_str().unwrap();
    let start = text.find("token=").expect("a link with a token") + "token=".len();
    text[start..].chars().take_while(|c| c.is_ascii_hexdigit()).collect()
}

#[actix_web::test]
async fn an_address_is_confirmed_by_email_and_then_resets_the_password() {
    let Some(pool) = shared_pool().await else {
        eprintln!("skipping an_address_is_confirmed_by_email_and_then_resets_the_password: TEST_DATABASE_URL not set");
        return;
    };
    let _serialized = DB_TEST_LOCK.lock().await;

    sqlx::query(r"DELETE FROM users WHERE username LIKE 'mailtest\_%'")
        .execute(&pool)
        .await
        .expect("clear test accounts");
    let hashed = bcrypt::hash("old password", 4).unwrap();
    for (id, username, hash) in [
        ("mailtest-a-id", "mailtest_a", hashed.as_str()),
        ("mailtest-b-id", "mailtest_b", hashed.as_str()),
        // Made through Google: no password at all.
        ("mailtest-g-id", "mailtest_g", ""),
    ] {
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, ?)")
            .bind(id)
            .bind(username)
            .bind(hash)
            .execute(&pool)
            .await
            .expect("create test account");
    }

    let (url, outbox) = fake_resend().await;
    let mut state = test_state(pool.clone());
    state.config.resend_api_key = "re_test".to_string();
    state.config.email_from = "RainDrippy <no-reply@example.com>".to_string();
    state.config.resend_api_url = url;
    state.config.public_site_url = "https://site.example".to_string();

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .app_data(web::Data::new(api_rd::middleware::rate_limit::RateLimiter::new(10_000, 60)))
            .configure(configure_api),
    )
    .await;

    let a = session_cookie("mailtest_a", "mailtest-a-id");
    let b = session_cookie("mailtest_b", "mailtest-b-id");
    let g = session_cookie("mailtest_g", "mailtest-g-id");

    macro_rules! call {
        ($req:expr) => {{
            let resp = test::call_service(&app, $req.to_request()).await;
            let status = resp.status();
            let body: serde_json::Value =
                serde_json::from_slice(&test::read_body(resp).await).unwrap_or(serde_json::Value::Null);
            (status, body)
        }};
    }
    let settings = |who: &actix_web::cookie::Cookie<'static>| {
        test::TestRequest::get().uri("/api/account/settings").cookie(who.clone())
    };
    let add_email = |who: &actix_web::cookie::Cookie<'static>, email: &str, password: Option<&str>| {
        test::TestRequest::post()
            .uri("/api/account/email")
            .cookie(who.clone())
            .set_json(serde_json::json!({ "email": email, "password": password }))
    };

    // A new account has no address and is asked for one.
    let (status, me) = call!(settings(&a));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(me["email"], serde_json::Value::Null);
    assert_eq!(me["emailVerified"], false);
    assert_eq!(me["hasPassword"], true);
    assert_eq!(me["emailEnabled"], true);

    // Adding one needs the password, so a stolen session cannot swap it.
    assert_eq!(call!(add_email(&a, "Mailtest.A@Example.com", None)).0, StatusCode::BAD_REQUEST);
    assert_eq!(call!(add_email(&a, "mailtest.a@example.com", Some("wrong"))).0, StatusCode::UNAUTHORIZED);
    assert_eq!(call!(add_email(&a, "not an address", Some("old password"))).0, StatusCode::BAD_REQUEST);

    let (status, body) = call!(add_email(&a, "Mailtest.A@Example.com", Some("old password")));
    assert_eq!(status, StatusCode::OK, "{body}");
    let mail = wait_for_mail(&outbox, 1).await;
    assert_eq!(mail[0]["to"][0], "mailtest.a@example.com");
    assert!(mail[0]["text"].as_str().unwrap().contains("https://site.example/verify-email?token="));
    let verify_token = link_token(&mail[0]);

    // Not stored as the account's address until the link is used.
    let (_, me) = call!(settings(&a));
    assert_eq!(me["email"], serde_json::Value::Null);
    assert_eq!(me["pendingEmail"], "mailtest.a@example.com");
    // Nor can a reset be sent to it yet.
    let forgot = |identifier: &str| {
        test::TestRequest::post()
            .uri("/api/password/forgot")
            .set_json(serde_json::json!({ "identifier": identifier }))
    };
    assert_eq!(call!(forgot("mailtest_a")).0, StatusCode::OK);

    // One link a minute.
    assert_eq!(
        call!(add_email(&a, "mailtest.a@example.com", Some("old password"))).0,
        StatusCode::TOO_MANY_REQUESTS
    );

    let verify = |token: &str| {
        test::TestRequest::post()
            .uri("/api/account/email/verify")
            .set_json(serde_json::json!({ "token": token }))
    };
    let (status, body) = call!(verify(&verify_token));
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["email"], "mailtest.a@example.com");
    assert_eq!(call!(verify(&verify_token)).0, StatusCode::BAD_REQUEST, "links work once");

    let (_, me) = call!(settings(&a));
    assert_eq!(me["email"], "mailtest.a@example.com");
    assert_eq!(me["emailVerified"], true);
    assert_eq!(me["pendingEmail"], serde_json::Value::Null);
    // The earlier forgot request, made before the address was confirmed,
    // sent nothing.
    assert_eq!(outbox.lock().unwrap().len(), 1);

    // Someone else asking for the same address gets the same answer, but the
    // inbox gets a note instead of a link.
    let (status, _) = call!(add_email(&b, "mailtest.a@example.com", Some("old password")));
    assert_eq!(status, StatusCode::OK);
    let mail = wait_for_mail(&outbox, 2).await;
    assert_eq!(mail[1]["subject"], "This address is already in use");
    assert!(!mail[1]["text"].as_str().unwrap().contains("token="));

    // Unknown accounts get the same reply and no email.
    let (status, unknown) = call!(forgot("mailtest_nobody"));
    assert_eq!(status, StatusCode::OK);

    // A reset by address.
    let (status, known) = call!(forgot("MAILTEST.A@example.com"));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(known, unknown, "the reply must not say whether the account exists");
    let mail = wait_for_mail(&outbox, 3).await;
    assert_eq!(mail.len(), 3, "no email for the unknown account");
    assert_eq!(mail[2]["subject"], "Reset your password");
    assert!(mail[2]["text"].as_str().unwrap().contains("https://site.example/reset-pwd?token="));
    let reset_token = link_token(&mail[2]);

    // And a second within the minute is quietly not sent.
    assert_eq!(call!(forgot("mailtest_a")).0, StatusCode::OK);

    let reset = |token: &str, password: &str| {
        test::TestRequest::post()
            .uri("/api/password/reset")
            .set_json(serde_json::json!({ "token": token, "newPassword": password }))
    };
    assert_eq!(call!(reset(&reset_token, "short")).0, StatusCode::BAD_REQUEST);
    assert_eq!(call!(reset(&verify_token, "new password 1")).0, StatusCode::BAD_REQUEST, "a verify link is not a reset link");
    let (status, body) = call!(reset(&reset_token, "new password 1"));
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(call!(reset(&reset_token, "new password 2")).0, StatusCode::BAD_REQUEST, "links work once");

    // The reset signed out the session that existed before it.
    assert_eq!(call!(settings(&a)).0, StatusCode::UNAUTHORIZED, "a reset must end existing sessions");

    let login = |password: &str| {
        test::TestRequest::post()
            .uri("/api/login")
            .set_json(serde_json::json!({ "username": "mailtest_a", "password": password }))
    };
    assert_eq!(call!(login("old password")).0, StatusCode::UNAUTHORIZED);
    let (status, a) = call_for_session!(&app, login("new password 1"));
    assert_eq!(status, StatusCode::OK);
    let a = a.expect("login sets a session");
    assert_eq!(call!(settings(&a)).0, StatusCode::OK);
    let mail = wait_for_mail(&outbox, 4).await;
    assert_eq!(mail[3]["subject"], "Your password was changed");

    // Changing it knowing the current one.
    let change = |who: &actix_web::cookie::Cookie<'static>, current: &str, new: &str| {
        test::TestRequest::post()
            .uri("/api/change-password")
            .cookie(who.clone())
            .set_json(serde_json::json!({ "currentPassword": current, "newPassword": new }))
    };
    assert_eq!(call!(change(&a, "wrong", "new password 2")).0, StatusCode::UNAUTHORIZED);
    let (_, other_device) = call_for_session!(&app, login("new password 1"));
    let other_device = other_device.unwrap();
    let (status, fresh) = call_for_session!(&app, change(&a, "new password 1", "new password 2"));
    assert_eq!(status, StatusCode::OK);
    // Every other session ends, including the one it was changed from, which
    // is handed a new cookie so it stays signed in.
    assert_eq!(call!(settings(&other_device)).0, StatusCode::UNAUTHORIZED);
    assert_eq!(call!(settings(&a)).0, StatusCode::UNAUTHORIZED);
    let a = fresh.expect("the changing device gets a fresh session");
    assert_eq!(call!(settings(&a)).0, StatusCode::OK);
    assert_eq!(call!(login("new password 2")).0, StatusCode::OK);

    // An account with no password is pointed at the email route instead.
    let (status, body) = call!(change(&g, "", "new password 1"));
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("Set a password"), "{body}");
    let (_, me) = call!(settings(&g));
    assert_eq!(me["hasPassword"], false);
    // ...and needs no password to add an address.
    assert_eq!(call!(add_email(&g, "mailtest.g@example.com", None)).0, StatusCode::OK);

    // A reset link stops working once the address it went to is removed.
    sqlx::query("DELETE FROM email_tokens WHERE user_id = 'mailtest-a-id'")
        .execute(&pool)
        .await
        .unwrap();
    let before = outbox.lock().unwrap().len();
    assert_eq!(call!(forgot("mailtest_a")).0, StatusCode::OK);
    let mail = wait_for_mail(&outbox, before + 1).await;
    let stale = link_token(mail.last().unwrap());
    let (status, _) = call!(test::TestRequest::delete()
        .uri("/api/account/email")
        .cookie(a.clone())
        .set_json(serde_json::json!({ "password": "new password 2" })));
    assert_eq!(status, StatusCode::OK);
    assert_eq!(call!(reset(&stale, "new password 3")).0, StatusCode::BAD_REQUEST);
    let (_, me) = call!(settings(&a));
    assert_eq!(me["email"], serde_json::Value::Null);

    // Only hashes are stored.
    let (stored,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM email_tokens WHERE token_hash = ?")
        .bind(&stale)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(stored, 0);

    sqlx::query(r"DELETE FROM users WHERE username LIKE 'mailtest\_%'")
        .execute(&pool)
        .await
        .expect("clear test accounts");
}

/// A session ends when it expires, when the password changes by any route,
/// and on "sign out everywhere" -- and every place that accepts the cookie
/// agrees.
#[actix_web::test]
async fn sessions_expire_and_can_be_revoked_everywhere() {
    let Some(pool) = shared_pool().await else {
        eprintln!("skipping sessions_expire_and_can_be_revoked_everywhere: TEST_DATABASE_URL not set");
        return;
    };
    let _serialized = DB_TEST_LOCK.lock().await;

    sqlx::query(r"DELETE FROM users WHERE username LIKE 'sesstest\_%'")
        .execute(&pool)
        .await
        .expect("clear test accounts");
    let hashed = bcrypt::hash("first password", 4).unwrap();
    for (id, username) in [("sesstest-u-id", "sesstest_u"), ("sesstest-admin-id", "sesstest_admin")] {
        sqlx::query("INSERT INTO users (id, username, password_hash) VALUES (?, ?, ?)")
            .bind(id)
            .bind(username)
            .bind(&hashed)
            .execute(&pool)
            .await
            .expect("create test account");
    }
    grant_role(&pool, "sesstest-admin-id", "admin").await;

    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(test_state(pool.clone())))
            .app_data(web::Data::new(api_rd::middleware::rate_limit::RateLimiter::new(10_000, 60)))
            .configure(configure_api),
    )
    .await;

    macro_rules! status {
        ($req:expr) => {
            test::call_service(&app, $req.to_request()).await.status()
        };
    }
    macro_rules! json {
        ($req:expr) => {{
            let resp = test::call_service(&app, $req.to_request()).await;
            serde_json::from_slice::<serde_json::Value>(&test::read_body(resp).await).unwrap()
        }};
    }
    let login = || {
        test::TestRequest::post()
            .uri("/api/login")
            .set_json(serde_json::json!({ "username": "sesstest_u", "password": "first password" }))
    };
    // Each kind of session check: the strict extractor, the fail-open one,
    // the one /validate does by hand, and the permission extractor.
    let alive = |c: &actix_web::cookie::Cookie<'static>| {
        (
            test::TestRequest::get().uri("/api/account/settings").cookie(c.clone()),
            test::TestRequest::get().uri("/api/logged-in").cookie(c.clone()),
            test::TestRequest::get().uri("/api/validate").cookie(c.clone()),
        )
    };

    // An expired token is refused, though correctly signed for a real account.
    let expired = {
        use jsonwebtoken::{encode, EncodingKey, Header};
        let past = chrono::Utc::now().timestamp() - 60 * 60;
        let token = encode(
            &Header::default(),
            &serde_json::json!({ "username": "sesstest_u", "id": "sesstest-u-id", "exp": past }),
            &EncodingKey::from_secret(b"test-secret"),
        )
        .unwrap();
        actix_web::cookie::Cookie::new("userToken", token)
    };
    let (settings, logged_in, validate) = alive(&expired);
    assert_eq!(status!(settings), StatusCode::UNAUTHORIZED);
    assert_eq!(json!(logged_in)["loggedIn"], false);
    assert_eq!(status!(validate), StatusCode::UNAUTHORIZED);

    // Two devices signed in.
    let (_, laptop) = call_for_session!(&app, login());
    let (_, phone) = call_for_session!(&app, login());
    let (laptop, phone) = (laptop.unwrap(), phone.unwrap());
    let (settings, logged_in, validate) = alive(&phone);
    assert_eq!(status!(settings), StatusCode::OK);
    assert_eq!(json!(logged_in)["loggedIn"], true);
    assert_eq!(status!(validate), StatusCode::OK);

    // "Sign out everywhere" from the laptop ends both.
    let (status, cleared) = call_for_session!(
        &app,
        test::TestRequest::post().uri("/api/account/sessions/revoke").cookie(laptop.clone())
    );
    assert_eq!(status, StatusCode::OK);
    assert!(cleared.is_none(), "this device is signed out too");
    for device in [&laptop, &phone] {
        let (settings, logged_in, validate) = alive(device);
        assert_eq!(status!(settings), StatusCode::UNAUTHORIZED);
        assert_eq!(json!(logged_in)["loggedIn"], false);
        assert_eq!(status!(validate), StatusCode::UNAUTHORIZED);
    }

    // Signing in again works, under the new version.
    let (status, phone) = call_for_session!(&app, login());
    assert_eq!(status, StatusCode::OK);
    let phone = phone.unwrap();
    assert_eq!(status!(alive(&phone).0), StatusCode::OK);

    // The admin-generated reset link (the fallback) also ends sessions.
    let (_, admin) = call_for_session!(
        &app,
        test::TestRequest::post()
            .uri("/api/login")
            .set_json(serde_json::json!({ "username": "sesstest_admin", "password": "first password" }))
    );
    let admin = admin.unwrap();
    let link = json!(test::TestRequest::post()
        .uri("/api/admin/gen-pwd-reset")
        .cookie(admin.clone())
        .set_json(serde_json::json!({ "username": "sesstest_u" })));
    let session = link["resetSession"].as_str().expect("a reset session").to_string();
    assert_eq!(
        status!(test::TestRequest::post()
            .uri("/api/forgot-password")
            .set_json(serde_json::json!({ "resetSession": session, "newPassword": "second password" }))),
        StatusCode::OK
    );
    assert_eq!(status!(alive(&phone).0), StatusCode::UNAUTHORIZED);
    // ...and the admin's own session is untouched.
    assert_eq!(status!(alive(&admin).0), StatusCode::OK);

    // A session for a deleted account is no session.
    sqlx::query("DELETE FROM users WHERE id = 'sesstest-admin-id'").execute(&pool).await.unwrap();
    assert_eq!(status!(alive(&admin).0), StatusCode::UNAUTHORIZED);

    sqlx::query(r"DELETE FROM users WHERE username LIKE 'sesstest\_%'")
        .execute(&pool)
        .await
        .expect("clear test accounts");
}

/// Linking starts only from a signed-in account that proves it knows its
/// password, and unlinking never leaves an account with no way in.
#[actix_web::test]
async fn google_can_be_linked_to_and_unlinked_from_an_existing_account() {
    let Some(pool) = shared_pool().await else {
        eprintln!("skipping google_can_be_linked_to_and_unlinked_from_an_existing_account: TEST_DATABASE_URL not set");
        return;
    };
    let _serialized = DB_TEST_LOCK.lock().await;

    sqlx::query(r"DELETE FROM users WHERE username LIKE 'linktest\_%'")
        .execute(&pool)
        .await
        .expect("clear test accounts");
    let hashed = bcrypt::hash("a password", 4).unwrap();
    for (id, username, hash, sub) in [
        ("linktest-p-id", "linktest_p", hashed.as_str(), None),
        ("linktest-l-id", "linktest_l", hashed.as_str(), Some("linktest-sub-1")),
        ("linktest-g-id", "linktest_g", "", Some("linktest-sub-2")),
    ] {
        sqlx::query(
            "INSERT INTO users (id, username, password_hash, oauth_provider, oauth_subject)
             VALUES (?, ?, ?, IF(? IS NULL, NULL, 'google'), ?)",
        )
        .bind(id)
        .bind(username)
        .bind(hash)
        .bind(sub)
        .bind(sub)
        .execute(&pool)
        .await
        .expect("create test account");
    }

    let mut state = test_state(pool.clone());
    state.config.google_client_id = "client-id".to_string();
    state.config.google_client_secret = "client-secret".to_string();
    state.config.google_redirect_uri = "https://site.example/api/auth/oauth/google/callback".to_string();
    let app = test::init_service(
        App::new()
            .app_data(web::Data::new(state))
            .app_data(web::Data::new(api_rd::middleware::rate_limit::RateLimiter::new(10_000, 60)))
            .configure(configure_api),
    )
    .await;

    let p = session_cookie("linktest_p", "linktest-p-id");
    let l = session_cookie("linktest_l", "linktest-l-id");
    let g = session_cookie("linktest_g", "linktest-g-id");
    let link = |who: &actix_web::cookie::Cookie<'static>, password: Option<&str>| {
        test::TestRequest::post()
            .uri("/api/auth/oauth/google/link")
            .cookie(who.clone())
            .set_json(serde_json::json!({ "password": password, "returnTo": "rd" }))
    };
    let unlink = |who: &actix_web::cookie::Cookie<'static>, password: Option<&str>| {
        test::TestRequest::delete()
            .uri("/api/account/google")
            .cookie(who.clone())
            .set_json(serde_json::json!({ "password": password }))
    };
    macro_rules! status {
        ($req:expr) => {
            test::call_service(&app, $req.to_request()).await.status()
        };
    }

    assert_eq!(status!(link(&p, None)), StatusCode::BAD_REQUEST);
    assert_eq!(status!(link(&p, Some("wrong"))), StatusCode::UNAUTHORIZED);

    let resp = test::call_service(&app, link(&p, Some("a password")).to_request()).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.response().cookies().any(|c| c.name() == "oauth_state" && !c.value().is_empty()),
        "the flow's state travels in a cookie"
    );
    let body: serde_json::Value = serde_json::from_slice(&test::read_body(resp).await).unwrap();
    let url = body["url"].as_str().unwrap();
    assert!(url.starts_with("https://accounts.google.com/"), "{url}");
    assert!(url.contains("code_challenge="), "PKCE is used: {url}");

    // Already linked.
    assert_eq!(status!(link(&l, Some("a password"))), StatusCode::CONFLICT);

    // Unlinking needs the password, and leaves password sign-in.
    assert_eq!(status!(unlink(&l, Some("wrong"))), StatusCode::UNAUTHORIZED);
    assert_eq!(status!(unlink(&l, Some("a password"))), StatusCode::OK);
    let (provider, sub): (Option<String>, Option<String>) =
        sqlx::query_as("SELECT oauth_provider, oauth_subject FROM users WHERE id = 'linktest-l-id'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((provider, sub), (None, None));
    assert_eq!(status!(unlink(&l, Some("a password"))), StatusCode::BAD_REQUEST, "nothing left to unlink");

    // A Google-only account cannot unlink its only way in.
    assert_eq!(status!(unlink(&g, None)), StatusCode::BAD_REQUEST);

    sqlx::query(r"DELETE FROM users WHERE username LIKE 'linktest\_%'")
        .execute(&pool)
        .await
        .expect("clear test accounts");
}
