# apiRD

Backend API for the BakoSMP Minecraft server community. This project manages authentication, status tracking, project hosting, and server moderation.

## 🚀 Project Status: Migration in Progress
We are currently migrating from a Node.js/Express implementation to a high-performance **Rust** implementation using **Actix-web** and **SQLx**.

- **Rust Source:** `rust-src/` (Active development)
- **Legacy Source:** `src/` (Node.js)

## 🛠 Tech Stack
- **Language:** Rust (Edition 2021)
- **Framework:** Actix-web 4
- **Database:** MariaDB (SQLx)
- **Authentication:** JWT (secure cookies)
- **Deployment:** Docker & Docker Compose
- **CI/CD:** GitHub Actions (Automated build & push to GHCR + SSH Deploy)

## 📦 Deployment

The project is containerized and deployed using GitHub Actions.

### Local Development (Rust)
1. Navigate to `rust-src/`
2. Create a `.env` file (see `.env.example`)
3. Run `cargo run`

### Production Deployment
Pushing to the `main` branch triggers a GitHub Action that:
1. Builds the Docker image.
2. Pushes the image to GitHub Container Registry (GHCR).
3. Connects to the Linux production server via SSH (port 2222).
4. Updates the `api` service using `docker-compose`.

### Infrastructure Requirements
- **Docker Compose:** The stack includes the API and the Minecraft server.
- **Environment:** Requires a `.env` file at `/home/useradmin/api/mainapi/.env` on the host.
- **Volumes:** Mounts include Minecraft server data, Nginx content, and project storage.

## 📂 Directory Structure
- `rust-src/src/`: Core Rust application logic.
- `src/private/`: Static HTML for dashboards (Admin/User).
- `rust-src/src/content/`: Internal static assets.
- `docker-compose.yml`: Production stack definition.
- `.github/workflows/deploy.yml`: CI/CD pipeline.

## 🛡 Security & Moderation
Includes a robust moderation system capable of:
- Permanent and timed bans.
- HWID poisoning for advanced player tracking.
- API Key management for external integrations.

---
*Maintained by the BakoSMP Team.*
