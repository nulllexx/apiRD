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

## Seasons
The **Minecraft Server → Archives** view ends seasons and keeps a record of each one (`src/src/season/`).

- **End season** (`season.end`) reads the season number, theme, Minecraft version and main world that ServerSideUtils publishes to `plugins/ServerSideUtils/season.json` on start-up, shows what will happen, and asks for the start/end dates, any additional info, and the next season's number, theme and (optional) world seed. Then, with progress shown live:
  1. Players are warned, the world is saved, and the server is stopped through the `mc-control` sidecar. Console power actions are refused until the job finishes.
  2. The main world, its `_nether` and `_the_end`, and `mods/*.jar` are zipped into `Season_<n>.zip`, and the zip's contents are checked against what was written. Other worlds (`eventhub`, `testing`, `authhub`, …) are not archived.
  3. The record is written to `ARCHIVES_DIR/Season_<n>.json` (served at `/archives/`; the folder is created if missing), then the zip is published in `SEASONS_PATH` (served at `/raindrippy/content/`).
  4. `level-seed` is set in `server.properties` (blank for random; nothing else in the file changes) and `SEASON_OVERRIDE_PATH` is written with the next season's number and theme.
  5. Only the archived world folders are deleted; every other world is left untouched.
  6. The server is started and generates the new world.

  Nothing is deleted unless every earlier step succeeded. A failure before step 5 undoes what was done (the new record is removed, the settings are put back) and starts the server again. It never replaces an existing zip or record, checks free space first, never follows symlinks and skips anything on the file browser's deny list.
- **Records** (`season.archives`) can be created for old seasons and edited (typos, details) from a form. Every record has exactly the same fields, written in the same order with `null` for anything unknown; the server refuses any other shape. Mods are listed by name with their version shortened (`1.21.1-10.7.7.64` becomes `10.7.7`).

ServerSideUtils reads the season number and theme from `plugins/ServerSideUtils/season-override.json` on start-up, falling back to the defaults in its `Main.java` if the file is missing or invalid, so a new season needs no plugin rebuild.

