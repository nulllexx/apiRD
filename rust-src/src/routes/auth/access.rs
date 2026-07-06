use actix_web::{web, HttpRequest, HttpResponse};
use serde_json::json;

use crate::error::AppError;
use crate::middleware::auth_failopen::OptionalAuthUser;
use crate::AppState;
use crate::middleware::rate_limit::RateLimiter;

use super::common::*;

/// GET /proj/allowed
pub(super) async fn proj_allowed(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    opt_auth: OptionalAuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let user = match opt_auth.0 {
        Some(u) => u,
        None => {
            return Ok(
                HttpResponse::Unauthorized().json(json!({ "allowed": false }))
            );
        }
    };

    if user.username.is_empty() {
        return Ok(
            HttpResponse::Unauthorized().json(json!({ "allowed": false }))
        );
    }

    let row: Option<(bool,)> =
        sqlx::query_as("SELECT is_projallowed FROM users WHERE username = ?")
            .bind(&user.username)
            .fetch_optional(&state.pool)
            .await?;

    match row {
        Some((allowed,)) => {
            if allowed {
                Ok(HttpResponse::Ok().json(json!({ "allowed": true })))
            } else {
                Ok(HttpResponse::Forbidden().json(json!({ "allowed": false })))
            }
        }
        None => Ok(
            HttpResponse::Unauthorized().json(json!({ "allowed": false }))
        ),
    }
}

/// GET /plex/allowed
pub(super) async fn plex_allowed(
    req: HttpRequest,
    state: web::Data<AppState>,
    limiter: web::Data<RateLimiter>,
    opt_auth: OptionalAuthUser,
) -> Result<HttpResponse, AppError> {
    check_rate_limit(&req, &limiter)?;

    let user = match opt_auth.0 {
        Some(u) => u,
        None => {
            return Ok(
                HttpResponse::Unauthorized().json(json!({ "allowed": false }))
            );
        }
    };

    if user.username.is_empty() {
        return Ok(
            HttpResponse::Unauthorized().json(json!({ "allowed": false }))
        );
    }

    let row: Option<(bool,)> =
        sqlx::query_as("SELECT is_plexallowed FROM users WHERE username = ?")
            .bind(&user.username)
            .fetch_optional(&state.pool)
            .await?;

    match row {
        Some((allowed,)) => {
            if allowed {
                Ok(HttpResponse::Ok().json(json!({ "allowed": true })))
            } else {
                Ok(HttpResponse::Forbidden().json(json!({ "allowed": false })))
            }
        }
        None => Ok(
            HttpResponse::Unauthorized().json(json!({ "allowed": false }))
        ),
    }
}

