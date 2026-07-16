use std::env;

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
    pub fn from_env() -> Self {
        Self {
            db_host: env::var("DB_HOST").expect("DB_HOST must be set"),
            db_port: env::var("DB_PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(3306),
            db_user: env::var("DB_USER").expect("DB_USER must be set"),
            db_password: env::var("DB_PASSWORD").expect("DB_PASSWORD must be set"),
            db_name: env::var("DB_NAME").expect("DB_NAME must be set"),
            jwt_secret: env::var("JWT_SECRET").expect("JWT_SECRET must be set"),
            port: env::var("PORT")
                .ok()
                .and_then(|p| p.parse().ok())
                .unwrap_or(5000),
            upload_tmp: env::var("UPLOAD_TMP").unwrap_or_else(|_| "data/uploads".to_string()),
            storage_base: env::var("STORAGE_BASE").unwrap_or_else(|_| {
                "/usr/share/nginx/html/raindrippy/projects/public".to_string()
            }),
            public_url_base: env::var("PUBLIC_URL_BASE")
                .unwrap_or_else(|_| "/raindrippy/projects/public".to_string()),
            minecraft_skin_path: env::var("MINECRAFT_SKIN_PATH")
                .unwrap_or_else(|_| "/mcserver/plugins/SkinsRestorer/skins".to_string()),
            authed_players_path: env::var("AUTHED_PLAYERS_PATH")
                .unwrap_or_else(|_| "/mcserver/authedPlayers.json".to_string()),
            player_count_path: env::var("PLAYER_COUNT_PATH")
                .unwrap_or_else(|_| "/mcserver/plrCount.json".to_string()),
            server_properties_path: env::var("SERVER_PROPERTIES_PATH")
                .unwrap_or_else(|_| "/mcserver/server.properties".to_string()),
            upload_logs_path: env::var("UPLOAD_LOGS_PATH")
                .unwrap_or_else(|_| "/home/useradmin/api/uploadlogs.json".to_string()),
            content_path: env::var("CONTENT_PATH")
                .unwrap_or_else(|_| "content".to_string()),
            seasons_path: env::var("SEASONS_PATH").unwrap_or_else(|_| {
                "/usr/share/nginx/html/raindrippy/content".to_string()
            }),
            media_path: env::var("MEDIA_PATH")
                .unwrap_or_else(|_| "C:/server/media".to_string()),
            cors_origin: env::var("CORS_ORIGIN")
                .unwrap_or_else(|_| "https://bakosmp.go.ro".to_string()),
            private_dir: env::var("PRIVATE_DIR")
                .unwrap_or_else(|_| "private".to_string()),
            google_client_id: env::var("GOOGLE_CLIENT_ID").unwrap_or_default(),
            google_client_secret: env::var("GOOGLE_CLIENT_SECRET").unwrap_or_default(),
            google_redirect_uri: env::var("GOOGLE_REDIRECT_URI").unwrap_or_default(),
            oauth_signup_redirect: env::var("OAUTH_SIGNUP_REDIRECT")
                .unwrap_or_else(|_| "https://bakosmp.go.ro/finish-signup".to_string()),
            oauth_success_redirect: env::var("OAUTH_SUCCESS_REDIRECT")
                .unwrap_or_else(|_| "https://bakosmp.go.ro/dashboard".to_string()),
        }
    }

    pub fn database_url(&self) -> String {
        format!(
            "mysql://{}:{}@{}:{}/{}",
            self.db_user, self.db_password, self.db_host, self.db_port, self.db_name
        )
    }
}
