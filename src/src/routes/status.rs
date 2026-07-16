use actix_web::{web, HttpResponse};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::AppError;
use crate::middleware::admin_auth::AdminUser;
use crate::models::component::Component;
use crate::models::incident::{Incident, IncidentStatusHistory, IncidentUpdate};
use crate::models::meta::Meta;
use crate::AppState;

// ---------------------------------------------------------------------------
// PDT time helper
// ---------------------------------------------------------------------------

fn format_time_to_pdt(iso_string: &str) -> String {
    // Parse the datetime string, subtract 7 hours for PDT (UTC-7), format as H:MM AM/PM (PDT)
    let dt = if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(iso_string, "%Y-%m-%d %H:%M:%S")
    {
        dt
    } else if let Ok(dt) =
        chrono::NaiveDateTime::parse_from_str(iso_string, "%Y-%m-%dT%H:%M:%S%.fZ")
    {
        dt
    } else if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(iso_string, "%Y-%m-%dT%H:%M:%S") {
        dt
    } else {
        return iso_string.to_string();
    };

    let pdt = dt - chrono::Duration::hours(7);
    let hour = pdt.format("%H").to_string().parse::<u32>().unwrap_or(0);
    let minute = pdt.format("%M").to_string();
    let ampm = if hour >= 12 { "PM" } else { "AM" };
    let display_hour = if hour % 12 == 0 { 12 } else { hour % 12 };
    format!("{}:{} {} (PDT)", display_hour, minute, ampm)
}

/// Attach status history to an incident row, returning a JSON value with history_status added.
async fn incident_with_history(
    pool: &sqlx::MySqlPool,
    incident: &Incident,
) -> Result<serde_json::Value, AppError> {
    let history: Vec<IncidentStatusHistory> = sqlx::query_as(
        "SELECT id, incident_id, status, status_text, CAST(status_updated_at AS CHAR) AS status_updated_at \
         FROM incident_status_history \
         WHERE incident_id = ? \
         ORDER BY status_updated_at ASC",
    )
    .bind(&incident.id)
    .fetch_all(pool)
    .await?;

    let history_status: Vec<serde_json::Value> = history
        .iter()
        .map(|h| {
            json!({
                "status": h.status,
                "context": h.status_text,
                "time": format_time_to_pdt(&h.status_updated_at)
            })
        })
        .collect();

    let mut val = serde_json::to_value(incident).unwrap_or(json!({}));
    if let Some(obj) = val.as_object_mut() {
        obj.insert("history_status".to_string(), json!(history_status));
    }
    Ok(val)
}

// ---------------------------------------------------------------------------
// Helper: current UTC timestamp formatted for MariaDB
// ---------------------------------------------------------------------------

fn now_formatted() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

// ---------------------------------------------------------------------------
// GET /status — public
// ---------------------------------------------------------------------------

async fn get_status(state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    let components: Vec<Component> =
        sqlx::query_as("SELECT id, name, status, CAST(last_updated AS CHAR) AS last_updated FROM components")
            .fetch_all(&state.pool)
            .await?;

    let incidents: Vec<Incident> = sqlx::query_as(
        "SELECT id, title, impact, status, status_text, CAST(status_updated_at AS CHAR) AS status_updated_at, \
         CAST(started_at AS CHAR) AS started_at, CAST(ended_at AS CHAR) AS ended_at, created_by, CAST(created_at AS CHAR) AS created_at \
         FROM incidents WHERE status != 'resolved' ORDER BY started_at DESC",
    )
    .fetch_all(&state.pool)
    .await?;

    let mut incident_values: Vec<serde_json::Value> = Vec::with_capacity(incidents.len());
    for inc in &incidents {
        incident_values.push(incident_with_history(&state.pool, inc).await?);
    }

    // Upsert generated_at in meta table
    let now = now_formatted();
    sqlx::query(
        "INSERT INTO meta (`key`, `value`) VALUES (?, ?) ON DUPLICATE KEY UPDATE `value` = ?",
    )
    .bind("generated_at")
    .bind(&now)
    .bind(&now)
    .execute(&state.pool)
    .await?;

    let meta = json!({ "generated_at": now });

    Ok(HttpResponse::Ok().json(json!({
        "components": components,
        "incidents": incident_values,
        "meta": meta
    })))
}

