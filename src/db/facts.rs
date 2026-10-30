//! `pothos.fact_mirror` — the engine's fact ledger, as exported. A repeat
//! sighting changes the engine row (its `last_seen_at` advances), and only
//! then does the mirror bump `recorded_at`: a re-seen fact is new autobio
//! material, an overlap re-fetch of an unchanged row is not.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::engine_client::{FactCursor, FactRow};

pub async fn load_cursor(pool: &PgPool) -> sqlx::Result<Option<FactCursor>> {
    let row: (Option<DateTime<Utc>>, Option<Uuid>) =
        sqlx::query_as("SELECT last_seen_at, last_id FROM pothos.fact_cursor WHERE id = 1")
            .fetch_one(pool)
            .await?;
    Ok(match row {
        (Some(last_seen_at), Some(id)) => Some(FactCursor { last_seen_at, id }),
        _ => None,
    })
}

/// Upsert every row and move the cursor to the last one, in one transaction.
/// Rows must arrive in the engine's `(last_seen_at, id)` order. An empty
/// slice is a no-op.
pub async fn apply_page(pool: &PgPool, rows: &[FactRow]) -> sqlx::Result<()> {
    let Some(last) = rows.last() else {
        return Ok(());
    };
    let mut tx = pool.begin().await?;
    for r in rows {
        sqlx::query(
            "INSERT INTO pothos.fact_mirror \
                (id, user_id, content, category, domain, evidence_type, temporality, \
                 persistence, confidence, occurrences, session_count, recurrence, \
                 first_seen_at, last_seen_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14) \
             ON CONFLICT (id) DO UPDATE SET \
                content = EXCLUDED.content, category = EXCLUDED.category, \
                domain = EXCLUDED.domain, evidence_type = EXCLUDED.evidence_type, \
                temporality = EXCLUDED.temporality, persistence = EXCLUDED.persistence, \
                confidence = EXCLUDED.confidence, occurrences = EXCLUDED.occurrences, \
                session_count = EXCLUDED.session_count, recurrence = EXCLUDED.recurrence, \
                first_seen_at = EXCLUDED.first_seen_at, last_seen_at = EXCLUDED.last_seen_at, \
                recorded_at = now() \
             WHERE pothos.fact_mirror.last_seen_at < EXCLUDED.last_seen_at",
        )
        .bind(r.id)
        .bind(r.user_id)
        .bind(&r.content)
        .bind(&r.category)
        .bind(&r.domain)
        .bind(&r.evidence_type)
        .bind(&r.temporality)
        .bind(&r.persistence)
        .bind(&r.confidence)
        .bind(r.occurrences)
        .bind(r.session_count)
        .bind(&r.recurrence)
        .bind(r.first_seen_at)
        .bind(r.last_seen_at)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE pothos.fact_cursor SET last_seen_at = $1, last_id = $2, synced_at = now() WHERE id = 1",
    )
    .bind(last.last_seen_at)
    .bind(last.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;

    pub(crate) fn fact(id: Uuid, user: Uuid, first: DateTime<Utc>, last: DateTime<Utc>) -> FactRow {
        FactRow {
            id,
            user_id: user,
            content: "plays weekend jazz".into(),
            category: "hobby".into(),
            domain: "hobby".into(),
            evidence_type: "explicit_statement".into(),
            temporality: "current".into(),
            persistence: "ongoing".into(),
            confidence: "medium".into(),
            occurrences: 1,
            session_count: 1,
            recurrence: "once".into(),
            first_seen_at: first,
            last_seen_at: last,
        }
    }

    fn at(min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, 12, min, 0).unwrap()
    }

    async fn stored(
        pool: &PgPool,
        id: Uuid,
    ) -> (String, i32, String, DateTime<Utc>, DateTime<Utc>) {
        sqlx::query_as(
            "SELECT confidence, occurrences, recurrence, last_seen_at, recorded_at \
             FROM pothos.fact_mirror WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_reseen_fact_updates_the_row_and_bumps_recorded_at(pool: PgPool) {
        let (f, u) = (Uuid::new_v4(), Uuid::new_v4());
        apply_page(&pool, &[fact(f, u, at(0), at(0))]).await.unwrap();
        let (_, _, _, _, recorded_first) = stored(&pool, f).await;

        let mut seen_again = fact(f, u, at(0), at(5));
        seen_again.occurrences = 3;
        seen_again.confidence = "high".into();
        seen_again.recurrence = "repeated_across_sessions".into();
        apply_page(&pool, &[seen_again]).await.unwrap();

        let (confidence, occurrences, recurrence, last_seen, recorded) = stored(&pool, f).await;
        assert_eq!(
            (confidence.as_str(), occurrences, recurrence.as_str()),
            ("high", 3, "repeated_across_sessions")
        );
        assert_eq!(last_seen, at(5));
        assert!(recorded > recorded_first, "a changed row is new material");
        let cur = load_cursor(&pool).await.unwrap().unwrap();
        assert_eq!((cur.last_seen_at, cur.id), (at(5), f));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_overlap_refetch_of_an_unchanged_fact_keeps_recorded_at(pool: PgPool) {
        let (f, u) = (Uuid::new_v4(), Uuid::new_v4());
        let row = fact(f, u, at(0), at(0));
        apply_page(&pool, std::slice::from_ref(&row)).await.unwrap();
        let before = stored(&pool, f).await;

        apply_page(&pool, &[row]).await.unwrap();
        let after = stored(&pool, f).await;
        assert_eq!(before, after, "an unchanged row is not new material");
    }
}
