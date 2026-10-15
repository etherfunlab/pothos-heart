//! Hard attributes, pushed by the product layer. The row's existence with
//! `active = true` is the pool membership.

use axum::extract::rejection::{JsonRejection, PathRejection};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::NaiveDate;
use serde::Deserialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::db::profiles::{self, ProfileWrite};
use crate::error::{AppError, ErrorBody};
use crate::state::AppState;

fn default_true() -> bool {
    true
}

/// Hard attributes for one user. `seeking_genders` is already expanded by the
/// product layer ("who I am looking for"); this service never maps an
/// orientation label to genders. Unknown keys are rejected: a mistyped optional
/// key must not silently store NULL.
#[derive(Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ProfileBody {
    /// A free-form label, matched case-insensitively: trimmed and stored lowercase.
    pub gender: String,
    /// Labels in the same vocabulary as `gender`; each is trimmed and stored lowercase.
    pub seeking_genders: Vec<String>,
    /// Age is derived from this at query time; it is never stored.
    pub birth_date: Option<NaiveDate>,
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    /// `false` keeps the row but takes the user out of the pool.
    #[serde(default = "default_true")]
    pub active: bool,
}

fn blank(s: &str) -> bool {
    s.trim().is_empty()
}

/// The hard filter compares gender labels exactly in SQL, so they are
/// normalised once, here.
fn label(s: &str) -> String {
    s.trim().to_lowercase()
}

fn validate(b: ProfileBody) -> Result<ProfileWrite, AppError> {
    if blank(&b.gender) {
        return Err(AppError::BadRequest("gender must not be blank".into()));
    }
    if b.seeking_genders.is_empty() || b.seeking_genders.iter().any(|g| blank(g)) {
        return Err(AppError::BadRequest(
            "seeking_genders must be a non-empty list of non-blank values".into(),
        ));
    }
    if let Some(d) = b.birth_date {
        if d > chrono::Utc::now().date_naive() {
            return Err(AppError::BadRequest(
                "birth_date must not be in the future".into(),
            ));
        }
    }
    let trim = |s: Option<String>| s.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    Ok(ProfileWrite {
        gender: label(&b.gender),
        seeking_genders: b.seeking_genders.iter().map(|g| label(g)).collect(),
        birth_date: b.birth_date,
        country: trim(b.country),
        region: trim(b.region),
        city: trim(b.city),
        active: b.active,
    })
}

