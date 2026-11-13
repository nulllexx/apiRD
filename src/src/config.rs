use std::env;

/// Configuration error surfaced when a required environment variable is
/// missing *or* present-but-empty. The empty case matters: a blank
/// `DB_HOST=""` used to slip past the old `env::var(..).expect(..)` (which only
/// caught *unset* vars) and then crash the sqlx pool at runtime with
/// `Configuration(EmptyHost)`. We reject it up front with a clear message
/// instead — see the deploy workflow, which guards the same class of mistake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    MissingOrEmpty(&'static str),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::MissingOrEmpty(key) => {
                write!(f, "required environment variable {key} is missing or empty")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub db_host: String,
    pub db_port: u16,
    pub db_user: String,
    pub db_password: String,
    pub db_name: String,
    pub jwt_secret: String,
    pub port: u16,
    pub upload_tmp: String,
    pub storage_base: String,
    pub public_url_base: String,
    pub minecraft_skin_path: String,
    pub authed_players_path: String,
    pub player_count_path: String,
    pub server_properties_path: String,
    /// Minecraft's rolling log file, tailed by the admin console.
    pub minecraft_log_path: String,
    /// The container's raw stdout, captured to a file by `start.sh` teeing it.
    /// Carries JVM crashes and start-up output that never reach `latest.log`.
    pub minecraft_stdout_path: String,
    /// How many recent console lines to retain for replay to new subscribers.
    pub console_backlog_lines: usize,
    /// `host:port` of the Minecraft server's RCON listener.
    pub rcon_address: String,
    /// RCON password. Empty disables the console's command box rather than
    /// failing startup, so an existing deployment keeps booting without it.
    pub rcon_password: String,
    /// Shared directory used to reach the `mc-control` sidecar, which holds the
    /// Docker socket so this container does not have to.
    pub control_dir: String,
    /// Where fetched item textures are kept. Must survive a container restart
    /// or every deploy re-fetches the whole set.
    pub item_texture_dir: String,
    /// Minecraft version whose assets the textures come from. Part of the cache
    /// path, so changing it does not serve the previous version's art.
    pub minecraft_assets_version: String,
    /// Mirror the textures are fetched from. Empty disables fetching, leaving a
    /// pre-populated cache as the only source — the air-gapped deployment.
    pub item_texture_base_url: String,
    /// The server's mod jars, read for the textures of modded items. Absent on
    /// a vanilla or plugin-only server, which is not an error.
    pub mods_dir: String,
    /// Root of the game server's files, as mounted into this container. The
    /// admin panel's file browser can read anything beneath it and nothing
    /// outside it.
    pub minecraft_server_dir: String,
    /// Extra patterns the file browser refuses to serve, on top of its built-in
    /// list of credential stores. Comma-separated; see `console::files`.
    pub server_files_deny: Vec<String>,
    /// Extra patterns the file browser will show but never write, on top of
    /// its built-in list. Comma-separated, same syntax as the deny list.
    pub server_files_readonly: Vec<String>,
    /// Where the file browser keeps the previous version of every file it
    /// overwrites. Must be outside the server directory -- backups hold
    /// unmasked secrets -- and writing is refused if it is not. Empty disables
    /// editing and uploads entirely.
    pub server_files_backup_dir: String,
    pub upload_logs_path: String,
    pub content_path: String,
    pub seasons_path: String,
    /// Where ServerSideUtils publishes the current season number, theme,
    /// Minecraft version and main world (`season.json`).
    pub season_info_path: String,
    /// The season number and theme ServerSideUtils uses in place of its
    /// built-in ones (`season-override.json`). Written by "End season" for the
    /// season that follows.
    pub season_override_path: String,
    /// Season archive records, served by the website at `/archives/`.
    pub archives_dir: String,
    pub media_path: String,
    pub cors_origin: String,
    pub private_dir: String,
    pub google_client_id: String,
    pub google_client_secret: String,
    pub google_redirect_uri: String,
    pub oauth_signup_redirect: String,
    pub oauth_success_redirect: String,
    /// OAuth client used to send mail through the Gmail API. Falls back to the
    /// Google sign-in client when `GMAIL_CLIENT_ID` / `GMAIL_CLIENT_SECRET`
    /// are unset; either way the refresh token must come from this client.
    pub gmail_client_id: String,
    pub gmail_client_secret: String,
    /// Refresh token of the sending Google account, granted the
    /// `https://www.googleapis.com/auth/gmail.send` scope. Empty turns email
    /// off: verification and reset links cannot be sent, and the API says so
    /// instead of failing at boot.
    pub gmail_refresh_token: String,
    /// Where the refresh token is exchanged and where messages are posted.
    /// Only overridden by tests, which point them at a local stand-in so
    /// nothing ever reaches the real service.
    pub gmail_token_url: String,
    pub gmail_send_url: String,
    /// The sender, e.g. `RainDrippy <no-reply@example.com>`. Gmail sends from
    /// the authorised account, so the address must be that account or one of
    /// its verified "Send mail as" aliases. Empty also turns email off.
    pub email_from: String,
    /// Base URL of the website, used to build the links in emails.
    pub public_site_url: String,
}

