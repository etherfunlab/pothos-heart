//! `GET /healthz` — liveness. Public: the only route outside the caller-secret layer.

use axum::Json;
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

#[derive(Serialize, utoipa::ToSchema)]
pub struct HealthResponse {
    pub status: String,
    pub service: String,
    pub version: String,
    pub timestamp: String,
}

#[utoipa::path(
    get,
    path = "/healthz",
    tag = "health",
    responses((status = 200, description = "Service is up", body = HealthResponse))
)]
async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
        service: "pothos-heart".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        timestamp: chrono::Utc::now().to_rfc3339(),
    })
}

pub fn router<S>() -> OpenApiRouter<S>
where
    S: Clone + Send + Sync + 'static,
{
    OpenApiRouter::new().routes(routes!(healthz))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_is_public_and_names_the_service() {
        let (router, _api) = super::router::<()>().split_for_parts();
        let app = router.with_state(());
        let res = app
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["service"], "pothos-heart");
    }
}