/// Push (or replace) a user's hard attributes. Creating the row admits the
/// user to the match pool.
#[utoipa::path(
    put,
    path = "/v1/profiles/{user_id}",
    tag = "profiles",
    params(("user_id" = Uuid, Path)),
    request_body = ProfileBody,
    responses(
        (status = 204, description = "stored"),
        (status = 400, description = "bad_request", body = ErrorBody),
        (status = 403, description = "forbidden", body = ErrorBody)
    ),
    security(("caller_secret" = []))
)]
async fn put_profile(
    State(state): State<AppState>,
    user_id: Result<Path<Uuid>, PathRejection>,
    body: Result<Json<ProfileBody>, JsonRejection>,
) -> Result<StatusCode, AppError> {
    let Path(user_id) = user_id?;
    let Json(body) = body?;
    let w = validate(body)?;
    profiles::upsert(&state.pool, user_id, &w).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Remove a user from the match pool. Their autobio and every verdict naming them are deleted with it; the mirrored engine data stays.
#[utoipa::path(
    delete,
    path = "/v1/profiles/{user_id}",
    tag = "profiles",
    params(("user_id" = Uuid, Path)),
    responses(
        (status = 204, description = "removed"),
        (status = 400, description = "bad_request", body = ErrorBody),
        (status = 403, description = "forbidden", body = ErrorBody),
        (status = 404, description = "not_in_pool", body = ErrorBody)
    ),
    security(("caller_secret" = []))
)]
async fn delete_profile(
    State(state): State<AppState>,
    user_id: Result<Path<Uuid>, PathRejection>,
) -> Result<StatusCode, AppError> {
    let Path(user_id) = user_id?;
    if profiles::delete(&state.pool, user_id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotInPool)
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(put_profile, delete_profile))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{header, Request, StatusCode};
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::auth::CALLER_SECRET_HEADER;
    use crate::routes::build_app;
    use crate::state::{test_state, TEST_SECRET};

    async fn send_raw(
        pool: sqlx::PgPool,
        method: &str,
        uri: &str,
        content_type: Option<&str>,
        body: String,
    ) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header(CALLER_SECRET_HEADER, TEST_SECRET);
        if let Some(ct) = content_type {
            req = req.header(header::CONTENT_TYPE, ct);
        }
        let res = build_app(test_state(pool))
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = to_bytes(res.into_body(), 1 << 16).await.unwrap();
        let json = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("every response body is JSON")
        };
        (status, json)
    }

    async fn send(
        pool: sqlx::PgPool,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let body = body.map(|b| b.to_string()).unwrap_or_default();
        send_raw(pool, method, uri, Some("application/json"), body).await
    }

    fn valid() -> serde_json::Value {
        serde_json::json!({
            "gender": "f",
            "seeking_genders": ["m"],
            "birth_date": "1996-04-02",
            "country": "SG",
            "city": "Singapore"
        })
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn put_upserts_and_defaults_active_true(pool: sqlx::PgPool) {
        let id = Uuid::new_v4();
        let (status, _) = send(
            pool.clone(),
            "PUT",
            &format!("/v1/profiles/{id}"),
            Some(valid()),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let first = crate::db::profiles::load(&pool, id).await.unwrap().unwrap();
        assert_eq!(first.gender, "f");
        assert_eq!(first.seeking_genders, vec!["m"]);
        assert_eq!(
            first.birth_date,
            Some(chrono::NaiveDate::from_ymd_opt(1996, 4, 2).unwrap())
        );
        assert_eq!(first.country.as_deref(), Some("SG"));
        assert!(first.region.is_none());
        assert_eq!(first.city.as_deref(), Some("Singapore"));
        assert!(first.active);

        // The second PUT changes gender, seeking_genders and country, and drops
        // region (never set), birth_date and city. Padding, case and a blank
        // string are normalised on the way in.
        let second_body = serde_json::json!({
            "gender": "M",
            "seeking_genders": [" F "],
            "birth_date": null,
            "country": "JP",
            "city": "  ",
            "active": false
        });
        let (status, _) = send(
            pool.clone(),
            "PUT",
            &format!("/v1/profiles/{id}"),
            Some(second_body),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let second = crate::db::profiles::load(&pool, id).await.unwrap().unwrap();
        assert_eq!(second.gender, "m", "stored lowercase");
        assert_eq!(second.seeking_genders, vec!["f"], "trimmed and lowercase");
        assert!(second.birth_date.is_none(), "PUT replaces the whole row");
        assert_eq!(second.country.as_deref(), Some("JP"));
        assert!(second.region.is_none());
        assert!(second.city.is_none(), "blank string is stored as NULL");
        assert!(!second.active);
        assert!(second.updated_at > first.updated_at);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn put_accepts_a_birth_date_of_today(pool: sqlx::PgPool) {
        let id = Uuid::new_v4();
        let mut b = valid();
        b["birth_date"] = serde_json::json!(chrono::Utc::now().date_naive().to_string());
        let (status, _) = send(pool.clone(), "PUT", &format!("/v1/profiles/{id}"), Some(b)).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn put_rejects_empty_or_blank_seeking_genders_and_blank_gender(pool: sqlx::PgPool) {
        let id = Uuid::new_v4();
        for (field, value) in [
            ("seeking_genders", serde_json::json!([])),
            ("seeking_genders", serde_json::json!(["m", " "])),
            ("gender", serde_json::json!("  ")),
        ] {
            let mut b = valid();
            b[field] = value;
            let (status, body) =
                send(pool.clone(), "PUT", &format!("/v1/profiles/{id}"), Some(b)).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{field}");
            assert_eq!(body["error"], "bad_request");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn put_rejects_a_birth_date_in_the_future(pool: sqlx::PgPool) {
        let id = Uuid::new_v4();
        let mut b = valid();
        let tomorrow = chrono::Utc::now().date_naive() + chrono::Days::new(1);
        b["birth_date"] = serde_json::json!(tomorrow.to_string());
        let (status, body) =
            send(pool.clone(), "PUT", &format!("/v1/profiles/{id}"), Some(b)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["message"].as_str().unwrap().contains("birth_date"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn put_rejects_an_unknown_key_instead_of_storing_null(pool: sqlx::PgPool) {
        let id = Uuid::new_v4();
        let mut b = valid();
        b.as_object_mut().unwrap().remove("birth_date");
        b["birthdate"] = serde_json::json!("1996-04-02");
        let (status, body) =
            send(pool.clone(), "PUT", &format!("/v1/profiles/{id}"), Some(b)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "bad_request");
        assert!(body["message"].as_str().unwrap().contains("birthdate"));
        assert!(crate::db::profiles::load(&pool, id)
            .await
            .unwrap()
            .is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn extractor_rejections_use_the_json_error_envelope(pool: sqlx::PgPool) {
        let id = Uuid::new_v4();
        let json = Some("application/json");
        let cases: [(&str, &str, Option<&str>, String); 6] = [
            ("PUT", "/v1/profiles/not-a-uuid", json, valid().to_string()),
            ("DELETE", "/v1/profiles/not-a-uuid", None, String::new()),
            (
                "PUT",
                &format!("/v1/profiles/{id}"),
                json,
                "{not json".into(),
            ),
            // Valid JSON of the wrong shape is 422 in axum; here it is 400.
            (
                "PUT",
                &format!("/v1/profiles/{id}"),
                json,
                r#"{"gender":"f"}"#.into(),
            ),
            (
                "PUT",
                &format!("/v1/profiles/{id}"),
                None,
                valid().to_string(),
            ),
            (
                "PUT",
                &format!("/v1/profiles/{id}"),
                Some("text/plain"),
                valid().to_string(),
            ),
        ];
        for (method, uri, content_type, body) in cases {
            let (status, json) = send_raw(pool.clone(), method, uri, content_type, body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{method} {uri}");
            assert_eq!(json["error"], "bad_request", "{method} {uri}");
            assert!(
                !json["message"].as_str().unwrap().is_empty(),
                "{method} {uri}"
            );
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn delete_removes_or_404s(pool: sqlx::PgPool) {
        let id = Uuid::new_v4();
        let (status, body) =
            send(pool.clone(), "DELETE", &format!("/v1/profiles/{id}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_in_pool");

        let other = Uuid::new_v4();
        for who in [id, other] {
            let (status, _) = send(
                pool.clone(),
                "PUT",
                &format!("/v1/profiles/{who}"),
                Some(valid()),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }
        let (status, _) = send(pool.clone(), "DELETE", &format!("/v1/profiles/{id}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(crate::db::profiles::load(&pool, id)
            .await
            .unwrap()
            .is_none());
        assert!(
            crate::db::profiles::load(&pool, other)
                .await
                .unwrap()
                .is_some(),
            "DELETE removes only the addressed user"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn leaving_the_pool_removes_autobio_data_but_not_the_mirrors(pool: sqlx::PgPool) {
        use crate::db::autobio::tests::{at, entry, export};
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for id in [a, b] {
            send(
                pool.clone(),
                "PUT",
                &format!("/v1/profiles/{id}"),
                Some(valid()),
            )
            .await;
            entry(&pool, id, "life", "x").await;
        }
        export(&pool, a, at(0), &[("city", "Singapore")]).await;
        sqlx::query(
            "INSERT INTO pothos.autobio_state (user_id, material_until, ran_at, model) VALUES ($1, now(), now(), 'm')",
        )
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();
        let (lo, hi) = crate::db::verdicts::pair(a, b);
        let (lo2, hi2) = crate::db::verdicts::pair(b, c);
        for (x, y) in [(lo, hi), (lo2, hi2)] {
            sqlx::query(
                "INSERT INTO pothos.pair_verdicts (user_lo, user_hi, lo_version, hi_version, grade, rationale, model) \
                 VALUES ($1, $2, now(), now(), 'promising', 'r', 'm')",
            )
            .bind(x)
            .bind(y)
            .execute(&pool)
            .await
            .unwrap();
            sqlx::query("INSERT INTO pothos.judge_queue (user_lo, user_hi) VALUES ($1, $2)")
                .bind(x)
                .bind(y)
                .execute(&pool)
                .await
                .unwrap();
        }

        let (status, _) = send(pool.clone(), "DELETE", &format!("/v1/profiles/{a}"), None).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let count = |sql: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sql)
                    .bind(a)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(
            count("SELECT count(*) FROM pothos.autobio_entries WHERE user_id = $1").await,
            0
        );
        assert_eq!(
            count("SELECT count(*) FROM pothos.autobio_state WHERE user_id = $1").await,
            0
        );
        assert_eq!(
            count("SELECT count(*) FROM pothos.pair_verdicts WHERE user_lo = $1 OR user_hi = $1")
                .await,
            0
        );
        assert_eq!(
            count("SELECT count(*) FROM pothos.judge_queue WHERE user_lo = $1 OR user_hi = $1")
                .await,
            0
        );
        assert_eq!(
            count("SELECT count(*) FROM pothos.observed_phrases WHERE user_id = $1").await,
            1,
            "mirror kept"
        );
        let others: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.pair_verdicts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(others, 1, "the b–c verdict stays");
        assert_eq!(
            crate::db::autobio::current_entries(&pool, b)
                .await
                .unwrap()
                .len(),
            1
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_repeated_delete_clears_leftovers_and_still_404s(pool: sqlx::PgPool) {
        use crate::db::autobio::tests::entry;
        let a = Uuid::new_v4();
        // Left already: no profile row, but an autobio entry survived.
        entry(&pool, a, "life", "x").await;

        let (status, body) = send(pool.clone(), "DELETE", &format!("/v1/profiles/{a}"), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_in_pool");
        let left: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pothos.autobio_entries WHERE user_id = $1")
                .bind(a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(left, 0, "the leftover entry is removed");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn profiles_require_the_caller_secret(pool: sqlx::PgPool) {
        let id = Uuid::new_v4();
        let req = Request::put(format!("/v1/profiles/{id}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(valid().to_string()))
            .unwrap();
        let res = build_app(test_state(pool)).oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
    }
}
