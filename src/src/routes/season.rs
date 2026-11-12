//! `/api/admin/season`: ending a season, and the archive records.
//!
//! Ending a season needs `season.end`; editing records needs
//! `season.archives`. Both write to the public website, so changes are only
//! accepted from the panel's own pages and every one is attributed in the
//! console audit log.

use actix_web::{web, HttpRequest, HttpResponse};
use serde::Deserialize;

use crate::access::perm;
use crate::console::audit::{self, Kind, Outcome};
use crate::error::AppError;
use crate::middleware::permission::Require;
use crate::routes::console::{audit_or_refuse, require_panel_request};
use crate::season::archive::{ArchiveInput, ArchiveStore};
use crate::season::end;
use crate::AppState;

fn store(state: &AppState) -> ArchiveStore {
    ArchiveStore::new(&state.config.archives_dir, &state.config.seasons_path)
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Result<T, AppError> {
    web::block(f).await.map_err(|e| {
        log::error!("season: blocking task failed: {e}");
        AppError::Internal("The request could not be completed".to_string())
    })
}

/// GET /api/admin/season/current -- what ending the season now would do.
async fn current(
    state: web::Data<AppState>,
    _admin: Require<perm::SeasonEnd>,
) -> Result<HttpResponse, AppError> {
    let cfg = state.config.clone();
    let plan = blocking(move || end::plan(&cfg)).await?.map_err(AppError::Conflict)?;
    Ok(HttpResponse::Ok().json(plan))
}

/// POST /api/admin/season/end -- start ending the season. Answers at once;
/// progress is at `/end/status`.
async fn end_season(
    req: HttpRequest,
    state: web::Data<AppState>,
    admin: Require<perm::SeasonEnd>,
    body: web::Json<end::EndRequest>,
) -> Result<HttpResponse, AppError> {
    require_panel_request(&req)?;
    let request = body.into_inner();

    // Everything that can be checked before anything long starts.
    let next = request.validate().map_err(AppError::BadRequest)?;

    let job = end::begin(request.season, &admin.username)
        .ok_or_else(|| AppError::Conflict("A season is already being ended".to_string()))?;
    let record = match audit_or_refuse(
        &state,
        &admin,
        Kind::Season,
        &format!("end season {} and start season {}", request.season, next.season),
        None,
    )
    .await
    {
        Ok(record) => record,
        Err(e) => {
            // Nothing ran; free the slot so it can be tried again.
            end::abandon(&job, e.to_string());
            return Err(e);
        }
    };
    log::info!("season: {} is ending season {}", admin.username, request.season);

    let progress = job.lock().map(|p| p.clone()).map_err(|_| AppError::Internal("Job state lost".into()))?;
    let task_state = state.clone();
    let by = admin.username.clone();
    actix_web::rt::spawn(async move {
        end::run(task_state.clone(), job.clone(), request, by).await;
        let (outcome, detail) = match job.lock() {
            Ok(p) if p.state == "done" => (Outcome::Ok, p.zip_url.clone()),
            Ok(p) => (Outcome::Failed, p.error.clone()),
            Err(_) => (Outcome::Failed, None),
        };
        audit::finish(&task_state.pool, &task_state.console.audit, record, outcome, detail.as_deref()).await;
    });

    Ok(HttpResponse::Accepted().json(progress))
}

/// GET /api/admin/season/end/status -- the running job, or the last one.
async fn end_status(_admin: Require<perm::SeasonEnd>) -> Result<HttpResponse, AppError> {
    Ok(HttpResponse::Ok().json(serde_json::json!({ "job": end::current() })))
}

/// GET /api/admin/season/archives
async fn list_archives(
    state: web::Data<AppState>,
    _admin: Require<perm::SeasonArchives>,
) -> Result<HttpResponse, AppError> {
    let store = store(&state);
    let listed = blocking(move || store.list()).await??;
    Ok(HttpResponse::Ok().json(serde_json::json!({ "archives": listed })))
}

/// GET /api/admin/season/archives/{season}
async fn get_archive(
    state: web::Data<AppState>,
    _admin: Require<perm::SeasonArchives>,
    path: web::Path<u32>,
) -> Result<HttpResponse, AppError> {
    let season = path.into_inner();
    let store = store(&state);
    let found = blocking(move || store.read(season)).await??;
    let (archive, version) =
        found.ok_or_else(|| AppError::NotFound("No archive record for that season".to_string()))?;
    Ok(HttpResponse::Ok().json(serde_json::json!({ "archive": archive, "version": version })))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SaveBody {
    archive: ArchiveInput,
    /// The version the editor loaded; absent when creating.
    version: Option<String>,
}

#[derive(Deserialize)]
struct SaveQuery {
    #[serde(default)]
    create: bool,
}

/// PUT /api/admin/season/archives/{season}?create= -- create or edit a record.
async fn save_archive(
    req: HttpRequest,
    state: web::Data<AppState>,
    admin: Require<perm::SeasonArchives>,
    path: web::Path<u32>,
    query: web::Query<SaveQuery>,
    body: web::Json<SaveBody>,
) -> Result<HttpResponse, AppError> {
    require_panel_request(&req)?;
    let season = path.into_inner();
    let SaveBody { archive, version } = body.into_inner();
    if archive.season != season {
        return Err(AppError::BadRequest(
            "The season in the record doesn't match the one being saved".to_string(),
        ));
    }
    let create = query.create;
    let verb = if create { "create" } else { "edit" };
    let target = format!("archives/Season_{season}.json");
    let record = audit_or_refuse(
        &state,
        &admin,
        Kind::Season,
        &format!("{verb} archive Season_{season}.json"),
        Some(&target),
    )
    .await?;

    let store = store(&state);
    let by = admin.username.clone();
    let result = blocking(move || store.save(&archive, version.as_deref(), create, &by))
        .await
        .and_then(|r| r.map_err(AppError::from));
    let (outcome, detail) = match &result {
        Ok(_) => (Outcome::Ok, None),
        Err(e) => (Outcome::Failed, Some(e.to_string())),
    };
    audit::finish(&state.pool, &state.console.audit, record, outcome, detail.as_deref()).await;

    let saved = result?;
    let store = self::store(&state);
    let version = blocking(move || store.read(season)).await??.map(|(_, v)| v);
    Ok(HttpResponse::Ok().json(serde_json::json!({ "archive": saved, "version": version })))
}

pub fn configure(cfg: &mut web::ServiceConfig) {
    cfg.service(
        web::scope("/admin/season")
            .route("/current", web::get().to(current))
            .route("/end", web::post().to(end_season))
            .route("/end/status", web::get().to(end_status))
            .route("/archives", web::get().to(list_archives))
            // One resource for both methods: a second one on the same path
            // would never be reached, the first answering 405 instead.
            .service(
                web::resource("/archives/{season}")
                    .app_data(web::JsonConfig::default().limit(1024 * 1024))
                    .route(web::get().to(get_archive))
                    .route(web::put().to(save_archive)),
            ),
    );
}
