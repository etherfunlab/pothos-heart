//! Pulls the engine's insights export into `pothos.insight_mirror`. Each page
//! is committed with its cursor in one transaction, so a failure mid-walk
//! leaves the cursor at the last page that landed — never ahead of the data.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::db::mirror::{self, MirrorRow};
use crate::engine_client::{EngineClient, EngineError, ExportRow};

#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

/// Trim, lowercase, drop empties and repeats (first occurrence wins). The SQL
/// hard filter compares these arrays with exact `&&` and the scorer lowercases
/// the same way, while engine values are LLM-extracted with varying case.
fn canonical(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(values.len());
    for v in values {
        let v = v.trim().to_lowercase();
        if !v.is_empty() && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

impl From<&ExportRow> for MirrorRow {
    fn from(r: &ExportRow) -> Self {
        Self {
            user_id: r.user_id,
            interests: canonical(&r.interests),
            personality_traits: canonical(&r.personality_traits),
            deal_breakers: canonical(&r.deal_breakers),
            age_min: r.age_min,
            age_max: r.age_max,
            engine_updated_at: r.updated_at,
        }
    }
}

/// How far behind the stored cursor each sweep starts. The engine stamps
/// `updated_at` when a write starts but the row is visible only at commit, so
/// a late-committing row can carry a timestamp the cursor already passed.
/// Re-returned rows are idempotent upserts.
const OVERLAP: chrono::Duration = chrono::Duration::seconds(5);

/// Walk the export from just behind the stored cursor to the end. Returns rows
/// mirrored. Within the walk `next` is passed back verbatim.
pub async fn run_once(pool: &PgPool, client: &EngineClient) -> Result<usize, SweepError> {
    let (mut since, mut after_user) = match mirror::load_cursor(pool).await? {
        Some(c) => (c.updated_at - OVERLAP, Uuid::nil()),
        None => (DateTime::<Utc>::UNIX_EPOCH, Uuid::nil()),
    };
    let mut total = 0;
    loop {
        let page = client.changes(since, after_user).await?;
        if page.profiles.is_empty() {
            break;
        }
        let rows: Vec<MirrorRow> = page.profiles.iter().map(MirrorRow::from).collect();
        mirror::apply_page(pool, &rows).await?;
        total += rows.len();
        match page.next {
            Some(next) => (since, after_user) = (next.updated_at, next.user_id),
            None => break,
        }
    }
    Ok(total)
}

/// Sweep forever at `interval`. Errors are logged; the next tick retries from
/// wherever the cursor stopped.
pub async fn run_loop(pool: PgPool, client: EngineClient, interval: Duration) {
    loop {
        match run_once(&pool, &client).await {
            Ok(n) if n > 0 => tracing::info!(rows = n, "mirror sweep"),
            Ok(_) => tracing::debug!("mirror sweep: nothing new"),
            Err(e) => tracing::warn!(error = %e, "mirror sweep failed; cursor unchanged"),
        }
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::EngineConfig;
    use crate::engine_client::Cursor;

    fn client(server: &MockServer) -> EngineClient {
        EngineClient::new(&EngineConfig {
            base_url: server.uri(),
            export_secret: "s".repeat(32),
        })
    }

    fn row(id: Uuid, ts: &str, interests: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "user_id": id, "interests": interests, "personality_traits": [],
            "deal_breakers": [], "age_min": null, "age_max": null, "updated_at": ts,
            "city": null, "occupation": null
        })
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn walks_two_pages_and_leaves_cursor_on_the_last_row(pool: PgPool) {
        let server = MockServer::start().await;
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let t1 = "2026-10-13T12:00:00Z";
        let t2 = "2026-10-13T12:05:00Z";

        Mock::given(method("GET"))
            .and(path("/svc/v1/insights/changes"))
            .and(query_param(
                "after_user",
                "00000000-0000-0000-0000-000000000000",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [row(a, t1, &["jazz"]), row(b, t1, &["tea"])],
                "next": { "updated_at": t1, "user_id": b }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("after_user", b.to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [row(c, t2, &["chess"])],
                "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        let n = run_once(&pool, &client(&server)).await.unwrap();
        assert_eq!(n, 3);
        assert_eq!(
            mirror::load(&pool, a).await.unwrap().unwrap().interests,
            vec!["jazz"]
        );
        assert_eq!(
            mirror::load(&pool, c).await.unwrap().unwrap().interests,
            vec!["chess"]
        );
        let cur = mirror::load_cursor(&pool).await.unwrap().unwrap();
        assert_eq!(
            cur,
            Cursor {
                updated_at: Utc.with_ymd_and_hms(2026, 10, 13, 12, 5, 0).unwrap(),
                user_id: c
            }
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn next_run_starts_overlap_behind_the_stored_cursor_and_an_empty_page_is_a_noop(
        pool: PgPool,
    ) {
        let server = MockServer::start().await;
        let c = Uuid::new_v4();
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 5, 0).unwrap();
        mirror::apply_page(
            &pool,
            &[MirrorRow {
                user_id: c,
                interests: vec![],
                personality_traits: vec![],
                deal_breakers: vec![],
                age_min: None,
                age_max: None,
                engine_updated_at: t,
            }],
        )
        .await
        .unwrap();

        Mock::given(method("GET"))
            // Resume starts OVERLAP behind the cursor with no after_user: a row
            // that committed late can carry a timestamp already walked past.
            .and(query_param("since", "2026-10-13T12:04:55Z"))
            .and(query_param(
                "after_user",
                "00000000-0000-0000-0000-000000000000",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({ "profiles": [], "next": null })),
            )
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(run_once(&pool, &client(&server)).await.unwrap(), 0);
        assert_eq!(
            mirror::load_cursor(&pool).await.unwrap().unwrap().user_id,
            c
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn engine_failure_leaves_cursor_and_mirror_untouched(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let err = run_once(&pool, &client(&server)).await.unwrap_err();
        assert!(matches!(err, SweepError::Engine(EngineError::Status(403))));
        assert!(mirror::load_cursor(&pool).await.unwrap().is_none());
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.insight_mirror")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn failure_on_page_two_keeps_page_one(pool: PgPool) {
        let server = MockServer::start().await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let t1 = "2026-10-13T12:00:00Z";
        Mock::given(method("GET"))
            .and(query_param(
                "after_user",
                "00000000-0000-0000-0000-000000000000",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [row(a, t1, &["jazz"])],
                "next": { "updated_at": t1, "user_id": a }
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(query_param("after_user", a.to_string().as_str()))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert!(run_once(&pool, &client(&server)).await.is_err());
        assert!(
            mirror::load(&pool, a).await.unwrap().is_some(),
            "page one landed"
        );
        assert!(mirror::load(&pool, b).await.unwrap().is_none());
        assert_eq!(
            mirror::load_cursor(&pool).await.unwrap().unwrap().user_id,
            a,
            "cursor at page one"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn mirror_arrays_are_trimmed_lowercased_and_deduplicated(pool: PgPool) {
        let server = MockServer::start().await;
        let a = Uuid::new_v4();
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [row(a, "2026-10-13T12:00:00Z", &["Jazz", " jazz ", "Hiking", ""])],
                "next": null
            })))
            .mount(&server)
            .await;

        run_once(&pool, &client(&server)).await.unwrap();
        assert_eq!(
            mirror::load(&pool, a).await.unwrap().unwrap().interests,
            vec!["jazz", "hiking"]
        );
    }
}