## Account email and password resets
Accounts can hold one email address, used to send password reset links (`src/src/routes/auth/email.rs`, `src/src/email.rs`). Mail goes out through the [Gmail API](https://developers.google.com/gmail/api/reference/rest/v1/users.messages/send), sent as one Google account.

- **Only confirmed addresses count.** `users.email` holds an address only once its owner has opened a link sent to it (or Google has vouched for it). Until then it waits in `email_tokens`. This matters beyond resets: a Google sign-in is linked to an existing account by matching *confirmed* addresses, so storing a typed-in one would let whoever owns that inbox into the account.
- **Changing or removing the address needs the current password**, when the account has one, so a stolen session cannot swap in the thief's inbox.
- **Links are single-use and short-lived**: 24 hours to confirm an address, 30 minutes to reset a password. Only their SHA-256 is stored. A reset link stops working if the account's address changes after it was sent, and every outstanding reset link (emailed or admin-generated) is dropped when the password changes.
- **Nothing leaks whether an account exists.** `POST /api/password/forgot` gives the same answer for every username or address and sends in the background. Asking to add an address another account holds sends that inbox a note instead of a link, and the requester sees the usual reply.
- **Limits**: one link of each kind per account per minute and five per hour, plus 40 emails per 10 minutes across the whole server.
- **Notices** go to the old address when the address changes or is removed, and to the account's address when its password changes.
- Google sign-ups store Google's address with Google's verified flag. A returning Google sign-in fills in a confirmed address for an account that has none.

| Route | Session | |
|---|---|---|
| `POST /api/register` | no | Optional `email`: sends a confirmation link, like adding it later. The reply's `emailStatus` is `sent`, `not_sent` or `unavailable`; registration succeeds either way, but a malformed address is refused with `400` before the account is created |
| `GET /api/account/settings` | yes | What the settings page shows |
| `POST /api/account/email` | yes | `{email, password}`: send a confirmation link |
| `DELETE /api/account/email` | yes | `{password}`: remove the address |
| `POST /api/account/email/verify` | no | `{token}`: the link's token proves it |
| `POST /api/change-password` | yes | `{currentPassword, newPassword}` |
| `POST /api/password/forgot` | no | `{identifier}`: username or address |
| `POST /api/password/reset` | no | `{token, newPassword}` |
| `POST /api/account/sessions/revoke` | yes | Sign out everywhere |
| `POST /api/auth/oauth/google/link` | yes | `{password, returnTo}`: start linking Google; answers `{url}` |
| `DELETE /api/account/google` | yes | `{password}`: unlink Google |

New passwords need at least 8 characters, and at most 72 bytes (bcrypt ignores the rest).

**Admin resets.** Staff with `users.reset_password` click **Generate Password Reset** in the admin panel (`/api/admin/gen-pwd-reset`). If the account has a verified address, the server emails it the ordinary reset link (same per-account limits) and the panel just says where it went. **Fallback:** only when that is impossible (no verified address, email not configured, or the send failed) does it return a one-time link for staff to pass on by hand; the panel shows it with the reason, and the reset page redeems it through `/api/forgot-password`.

**Setup.** Enable the Gmail API in the Google Cloud project, authorise the sending account once with the `https://www.googleapis.com/auth/gmail.send` scope, and add its refresh token as the `GMAIL_REFRESH_TOKEN` repository secret. `GMAIL_CLIENT_ID` / `GMAIL_CLIENT_SECRET` name the OAuth client that issued it and default to the Google sign-in client. Set the `EMAIL_FROM` repository *variable* (`gh variable set EMAIL_FROM`; the account's address or one of its verified "Send mail as" aliases, e.g. `RainDrippy <mailer@gmail.com>`); `PUBLIC_SITE_URL` sets the base of the links. With any of these missing, email is off and the routes answer `503` instead of pretending to send. A consent screen left in "Testing" makes Google expire the refresh token after 7 days.

## Sessions
A session is a signed JWT in the `userToken` cookie (`src/src/middleware/auth.rs`). It ends when:

- **it expires**, 7 days after sign-in. Tokens without an expiry are refused.
- **the account's sessions are revoked.** Each token carries the account's `session_version` from when it was issued, and `authenticate()` refuses it once `users.session_version` has moved on. The number goes up when the password is changed or reset (by email, by an admin-generated link, or through the old `/reset-password`), and on **Sign out everywhere**. Changing the password while signed in gives that device a fresh session, so only the others are signed out.

Every place that accepts the cookie goes through `authenticate()`: `AuthUser`, `OptionalAuthUser`, `Principal`/`Require<P>`, the dashboard pages, `/validate` (which the AI group chat server asks), `/profile`, `/account-status` and `/refresh-token`. `decode_jwt()` checks the signature and expiry only, so on its own it is not enough to accept a session.

## Linking Google to an existing account
A signed-in account can link Google from its settings, then sign in with either.

- Linking starts with a POST that needs the current password (when the account has one): it adds a way in, so a stolen session alone must not be able to add the thief's Google account. Being a POST, another site cannot start it.
- Google's callback finishes the link only if the browser is still signed in as the account that started it. A Google account already linked elsewhere is refused. If the account has no confirmed email address, it takes Google's verified one.
- Either way it ends on the settings page that started it (`?google=linked|taken|already|cancelled|session|error`); only the two built-in settings pages are possible destinations.
- Unlinking needs the password, and is refused for an account whose only way in is Google.

Accounts are also still linked automatically on Google sign-in when Google's verified address matches a confirmed address on the account.

## Deployment
Locally, you need to create a `.env` file (see the structure of the env example file) in the src folder & run `cargo run`.
For production purposes, we use a GitHub actions workflow that builds the Docker image, pushes it to GHCR and uploads & updates it to our server.

---

*Maintained by RainDrippy*
