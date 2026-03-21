use actix_web::{HttpResponse, ResponseError};
use std::fmt;

#[derive(Debug)]
pub enum AppError {
    BadRequest(String),
    Unauthorized(String),
    Forbidden(String),
    NotFound(String),
    Conflict(String),
    PayloadTooLarge(String),
    TooManyRequests(String),
    Internal(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::BadRequest(msg) => write!(f, "{}", msg),
            AppError::Unauthorized(msg) => write!(f, "{}", msg),
            AppError::Forbidden(msg) => write!(f, "{}", msg),
            AppError::NotFound(msg) => write!(f, "{}", msg),
            AppError::Conflict(msg) => write!(f, "{}", msg),
            AppError::PayloadTooLarge(msg) => write!(f, "{}", msg),
            AppError::TooManyRequests(msg) => write!(f, "{}", msg),
            AppError::Internal(msg) => write!(f, "{}", msg),
        }
    }
}

impl ResponseError for AppError {
    fn error_response(&self) -> HttpResponse {
        let (status, message) = match self {
            AppError::BadRequest(msg) => (actix_web::http::StatusCode::BAD_REQUEST, msg),
            AppError::Unauthorized(msg) => (actix_web::http::StatusCode::UNAUTHORIZED, msg),
            AppError::Forbidden(msg) => (actix_web::http::StatusCode::FORBIDDEN, msg),
            AppError::NotFound(msg) => (actix_web::http::StatusCode::NOT_FOUND, msg),
            AppError::Conflict(msg) => (actix_web::http::StatusCode::CONFLICT, msg),
            AppError::PayloadTooLarge(msg) => {
                (actix_web::http::StatusCode::PAYLOAD_TOO_LARGE, msg)
            }
            AppError::TooManyRequests(msg) => {
                (actix_web::http::StatusCode::TOO_MANY_REQUESTS, msg)
            }
            AppError::Internal(msg) => {
                (actix_web::http::StatusCode::INTERNAL_SERVER_ERROR, msg)
            }
        };
        HttpResponse::build(status).json(serde_json::json!({ "error": message }))
    }
}

impl From<sqlx::Error> for AppError {
    fn from(err: sqlx::Error) -> Self {
        log::error!("Database error: {}", err);
        AppError::Internal("Internal server error".to_string())
    }
}

impl From<jsonwebtoken::errors::Error> for AppError {
    fn from(_: jsonwebtoken::errors::Error) -> Self {
        AppError::Unauthorized("Invalid or expired token".to_string())
    }
}

impl From<bcrypt::BcryptError> for AppError {
    fn from(err: bcrypt::BcryptError) -> Self {
        log::error!("Bcrypt error: {}", err);
        AppError::Internal("Internal server error".to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        log::error!("IO error: {}", err);
        AppError::Internal("Internal server error".to_string())
    }
}
