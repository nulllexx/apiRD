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
}
