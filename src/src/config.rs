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
    pub upload_logs_path: String,
    pub content_path: String,
    pub seasons_path: String,
    pub media_path: String,
    pub cors_origin: String,
    pub private_dir: String,
    pub google_client_id: String,
    pub google_client_secret: String,
    pub google_redirect_uri: String,
    pub oauth_signup_redirect: String,
    pub oauth_success_redirect: String,
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
            minecraft_assets_version: optional("MINECRAFT_ASSETS_VERSION", "1.21.4"),
            mods_dir: optional("MODS_DIR", "/mcserver/mods"),
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
            media_path: optional("MEDIA_PATH", "C:/server/media"),
            cors_origin: optional("CORS_ORIGIN", "https://bakosmp.go.ro"),
            private_dir: optional("PRIVATE_DIR", "private"),
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
        })
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
        assert_eq!(cfg.minecraft_assets_version, "1.21.4");
        assert!(cfg.item_texture_base_url.starts_with("https://"));
        // Next to server.properties in the same bind mount.
        assert_eq!(cfg.mods_dir, "/mcserver/mods");
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
