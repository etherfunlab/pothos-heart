//! Service-to-service admission. Every `/v1/*` request must carry the
//! deployment's caller secret; there is no user identity here by design.

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::error::AppError;
use crate::state::AppState;

pub const CALLER_SECRET_HEADER: &str = "x-pothos-caller-secret";

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn require_caller_secret(
    State(state): State<AppState>,
    req: Request,
    next: Next,
) -> Response {
    let sent = req
        .headers()
        .get(CALLER_SECRET_HEADER)
        .map(|v| v.as_bytes())
        .unwrap_or_default();
    if constant_time_eq(sent, state.config.caller_secret.as_bytes()) {
        next.run(req).await
    } else {
        AppError::Forbidden.into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use axum::middleware::from_fn_with_state;
    use axum::routing::get;
    use axum::Router;
    use tower::ServiceExt;

    use super::*;
    use crate::state::{test_state, TEST_SECRET};

    fn probe(state: AppState) -> Router {
        Router::new()
            .route("/probe", get(|| async { "ok" }))
            .layer(from_fn_with_state(state.clone(), require_caller_secret))
            .with_state(state)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn missing_or_wrong_secret_is_forbidden_with_generic_body(pool: sqlx::PgPool) {
        let res = probe(test_state(pool.clone()))
            .oneshot(Request::get("/probe").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["error"], "forbidden");

        let res = probe(test_state(pool.clone()))
            .oneshot(
                Request::get("/probe")
                    .header(CALLER_SECRET_HEADER, "wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);

        // Same length as the real secret, last byte differs: gets past the
        // length check, so only the byte comparison can reject it.
        let mut same_len = TEST_SECRET.to_string();
        same_len.pop();
        same_len.push('0');
        assert_eq!(same_len.len(), TEST_SECRET.len());
        assert_ne!(same_len, TEST_SECRET);
        let res = probe(test_state(pool))
            .oneshot(
                Request::get("/probe")
                    .header(CALLER_SECRET_HEADER, same_len)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(res.into_body(), 4096).await.unwrap()).unwrap();
        assert_eq!(body["error"], "forbidden");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn right_secret_passes_through(pool: sqlx::PgPool) {
        let res = probe(test_state(pool))
            .oneshot(
                Request::get("/probe")
                    .header(CALLER_SECRET_HEADER, TEST_SECRET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
    }
}
