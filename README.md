# apiRD

apiRD is the official repository for the RainDrippy backend API. It manages a load of features, such as accounts, moderation, file uploads and more.

## Stack
- Language: Rust 2021
- Database: MariaDB
- Framework: Actix
- Auth: JWT
- Deployment: Docker & Docker Compose

## Deployment
Locally, you need to create a `.env` file (see the structure of the env example file) in the src folder & run `cargo run`.
For production purposes, we use a GitHub actions workflow that builds the Docker image, pushes it to GHCR and uploads & updates it to our server.

---

*Maintained by RainDrippy*
