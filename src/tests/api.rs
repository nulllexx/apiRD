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
        ),
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
        "/api/admin/console/stats",
        "/api/admin/console/online",
        "/api/admin/console/players",
        // Reads a file off the server's disk, so it must be behind the
        // admin gate exactly like everything else here.
        "/api/admin/console/players/069a79f4-44e9-4726-a5be-fca90e38aaf5/inventory",
        // Fetches from a mirror and writes to disk on a miss, so an
        // anonymous caller must not be able to drive it either.
        "/api/admin/console/item-texture/minecraft/diamond_sword.png",
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
        ),
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

