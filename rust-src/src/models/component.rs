use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Component {
    pub id: String,
    pub name: String,
    pub status: String,
    pub last_updated: String,
}

#[derive(Debug, Deserialize)]
pub struct UpdateComponentRequest {
    pub status: Option<String>,
    pub name: Option<String>,
}
