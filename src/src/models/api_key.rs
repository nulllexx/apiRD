use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub name: String,
    pub api_key: String,
    pub hourly_limit: i32,
    pub created_at: String,
    pub last_reset: String,
    pub request_count: i32,
}

#[derive(Debug, Deserialize)]
pub struct CreateApiKeyRequest {
    pub name: String,
    pub hourly_limit: Option<i32>,
}