/// A comma-separated list of patterns, blanks dropped.
fn patterns(raw: &str) -> Vec<String> {
    raw.split(',').map(str::trim).filter(|p| !p.is_empty()).map(str::to_string).collect()
}

impl AppConfig {
    /// Build the config from the process environment. Panics with a clear
    /// message if a required variable is missing or empty — the server cannot
    /// run without valid DB credentials, so failing loudly at startup beats a
    /// cryptic pool error later.
    pub fn from_env() -> Self {
        Self::build(|key| env::var(key).ok())
            .unwrap_or_else(|e| panic!("Invalid configuration: {e}"))
    }

    /// Build the config from an arbitrary variable lookup. This is the pure,
    /// testable core of [`from_env`]: it never touches the process
    /// environment, so unit tests can exercise the required/default/parse
    /// logic deterministically and in parallel without racing on global state.
    pub fn build<F>(get: F) -> Result<Self, ConfigError>
    where
        F: Fn(&str) -> Option<String>,
    {
        // Required: must be present AND non-blank (whitespace counts as blank).
        let required = |key: &'static str| -> Result<String, ConfigError> {
            match get(key) {
                Some(v) if !v.trim().is_empty() => Ok(v),
                _ => Err(ConfigError::MissingOrEmpty(key)),
            }
        };
        // Optional: fall back to `default` when unset or empty.
        let optional = |key: &str, default: &str| -> String {
            match get(key) {
                Some(v) if !v.is_empty() => v,
                _ => default.to_string(),
            }
        };
        // Optional numeric port, ignoring unparseable values.
        let port = |key: &str, default: u16| -> u16 {
            get(key).and_then(|p| p.parse().ok()).unwrap_or(default)
        };
        // Optional bounded count, ignoring unparseable values. Clamped so a
        // typo cannot pin an unbounded amount of memory in the console buffer.
        let count = |key: &str, default: usize| -> usize {
            get(key)
                .and_then(|v| v.parse::<usize>().ok())
                .map(|v| v.clamp(50, 10_000))
                .unwrap_or(default)
        };

