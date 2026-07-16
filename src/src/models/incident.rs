use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Incident {
    pub id: String,
    pub title: String,
    pub impact: String,
    pub status: String,
    pub status_text: Option<String>,
    pub status_updated_at: String,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub created_by: String,
    pub created_at: String,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct IncidentUpdate {
    pub id: i64,
    pub incident_id: String,
    pub time: String,
    pub body: String,
    pub author: String,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct IncidentStatusHistory {
    pub id: i64,
    pub incident_id: String,
    pub status: String,
    pub status_text: Option<String>,
    pub status_updated_at: String,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct IncidentAffectedComponent {
    pub incident_id: String,
    pub component_id: String,
}

#[derive(Debug, Deserialize)]
pub struct CreateIncidentRequest {
    pub title: String,
    pub impact: String,
    pub status: Option<String>,
    pub status_text: Option<String>,
    pub message: Option<String>,
    pub affected_components: Option<Vec<AffectedComponentInput>>,
}

#[derive(Debug, Deserialize)]
pub struct AffectedComponentInput {
    pub id: String,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AddIncidentUpdateRequest {
    pub body: String,
    pub status: Option<String>,
    pub status_text: Option<String>,
}
