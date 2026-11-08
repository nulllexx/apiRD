use actix_web::{dev::Payload, web, FromRequest, HttpRequest};
use jsonwebtoken::{decode, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use std::future::Future;
use std::pin::Pin;

use crate::error::AppError;
use crate::AppState;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JwtClaims {
    pub username: String,
    pub id: String,
    // Tokens issued before roles also carry an `isAdmin` claim. It is ignored
    // on decode and no longer written: access is read from the database on
    // every request (see middleware::permission), never from the token.
    #[serde(rename = "isOG")]
    #[serde(default)]
    pub is_og: bool,
    pub exp: Option<usize>,
    pub iat: Option<usize>,
}

/// Strict auth extractor — returns 401 if no valid JWT.
///
/// Identifies the caller only. For anything gated on privilege use
/// `middleware::permission::Require`, which reads the caller's roles from the
/// database rather than trusting the token.
#[derive(Debug, Clone)]
pub struct AuthUser {
    pub username: String,
    pub id: String,
    pub is_og: bool,
}

impl FromRequest for AuthUser {
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
                .ok_or_else(|| AppError::Unauthorized("Unauthorized".to_string()))?;

            let mut validation = Validation::default();
            validation.validate_exp = false;
            validation.required_spec_claims.clear();

            let token_data = decode::<JwtClaims>(
                &token,
                &DecodingKey::from_secret(state.config.jwt_secret.as_bytes()),
                &validation,
            )
            .map_err(|_| AppError::Unauthorized("Unauthorized".to_string()))?;

            let claims = token_data.claims;
            log::debug!("Decoded Token: {:?}", claims);

            Ok(AuthUser {
                username: claims.username,
                id: claims.id,
                is_og: claims.is_og,
            })
        })
    }
}

/// Helper to decode JWT from a cookie value
pub fn decode_jwt(token: &str, secret: &str) -> Result<JwtClaims, jsonwebtoken::errors::Error> {
    let mut validation = Validation::default();
    validation.validate_exp = false;
    validation.required_spec_claims.clear();

    let token_data = decode::<JwtClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )?;
    Ok(token_data.claims)
}

/// Helper to create a JWT
pub fn create_jwt(
    username: &str,
    id: &str,
    is_og: bool,
    secret: &str,
    expires_in_secs: u64,
) -> Result<String, jsonwebtoken::errors::Error> {
    use jsonwebtoken::{encode, EncodingKey, Header};

    let now = chrono::Utc::now().timestamp() as usize;
    let claims = JwtClaims {
        username: username.to_string(),
        id: id.to_string(),
        is_og,
        iat: Some(now),
        exp: Some(now + expires_in_secs as usize),
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
}
