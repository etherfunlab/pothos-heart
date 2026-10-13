//! Route composition. Two sub-trees: public `/healthz`, and `/v1/*` behind the
//! caller-secret layer. Merged, not nested — handler paths carry their prefix.

use axum::middleware::from_fn_with_state;
use axum::Router;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::require_caller_secret;
use crate::state::AppState;

pub mod health;

pub fn router(state: AppState) -> OpenApiRouter<AppState> {
    let v1 = OpenApiRouter::new().layer(from_fn_with_state(state, require_caller_secret));
    OpenApiRouter::new().merge(health::router()).merge(v1)
}

/// Same shape without the auth layer or a live state — for `print-openapi`.
// wired by print-openapi in Task 11
#[allow(dead_code)]
pub fn router_for_openapi() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().merge(health::router())
}

pub fn build_app(state: AppState) -> Router {
    let (router, _api) = router(state.clone()).split_for_parts();
    router.with_state(state)
}
