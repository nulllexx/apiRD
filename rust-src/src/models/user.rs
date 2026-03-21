use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub username: String,
    pub password_hash: String,
    pub is_admin: bool,
    #[sqlx(rename = "apiKeyId")]
    pub api_key_id: Option<String>,
    pub created_at: String,
    pub is_member: bool,
    pub is_projallowed: bool,
    pub is_plexallowed: bool,
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub username: String,
    pub password: String,
    pub hwid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct ValidateCredsRequest {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct ModerateRequest {
    pub username: String,
    #[serde(rename = "type")]
    pub ban_type: String,
    pub mod_note: Option<String>,
    pub incriminatory: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAdminStatusRequest {
    pub username: String,
    pub is_admin: bool,
}

#[derive(Debug, Deserialize)]
pub struct DeleteUserRequest {
    pub username: String,
}

#[derive(Debug, Deserialize)]
pub struct SkinDeleteRequest {
    pub username: String,
    pub password: String,
    pub filename: String,
}
