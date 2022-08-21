use axum::response::IntoResponse;
use http::StatusCode;

pub use self::address::*;
pub use self::authorization::*;
pub use self::docs::*;
pub use self::healthcheck::*;

mod address;
mod authorization;
mod docs;
mod healthcheck;

pub async fn handler_404() -> impl IntoResponse {
    (StatusCode::NOT_FOUND).into_response()
}