        Ok(Self {
            db_host: required("DB_HOST")?,
            db_port: port("DB_PORT", 3306),
            db_user: required("DB_USER")?,
            db_password: required("DB_PASSWORD")?,
            db_name: required("DB_NAME")?,
            jwt_secret: required("JWT_SECRET")?,
            port: port("PORT", 5000),
            upload_tmp: optional("UPLOAD_TMP", "data/uploads"),
            storage_base: optional(
                "STORAGE_BASE",
                "/usr/share/nginx/html/raindrippy/projects/public",
            ),
            public_url_base: optional("PUBLIC_URL_BASE", "/raindrippy/projects/public"),
            minecraft_skin_path: optional(
                "MINECRAFT_SKIN_PATH",
                "/mcserver/plugins/SkinsRestorer/skins",
            ),
            authed_players_path: optional("AUTHED_PLAYERS_PATH", "/mcserver/authedPlayers.json"),
            player_count_path: optional("PLAYER_COUNT_PATH", "/mcserver/plrCount.json"),
            server_properties_path: optional(
                "SERVER_PROPERTIES_PATH",
                "/mcserver/server.properties",
            ),
            minecraft_log_path: optional("MINECRAFT_LOG_PATH", "/mcserver/logs/latest.log"),
            minecraft_stdout_path: optional(
                "MINECRAFT_STDOUT_PATH",
                "/mcserver/logs/console.log",
            ),
            console_backlog_lines: count("CONSOLE_BACKLOG_LINES", 500),
            rcon_address: optional("RCON_ADDRESS", "127.0.0.1:29875"),
            rcon_password: get("RCON_PASSWORD").unwrap_or_default(),
            control_dir: optional("CONTROL_DIR", "/control"),
            item_texture_dir: optional(
                "ITEM_TEXTURE_DIR",
                "/home/useradmin/api/mainapi/data/item-textures",
            ),
            minecraft_assets_version: optional("MINECRAFT_ASSETS_VERSION", "1.21.1"),
            mods_dir: optional("MODS_DIR", "/mcserver/mods"),
            minecraft_server_dir: optional("MINECRAFT_SERVER_DIR", "/mcserver"),
            server_files_deny: get("SERVER_FILES_DENY").map(|v| patterns(&v)).unwrap_or_default(),
            server_files_readonly: get("SERVER_FILES_READONLY")
                .map(|v| patterns(&v))
                .unwrap_or_default(),
            // Through `get`, like the texture mirror: set-but-empty means
            // "writes off", unset means the default.
            server_files_backup_dir: get("SERVER_FILES_BACKUP_DIR").unwrap_or_else(|| {
                "/home/useradmin/api/mainapi/data/file-backups".to_string()
            }),
            // Read through `get` rather than `optional` so that setting the
            // variable to an empty string is honoured as "never fetch" instead
            // of silently falling back to the default mirror. Unset still means
            // the default.
            item_texture_base_url: get("ITEM_TEXTURE_BASE_URL").unwrap_or_else(|| {
                "https://raw.githubusercontent.com/InventivetalentDev/minecraft-assets".to_string()
            }),
            upload_logs_path: optional(
                "UPLOAD_LOGS_PATH",
                "/home/useradmin/api/uploadlogs.json",
            ),
            content_path: optional("CONTENT_PATH", "content"),
            seasons_path: optional(
                "SEASONS_PATH",
                "/usr/share/nginx/html/raindrippy/content",
            ),
            season_info_path: optional(
                "SEASON_INFO_PATH",
                "/mcserver/plugins/ServerSideUtils/season.json",
            ),
            season_override_path: optional(
                "SEASON_OVERRIDE_PATH",
                "/mcserver/plugins/ServerSideUtils/season-override.json",
            ),
            archives_dir: optional("ARCHIVES_DIR", "/usr/share/nginx/html/archives"),
            media_path: optional("MEDIA_PATH", "C:/server/media"),
            cors_origin: optional("CORS_ORIGIN", "https://bakosmp.go.ro"),
            private_dir: optional("PRIVATE_DIR", "private"),
            gmail_client_id: get("GMAIL_CLIENT_ID")
                .filter(|v| !v.trim().is_empty())
                .or_else(|| get("GOOGLE_CLIENT_ID"))
                .unwrap_or_default()
                .trim()
                .to_string(),
            gmail_client_secret: get("GMAIL_CLIENT_SECRET")
                .filter(|v| !v.trim().is_empty())
                .or_else(|| get("GOOGLE_CLIENT_SECRET"))
                .unwrap_or_default()
                .trim()
                .to_string(),
            google_client_id: get("GOOGLE_CLIENT_ID").unwrap_or_default(),
            google_client_secret: get("GOOGLE_CLIENT_SECRET").unwrap_or_default(),
            google_redirect_uri: get("GOOGLE_REDIRECT_URI").unwrap_or_default(),
            oauth_signup_redirect: optional(
                "OAUTH_SIGNUP_REDIRECT",
                "https://bakosmp.go.ro/finish-signup",
            ),
            oauth_success_redirect: optional(
                "OAUTH_SUCCESS_REDIRECT",
                "https://bakosmp.go.ro/dashboard",
            ),
            gmail_refresh_token: get("GMAIL_REFRESH_TOKEN").unwrap_or_default().trim().to_string(),
            gmail_token_url: optional("GMAIL_TOKEN_URL", "https://oauth2.googleapis.com/token"),
            gmail_send_url: optional(
                "GMAIL_SEND_URL",
                "https://gmail.googleapis.com/gmail/v1/users/me/messages/send",
            ),
            email_from: get("EMAIL_FROM").unwrap_or_default().trim().to_string(),
            public_site_url: optional("PUBLIC_SITE_URL", "https://bakosmp.go.ro")
                .trim_end_matches('/')
                .to_string(),
        })
    }

    /// Whether emails can be sent at all.
    pub fn email_enabled(&self) -> bool {
        !self.gmail_refresh_token.is_empty()
            && !self.gmail_client_id.is_empty()
            && !self.gmail_client_secret.is_empty()
            && !self.email_from.is_empty()
    }

    pub fn database_url(&self) -> String {
        format!(
            "mysql://{}:{}@{}:{}/{}",
            self.db_user, self.db_password, self.db_host, self.db_port, self.db_name
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A lookup closure backed by a fixed set of key/value pairs — stands in
    /// for the process environment without touching global state.
    fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| map.get(k).cloned()
    }

    /// The minimal set of required vars for a successful build.
    fn required_pairs() -> Vec<(&'static str, &'static str)> {
        vec![
            ("DB_HOST", "db.example"),
            ("DB_USER", "apiuser"),
            ("DB_PASSWORD", "s3cret"),
            ("DB_NAME", "apird"),
            ("JWT_SECRET", "jwt-signing-key"),
        ]
    }

    #[test]
    fn builds_when_required_present() {
        let cfg = AppConfig::build(lookup(&required_pairs())).expect("should build");
        assert_eq!(cfg.db_host, "db.example");
        assert_eq!(cfg.db_user, "apiuser");
        // Optional vars fall back to their documented defaults.
        assert_eq!(cfg.db_port, 3306);
        assert_eq!(cfg.port, 5000);
        assert_eq!(cfg.content_path, "content");
        assert_eq!(cfg.private_dir, "private");
    }

    #[test]
    fn database_url_is_formatted_correctly() {
        let cfg = AppConfig::build(lookup(&required_pairs())).unwrap();
        assert_eq!(cfg.database_url(), "mysql://apiuser:s3cret@db.example:3306/apird");
    }

    #[test]
    fn rejects_missing_required_var() {
        let pairs: Vec<_> = required_pairs()
            .into_iter()
            .filter(|(k, _)| *k != "DB_HOST")
            .collect();
        assert!(matches!(
            AppConfig::build(lookup(&pairs)),
            Err(ConfigError::MissingOrEmpty("DB_HOST"))
        ));
    }

    #[test]
    fn rejects_empty_required_var() {
        // Regression guard for the `Configuration(EmptyHost)` deploy crash:
        // an empty DB_HOST must be rejected, not silently accepted.
        let mut pairs = required_pairs();
        pairs.iter_mut().find(|(k, _)| *k == "DB_HOST").unwrap().1 = "";
        assert!(matches!(
            AppConfig::build(lookup(&pairs)),
            Err(ConfigError::MissingOrEmpty("DB_HOST"))
        ));
    }

    #[test]
    fn rejects_whitespace_only_required_var() {
        let mut pairs = required_pairs();
        pairs
            .iter_mut()
            .find(|(k, _)| *k == "DB_PASSWORD")
            .unwrap()
            .1 = "   ";
        assert!(matches!(
            AppConfig::build(lookup(&pairs)),
            Err(ConfigError::MissingOrEmpty("DB_PASSWORD"))
        ));
    }

    #[test]
    fn parses_custom_ports() {
        let mut pairs = required_pairs();
        pairs.push(("DB_PORT", "3307"));
        pairs.push(("PORT", "8080"));
        let cfg = AppConfig::build(lookup(&pairs)).unwrap();
        assert_eq!(cfg.db_port, 3307);
        assert_eq!(cfg.port, 8080);
    }

    #[test]
    fn ignores_unparseable_port() {
        let mut pairs = required_pairs();
        pairs.push(("DB_PORT", "not-a-number"));
        let cfg = AppConfig::build(lookup(&pairs)).unwrap();
        assert_eq!(cfg.db_port, 3306);
    }

    #[test]
    fn optional_override_is_applied() {
        let mut pairs = required_pairs();
        pairs.push(("CONTENT_PATH", "/srv/content"));
        let cfg = AppConfig::build(lookup(&pairs)).unwrap();
        assert_eq!(cfg.content_path, "/srv/content");
    }

    #[test]
    fn console_defaults_match_the_deployed_mount() {
        // The API container bind-mounts the server dir at /mcserver and shares
        // /control with the sidecar, so these defaults must work with no extra
        // env in docker-compose.
        let cfg = AppConfig::build(lookup(&required_pairs())).unwrap();
        assert_eq!(cfg.minecraft_log_path, "/mcserver/logs/latest.log");
        assert_eq!(cfg.minecraft_stdout_path, "/mcserver/logs/console.log");
        assert_eq!(cfg.console_backlog_lines, 500);
        assert_eq!(cfg.rcon_address, "127.0.0.1:29875");
        assert_eq!(cfg.control_dir, "/control");
    }

    #[test]
    fn item_texture_defaults_land_in_the_persisted_data_mount() {
        // The cache has to outlive the container: /home/useradmin/api is
        // bind-mounted from the host, so a default anywhere else would mean
        // re-fetching every texture after each deploy.
        let cfg = AppConfig::build(lookup(&required_pairs())).unwrap();
        assert_eq!(
            cfg.item_texture_dir,
            "/home/useradmin/api/mainapi/data/item-textures"
        );
        // Must match the server: asset layout and item ids both move between
        // Minecraft versions, and 1.21.4 relocated item models entirely.
        assert_eq!(cfg.minecraft_assets_version, "1.21.1");
        assert!(cfg.item_texture_base_url.starts_with("https://"));
        // Next to server.properties in the same bind mount.
        assert_eq!(cfg.mods_dir, "/mcserver/mods");
        assert_eq!(cfg.minecraft_server_dir, "/mcserver");
        assert!(cfg.server_files_deny.is_empty());
        assert!(cfg.server_files_readonly.is_empty());
        // In the persisted data mount, outside the server directory.
        assert_eq!(cfg.server_files_backup_dir, "/home/useradmin/api/mainapi/data/file-backups");
    }

    #[test]
    fn item_texture_fetching_can_be_turned_off() {
        // An empty base URL is a supported configuration - serve whatever is
        // already cached and never dial out - not a missing setting.
        let mut pairs = required_pairs();
        pairs.push(("ITEM_TEXTURE_BASE_URL", ""));
        pairs.push(("ITEM_TEXTURE_DIR", "/srv/textures"));
        pairs.push(("MINECRAFT_ASSETS_VERSION", "1.20.6"));
        let cfg = AppConfig::build(lookup(&pairs)).unwrap();
        assert_eq!(cfg.item_texture_base_url, "");
        assert_eq!(cfg.item_texture_dir, "/srv/textures");
        assert_eq!(cfg.minecraft_assets_version, "1.20.6");
    }

    #[test]
    fn console_overrides_are_applied() {
        let mut pairs = required_pairs();
        pairs.push(("MINECRAFT_LOG_PATH", "/tmp/latest.log"));
        pairs.push(("MINECRAFT_STDOUT_PATH", "/tmp/console.log"));
        pairs.push(("CONSOLE_BACKLOG_LINES", "1200"));
        pairs.push(("RCON_ADDRESS", "10.0.0.5:25575"));
        pairs.push(("CONTROL_DIR", "/srv/control"));
        let cfg = AppConfig::build(lookup(&pairs)).unwrap();
        assert_eq!(cfg.minecraft_log_path, "/tmp/latest.log");
        assert_eq!(cfg.minecraft_stdout_path, "/tmp/console.log");
        assert_eq!(cfg.console_backlog_lines, 1200);
        assert_eq!(cfg.rcon_address, "10.0.0.5:25575");
        assert_eq!(cfg.control_dir, "/srv/control");
    }

    #[test]
    fn missing_rcon_password_does_not_fail_startup() {
        // Deliberately optional: an existing deployment without RCON_PASSWORD
        // must still boot, with only the console's command box disabled.
        let cfg = AppConfig::build(lookup(&required_pairs())).unwrap();
        assert_eq!(cfg.rcon_password, "");

        let mut pairs = required_pairs();
        pairs.push(("RCON_PASSWORD", "s3cret"));
        let cfg = AppConfig::build(lookup(&pairs)).unwrap();
        assert_eq!(cfg.rcon_password, "s3cret");
    }

    #[test]
    fn email_is_off_until_credentials_and_sender_are_set() {
        let cfg = AppConfig::build(lookup(&required_pairs())).unwrap();
        assert!(!cfg.email_enabled());
        assert_eq!(cfg.gmail_token_url, "https://oauth2.googleapis.com/token");
        assert_eq!(cfg.gmail_send_url, "https://gmail.googleapis.com/gmail/v1/users/me/messages/send");
        assert_eq!(cfg.public_site_url, "https://bakosmp.go.ro");

        let mut pairs = required_pairs();
        pairs.push(("GMAIL_REFRESH_TOKEN", "1//test"));
        assert!(!AppConfig::build(lookup(&pairs)).unwrap().email_enabled());
        // The sign-in client stands in when no Gmail-specific one is given.
        pairs.push(("GOOGLE_CLIENT_ID", "signin-id"));
        pairs.push(("GOOGLE_CLIENT_SECRET", "signin-secret"));
        assert_eq!(AppConfig::build(lookup(&pairs)).unwrap().gmail_client_id, "signin-id");
        assert!(!AppConfig::build(lookup(&pairs)).unwrap().email_enabled());
        // Compose passes unset variables through as empty strings.
        pairs.push(("GMAIL_CLIENT_ID", ""));
        assert_eq!(AppConfig::build(lookup(&pairs)).unwrap().gmail_client_id, "signin-id");
        pairs.pop();
        pairs.push(("GMAIL_CLIENT_ID", "gmail-id"));
        assert_eq!(AppConfig::build(lookup(&pairs)).unwrap().gmail_client_id, "gmail-id");

        pairs.push(("EMAIL_FROM", "RainDrippy <no-reply@example.com>"));
        pairs.push(("PUBLIC_SITE_URL", "https://example.com/"));
        let cfg = AppConfig::build(lookup(&pairs)).unwrap();
        assert!(cfg.email_enabled());
        // A trailing slash would put `//` into every link.
        assert_eq!(cfg.public_site_url, "https://example.com");
    }

    #[test]
    fn console_backlog_is_clamped_and_unparseable_falls_back() {
        let build_with = |v: &str| {
            let mut pairs = required_pairs();
            pairs.push(("CONSOLE_BACKLOG_LINES", v));
            AppConfig::build(lookup(&pairs)).unwrap().console_backlog_lines
        };
        // A zero would make the console replay nothing; a huge value would pin
        // memory. Both are clamped rather than trusted.
        assert_eq!(build_with("0"), 50);
        assert_eq!(build_with("999999"), 10_000);
        assert_eq!(build_with("not-a-number"), 500);
    }
}
