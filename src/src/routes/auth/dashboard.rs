use actix_web::{web, HttpRequest, HttpResponse};

use crate::middleware::dashboard::{
    check_rd_admin_auth, check_signed_in, check_status_dashboard_auth,
};
use crate::AppState;

/// Serve a page out of the private directory.
///
/// Read from disk per request rather than baked into the binary, so a change
/// to the markup is live on reload without a rebuild.
fn serve_private_page(state: &web::Data<AppState>, file: &str, what: &str) -> HttpResponse {
    let path = format!("{}/{}", state.config.private_dir, file);
    match std::fs::read_to_string(&path) {
        Ok(content) => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(content),
        Err(e) => {
            log::error!("Error reading {}: {}", file, e);
            HttpResponse::InternalServerError().body(format!("Error loading {what}"))
        }
    }
}

pub async fn serve_dashboard(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> HttpResponse {
    if let Err(resp) = check_status_dashboard_auth(&req, &state).await {
        return resp;
    }

    HttpResponse::Found()
        .append_header(("Location", "/rdadmin.html?page=status"))
        .finish()
}

pub async fn serve_rdadmin(
    req: HttpRequest,
    state: web::Data<AppState>,
) -> HttpResponse {
    if let Err(resp) = check_rd_admin_auth(&req, &state).await {
        return resp;
    }

    serve_private_page(&state, "rdadmin.html", "admin panel")
}

/// The player-facing polls page.
///
/// Signed in is the whole requirement: which polls somebody may actually
/// answer is decided per poll by the API this page calls, not here.
pub async fn serve_polls(req: HttpRequest, state: web::Data<AppState>) -> HttpResponse {
    if let Err(resp) = check_signed_in(&req, &state).await {
        return resp;
    }

    serve_private_page(&state, "polls.html", "polls page")
}

