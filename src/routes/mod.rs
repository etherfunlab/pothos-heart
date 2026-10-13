//! Route composition. Two sub-trees: public `/healthz`, and `/v1/*` behind the
//! caller-secret layer. Merged, not nested — handler paths carry their prefix.

use axum::middleware::from_fn_with_state;
use axum::Router;
use utoipa_axum::router::OpenApiRouter;

use crate::auth::require_caller_secret;
use crate::state::AppState;

pub mod candidates;
pub mod health;
pub mod profiles;

/// Every `/v1` route is merged here, before the caller-secret layer is applied
/// in `router()` — a route added after `.layer` would bypass auth.
fn v1_routes() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(profiles::router())
        .merge(candidates::router())
}

pub fn router(state: AppState) -> OpenApiRouter<AppState> {
    let v1 = v1_routes().layer(from_fn_with_state(state, require_caller_secret));
    OpenApiRouter::new().merge(health::router()).merge(v1)
}

/// Same shape without the auth layer or a live state — for `print-openapi`.
pub fn router_for_openapi() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .merge(health::router())
        .merge(v1_routes())
}

pub fn build_app(state: AppState) -> Router {
    let (router, _api) = router(state.clone()).split_for_parts();
    router.with_state(state)
}

#[cfg(test)]
mod tests {
    #[test]
    fn openapi_lists_the_three_v1_routes_and_healthz() {
        use utoipa::OpenApi;
        let (_r, api) = super::router_for_openapi().split_for_parts();
        let mut doc = crate::openapi::ApiDoc::openapi();
        doc.merge(api);
        let v = serde_json::to_value(doc).unwrap();
        for p in [
            "/healthz",
            "/v1/profiles/{user_id}",
            "/v1/match/candidates/{user_id}",
        ] {
            assert!(v["paths"][p].is_object(), "{p}");
        }
        assert!(v["components"]["securitySchemes"]["caller_secret"].is_object());
    }
}
