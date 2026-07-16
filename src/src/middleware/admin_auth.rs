use actix_web::{dev::Payload, web, FromRequest, HttpRequest};
use std::future::Future;
use std::pin::Pin;

use crate::error::AppError;
use crate::middleware::auth::decode_jwt;
use crate::AppState;

/// Admin auth extractor — verifies JWT and checks is_admin in database
#[derive(Debug, Clone)]
pub struct AdminUser {
    pub username: String,
}

impl FromRequest for AdminUser {
    type Error = AppError;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let req = req.clone();
        Box::pin(async move {
            let state = req
                .app_data::<web::Data<AppState>>()
                .ok_or_else(|| AppError::Internal("App state not found".to_string()))?;

            let token = req
                .cookie("userToken")
                .map(|c| c.value().to_string())
                .ok_or_else(|| AppError::Unauthorized("No auth token".to_string()))?;

            let claims = decode_jwt(&token, &state.config.jwt_secret)
                .map_err(|_| AppError::Unauthorized("Invalid or expired token".to_string()))?;

            if claims.username.is_empty() {
                return Err(AppError::Unauthorized("Invalid token".to_string()));
            }

            // Verify admin status in database
            let is_admin: Option<(String,)> = sqlx::query_as(
                "SELECT id FROM users WHERE username = ? AND is_admin = 1",
            )
            .bind(&claims.username)
            .fetch_optional(&state.pool)
            .await
            .map_err(|e| {
                log::error!("adminAuth DB error: {}", e);
                AppError::Internal("Server error".to_string())
            })?;

            if is_admin.is_none() {
                return Err(AppError::Forbidden("Not an admin user".to_string()));
            }

            Ok(AdminUser {
                username: claims.username,
            })
        })
    }
}
