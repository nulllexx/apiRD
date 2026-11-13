//! Session tokens: issuing them, and deciding whether one still counts.
//!
//! A session is a signed JWT in the `userToken` cookie. Two things end one:
//!
//! * **Expiry.** Every token carries `exp` (seven days for a sign-in) and is
//!   refused after it. A token without `exp` is refused too.
//! * **Revocation.** Every token carries the account's `session_version`
//!   (`sv`) from when it was issued. Changing or resetting the password, or
//!   "Sign out everywhere", bumps the number in `users`, and every token
//!   carrying the old one stops working at once. See [`revoke_sessions`].
//!
//! [`authenticate`] applies both, and every place that accepts the cookie goes
//! through it: [`AuthUser`], `OptionalAuthUser`, `permission::Principal` and
//! the handful of routes that read the cookie directly. [`decode_jwt`] alone
//! checks the signature and expiry but not revocation, so it is not enough to
//! accept a session.

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
    /// The account's `session_version` when this token was issued. Tokens
    /// from before revocation existed have none, which reads as 0 -- the
    /// column's starting value -- so they keep working until they expire or
    /// the account's sessions are revoked.
    #[serde(rename = "sv")]
    #[serde(default)]
    pub session_version: u32,
    pub exp: Option<usize>,
    pub iat: Option<usize>,
}

/// Strict auth extractor — returns 401 unless the request carries a live
/// session (see [`authenticate`]).
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

impl From<JwtClaims> for AuthUser {
    fn from(claims: JwtClaims) -> Self {
        AuthUser { username: claims.username, id: claims.id, is_og: claims.is_og }
    }
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

            authenticate(state, &token).await.map(AuthUser::from)
        })
    }
}

/// Accepts a session token, or says why not.
///
/// Signature and expiry are checked first, so a forged or stale token is
/// refused without touching the database. Then the account must still exist
/// and still be on the session version the token was issued under.
pub async fn authenticate(state: &AppState, token: &str) -> Result<JwtClaims, AppError> {
    let unauthorized = || AppError::Unauthorized("Your session has expired. Please log in again.".to_string());

    let claims = decode_jwt(token, &state.config.jwt_secret).map_err(|_| unauthorized())?;
    if claims.id.is_empty() {
        return Err(unauthorized());
    }

    let current: Option<(u32,)> = sqlx::query_as("SELECT session_version FROM users WHERE id = ?")
        .bind(&claims.id)
        .fetch_optional(&state.pool)
        .await?;

    match current {
        Some((version,)) if version == claims.session_version => Ok(claims),
        _ => Err(unauthorized()),
    }
}

/// Like [`authenticate`], for callers that treat "no session" and "the
/// database is unreachable" alike: both come back as `None`.
pub async fn authenticate_opt(state: &AppState, token: &str) -> Option<JwtClaims> {
    match authenticate(state, token).await {
        Ok(claims) => Some(claims),
        Err(AppError::Unauthorized(_)) => None,
        Err(e) => {
            log::error!("Session check failed: {e}");
            None
        }
    }
}

/// Ends every session the account has, everywhere, and returns the new
/// version for the caller to issue a fresh token under if it wants this
/// device to stay signed in.
pub async fn revoke_sessions(pool: &sqlx::MySqlPool, user_id: &str) -> Result<u32, sqlx::Error> {
    sqlx::query("UPDATE users SET session_version = session_version + 1 WHERE id = ?")
        .bind(user_id)
        .execute(pool)
        .await?;
    current_session_version(pool, user_id).await
}

pub async fn current_session_version(pool: &sqlx::MySqlPool, user_id: &str) -> Result<u32, sqlx::Error> {
    let (version,): (u32,) = sqlx::query_as("SELECT session_version FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_one(pool)
        .await?;
    Ok(version)
}

/// Checks a token's signature and expiry. Does **not** check revocation; use
/// [`authenticate`] to accept a session.
pub fn decode_jwt(token: &str, secret: &str) -> Result<JwtClaims, jsonwebtoken::errors::Error> {
    let mut validation = Validation::default();
    // `exp` must be present as well as in the future. Before this, expiry was
    // not checked at all, so a token copied off a machine worked forever.
    validation.validate_exp = true;
    validation.set_required_spec_claims(&["exp"]);

    let token_data = decode::<JwtClaims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &validation,
    )?;
    Ok(token_data.claims)
}

/// Issues a session token for the account at `session_version`.
pub fn create_jwt(
    username: &str,
    id: &str,
    is_og: bool,
    session_version: u32,
    secret: &str,
    expires_in_secs: u64,
) -> Result<String, jsonwebtoken::errors::Error> {
    use jsonwebtoken::{encode, EncodingKey, Header};

    let now = chrono::Utc::now().timestamp() as usize;
    let claims = JwtClaims {
        username: username.to_string(),
        id: id.to_string(),
        is_og,
        session_version,
        iat: Some(now),
        exp: Some(now + expires_in_secs as usize),
    };

    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

    const SECRET: &str = "secret";

    fn sign(claims: serde_json::Value) -> String {
        encode(&Header::default(), &claims, &EncodingKey::from_secret(SECRET.as_bytes())).unwrap()
    }

    #[test]
    fn a_fresh_token_decodes_with_its_session_version() {
        let token = create_jwt("steve", "id-1", false, 3, SECRET, 3600).unwrap();
        let claims = decode_jwt(&token, SECRET).unwrap();
        assert_eq!(claims.id, "id-1");
        assert_eq!(claims.session_version, 3);
    }

    /// The bug this guards: expiry used to be ignored entirely.
    #[test]
    fn an_expired_token_is_refused() {
        let past = chrono::Utc::now().timestamp() - 3600;
        let token = sign(serde_json::json!({ "username": "steve", "id": "id-1", "exp": past }));
        assert!(decode_jwt(&token, SECRET).is_err());
    }

    #[test]
    fn a_token_without_an_expiry_is_refused() {
        let token = sign(serde_json::json!({ "username": "steve", "id": "id-1" }));
        assert!(decode_jwt(&token, SECRET).is_err());
    }

    #[test]
    fn tokens_from_before_revocation_read_as_version_zero() {
        let future = chrono::Utc::now().timestamp() + 3600;
        let token = sign(serde_json::json!({ "username": "steve", "id": "id-1", "exp": future }));
        assert_eq!(decode_jwt(&token, SECRET).unwrap().session_version, 0);
    }

    #[test]
    fn a_token_signed_with_another_secret_is_refused() {
        let token = create_jwt("steve", "id-1", false, 0, "other", 3600).unwrap();
        assert!(decode_jwt(&token, SECRET).is_err());
    }
}
