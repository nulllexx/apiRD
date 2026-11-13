use actix_web::{dev::Payload, web, FromRequest, HttpRequest};
use std::future::Future;
use std::pin::Pin;

use crate::middleware::auth::{authenticate_opt, AuthUser};
use crate::AppState;

/// Fail-open auth extractor — always succeeds, user may be None.
///
/// `None` for no cookie, and for one that is expired, revoked or forged (see
/// `auth::authenticate`).
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
                    authenticate_opt(state, &token).await.map(AuthUser::from)
                }
                _ => None,
            };

            Ok(OptionalAuthUser(user))
        })
    }
}
