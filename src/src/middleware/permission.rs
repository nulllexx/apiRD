//! Request extractors for roles-based access control.
//!
//! * [`Principal`] — a signed-in account, with its roles and permissions read
//!   from the database on this request.
//! * [`Require<P>`] — the same, but refuses with 403 unless the account holds
//!   permission `P`. A handler states what it needs in its signature:
//!
//!   ```ignore
//!   async fn run_command(actor: Require<perm::ConsoleCommand>, ...) { ... }
//!   ```
//!
//! Neither trusts anything in the session token beyond *which account* it
//! names. `middleware::auth` does not validate expiry, so any claim about
//! privilege a token carries can be arbitrarily old; access is decided from
//! the `user_roles` table every time.
//!
//! Both check for the cookie and decode the token before any query runs, so an
//! anonymous request is rejected with 401 without touching the database.

use actix_web::{dev::Payload, web, FromRequest, HttpRequest};
use std::future::Future;
use std::marker::PhantomData;
use std::ops::Deref;
use std::pin::Pin;

use crate::access::perm::PermissionMarker;
use crate::access::{store, Permission, PermissionSet};
use crate::error::AppError;
use crate::middleware::auth::decode_jwt;
use crate::AppState;

/// A signed-in account and what it may do.
#[derive(Debug, Clone)]
pub struct Principal {
    pub id: String,
    pub username: String,
    pub roles: Vec<String>,
    pub permissions: PermissionSet,
    pub is_og: bool,
}

impl Principal {
    pub fn has(&self, p: Permission) -> bool {
        self.permissions.has(p)
    }

    pub fn is_superuser(&self) -> bool {
        self.permissions.is_superuser()
    }

    pub fn require(&self, p: Permission) -> Result<(), AppError> {
        if self.has(p) {
            Ok(())
        } else {
            Err(forbidden(p))
        }
    }
}

fn forbidden(p: Permission) -> AppError {
    AppError::Forbidden(format!("Missing permission: {}", p))
}

/// Loads the account a session cookie names.
///
/// Matched on the token's account id. The username is used only when a token
/// carries no id at all: falling back to it whenever the id misses would let a
/// token for a deleted account inherit the access of whoever registers that
/// name next.
pub async fn load_principal(
    pool: &sqlx::MySqlPool,
    id: &str,
    username: &str,
) -> Result<Option<Principal>, sqlx::Error> {
    let row: Option<(String, String, Option<bool>)> = if !id.is_empty() {
        sqlx::query_as("SELECT id, username, is_og FROM users WHERE id = ?")
            .bind(id)
            .fetch_optional(pool)
            .await?
    } else if !username.is_empty() {
        sqlx::query_as("SELECT id, username, is_og FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(pool)
            .await?
    } else {
        None
    };

    let Some((id, username, is_og)) = row else {
        return Ok(None);
    };

    let (roles, permissions) = store::grants_for(pool, &id).await?;
    Ok(Some(Principal { id, username, roles, permissions, is_og: is_og.unwrap_or(false) }))
}

async fn principal_from_request(req: &HttpRequest) -> Result<Principal, AppError> {
    let state = req
        .app_data::<web::Data<AppState>>()
        .ok_or_else(|| AppError::Internal("App state not found".to_string()))?;

    let token = req
        .cookie("userToken")
        .map(|c| c.value().to_string())
        .ok_or_else(|| AppError::Unauthorized("No auth token".to_string()))?;

    let claims = decode_jwt(&token, &state.config.jwt_secret)
        .map_err(|_| AppError::Unauthorized("Invalid or expired token".to_string()))?;

    load_principal(&state.pool, &claims.id, &claims.username)
        .await
        .map_err(|e| {
            log::error!("permission lookup failed: {}", e);
            AppError::Internal("Server error".to_string())
        })?
        .ok_or_else(|| AppError::Unauthorized("Unknown account".to_string()))
}

impl FromRequest for Principal {
    type Error = AppError;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let req = req.clone();
        Box::pin(async move { principal_from_request(&req).await })
    }
}

/// A signed-in account holding permission `P`.
pub struct Require<P: PermissionMarker> {
    principal: Principal,
    _p: PhantomData<P>,
}

impl<P: PermissionMarker> Require<P> {
    pub fn into_inner(self) -> Principal {
        self.principal
    }
}

impl<P: PermissionMarker> Deref for Require<P> {
    type Target = Principal;
    fn deref(&self) -> &Principal {
        &self.principal
    }
}

impl<P: PermissionMarker> std::fmt::Debug for Require<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Require")
            .field("permission", &P::PERMISSION)
            .field("principal", &self.principal)
            .finish()
    }
}

impl<P: PermissionMarker + 'static> FromRequest for Require<P> {
    type Error = AppError;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let req = req.clone();
        Box::pin(async move {
            let principal = principal_from_request(&req).await?;
            if !principal.has(P::PERMISSION) {
                log::info!("{} refused: lacks {}", principal.username, P::PERMISSION);
                return Err(forbidden(P::PERMISSION));
            }
            Ok(Require { principal, _p: PhantomData })
        })
    }
}
