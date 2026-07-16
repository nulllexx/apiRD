use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct PasswordResetSession {
    pub id: i64,
    pub username: String,
    pub session_token: String,
    pub expires_at: String,
    pub created_at: String,
}
