//! Pulls the engine's exports into the mirror and the ledger. Each page is
//! committed with its cursor in one transaction, so a failure mid-walk
//! leaves the cursor at the last page that landed — never ahead of the data.

use std::time::Duration;

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::concepts::ConceptTask;
use crate::db::mirror::{self, MirrorRow};
use crate::db::{erasures, facts, memories, row_deletions};
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
        // Arrays are canonicalised and capped when written (`mirror::upsert_row`).
        Self {
            user_id: r.user_id,
            interests: r.interests.clone(),
            personality_traits: r.personality_traits.clone(),
            deal_breakers: r.deal_breakers.clone(),
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

/// Walk the memories export from just behind its cursor to the end, like
/// `run_once`. Returns rows received (re-fetched ones included).
pub async fn run_memories_once(pool: &PgPool, client: &EngineClient) -> Result<usize, SweepError> {
    let (mut since, mut after_id) = match memories::load_cursor(pool).await? {
        Some(c) => (c.created_at - OVERLAP, Uuid::nil()),
        None => (DateTime::<Utc>::UNIX_EPOCH, Uuid::nil()),
    };
    let mut total = 0;
    loop {
        let page = client.memory_changes(since, after_id).await?;
        if page.memories.is_empty() {
            break;
        }
        memories::apply_page(pool, &page.memories).await?;
        total += page.memories.len();
        let Some(next) = page.next else { break };
        if (next.created_at, next.id) <= (since, after_id) {
            tracing::warn!(
                ?next,
                "engine memories cursor did not advance; ending sweep"
            );
            break;
        }
        (since, after_id) = (next.created_at, next.id);
    }
    Ok(total)
}

/// Walk the erasure (tombstone) stream from just behind its cursor to the
/// end, dropping every erased user's mirrored and derived rows. Returns
/// tombstones applied (re-fetched ones included).
pub async fn run_erasures_once(pool: &PgPool, client: &EngineClient) -> Result<usize, SweepError> {
    let (mut since, mut after_user) = match erasures::load_cursor(pool).await? {
        Some(c) => (c.erased_at - OVERLAP, Uuid::nil()),
        None => (DateTime::<Utc>::UNIX_EPOCH, Uuid::nil()),
    };
    let mut total = 0;
    loop {
        let page = client.deletions(since, after_user).await?;
        if page.erasures.is_empty() {
            break;
        }
        erasures::apply_page(pool, &page.erasures).await?;
        total += page.erasures.len();
        let Some(next) = page.next else { break };
        if (next.erased_at, next.user_id) <= (since, after_user) {
            tracing::warn!(?next, "engine erasure cursor did not advance; ending sweep");
            break;
        }
        (since, after_user) = (next.erased_at, next.user_id);
    }
    Ok(total)
}

/// Walk the row-deletion stream from just behind its cursor to the end,
/// dropping each deleted memory or fact from its mirror. Returns tombstones
/// applied (re-fetched ones included).
pub async fn run_row_deletions_once(
    pool: &PgPool,
    client: &EngineClient,
) -> Result<usize, SweepError> {
    let (mut since, mut after_id) = match row_deletions::load_cursor(pool).await? {
        Some(c) => (c.deleted_at - OVERLAP, Uuid::nil()),
        None => (DateTime::<Utc>::UNIX_EPOCH, Uuid::nil()),
    };
    let mut total = 0;
    loop {
        let page = client.row_deletions(since, after_id).await?;
        if page.deletions.is_empty() {
            break;
        }
        row_deletions::apply_page(pool, &page.deletions).await?;
        total += page.deletions.len();
        let Some(next) = page.next else { break };
        if (next.deleted_at, next.id) <= (since, after_id) {
            tracing::warn!(
                ?next,
                "engine row-deletion cursor did not advance; ending sweep"
            );
            break;
        }
        (since, after_id) = (next.deleted_at, next.id);
    }
    Ok(total)
}

/// Walk the facts export from just behind its cursor to the end, like
/// `run_memories_once`. Self-gating: a client without the facts secret skips
/// the step (`Ok(0)`) without a request. Returns rows received (re-fetched
/// ones included).
pub async fn run_facts_once(pool: &PgPool, client: &EngineClient) -> Result<usize, SweepError> {
    if !client.has_facts() {
        tracing::debug!("no facts secret configured; facts step skipped");
        return Ok(0);
    }
    let (mut since, mut after_id) = match facts::load_cursor(pool).await? {
        Some(c) => (c.last_seen_at - OVERLAP, Uuid::nil()),
        None => (DateTime::<Utc>::UNIX_EPOCH, Uuid::nil()),
    };
    let mut total = 0;
    loop {
        let page = client.facts_changes(since, after_id).await?;
        if page.facts.is_empty() {
            break;
        }
        facts::apply_page(pool, &page.facts).await?;
        total += page.facts.len();
        let Some(next) = page.next else { break };
        if (next.last_seen_at, next.id) <= (since, after_id) {
            tracing::warn!(?next, "engine facts cursor did not advance; ending sweep");
            break;
        }
        (since, after_id) = (next.last_seen_at, next.id);
    }
    Ok(total)
}

/// Sweep forever at `interval`: mirror both exports, then run the model steps
/// that are configured, whatever the earlier steps did. Errors are logged; the
/// next tick retries from wherever each step stopped.
pub async fn run_loop(
    pool: PgPool,
    client: EngineClient,
    concepts: Option<ConceptTask>,
    autobio: Option<crate::autobio::AutobioTask>,
    judge: Option<crate::judge::JudgeTask>,
    interval: Duration,
) {
    loop {
        // Tombstones first, so the rest of the tick never works on data the
        // engine has already erased.
        match run_erasures_once(&pool, &client).await {
            Ok(n) if n > 0 => tracing::info!(users = n, "erasure sweep"),
            Ok(_) => tracing::debug!("erasure sweep: nothing new"),
            // An engine older than the deletion streams.
            Err(SweepError::Engine(EngineError::Status(404))) => {
                tracing::debug!("engine has no deletion stream; skipped")
            }
            Err(e) => {
                tracing::warn!(error = ?e, "erasure sweep failed; cursor kept at last landed page")
            }
        }
        // Single rows their owner deleted, before the changes steps for the
        // same reason.
        match run_row_deletions_once(&pool, &client).await {
            Ok(n) if n > 0 => tracing::info!(rows = n, "row-deletion sweep"),
            Ok(_) => tracing::debug!("row-deletion sweep: nothing new"),
            // An engine older than the row-deletion stream.
            Err(SweepError::Engine(EngineError::Status(404))) => {
                tracing::debug!("engine has no row-deletion stream; skipped")
            }
            Err(e) => {
                tracing::warn!(error = ?e, "row-deletion sweep failed; cursor kept at last landed page")
            }
        }
        match run_once(&pool, &client).await {
            Ok(n) if n > 0 => tracing::info!(rows = n, "mirror sweep"),
            Ok(_) => tracing::debug!("mirror sweep: nothing new"),
            // Debug, not Display: it carries the cause chain, so the operator
            // can tell a refused connection from DNS or TLS.
            Err(e) => {
                tracing::warn!(error = ?e, "mirror sweep failed; cursor kept at last landed page")
            }
        }
        match run_memories_once(&pool, &client).await {
            Ok(n) if n > 0 => tracing::info!(rows = n, "memories sweep"),
            Ok(_) => tracing::debug!("memories sweep: nothing new"),
            // An engine older than the memories export: autobio runs on the
            // material that is there.
            Err(SweepError::Engine(EngineError::Status(404))) => {
                tracing::debug!("engine has no memories export; skipped")
            }
            Err(e) => {
                tracing::warn!(error = ?e, "memories sweep failed; cursor kept at last landed page")
            }
        }
        match run_facts_once(&pool, &client).await {
            Ok(n) if n > 0 => tracing::info!(rows = n, "facts sweep"),
            Ok(_) => tracing::debug!("facts sweep: nothing new"),
            // An engine without the facts export mounted: autobio uses the
            // memory material alone.
            Err(SweepError::Engine(EngineError::Status(404))) => {
                tracing::debug!("engine has no facts export; skipped")
            }
            Err(e) => {
                tracing::warn!(error = ?e, "facts sweep failed; cursor kept at last landed page")
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
        if let Some(task) = &autobio {
            match crate::autobio::run_once(&pool, task).await {
                Ok(r) if r.users > 0 => tracing::info!(
                    users = r.users,
                    failed = r.failed,
                    added = r.added,
                    revised = r.revised,
                    retired = r.retired,
                    skipped = r.skipped,
                    tokens = r.tokens,
                    "autobio step"
                ),
                Ok(_) => tracing::debug!("autobio step: nobody due"),
                Err(e) => {
                    tracing::warn!(error = ?e, "autobio step failed; the rest wait for the next tick")
                }
            }
        }
        if let Some(task) = &judge {
            match crate::judge::run_once(&pool, task).await {
                Ok(r) if r.judged + r.dropped > 0 => tracing::info!(
                    judged = r.judged,
                    dropped = r.dropped,
                    tokens = r.tokens,
                    "judge step"
                ),
                Ok(_) => tracing::debug!("judge step: queue empty"),
                Err(e) => tracing::warn!(error = ?e, "judge step failed; pairs stay queued"),
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
            facts_secret: None,
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

    fn memory(id: Uuid, user: Uuid, ts: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "user_id": user, "content": "c", "category": "fact",
            "metadata": null, "created_at": ts
        })
    }

    async fn mirrored_memories(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM pothos.memory_mirror")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn memories_walk_two_pages_and_resume_overlap_behind(pool: PgPool) {
        let server = MockServer::start().await;
        let u = Uuid::new_v4();
        let (m1, m2) = (Uuid::new_v4(), Uuid::new_v4());
        let t1 = "2026-10-13T12:00:00Z";
        let t2 = "2026-10-13T12:05:00Z";
        Mock::given(method("GET"))
            .and(path("/svc/v1/memories/changes"))
            .and(query_param("since", "1970-01-01T00:00:00Z"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "memories": [memory(m1, u, t1)],
                "next": { "created_at": t1, "id": m1 }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/svc/v1/memories/changes"))
            .and(query_param("since", t1))
            .and(query_param("after_id", m1.to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "memories": [memory(m2, u, t2)], "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(run_memories_once(&pool, &client(&server)).await.unwrap(), 2);
        assert_eq!(mirrored_memories(&pool).await, 2);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/svc/v1/memories/changes"))
            .and(query_param("since", "2026-10-13T12:04:55Z"))
            .and(query_param(
                "after_id",
                "00000000-0000-0000-0000-000000000000",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                // The overlap re-returns a row already stored: a no-op.
                "memories": [memory(m2, u, t2)], "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(run_memories_once(&pool, &client(&server)).await.unwrap(), 1);
        assert_eq!(mirrored_memories(&pool).await, 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn text_columns_in_the_export_are_ignored(pool: PgPool) {
        let server = MockServer::start().await;
        let a = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path("/svc/v1/insights/changes"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [{
                    "user_id": a, "interests": ["jazz"], "personality_traits": [],
                    "deal_breakers": [], "updated_at": "2026-10-13T12:00:00Z",
                    "love_values": "slow to warm up", "occupation": "dev"
                }],
                "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(run_once(&pool, &client(&server)).await.unwrap(), 1);
        let rows = mirror::observed(&pool, a).await.unwrap();
        assert_eq!(rows.len(), 1, "only the array phrase enters the ledger");
        assert_eq!(rows[0].dimension, "interests");
        assert_eq!(rows[0].phrase, "jazz");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn erasures_walk_two_pages_and_resume_overlap_behind(pool: PgPool) {
        let server = MockServer::start().await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let t1 = "2026-10-13T12:00:00Z";
        let t2 = "2026-10-13T12:05:00Z";
        Mock::given(method("GET"))
            .and(path("/svc/v1/insights/deletions"))
            .and(query_param("since", "1970-01-01T00:00:00Z"))
            .and(query_param(
                "after_user",
                "00000000-0000-0000-0000-000000000000",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "erasures": [{ "user_id": a, "erased_at": t1 }],
                "next": { "erased_at": t1, "user_id": a }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/svc/v1/insights/deletions"))
            .and(query_param("since", t1))
            .and(query_param("after_user", a.to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "erasures": [{ "user_id": b, "erased_at": t2 }], "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(run_erasures_once(&pool, &client(&server)).await.unwrap(), 2);
        let cur = crate::db::erasures::load_cursor(&pool)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cur.user_id, b);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/svc/v1/insights/deletions"))
            .and(query_param("since", "2026-10-13T12:04:55Z"))
            .and(query_param(
                "after_user",
                "00000000-0000-0000-0000-000000000000",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "erasures": [{ "user_id": b, "erased_at": t2 }], "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(run_erasures_once(&pool, &client(&server)).await.unwrap(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn erasures_404_leaves_cursor_untouched(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = run_erasures_once(&pool, &client(&server))
            .await
            .unwrap_err();
        assert!(matches!(err, SweepError::Engine(EngineError::Status(404))));
        assert!(crate::db::erasures::load_cursor(&pool)
            .await
            .unwrap()
            .is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn row_deletions_walk_two_pages_and_resume_overlap_behind(pool: PgPool) {
        let server = MockServer::start().await;
        let u = Uuid::new_v4();
        let f =
            crate::db::autobio::tests::fact(&pool, u, "likes jazz", Utc::now(), Utc::now()).await;
        let m = crate::db::autobio::tests::memory(&pool, u, "hikes", Utc::now()).await;
        let t1 = "2026-11-10T12:00:00Z";
        let t2 = "2026-11-10T12:05:00Z";
        Mock::given(method("GET"))
            .and(path("/svc/v1/rows/deletions"))
            .and(query_param("since", "1970-01-01T00:00:00Z"))
            .and(query_param(
                "after_id",
                "00000000-0000-0000-0000-000000000000",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "deletions": [{ "kind": "fact", "id": f, "deleted_at": t1 }],
                "next": { "deleted_at": t1, "id": f }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/svc/v1/rows/deletions"))
            .and(query_param("since", t1))
            .and(query_param("after_id", f.to_string()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "deletions": [
                    { "kind": "memory", "id": m, "deleted_at": t2 },
                    { "kind": "journal", "id": Uuid::new_v4(), "deleted_at": t2 }
                ],
                "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;

        assert_eq!(
            run_row_deletions_once(&pool, &client(&server))
                .await
                .unwrap(),
            3
        );
        assert_eq!(mirrored_facts(&pool).await, 0);
        let memories: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.memory_mirror")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(memories, 0);
        let cur = crate::db::row_deletions::load_cursor(&pool)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(cur.deleted_at.to_rfc3339(), "2026-11-10T12:05:00+00:00");

        // The next sweep resumes five seconds behind the cursor.
        Mock::given(method("GET"))
            .and(path("/svc/v1/rows/deletions"))
            .and(query_param("since", "2026-11-10T12:04:55Z"))
            .and(query_param(
                "after_id",
                "00000000-0000-0000-0000-000000000000",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "deletions": [{ "kind": "memory", "id": m, "deleted_at": t2 }], "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            run_row_deletions_once(&pool, &client(&server))
                .await
                .unwrap(),
            1
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn row_deletions_404_leaves_cursor_untouched(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = run_row_deletions_once(&pool, &client(&server))
            .await
            .unwrap_err();
        assert!(matches!(err, SweepError::Engine(EngineError::Status(404))));
        assert!(crate::db::row_deletions::load_cursor(&pool)
            .await
            .unwrap()
            .is_none());
    }

    fn facts_client(server: &MockServer) -> EngineClient {
        EngineClient::new(&EngineConfig {
            base_url: server.uri(),
            export_secret: "s".repeat(32),
            facts_secret: Some("f".repeat(32)),
        })
    }

    async fn mirrored_facts(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM pothos.fact_mirror")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn facts_walk_two_pages_and_resume_overlap_behind(pool: PgPool) {
        use crate::engine_client::FACTS_SECRET_HEADER;
        let server = MockServer::start().await;
        let u = Uuid::new_v4();
        let (f1, f2) = (Uuid::new_v4(), Uuid::new_v4());
        let t1 = "2026-10-13T12:00:00Z";
        let t2 = "2026-10-13T12:05:00Z";
        let fact = |id: Uuid, last: &str| {
            serde_json::json!({
                "id": id, "user_id": u, "content": "c", "category": "fact",
                "domain": "other", "evidence_type": "explicit_statement",
                "temporality": "current", "persistence": "stable",
                "confidence": "high", "occurrences": 1, "session_count": 1,
                "recurrence": "once", "first_seen_at": t1, "last_seen_at": last
            })
        };
        Mock::given(method("GET"))
            .and(path("/svc/v1/facts/changes"))
            .and(wiremock::matchers::header(
                FACTS_SECRET_HEADER,
                "f".repeat(32).as_str(),
            ))
            .and(query_param("since", "1970-01-01T00:00:00Z"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "facts": [fact(f1, t1)],
                "next": { "last_seen_at": t1, "id": f1 }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/svc/v1/facts/changes"))
            .and(query_param("since", t1))
            .and(query_param("after_id", f1.to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "facts": [fact(f2, t2)], "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            run_facts_once(&pool, &facts_client(&server)).await.unwrap(),
            2
        );
        assert_eq!(mirrored_facts(&pool).await, 2);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/svc/v1/facts/changes"))
            .and(query_param("since", "2026-10-13T12:04:55Z"))
            .and(query_param(
                "after_id",
                "00000000-0000-0000-0000-000000000000",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                // The overlap re-returns a row already stored: a no-op.
                "facts": [fact(f2, t2)], "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            run_facts_once(&pool, &facts_client(&server)).await.unwrap(),
            1
        );
        assert_eq!(mirrored_facts(&pool).await, 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn facts_without_a_secret_skip_without_a_request(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        assert_eq!(run_facts_once(&pool, &client(&server)).await.unwrap(), 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn facts_404_leaves_cursor_and_mirror_untouched(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = run_facts_once(&pool, &facts_client(&server))
            .await
            .unwrap_err();
        assert!(matches!(err, SweepError::Engine(EngineError::Status(404))));
        assert!(crate::db::facts::load_cursor(&pool)
            .await
            .unwrap()
            .is_none());
        assert_eq!(mirrored_facts(&pool).await, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn memories_404_leaves_cursor_and_mirror_untouched(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = run_memories_once(&pool, &client(&server))
            .await
            .unwrap_err();
        assert!(matches!(err, SweepError::Engine(EngineError::Status(404))));
        assert!(crate::db::memories::load_cursor(&pool)
            .await
            .unwrap()
            .is_none());
        assert_eq!(mirrored_memories(&pool).await, 0);
    }
}
