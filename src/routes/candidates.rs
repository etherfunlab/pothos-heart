//! The match: hard filter in SQL, soft score in Rust, nothing persisted.

use axum::extract::rejection::{PathRejection, QueryRejection};
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::db::{candidates, mirror, profiles};
use crate::error::{AppError, ErrorBody};
use crate::scoring::{self, Geo, Scored, Side};
use crate::state::AppState;

const DEFAULT_LIMIT: i64 = 20;
const MAX_LIMIT: i64 = 100;

fn effective_limit(requested: Option<i64>) -> usize {
    requested.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as usize
}

#[derive(Deserialize, IntoParams)]
pub struct CandidatesQuery {
    /// Default 20, clamped to 1..=100.
    pub limit: Option<i64>,
}

/// One ranked candidate: the user and why they scored as they did.
#[derive(Serialize, ToSchema)]
pub struct Candidate {
    pub user_id: Uuid,
    #[serde(flatten)]
    pub scored: Scored,
}

#[derive(Serialize, ToSchema)]
pub struct CandidatesResponse {
    pub user_id: Uuid,
    /// `no_insights_yet` when the user is in the pool but the engine has not
    /// produced a profile for them yet — a normal state, not an error.
    pub reason: Option<&'static str>,
    pub candidates: Vec<Candidate>,
}

