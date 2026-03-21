# Rebuild apiRD in Rust

## Context
The existing `apiRD` is a Node.js/Express API serving a Minecraft server community (authentication, status/incident tracking, project file hosting, API key management, server control via Docker, moderation with HWID poisoning). The user wants an exact functional copy in Rust — same endpoints, same database schema, same behavior.

## Rust Stack
- **actix-web 4** — web framework
- **sqlx** (mysql feature) — async MariaDB driver with pool
- **jsonwebtoken 9** — JWT
- **bcrypt** — password hashing
- **serde / serde_json** — serialization
- **actix-multipart** — file uploads
- **actix-cors / actix-files** — CORS + static serving
- **tokio** — async runtime
- **uuid, chrono, dotenvy, reqwest, notify, sha2, rand**

## Location
All Rust code lives in `rust-src/` at the repo root, keeping the original Node.js `src/` untouched.

## Project Structure
```
rust-src/
  Cargo.toml
  Dockerfile
  .env.example
  src/
    main.rs             # Entry point, server setup, background tasks
    config.rs           # Env vars + constants (AppConfig)
    db.rs               # Pool init, CREATE TABLE, migrations, seeding
    error.rs            # AppError enum implementing ResponseError
    models/
      mod.rs, user.rs, api_key.rs, component.rs, incident.rs,
      project.rs, moderation.rs, meta.rs, password_reset.rs
    middleware/
      mod.rs, auth.rs, auth_failopen.rs, admin_auth.rs,
      api_key_auth.rs, rate_limit.rs, dashboard.rs
    routes/
      mod.rs, auth.rs, status.rs, projects.rs,
      api_keys.rs, api.rs, util.rs
```

---

## Implementation Phases

### Phase 1: Scaffolding + DB
**Files:** `Cargo.toml`, `src/main.rs`, `src/config.rs`, `src/db.rs`, `src/error.rs`, `src/models/*`

- `Cargo.toml` with all dependencies
- `AppConfig::from_env()` — reads all env vars with same defaults as JS
- `init_database()` — 12 CREATE TABLE IF NOT EXISTS statements (matching `src/db.js`)
- `fix_database_schema()` — column renames/adds migration
- `seed_components()` — 6 default components
- `AppState` struct: `MySqlPool`, `AppConfig`, `Arc<RwLock<u32>>` for player_count/max_players
- Model structs deriving `sqlx::FromRow`, `Serialize`, `Deserialize`
- `AppError` enum: BadRequest/Unauthorized/Forbidden/NotFound/Conflict/PayloadTooLarge/Internal

**Test:** Server starts, connects to MariaDB, creates tables.

### Phase 2: Middleware
**Files:** `src/middleware/*`

- `AuthUser` — FromRequest extractor, verifies JWT from `userToken` cookie
- `OptionalAuthUser` — same but fail-open (returns `Option<AuthUser>`)
- `AdminUser` — JWT + `is_admin=1` DB check
- API key middleware — `X-API-Key` header, DB validation, hourly rate limit (reset if 60+ min since last_reset)
- In-memory rate limiter — `DashMap<IpAddr, VecDeque<Instant>>`, 50 req/60s
- Dashboard guards — JWT+admin check, redirect to login URLs on failure

### Phase 3: Auth Routes (`/api`) — largest file
**File:** `src/routes/auth.rs` (replicates `routes/auth.js`, ~850 lines)

Endpoints:
- `POST /register` — HWID poison check, bcrypt(10), JWT cookie (7d, httpOnly/Secure/SameSite=Strict)
- `POST /login` — bcrypt compare, JWT cookie
- `POST /v-creds` — credential validation
- `GET /validate`, `/account-status`, `/account-data`, `/logged-in`
- `DELETE /delete-account`
- `GET /get-key` — generate API key with SHA256-based UUID
- `GET /api-usage`, `/proj/allowed`, `/plex/allowed`
- `POST /refresh-token`, `/logout`, `/purge-logout`
- `GET /fetch-worlds` — list Season_*.zip files
- `POST /uploadskinfile`, `/delskin`, `GET /userskins/:username`
- `DELETE /admin/delete-user`, `POST /admin/moderate`, `/update-admin-status`

Key details:
- Device fingerprint UUID: SHA256(system_info + random bytes), skip canvas (negligible entropy)
- authedPlayers.json file locking: use `fs2` crate
- Moderation: ban types 1d/3d/7d/14d/perm/poison with calculated expiration

### Phase 4: Status Routes (`/api/status`)
**File:** `src/routes/status.rs` (replicates `controllers/statusController.js`)