// ---------------------------------------------------------------------------
// GET /incidents — public (all incidents with status history)
// ---------------------------------------------------------------------------

async fn list_incidents(state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    let incidents: Vec<Incident> = sqlx::query_as(
        "SELECT id, title, impact, status, status_text, CAST(status_updated_at AS CHAR) AS status_updated_at, \
         CAST(started_at AS CHAR) AS started_at, CAST(ended_at AS CHAR) AS ended_at, created_by, CAST(created_at AS CHAR) AS created_at \
         FROM incidents ORDER BY started_at DESC",
    )
    .fetch_all(&state.pool)
    .await?;

    let mut result: Vec<serde_json::Value> = Vec::with_capacity(incidents.len());
    for inc in &incidents {
        result.push(incident_with_history(&state.pool, inc).await?);
    }

    Ok(HttpResponse::Ok().json(result))
}

// ---------------------------------------------------------------------------
// GET /incidents/{id} — public (single incident + updates)
// ---------------------------------------------------------------------------

async fn get_incident(
    state: web::Data<AppState>,
    path: web::Path<String>,
) -> Result<HttpResponse, AppError> {
    let id = path.into_inner();

    let incident: Option<Incident> = sqlx::query_as(
        "SELECT id, title, impact, status, status_text, CAST(status_updated_at AS CHAR) AS status_updated_at, \
         CAST(started_at AS CHAR) AS started_at, CAST(ended_at AS CHAR) AS ended_at, created_by, CAST(created_at AS CHAR) AS created_at \
         FROM incidents WHERE id = ?",
    )
    .bind(&id)
    .fetch_optional(&state.pool)
    .await?;

    let incident = incident.ok_or_else(|| AppError::NotFound("not found".to_string()))?;
    let incident_val = incident_with_history(&state.pool, &incident).await?;

    let updates: Vec<IncidentUpdate> = sqlx::query_as(
        "SELECT id, incident_id, CAST(time AS CHAR) AS time, body, author \
         FROM incident_updates WHERE incident_id = ? ORDER BY time ASC",
    )
    .bind(&id)
    .fetch_all(&state.pool)
    .await?;

    Ok(HttpResponse::Ok().json(json!({
        "incident": incident_val,
        "updates": updates
    })))
}

// ---------------------------------------------------------------------------
// GET /incidentHistory — public (resolved incidents only)
// ---------------------------------------------------------------------------

async fn list_resolved_incidents(state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    let incidents: Vec<Incident> = sqlx::query_as(
        "SELECT id, title, impact, status, status_text, CAST(status_updated_at AS CHAR) AS status_updated_at, \
         CAST(started_at AS CHAR) AS started_at, CAST(ended_at AS CHAR) AS ended_at, created_by, CAST(created_at AS CHAR) AS created_at \
         FROM incidents WHERE status = 'resolved' ORDER BY started_at DESC",
    )
    .fetch_all(&state.pool)
    .await?;

    let mut result: Vec<serde_json::Value> = Vec::with_capacity(incidents.len());
    for inc in &incidents {
        result.push(incident_with_history(&state.pool, inc).await?);
    }

    Ok(HttpResponse::Ok().json(result))
}

// ---------------------------------------------------------------------------
// GET /debug — public (counts + sample data)
// ---------------------------------------------------------------------------

