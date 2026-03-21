use actix_web::{dev::Payload, web, FromRequest, HttpRequest};
use std::future::Future;
use std::pin::Pin;

use crate::middleware::auth::{decode_jwt, AuthUser};
use crate::AppState;

/// Fail-open auth extractor — always succeeds, user may be None
pub struct OptionalAuthUser(pub Option<AuthUser>);

impl FromRequest for OptionalAuthUser {
    type Error = actix_web::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self, Self::Error>>>>;

    fn from_request(req: &HttpRequest, _payload: &mut Payload) -> Self::Future {
        let req = req.clone();
        Box::pin(async move {
            let state = req.app_data::<web::Data<AppState>>();
            let token = req.cookie("userToken").map(|c| c.value().to_string());

            let user = match (state, token) {
                (Some(state), Some(token)) => {
                    match decode_jwt(&token, &state.config.jwt_secret) {
                        Ok(claims) => Some(AuthUser {
                            username: claims.username,
                            id: claims.id,
                            is_admin: claims.is_admin,
                        }),
                        Err(_) => None,
                    }
                }
                _ => None,
            };

            Ok(OptionalAuthUser(user))
        })
    }
}