/// Ranked candidates for one user, with the shared items as the reason.
/// Computed on request from the two mirrors; nothing is stored. A user with no
/// profile row, or an inactive one, is not in the pool (404).
#[utoipa::path(
    get,
    path = "/v1/match/candidates/{user_id}",
    tag = "match",
    params(("user_id" = Uuid, Path), CandidatesQuery),
    responses(
        (status = 200, body = CandidatesResponse),
        (status = 400, description = "bad_request", body = ErrorBody),
        (status = 403, description = "forbidden", body = ErrorBody),
        (status = 404, description = "not_in_pool", body = ErrorBody)
    ),
    security(("caller_secret" = []))
)]
async fn get_candidates(
    State(state): State<AppState>,
    user_id: Result<Path<Uuid>, PathRejection>,
    q: Result<Query<CandidatesQuery>, QueryRejection>,
) -> Result<Json<CandidatesResponse>, AppError> {
    let Path(user_id) = user_id?;
    let Query(q) = q?;
    let limit = effective_limit(q.limit);

    let me_profile = profiles::load(&state.pool, user_id)
        .await?
        .filter(|p| p.active)
        .ok_or(AppError::NotInPool)?;
    let Some(me_mirror) = mirror::load(&state.pool, user_id).await? else {
        return Ok(Json(CandidatesResponse {
            user_id,
            reason: Some("no_insights_yet"),
            candidates: Vec::new(),
        }));
    };

    let me = Side {
        interests: &me_mirror.interests,
        traits: &me_mirror.personality_traits,
        geo: Geo {
            country: me_profile.country.as_deref(),
            region: me_profile.region.as_deref(),
            city: me_profile.city.as_deref(),
        },
    };

    let rows = candidates::hard_filtered(&state.pool, user_id).await?;
    let mut ranked: Vec<Candidate> = rows
        .iter()
        .map(|c| {
            let other = Side {
                interests: &c.interests,
                traits: &c.personality_traits,
                geo: Geo {
                    country: c.country.as_deref(),
                    region: c.region.as_deref(),
                    city: c.city.as_deref(),
                },
            };
            Candidate {
                user_id: c.user_id,
                scored: scoring::score(&state.weights, &me, &other),
            }
        })
        .collect();
    // Highest score first; ties broken by user_id so the order is stable.
    ranked.sort_by(|a, b| {
        b.scored
            .score
            .total_cmp(&a.scored.score)
            .then_with(|| a.user_id.cmp(&b.user_id))
    });
    ranked.truncate(limit);

    Ok(Json(CandidatesResponse {
        user_id,
        reason: None,
        candidates: ranked,
    }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(get_candidates))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use chrono::Utc;
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::auth::CALLER_SECRET_HEADER;
    use crate::db::mirror::{self, MirrorRow};
    use crate::db::profiles::{self, ProfileWrite};
    use crate::routes::build_app;
    use crate::state::{test_state, TEST_SECRET};

    async fn get(pool: PgPool, uri: &str) -> (StatusCode, serde_json::Value) {
        let res = build_app(test_state(pool))
            .oneshot(
                Request::get(uri)
                    .header(CALLER_SECRET_HEADER, TEST_SECRET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    async fn profile_as(
        pool: &PgPool,
        id: Uuid,
        gender: &str,
        seeking: &[&str],
        city: Option<&str>,
        active: bool,
    ) {
        profiles::upsert(
            pool,
            id,
            &ProfileWrite {
                gender: gender.into(),
                seeking_genders: seeking.iter().map(|s| s.to_string()).collect(),
                birth_date: None,
                country: Some("SG".into()),
                region: None,
                city: city.map(String::from),
                active,
            },
        )
        .await
        .unwrap();
    }

    async fn profile_with(
        pool: &PgPool,
        gender: &str,
        seeking: &[&str],
        city: Option<&str>,
        active: bool,
    ) -> Uuid {
        let id = Uuid::new_v4();
        profile_as(pool, id, gender, seeking, city, active).await;
        id
    }

    async fn profile(pool: &PgPool, gender: &str, seeking: &[&str], city: Option<&str>) -> Uuid {
        profile_with(pool, gender, seeking, city, true).await
    }

    async fn mirror(pool: &PgPool, id: Uuid, interests: &[&str], traits: &[&str]) {
        mirror::upsert_one(
            pool,
            &MirrorRow {
                user_id: id,
                interests: interests.iter().map(|s| s.to_string()).collect(),
                personality_traits: traits.iter().map(|s| s.to_string()).collect(),
                deal_breakers: vec![],
                age_min: None,
                age_max: None,
                engine_updated_at: Utc::now(),
            },
        )
        .await
        .unwrap();
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn unknown_user_is_404_not_in_pool(pool: PgPool) {
        let (status, body) = get(pool, &format!("/v1/match/candidates/{}", Uuid::new_v4())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_in_pool");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn bad_path_or_query_is_400_bad_request_json(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], None).await;
        for uri in [
            "/v1/match/candidates/not-a-uuid".to_string(),
            format!("/v1/match/candidates/{me}?limit=abc"),
        ] {
            let (status, body) = get(pool.clone(), &uri).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{uri}");
            assert_eq!(body["error"], "bad_request", "{uri}");
            assert!(!body["message"].as_str().unwrap().is_empty(), "{uri}");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn inactive_profile_is_404_not_in_pool(pool: PgPool) {
        // The row exists and a compatible candidate is waiting, so only the
        // `active = false` check can produce the 404.
        let me = profile_with(&pool, "f", &["m"], None, false).await;
        mirror(&pool, me, &["jazz"], &["calm"]).await;
        let him = profile(&pool, "m", &["f"], None).await;
        mirror(&pool, him, &["jazz"], &["calm"]).await;

        let (status, body) = get(pool, &format!("/v1/match/candidates/{me}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_in_pool");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn in_pool_without_insights_is_200_empty_with_reason(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], None).await;
        let _him = profile(&pool, "m", &["f"], None).await;
        let (status, body) = get(pool, &format!("/v1/match/candidates/{me}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["reason"], "no_insights_yet");
        assert_eq!(body["candidates"].as_array().unwrap().len(), 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn ranks_by_score_and_explains(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], Some("Singapore")).await;
        mirror(&pool, me, &["jazz", "hiking", "tea"], &["calm"]).await;
        let best = profile(&pool, "m", &["f"], Some("Singapore")).await;
        mirror(&pool, best, &["Jazz", "hiking"], &["calm"]).await;
        let meh = profile(&pool, "m", &["f"], None).await;
        mirror(&pool, meh, &["chess"], &["loud"]).await;
        // Two candidates with identical inputs, hence identical scores, between
        // `best` and `meh`; only the id tie-break orders them.
        let mut tied = Vec::new();
        for _ in 0..2 {
            let id = profile(&pool, "m", &["f"], None).await;
            mirror(&pool, id, &["tea"], &["calm"]).await;
            tied.push(id);
        }
        tied.sort();
        let _wrong_gender = profile(&pool, "f", &["m"], Some("Singapore")).await;

        let (status, body) = get(pool, &format!("/v1/match/candidates/{me}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["reason"].is_null());
        let c = body["candidates"].as_array().unwrap();
        assert_eq!(c.len(), 4);
        assert_eq!(c[0]["user_id"], best.to_string());
        assert_eq!(c[1]["user_id"], tied[0].to_string(), "tie: lower id first");
        assert_eq!(c[2]["user_id"], tied[1].to_string(), "tie: lower id first");
        assert_eq!(c[3]["user_id"], meh.to_string());
        assert_eq!(c[1]["score"], c[2]["score"]);
        assert!(c[0]["score"].as_f64().unwrap() > c[1]["score"].as_f64().unwrap());
        assert!(c[2]["score"].as_f64().unwrap() > c[3]["score"].as_f64().unwrap());
        assert_eq!(
            c[0]["shared_interests"],
            serde_json::json!(["hiking", "jazz"])
        );
        assert_eq!(c[0]["shared_traits"], serde_json::json!(["calm"]));
        assert_eq!(c[0]["geo_match"], "city");
        assert_eq!(c[0]["scores"]["interests"], 1.0);
        assert_eq!(c[3]["geo_match"], "country");
        assert_eq!(c[3]["scores"]["interests"], 0.0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn limit_is_clamped_to_1_100(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], None).await;
        mirror(&pool, me, &["jazz"], &[]).await;
        for _ in 0..3 {
            let him = profile(&pool, "m", &["f"], None).await;
            mirror(&pool, him, &["jazz"], &[]).await;
        }
        for bad in ["0", "-1"] {
            let (status, body) = get(
                pool.clone(),
                &format!("/v1/match/candidates/{me}?limit={bad}"),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "limit={bad}");
            assert_eq!(
                body["candidates"].as_array().unwrap().len(),
                1,
                "limit={bad}"
            );
        }
        let (status, body) = get(
            pool.clone(),
            &format!("/v1/match/candidates/{me}?limit=500"),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["candidates"].as_array().unwrap().len(), 3);
        let (_, body) = get(pool, &format!("/v1/match/candidates/{me}?limit=2")).await;
        assert_eq!(body["candidates"].as_array().unwrap().len(), 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn limit_one_returns_the_best_candidate_not_the_lowest_id(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], Some("Singapore")).await;
        mirror(&pool, me, &["jazz", "hiking"], &["calm"]).await;
        // SQL returns candidates by id ascending, so the best one must sit
        // on the higher id for the test to see ranking rather than SQL order.
        let mut ids = [Uuid::new_v4(), Uuid::new_v4()];
        ids.sort();
        let [low, high] = ids;
        profile_as(&pool, high, "m", &["f"], Some("Singapore"), true).await;
        mirror(&pool, high, &["jazz", "hiking"], &["calm"]).await;
        profile_as(&pool, low, "m", &["f"], None, true).await;
        mirror(&pool, low, &["chess"], &["loud"]).await;

        let (status, body) = get(pool, &format!("/v1/match/candidates/{me}?limit=1")).await;
        assert_eq!(status, StatusCode::OK);
        let c = body["candidates"].as_array().unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0]["user_id"], high.to_string());
    }

    #[test]
    fn effective_limit_defaults_to_20_and_clamps_to_1_100() {
        use super::effective_limit;
        assert_eq!(effective_limit(None), 20);
        assert_eq!(effective_limit(Some(0)), 1);
        assert_eq!(effective_limit(Some(-1)), 1);
        assert_eq!(effective_limit(Some(100)), 100);
        assert_eq!(effective_limit(Some(101)), 100);
        assert_eq!(effective_limit(Some(7)), 7);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn missing_caller_secret_is_403_forbidden(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], None).await;
        let res = build_app(test_state(pool))
            .oneshot(
                Request::get(format!("/v1/match/candidates/{me}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        let bytes = to_bytes(res.into_body(), 1 << 20).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "forbidden");
    }
}