async fn debug_database(state: web::Data<AppState>) -> Result<HttpResponse, AppError> {
    let components: Vec<Component> =
        sqlx::query_as("SELECT id, name, status, CAST(last_updated AS CHAR) AS last_updated FROM components")
            .fetch_all(&state.pool)
            .await?;

    let incidents: Vec<Incident> = sqlx::query_as(
        "SELECT id, title, impact, status, status_text, CAST(status_updated_at AS CHAR) AS status_updated_at, \
         CAST(started_at AS CHAR) AS started_at, CAST(ended_at AS CHAR) AS ended_at, created_by, CAST(created_at AS CHAR) AS created_at FROM incidents",
    )
    .fetch_all(&state.pool)
    .await?;

    let updates: Vec<IncidentUpdate> =
        sqlx::query_as("SELECT id, incident_id, CAST(time AS CHAR) AS time, body, author FROM incident_updates")
            .fetch_all(&state.pool)
            .await?;

    let status_history: Vec<IncidentStatusHistory> = sqlx::query_as(
        "SELECT id, incident_id, status, status_text, CAST(status_updated_at AS CHAR) AS status_updated_at FROM incident_status_history",
    )
    .fetch_all(&state.pool)
    .await?;

    let affected_components: Vec<(String, String)> =
        sqlx::query_as("SELECT incident_id, component_id FROM incident_affected_components")
            .fetch_all(&state.pool)
            .await?;

    let meta: Vec<Meta> = sqlx::query_as("SELECT `key`, `value` FROM meta")
        .fetch_all(&state.pool)
        .await?;

    // Slice for sample data — mirror JS behaviour
    let incidents_sample: Vec<&Incident> = incidents.iter().take(5).collect();
    let recent_updates: Vec<&IncidentUpdate> = updates.iter().rev().take(10).collect();

    Ok(HttpResponse::Ok().json(json!({
        "tables": {
            "components": components.len(),
            "incidents": incidents.len(),
            "incident_updates": updates.len(),
            "incident_status_history": status_history.len(),
            "incident_affected_components": affected_components.len(),
            "meta": meta.len()
        },
        "data": {
            "components": components,
            "incidents": incidents_sample,
            "recent_updates": recent_updates
        }
    })))
}

// ---------------------------------------------------------------------------
// POST /incidents — admin only
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct CreateIncidentBody {
    title: Option<String>,
    impact: Option<String>,
    status: Option<String>,
    status_text: Option<String>,
    update: Option<String>,
    started_at: Option<String>,
    #[serde(rename = "componentUpdates")]
    component_updates: Option<Vec<ComponentUpdateItem>>,
}

#[derive(Debug, Deserialize)]
struct ComponentUpdateItem {
    #[serde(rename = "componentId")]
    component_id: Option<String>,
    status: Option<String>,
    name: Option<String>,
}

