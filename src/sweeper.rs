//! Pulls the engine's insights export into the mirror and the phrase ledger. Each page
//! is committed with its cursor in one transaction, so a failure mid-walk
//! leaves the cursor at the last page that landed — never ahead of the data.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::concepts::ConceptTask;
use crate::db::mirror::{self, MirrorRow};
use crate::engine_client::{EngineClient, EngineError, ExportRow};

#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error(transparent)]
    Engine(#[from] EngineError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

impl From<&ExportRow> for MirrorRow {
    fn from(r: &ExportRow) -> Self {
        // An inverted window (min > max) can match nobody, so it is dropped
        // rather than mirrored: "no usable age preference" excludes no one.
        let (age_min, age_max) = match (r.age_min, r.age_max) {
            (Some(min), Some(max)) if min > max => (None, None),
            window => window,
        };
        // Arrays are canonicalised when written (`mirror::upsert_row`).
        Self {
            user_id: r.user_id,
            interests: r.interests.clone(),
            personality_traits: r.personality_traits.clone(),
            deal_breakers: r.deal_breakers.clone(),
            age_min,
            age_max,
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
        let Some(next) = page.next else { break };
        // A conforming engine's `next` is always strictly past the request
        // position; anything else would re-fetch the same page forever.
        if (next.updated_at, next.user_id) <= (since, after_user) {
            tracing::warn!(?next, "engine export cursor did not advance; ending sweep");
            break;
        }
        (since, after_user) = (next.updated_at, next.user_id);
    }
    Ok(total)
}

/// Sweep forever at `interval`: mirror the export, then — when a model is
/// configured — decide undecided phrases, whatever the mirror step did.
/// Errors are logged; the next tick retries from wherever each step stopped.
pub async fn run_loop(
    pool: PgPool,
    client: EngineClient,
    concepts: Option<ConceptTask>,
    interval: Duration,
) {
    loop {
        match run_once(&pool, &client).await {
            Ok(n) if n > 0 => tracing::info!(rows = n, "mirror sweep"),
            Ok(_) => tracing::debug!("mirror sweep: nothing new"),
            // Debug, not Display: it carries the cause chain, so the operator
            // can tell a refused connection from DNS or TLS.
            Err(e) => {
                tracing::warn!(error = ?e, "mirror sweep failed; cursor kept at last landed page")
            }
        }
        if let Some(task) = &concepts {
            match crate::concepts::run_once(&pool, task).await {
                Ok(r) if r.decided + r.skipped > 0 => tracing::info!(
                    decided = r.decided,
                    skipped = r.skipped,
                    new_concepts = r.new_concepts,
                    vocabulary = r.vocabulary,
                    tokens = r.tokens,
                    "concept step"
                ),
                Ok(_) => tracing::debug!("concept step: nothing undecided"),
                Err(e) => tracing::warn!(error = ?e, "concept step failed; phrases stay undecided"),
            }
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
            .and(query_param("since", "1970-01-01T00:00:00Z"))
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
            .and(query_param("since", t1))
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
            mirror::current(&pool, a, "interests").await.unwrap(),
            vec!["jazz"]
        );
        assert_eq!(
            mirror::current(&pool, c, "interests").await.unwrap(),
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
            .and(query_param("since", "1970-01-01T00:00:00Z"))
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
            .and(query_param("since", t1))
            .and(query_param("after_user", a.to_string().as_str()))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        assert!(run_once(&pool, &client(&server)).await.is_err());
        assert!(mirror::exists(&pool, a).await.unwrap(), "page one landed");
        assert!(!mirror::exists(&pool, b).await.unwrap());
        assert_eq!(
            mirror::load_cursor(&pool).await.unwrap().unwrap().user_id,
            a,
            "cursor at page one"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn mirror_phrases_are_trimmed_lowercased_and_deduplicated(pool: PgPool) {
        let server = MockServer::start().await;
        let a = Uuid::new_v4();
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [{
                    "user_id": a,
                    "interests": ["Jazz", " jazz ", "Hiking", "", "   "],
                    "personality_traits": ["Calm", " calm "],
                    "deal_breakers": ["Smoking", "   ", "smoking"],
                    "age_min": null, "age_max": null,
                    "updated_at": "2026-10-13T12:00:00Z"
                }],
                "next": null
            })))
            .mount(&server)
            .await;

        run_once(&pool, &client(&server)).await.unwrap();
        assert_eq!(
            mirror::current(&pool, a, "interests").await.unwrap(),
            vec!["hiking", "jazz"]
        );
        assert_eq!(
            mirror::current(&pool, a, "personality_traits")
                .await
                .unwrap(),
            vec!["calm"]
        );
        assert_eq!(
            mirror::current(&pool, a, "deal_breakers").await.unwrap(),
            vec!["smoking"]
        );
    }

    fn export_row(age_min: Option<i32>, age_max: Option<i32>) -> ExportRow {
        ExportRow {
            user_id: Uuid::new_v4(),
            interests: vec![],
            personality_traits: vec![],
            deal_breakers: vec![],
            age_min,
            age_max,
            updated_at: Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap(),
        }
    }

    #[test]
    fn an_inverted_age_window_is_mirrored_as_no_preference() {
        for ((min, max), want) in [
            ((Some(30), Some(25)), (None, None)),
            ((Some(25), Some(30)), (Some(25), Some(30))),
            ((Some(30), Some(30)), (Some(30), Some(30))),
            ((Some(30), None), (Some(30), None)),
            ((None, Some(25)), (None, Some(25))),
            ((None, None), (None, None)),
        ] {
            let m = MirrorRow::from(&export_row(min, max));
            assert_eq!((m.age_min, m.age_max), want, "export ({min:?}, {max:?})");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_failing_cursor_update_rolls_the_whole_page_back(pool: PgPool) {
        sqlx::query(
            "ALTER TABLE pothos.sync_cursor ADD CONSTRAINT no_cursor CHECK (last_user_id IS NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let a = Uuid::new_v4();
        let page = [MirrorRow {
            user_id: a,
            interests: vec!["jazz".into()],
            personality_traits: vec![],
            deal_breakers: vec![],
            age_min: None,
            age_max: None,
            engine_updated_at: Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap(),
        }];

        assert!(mirror::apply_page(&pool, &page).await.is_err());
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.insight_mirror")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0, "upserts rolled back with the failed cursor update");
        let phrases: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.observed_phrases")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(phrases, 0, "phrase upserts rolled back too");
        assert!(mirror::load_cursor(&pool).await.unwrap().is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_next_that_does_not_advance_ends_the_walk(pool: PgPool) {
        let server = MockServer::start().await;
        let a = Uuid::new_v4();
        // `next` equals the position this request was made from: a conforming
        // engine never does this, and following it would loop forever.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [row(a, "2026-10-13T12:00:00Z", &["jazz"])],
                "next": { "updated_at": "1970-01-01T00:00:00Z", "user_id": Uuid::nil() }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let n = tokio::time::timeout(Duration::from_secs(10), run_once(&pool, &client(&server)))
            .await
            .expect("walk must terminate")
            .unwrap();
        assert_eq!(n, 1);
        assert!(mirror::exists(&pool, a).await.unwrap());
    }
}
