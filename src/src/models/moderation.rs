use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct UserModeration {
    pub id: i64,
    pub user_id: String,
    #[sqlx(rename = "type")]
    pub mod_type: String,
    pub mod_note: Option<String>,
    pub moderated_at: String,
    pub expires_at: Option<String>,
    pub created_by: String,
    pub incriminatory: Option<serde_json::Value>,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct PoisonHwid {
    pub id: i64,
    pub hwid: String,
    pub user_id: String,
    pub poisoned_at: String,
}
