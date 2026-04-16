# GEMINI.md - apiRD

## Project Overview
**apiRD** is the backend API for the BakoSMP Minecraft server community. It manages critical infrastructure including user authentication, status and incident tracking (status page), project file hosting, API key management, and server moderation (including HWID poisoning).

The project is currently in a **migration phase** from a Node.js/Express implementation to a high-performance Rust implementation using Actix-web and SQLx.

### Core Technologies
- **Rust (Target):** Actix-web 4, SQLx (MariaDB), JWT (jsonwebtoken), Bcrypt, Serde.
- **Node.js (Legacy):** Express, MariaDB, JWT, Bcrypt.
- **Database:** MariaDB (Primary).
- **Infrastructure:** Docker-based deployment, integrated with Minecraft server files and Nginx for static serving.

### Key Features
- **Auth:** JWT-based authentication with secure cookies, registration, login, and account management.
- **Status Dashboard:** Component status tracking and incident reporting system.
- **Projects:** User-managed file hosting with quotas (3 projects, 1GB total).
- **Moderation:** Advanced ban system including timed bans, permanent bans, and HWID "poisoning".
- **Minecraft Integration:** Synchronizes with `server.properties`, `plrCount.json`, and handles player skins.

---

## Directory Structure
- `rust-src/`: The primary active development directory (Rust).
  - `src/main.rs`: Server entry point and background tasks (auto-unban, file watchers).
  - `src/db.rs`: Database initialization, migrations, and seeding.
  - `src/routes/`: API endpoint handlers grouped by domain (auth, status, projects, etc.).
  - `src/models/`: SQLx models and Serde structs.
  - `IMPLEMENTATION_PLAN.md`: Detailed roadmap of the Node.js to Rust migration.
- `src/`: Legacy Node.js implementation.
- `content/`: Static content and assets served by the API.
- `Dockerfile`: Multi-stage Docker build for the Rust application (located at root for GitHub Actions).

---

## Building and Running

### Rust Version (Preferred)
1. **Navigate to the Rust directory:**
   ```bash
   cd rust-src
   ```
2. **Setup Environment:**
   Copy `.env.example` to `.env` and configure your MariaDB credentials and JWT secret.
3. **Run the application:**
   ```bash
   cargo run
   ```
4. **Build for production:**
   ```bash
   cargo build --release
   ```

### Node.js Version (Legacy)
1. **Install dependencies:**
   ```bash
   npm install
   ```
2. **Start the server:**
   ```bash
   npm start
   ```

---

## Development Conventions

### Database Schema
The database schema is managed automatically by the application on startup.
- In Rust: See `rust-src/src/db.rs` for `CREATE TABLE` statements and migrations.
- In Node.js: See `src/db.js` and `src/migrations/`.
- **Note:** Always ensure schema changes are mirrored in the `init_database` or `fix_database_schema` functions in `rust-src/src/db.rs`.

### API Design
- Endpoints are prefixed with `/api`.
- RESTful principles are generally followed.
- Authentication is handled via JWT stored in the `userToken` cookie.

### Error Handling
- **Rust:** Uses a custom `AppError` enum (in `error.rs`) that implements `actix_web::ResponseError` for consistent JSON error responses.

### Background Tasks
- **Auto-unban:** A background loop runs every 60 seconds to clear expired bans.
- **File Watchers:** Uses the `notify` crate to watch Minecraft server files (`plrCount.json`, `server.properties`) and update in-memory state.

### Migration Guidelines
When implementing new features or fixing bugs, prioritize the **Rust** version in `rust-src/`. Refer to `rust-src/IMPLEMENTATION_PLAN.md` to check the status of specific modules.
