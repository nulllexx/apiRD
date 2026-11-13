use actix_web::{web, HttpRequest, HttpResponse};

use crate::access::Permission;
use crate::middleware::auth::authenticate_opt;
use crate::middleware::permission::load_principal;
use crate::AppState;

/// Check panel access for the status dashboard — redirects to status login on failure
pub async fn check_status_dashboard_auth(
    req: &HttpRequest,
    state: &web::Data<AppState>,
) -> Result<(), HttpResponse> {
    check_dashboard_auth(req, state, "https://bakosmp.go.ro/status/login.html", true).await
}

/// Check panel access for the RD admin dashboard — redirects to raindrippy login on failure
pub async fn check_rd_admin_auth(
    req: &HttpRequest,
    state: &web::Data<AppState>,
) -> Result<(), HttpResponse> {
    check_dashboard_auth(req, state, "https://bakosmp.go.ro/raindrippy/login.html", true).await
}

/// Check that *some* account is signed in — no permission required.
///
/// For pages every player uses rather than the two staff dashboards, so it
/// sends people to the site's ordinary sign-in rather than a staff login.
pub async fn check_signed_in(
    req: &HttpRequest,
    state: &web::Data<AppState>,
) -> Result<(), HttpResponse> {
    check_dashboard_auth(req, state, "https://bakosmp.go.ro/auth.html", false).await
}

async fn check_dashboard_auth(
    req: &HttpRequest,
    state: &web::Data<AppState>,
    redirect_url: &str,
    require_panel: bool,
) -> Result<(), HttpResponse> {
    let redirect = || HttpResponse::Found()
        .append_header(("Location", redirect_url))
        .finish();

    let token = match req.cookie("userToken") {
        Some(c) => c.value().to_string(),
        None => {
            log::info!("No cookie");
            return Err(redirect());
        }
    };

    let claims = match authenticate_opt(state, &token).await {
        Some(c) => c,
        None => {
            log::info!("Dashboard: no live session");
            return Err(redirect());
        }
    };

    let principal = match load_principal(&state.pool, &claims.id, &claims.username).await {
        Ok(Some(p)) => p,
        Ok(None) => {
            log::info!("User not found");
            return Err(redirect());
        }
        Err(e) => {
            log::error!("Dashboard permission lookup failed: {}", e);
            return Err(redirect());
        }
    };

    if require_panel && !principal.has(Permission::PanelAccess) {
        log::info!("{} lacks {}", principal.username, Permission::PanelAccess);
        return Err(redirect());
    }
    Ok(())
}
