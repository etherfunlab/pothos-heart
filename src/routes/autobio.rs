//! The owner's view of their autobio. Only the owner may see it; making
//! sure the caller is the owner is the product layer's job.

use axum::extract::rejection::PathRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Utc};
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::db::{autobio, profiles};
use crate::error::{AppError, ErrorBody};
use crate::state::AppState;

#[derive(Serialize, ToSchema)]
pub struct AutobioEntry {
    /// Pass to `DELETE /v1/autobio/{user_id}/entries/{entry_id}`.
    pub id: i64,
    pub statement: String,
    /// `current`, `past` or `planned`.
    pub status: String,
    /// When the latest material behind this entry was first said; `null`
    /// when none is left.
    pub as_of: Option<DateTime<Utc>>,
}

#[derive(Serialize, ToSchema)]
pub struct AutobioSection {
    /// `basics`, `life`, `personality`, `love`, `intimacy`, `outlook` or `roleplay`.
    pub section: String,
    pub entries: Vec<AutobioEntry>,
}

#[derive(Serialize, ToSchema)]
pub struct AutobioResponse {
    pub user_id: Uuid,
    /// `no_autobio_yet` while there is no current entry.
    pub reason: Option<&'static str>,
    /// The autobio's version: the last time any entry changed.
    pub updated_at: Option<DateTime<Utc>>,
    /// Sections in fixed order; empty ones are left out.
    pub sections: Vec<AutobioSection>,
}

/// The owner's current autobio. Any `match_profiles` row may read it,
/// `active: false` included, so a user can preview it before joining.
#[utoipa::path(
    get,
    path = "/v1/autobio/{user_id}",
    tag = "autobio",
    params(("user_id" = Uuid, Path)),
    responses(
        (status = 200, body = AutobioResponse),
        (status = 400, description = "bad_request", body = ErrorBody),
        (status = 403, description = "forbidden", body = ErrorBody),
        (status = 404, description = "not_in_pool", body = ErrorBody)
    ),
    security(("caller_secret" = []))
)]
async fn get_autobio(
    State(state): State<AppState>,
    user_id: Result<Path<Uuid>, PathRejection>,
) -> Result<Json<AutobioResponse>, AppError> {
    let Path(user_id) = user_id?;
    profiles::load(&state.pool, user_id)
        .await?
        .ok_or(AppError::NotInPool)?;
    let entries = autobio::current_entries(&state.pool, user_id).await?;
    let updated_at = autobio::versions(&state.pool, &[user_id])
        .await?
        .remove(&user_id);
    let sections: Vec<AutobioSection> = autobio::SECTIONS
        .iter()
        .filter_map(|s| {
            let entries: Vec<AutobioEntry> = entries
                .iter()
                .filter(|e| e.section == *s)
                .map(|e| AutobioEntry {
                    id: e.id,
                    statement: e.statement.clone(),
                    status: e.status.clone(),
                    as_of: e.as_of,
                })
                .collect();
            (!entries.is_empty()).then(|| AutobioSection {
                section: s.to_string(),
                entries,
            })
        })
        .collect();
    Ok(Json(AutobioResponse {
        user_id,
        reason: sections.is_empty().then_some("no_autobio_yet"),
        updated_at,
        sections,
    }))
}

/// The owner deletes an entry. It is gone at once and is never written back.
#[utoipa::path(
    delete,
    path = "/v1/autobio/{user_id}/entries/{entry_id}",
    tag = "autobio",
    params(("user_id" = Uuid, Path), ("entry_id" = i64, Path)),
    responses(
        (status = 204, description = "deleted"),
        (status = 400, description = "bad_request", body = ErrorBody),
        (status = 403, description = "forbidden", body = ErrorBody),
        (status = 404, description = "entry_not_found", body = ErrorBody)
    ),
    security(("caller_secret" = []))
)]
async fn delete_entry(
    State(state): State<AppState>,
    ids: Result<Path<(Uuid, i64)>, PathRejection>,
) -> Result<StatusCode, AppError> {
    let Path((user_id, entry_id)) = ids?;
    if autobio::veto(&state.pool, user_id, entry_id).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::EntryNotFound)
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_autobio))
        .routes(routes!(delete_entry))
}

#[cfg(test)]
mod tests {
    use axum::body::{to_bytes, Body};
    use axum::http::{Request, StatusCode};
    use sqlx::PgPool;
    use tower::ServiceExt;
    use uuid::Uuid;

