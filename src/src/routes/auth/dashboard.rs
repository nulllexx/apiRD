use actix_web::{web, HttpRequest, HttpResponse};

use crate::middleware::dashboard::{check_rd_admin_auth, check_status_dashboard_auth};
use crate::AppState;

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

    let path = format!("{}/rdadmin.html", state.config.private_dir);
    match std::fs::read_to_string(&path) {
        Ok(content) => HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(content),
        Err(e) => {
            log::error!("Error reading rdadmin.html: {}", e);
            HttpResponse::InternalServerError().body("Error loading admin panel")
        }
    }
}

