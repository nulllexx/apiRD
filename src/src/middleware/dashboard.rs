use actix_web::{web, HttpRequest, HttpResponse};

use crate::middleware::auth::decode_jwt;
use crate::AppState;

/// Check admin auth for status dashboard — redirects to status login on failure
pub async fn check_status_dashboard_auth(
    req: &HttpRequest,
    state: &web::Data<AppState>,
) -> Result<(), HttpResponse> {
    check_dashboard_auth(req, state, "https://bakosmp.go.ro/status/login.html", true).await
}

/// Check admin auth for RD admin dashboard — redirects to raindrippy login on failure
pub async fn check_rd_admin_auth(
    req: &HttpRequest,
    state: &web::Data<AppState>,
) -> Result<(), HttpResponse> {
    check_dashboard_auth(req, state, "https://bakosmp.go.ro/raindrippy/login.html", true).await
}

/// Check that *some* account is signed in — no admin flag required.
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
    require_admin: bool,
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

    let claims = match decode_jwt(&token, &state.config.jwt_secret) {
        Ok(c) => c,
        Err(e) => {
            log::error!("Dashboard auth error: {}", e);
            return Err(redirect());
        }
    };

    // Look up user in DB
    let user: Option<(String, bool)> = sqlx::query_as(
        "SELECT id, is_admin FROM users WHERE id = ?",
    )
    .bind(&claims.id)
    .fetch_optional(&state.pool)
    .await
    .unwrap_or(None);

    match user {
        Some((_, true)) => Ok(()),
        Some((_, false)) if !require_admin => Ok(()),
        Some((_, false)) => {
            log::info!("User not an admin");
            Err(redirect())
        }
        None => {
            log::info!("User not found");
            Err(redirect())
        }
    }
}