async fn create_incident(
    state: web::Data<AppState>,
    admin: AdminUser,
    body: web::Json<CreateIncidentBody>,
) -> Result<HttpResponse, AppError> {
    let title = body
        .title
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("title required".to_string()))?;

    let impact = body.impact.as_deref().unwrap_or("partial_outage");
    let allowed_impacts = ["minimal_outage", "partial_outage", "full_outage"];
    if !allowed_impacts.contains(&impact) {
        return Err(AppError::BadRequest(
            "impact must be one of: minimal_outage, partial_outage, full_outage".to_string(),
        ));
    }

    let status = body.status.as_deref().unwrap_or("investigating");
    let status_text = body
        .status_text
        .as_deref()
        .unwrap_or("No context provided");

    let id = Uuid::new_v4().to_string();
    let now_iso = chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%SZ")
        .to_string();
    let formatted_now = now_formatted();
    let started = body.started_at.as_deref().unwrap_or(&formatted_now);

    sqlx::query(
        "INSERT INTO incidents (id, title, impact, status, status_text, status_updated_at, started_at, created_by, created_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(title)
    .bind(impact)
    .bind(status)
    .bind(status_text)
    .bind(&formatted_now)
    .bind(started)
    .bind(&admin.username)
    .bind(&formatted_now)
    .execute(&state.pool)
    .await?;

    // Insert initial status history
    sqlx::query(
        "INSERT INTO incident_status_history (incident_id, status, status_text, status_updated_at) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(status)
    .bind(status_text)
    .bind(&formatted_now)
    .execute(&state.pool)
    .await?;

    // Optional initial update body
    if let Some(update_body) = body.update.as_deref() {
        if !update_body.is_empty() {
            sqlx::query(
                "INSERT INTO incident_updates (incident_id, time, body, author) VALUES (?, ?, ?, ?)",
            )
            .bind(&id)
            .bind(&now_iso)
            .bind(update_body)
            .bind(&admin.username)
            .execute(&state.pool)
            .await?;
        }
    }

    // Component updates
    if let Some(ref updates) = body.component_updates {
        let allowed_statuses = ["operational", "degraded", "disruption"];
        for cu in updates {
            let comp_id = match cu.component_id.as_deref() {
                Some(c) if !c.is_empty() => c,
                _ => continue,
            };
            let comp_status = match cu.status.as_deref() {
                Some(s) if allowed_statuses.contains(&s) => s,
                _ => continue,
            };

            let existing: Option<(String,)> =
                sqlx::query_as("SELECT id FROM components WHERE id = ?")
                    .bind(comp_id)
                    .fetch_optional(&state.pool)
                    .await?;

            if existing.is_some() {
                sqlx::query("UPDATE components SET status = ?, last_updated = ? WHERE id = ?")
                    .bind(comp_status)
                    .bind(&formatted_now)
                    .bind(comp_id)
                    .execute(&state.pool)
                    .await?;
            } else {
                let comp_name = cu.name.as_deref().unwrap_or(comp_id);
                sqlx::query(
                    "INSERT INTO components (id, name, status, last_updated) VALUES (?, ?, ?, ?)",
                )
                .bind(comp_id)
                .bind(comp_name)
                .bind(comp_status)
                .bind(&formatted_now)
                .execute(&state.pool)
                .await?;
            }

            // Track affected component if not operational
            if comp_status != "operational" {
                sqlx::query(
                    "INSERT IGNORE INTO incident_affected_components (incident_id, component_id) VALUES (?, ?)",
                )
                .bind(&id)
                .bind(comp_id)
                .execute(&state.pool)
                .await?;
            }
        }
    }

    Ok(HttpResponse::Created().json(json!({ "id": id })))
}

// ---------------------------------------------------------------------------
// POST /incidents/{id}/updates — admin only
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct AddUpdateBody {
    body: Option<String>,
    status: Option<String>,
    status_text: Option<String>,
    #[serde(rename = "componentUpdates")]
    component_updates: Option<Vec<ComponentUpdateItem>>,
}

async fn add_update(
    state: web::Data<AppState>,
    admin: AdminUser,
    path: web::Path<String>,
    payload: web::Json<AddUpdateBody>,
) -> Result<HttpResponse, AppError> {
    let id = path.into_inner();

    let update_body = payload
        .body
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("body required".to_string()))?;

    let existing: Option<(String,)> =
        sqlx::query_as("SELECT id FROM incidents WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.pool)
            .await?;

    if existing.is_none() {
        return Err(AppError::NotFound("incident not found".to_string()));
    }

    let now_time = now_formatted();

    // Add update
    sqlx::query(
        "INSERT INTO incident_updates (incident_id, time, body, author) VALUES (?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&now_time)
    .bind(update_body)
    .bind(&admin.username)
    .execute(&state.pool)
    .await?;

    // Update status if provided
    if let Some(ref new_status) = payload.status {
        let new_status_text = payload
            .status_text
            .as_deref()
            .unwrap_or("No context provided");

        // Update incident
        sqlx::query(
            "UPDATE incidents SET status = ?, status_text = ?, status_updated_at = ? WHERE id = ?",
        )
        .bind(new_status.as_str())
        .bind(new_status_text)
        .bind(&now_time)
        .bind(&id)
        .execute(&state.pool)
        .await?;

        // Add to status history
        sqlx::query(
            "INSERT INTO incident_status_history (incident_id, status, status_text, status_updated_at) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(new_status.as_str())
        .bind(new_status_text)
        .bind(&now_time)
        .execute(&state.pool)
        .await?;

        if new_status == "resolved" {
            // Set end time
            sqlx::query("UPDATE incidents SET ended_at = ? WHERE id = ?")
                .bind(&now_time)
                .bind(&id)
                .execute(&state.pool)
                .await?;

            // Reset all affected components back to operational
            let affected: Vec<(String,)> = sqlx::query_as(
                "SELECT component_id FROM incident_affected_components WHERE incident_id = ?",
            )
            .bind(&id)
            .fetch_all(&state.pool)
            .await?;

            for (comp_id,) in &affected {
                sqlx::query(
                    "UPDATE components SET status = ?, last_updated = ? WHERE id = ?",
                )
                .bind("operational")
                .bind(&now_time)
                .bind(comp_id)
                .execute(&state.pool)
                .await?;
            }

            // Clear affected components tracking
            sqlx::query("DELETE FROM incident_affected_components WHERE incident_id = ?")
                .bind(&id)
                .execute(&state.pool)
                .await?;
        }
    }

    // Component updates
    if let Some(ref updates) = payload.component_updates {
        let allowed_statuses = ["operational", "degraded", "disruption"];
        for cu in updates {
            let comp_id = match cu.component_id.as_deref() {
                Some(c) if !c.is_empty() => c,
                _ => continue,
            };
            let comp_status = match cu.status.as_deref() {
                Some(s) if allowed_statuses.contains(&s) => s,
                _ => continue,
            };

            let existing: Option<(String,)> =
                sqlx::query_as("SELECT id FROM components WHERE id = ?")
                    .bind(comp_id)
                    .fetch_optional(&state.pool)
                    .await?;

            if existing.is_some() {
                sqlx::query("UPDATE components SET status = ?, last_updated = ? WHERE id = ?")
                    .bind(comp_status)
                    .bind(&now_time)
                    .bind(comp_id)
                    .execute(&state.pool)
                    .await?;
            } else {
                let comp_name = cu.name.as_deref().unwrap_or(comp_id);
                sqlx::query(
                    "INSERT INTO components (id, name, status, last_updated) VALUES (?, ?, ?, ?)",
                )
                .bind(comp_id)
                .bind(comp_name)
                .bind(comp_status)
                .bind(&now_time)
                .execute(&state.pool)
                .await?;
            }

            // Track affected component if not operational
            if comp_status != "operational" {
                sqlx::query(
                    "INSERT IGNORE INTO incident_affected_components (incident_id, component_id) VALUES (?, ?)",
                )
                .bind(&id)
                .bind(comp_id)
                .execute(&state.pool)
                .await?;
            }
        }
    }

    Ok(HttpResponse::Ok().json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// PATCH /components/{id} — admin only
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct UpdateComponentBody {
    status: Option<String>,
    name: Option<String>,
}

async fn update_component(
    state: web::Data<AppState>,
    _admin: AdminUser,
    path: web::Path<String>,
    body: web::Json<UpdateComponentBody>,
) -> Result<HttpResponse, AppError> {
    let id = path.into_inner();

    let status = body
        .status
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::BadRequest("status required".to_string()))?;

    let allowed_statuses = ["operational", "degraded", "disruption"];
    if !allowed_statuses.contains(&status) {
        return Err(AppError::BadRequest(
            "status must be one of: operational, degraded, disruption".to_string(),
        ));
    }

    let now = now_formatted();

    let existing: Option<(String,)> =
        sqlx::query_as("SELECT id FROM components WHERE id = ?")
            .bind(&id)
            .fetch_optional(&state.pool)
            .await?;

    if existing.is_some() {
        sqlx::query("UPDATE components SET status = ?, last_updated = ? WHERE id = ?")
            .bind(status)
            .bind(&now)
            .bind(&id)
            .execute(&state.pool)
            .await?;
    } else {
        let name = body.name.as_deref().unwrap_or(&id);
        sqlx::query("INSERT INTO components (id, name, status, last_updated) VALUES (?, ?, ?, ?)")
            .bind(&id)
            .bind(name)
            .bind(status)
            .bind(&now)
            .execute(&state.pool)
            .await?;
    }

    Ok(HttpResponse::Ok().json(json!({ "ok": true })))
}

// ---------------------------------------------------------------------------
// Route registration
// ---------------------------------------------------------------------------

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/status")
            .route("/status", web::get().to(get_status))
            .route("/incidents", web::get().to(list_incidents))
            .route("/incidents/{id}", web::get().to(get_incident))
            .route("/incidents", web::post().to(create_incident))
            .route("/incidents/{id}/updates", web::post().to(add_update))
            .route("/incidentHistory", web::get().to(list_resolved_incidents))
            .route("/debug", web::get().to(debug_database))
            .route("/components/{id}", web::patch().to(update_component)),
    );
}