    use crate::auth::CALLER_SECRET_HEADER;
    use crate::db::autobio::tests::{entry, join_pool};
    use crate::routes::build_app;
    use crate::state::{test_state, TEST_SECRET};

    async fn call(pool: PgPool, method: &str, uri: &str) -> (StatusCode, serde_json::Value) {
        let res = build_app(test_state(pool))
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
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

    #[sqlx::test(migrations = "./migrations")]
    async fn get_returns_current_entries_by_section_order(pool: PgPool) {
        let a = Uuid::new_v4();
        join_pool(&pool, a, false).await; // active:false may preview
        entry(&pool, a, "love", "Slow to warm up").await;
        entry(&pool, a, "basics", "Lives in Singapore").await;
        entry(&pool, a, "love", "Was single for two years").await;
        let gone = entry(&pool, a, "life", "Deleted").await;
        sqlx::query(
            "UPDATE pothos.autobio_entries SET retired_at = now(), retired_reason = 'vetoed' WHERE id = $1",
        )
        .bind(gone)
        .execute(&pool)
        .await
        .unwrap();

        let (status, body) = call(pool, "GET", &format!("/v1/autobio/{a}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["reason"].is_null());
        assert!(body["updated_at"].is_string());
        let sections = body["sections"].as_array().unwrap();
        assert_eq!(
            sections
                .iter()
                .map(|s| s["section"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["basics", "love"],
            "fixed order, empty sections omitted"
        );
        let love = sections[1]["entries"].as_array().unwrap();
        assert_eq!(love.len(), 2);
        assert_eq!(love[0]["statement"], "Slow to warm up");
        assert_eq!(love[0]["status"], "current");
        assert!(love[0]["as_of"].is_null(), "no sources, no time anchor");
        assert!(body.to_string().find("Deleted").is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn get_without_entries_is_no_autobio_yet_and_without_a_profile_404(pool: PgPool) {
        let a = Uuid::new_v4();
        join_pool(&pool, a, true).await;
        let (status, body) = call(pool.clone(), "GET", &format!("/v1/autobio/{a}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["reason"], "no_autobio_yet");
        assert_eq!(body["sections"], serde_json::json!([]));

        let (status, body) = call(pool, "GET", &format!("/v1/autobio/{}", Uuid::new_v4())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "not_in_pool");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn delete_vetoes_the_entry_and_moves_the_version(pool: PgPool) {
        let a = Uuid::new_v4();
        join_pool(&pool, a, true).await;
        let keep = entry(&pool, a, "life", "Keep").await;
        let doomed = entry(&pool, a, "life", "Doomed").await;
        let before = crate::db::autobio::versions(&pool, &[a]).await.unwrap()[&a];

        let (status, _) = call(
            pool.clone(),
            "DELETE",
            &format!("/v1/autobio/{a}/entries/{doomed}"),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let reason: Option<String> =
            sqlx::query_scalar("SELECT retired_reason FROM pothos.autobio_entries WHERE id = $1")
                .bind(doomed)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(reason.as_deref(), Some("vetoed"));
        let after = crate::db::autobio::versions(&pool, &[a]).await.unwrap()[&a];
        assert!(after > before);
        let left = crate::db::autobio::current_entries(&pool, a).await.unwrap();
        assert_eq!(left.iter().map(|e| e.id).collect::<Vec<_>>(), vec![keep]);

        let (status, body) =
            call(pool, "DELETE", &format!("/v1/autobio/{a}/entries/{doomed}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "already retired");
        assert_eq!(body["error"], "entry_not_found");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn delete_rejects_bad_ids_and_other_users_entries(pool: PgPool) {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        join_pool(&pool, a, true).await;
        let theirs = entry(&pool, b, "life", "Not a's").await;

        let (status, body) = call(
            pool.clone(),
            "DELETE",
            &format!("/v1/autobio/{a}/entries/abc"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "bad_request");
        for id in ["-1".to_string(), theirs.to_string()] {
            let (status, body) = call(
                pool.clone(),
                "DELETE",
                &format!("/v1/autobio/{a}/entries/{id}"),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{id}");
            assert_eq!(body["error"], "entry_not_found", "{id}");
        }
        assert_eq!(
            crate::db::autobio::current_entries(&pool, b)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
