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

## Server file browser
The admin panel's **Minecraft Server → Files** view browses `MINECRAFT_SERVER_DIR` (default `/mcserver`). It needs its own `console.files` permission — `console.view` does not include it — and is read-only. Paths can't leave the server directory (symlinks included), credential stores such as `authedPlayers.json` and `.rcon-cli.*` are never served (extend the list with `SERVER_FILES_DENY`), and values of secret-looking config keys like `rcon.password` are masked in both the viewer and downloads. Every file viewed or downloaded is recorded in the console audit log. See `src/src/console/files.rs`.

Text files open in the viewer with syntax highlighting. Two more permissions allow changes; neither is in the starter `moderator` role:

- `console.edit` — edit text config files in place. High trust: configs decide how the server and its mods behave.
- `console.upload` — upload new config files and images. Replacing an existing file also needs `console.edit`.

Whoever holds them, these hold (`src/src/console/files_write.rs`):

- **No code.** Only text config formats (and real images, for uploads) are written. Jars, scripts, `mods/`, `kubejs/` and datapacks are read-only, as are logs and `ops.json` / the whitelist / ban lists (those belong to the Players view).
- **No reaching other permissions.** The `server.properties` keys for RCON, query, online mode, whitelist and op levels are locked, parsed the way Java parses them.
- **Secrets stay put.** Masked values are kept as they are on save. An existing secret can't be changed, removed, moved or copied to another file, and nothing reveals one.
- **Nothing is lost.** Writes are atomic, refused if the file changed since it was opened, and backed up first to `SERVER_FILES_BACKUP_DIR` (outside the server directory; writes are refused if it isn't). If the backup fails, nothing is written.
- **Panel only, and attributed.** Writes need an `X-RD-Panel` header, which pages on other origins can't send, and every attempt, refused ones included, is in the audit log.

## Deployment
Locally, you need to create a `.env` file (see the structure of the env example file) in the src folder & run `cargo run`.
For production purposes, we use a GitHub actions workflow that builds the Docker image, pushes it to GHCR and uploads & updates it to our server.

---

*Maintained by RainDrippy*
