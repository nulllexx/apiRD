# apiRD

[![License: GPL v3](https://img.shields.io/badge/License-GPLv3-blue.svg)](https://www.gnu.org/licenses/gpl-3.0)

apiRD is the official repository for the RainDrippy backend API. It manages a load of features, such as accounts, moderation, file uploads and more.

## Stack
- Language: Rust 2021
- Database: MariaDB
- Framework: Actix
- Auth: JWT
- Deployment: Docker & Docker Compose

## Access control
Staff access is roles-based (`src/src/access/`). A **role** is a named set of permissions (`console.command`, `users.moderate`, …); an account can hold several roles. Roles are managed in the admin panel's **Roles** tab or under `/api/admin/access/*`.

- The built-in `admin` role holds every permission, including ones added later, and can't be edited or deleted. Only an admin can grant it, and the last one can't be removed.
- Permissions are read from the database on every request, so a grant or revocation takes effect immediately. Session tokens carry no privilege.
- No escalation: you can only create, edit, grant or revoke roles made of permissions you hold yourself. You can't moderate, delete or reset the password of an account that has access you lack.
- Every change is recorded in `access_audit`, shown in the Roles tab.
- On first start-up, accounts with the old `users.is_admin` flag are moved into the `admin` role once, and a starter `moderator` role is created. The `is_admin` column is not read after that.

## Deployment
Locally, you need to create a `.env` file (see the structure of the env example file) in the src folder & run `cargo run`.
For production purposes, we use a GitHub actions workflow that builds the Docker image, pushes it to GHCR and uploads & updates it to our server.

---

*Maintained by RainDrippy*