- `GET /status` — components + active incidents with history
- `GET /incidents`, `/incidents/:id`, `/incidentHistory`, `/debug`
- `POST /incidents` (admin) — create with status history + affected components
- `POST /incidents/:id/updates` (admin) — add update, optionally change status, handle resolution
- `PATCH /components/:id` (admin) — update component status

PDT formatting: `chrono::FixedOffset::west_opt(7*3600)` for UTC-7.

### Phase 5: Project Routes (`/api/projects`)
**File:** `src/routes/projects.rs` (replicates `controllers/projectController.js`)

- Full CRUD for projects and files
- Quota: 3 projects max, 1GB per user
- Storage base: `/usr/share/nginx/html/raindrippy/projects/public`
- Cross-device file move: `tokio::fs::rename`, fallback to copy+unlink on EXDEV
- Multipart upload via actix-multipart

### Phase 6: API Key + Protected API Routes
**Files:** `src/routes/api_keys.rs`, `src/routes/api.rs`

- API keys: admin-only CRUD
- Protected: playercount, hash, files, serverrunning, restart, startserver
- Docker commands via `tokio::process::Command`

### Phase 7: Utility Routes (`/api/util`)
**File:** `src/routes/util.rs`

- `POST /check-updates` — version file comparison
- `GET /games` — read data folder, group by name
- `GET /router/perf` — HTTPS to 192.168.0.1 with `reqwest` (accept invalid certs)

### Phase 8: Static + Dashboard + Background Tasks
**In `main.rs`:**

- Dashboard HTML: serve `src/private/*.html` behind admin guards
- Static: `actix_files::Files::new("/content", "../content")`
- Auto-unban: `tokio::spawn` with 60s interval, delete expired moderation rows
- File watchers: `notify` crate watching plrCount.json + server.properties

### Phase 9: Dockerfile + Cleanup + Plan Copy
- Multi-stage: `rust:1.82` builder -> `debian:bookworm-slim` runtime
- `.env.example` documenting all env vars
- Copy this implementation plan as `rust-src/IMPLEMENTATION_PLAN.md` for reference

### Phase 10: Server Switchover (Node.js -> Rust)

The `api` service in Docker Compose currently builds from the repo root using the Node.js `Dockerfile`. To switch to Rust:

**Step 1 — New Dockerfile in `rust-src/`**
```dockerfile
FROM rust:1.82 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates libssl3 && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/api-rd /usr/local/bin/
WORKDIR /home/useradmin/api/mainapi
EXPOSE 5000
CMD ["api-rd"]
```

**Step 2 — Update `docker-compose.yml` on the server**
Change the `api` service to point to the Rust build context:
```yaml
api:
  build:
    context: ./apiRD/rust-src    # was: ./apiRD
    dockerfile: Dockerfile
  # ports, volumes, env_file, networks stay the same
```

**Step 3 — Preserve volume mounts**
The Rust binary expects the same filesystem paths as the Node.js app. Ensure these volume mounts remain:
- `/mcserver` — Minecraft server directory (skins, plrCount.json, server.properties, authedPlayers.json)
- `/usr/share/nginx/html/raindrippy` — project file storage + season ZIPs
- `/home/useradmin/api` — upload logs
- `.env` file mounted or env vars passed via `env_file`/`environment` in compose

**Step 4 — Rebuild and swap**
```bash
# On the server:
cd /path/to/compose
docker compose stop api
docker compose build api        # builds the Rust image
docker compose up -d api        # starts the new Rust binary
docker compose logs -f api      # verify startup + DB connection
```

**Step 5 — Rollback if needed**
To revert, change `docker-compose.yml` back to the original context and rebuild:
```yaml
api:
  build:
    context: ./apiRD
    dockerfile: Dockerfile       # original Node.js Dockerfile
```
```bash
docker compose stop api && docker compose build api && docker compose up -d api
```

**Step 6 — Cleanup (after confirming Rust version is stable)**
- Remove the Node.js `Dockerfile` at repo root (optional, keep for rollback)
- Remove `node_modules/`, `package.json`, `package-lock.json` if no longer needed
- Update the repo README to reflect the Rust stack

---

## Verification

1. `cargo build` — compiles without errors
2. Start with MariaDB running — tables created, components seeded
3. `POST /api/register` + `POST /api/login` — JWT cookie returned
4. `GET /api/validate` — returns user info
5. `GET /api/status/status` — returns components and incidents
6. Project CRUD + file upload — quota enforced
7. API key creation + rate limiting
8. Docker commands (if Minecraft container available)
9. Auto-unban fires every 60s (check logs)
10. Compare response shapes with original JS endpoints
11. After switchover: hit every endpoint from the frontend at `bakosmp.go.ro` and confirm identical behavior
