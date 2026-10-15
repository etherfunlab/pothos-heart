//! The engine export as this service stores it: the scalars in
//! `pothos.insight_mirror`, every array item ever exported in
//! `pothos.observed_phrases`. A phrase is current when its `last_seen_at`
//! equals the mirror row's `engine_updated_at`. Nothing here is deleted: a
//! phrase the engine stops exporting stays, its `last_seen_at` frozen.

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

use crate::engine_client::Cursor;
use crate::scoring;

/// One exported profile as the sweeper hands it over. Arrays may be raw; they
/// are canonicalised on write.
#[derive(Debug, Clone)]
pub struct MirrorRow {
    pub user_id: Uuid,
    pub interests: Vec<String>,
    pub personality_traits: Vec<String>,
    pub deal_breakers: Vec<String>,
    pub age_min: Option<i32>,
    pub age_max: Option<i32>,
    pub engine_updated_at: DateTime<Utc>,
}

/// Whether the engine has exported this user at least once.
pub async fn exists(pool: &PgPool, user_id: Uuid) -> sqlx::Result<bool> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pothos.insight_mirror WHERE user_id = $1)")
        .bind(user_id)
        .fetch_one(pool)
        .await
}

/// The scalar upsert and the phrase upsert for one row; `upsert_one` and
/// `apply_page` both run it inside their transaction.
async fn upsert_row(conn: &mut PgConnection, row: &MirrorRow) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO pothos.insight_mirror (user_id, age_min, age_max, engine_updated_at, synced_at) \
         VALUES ($1, $2, $3, $4, now()) \
         ON CONFLICT (user_id) DO UPDATE SET \
            age_min = EXCLUDED.age_min, age_max = EXCLUDED.age_max, \
            engine_updated_at = EXCLUDED.engine_updated_at, synced_at = now()",
    )
    .bind(row.user_id)
    .bind(row.age_min)
    .bind(row.age_max)
    .bind(row.engine_updated_at)
    .execute(&mut *conn)
    .await?;
    // A new phrase starts and ends at this export; a known one moves its end.
    sqlx::query(
        "INSERT INTO pothos.observed_phrases (user_id, dimension, phrase, first_seen_at, last_seen_at) \
         SELECT $1, d.dimension, d.phrase, $5, $5 FROM ( \
            SELECT 'interests' AS dimension, unnest($2::text[]) AS phrase \
            UNION ALL SELECT 'personality_traits', unnest($3::text[]) \
            UNION ALL SELECT 'deal_breakers', unnest($4::text[]) \
         ) d \
         ON CONFLICT (user_id, dimension, phrase) DO UPDATE SET last_seen_at = EXCLUDED.last_seen_at",
    )
    .bind(row.user_id)
    .bind(scoring::canonical(&row.interests))
    .bind(scoring::canonical(&row.personality_traits))
    .bind(scoring::canonical(&row.deal_breakers))
    .bind(row.engine_updated_at)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Single-row form of the upsert, for test fixtures.
#[cfg(test)]
pub async fn upsert_one(pool: &PgPool, row: &MirrorRow) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    upsert_row(&mut tx, row).await?;
    tx.commit().await
}

pub async fn load_cursor(pool: &PgPool) -> sqlx::Result<Option<Cursor>> {
    let row: (Option<DateTime<Utc>>, Option<Uuid>) =
        sqlx::query_as("SELECT last_updated_at, last_user_id FROM pothos.sync_cursor WHERE id = 1")
            .fetch_one(pool)
            .await?;
    Ok(match row {
        (Some(updated_at), Some(user_id)) => Some(Cursor {
            updated_at,
            user_id,
        }),
        _ => None,
    })
}

/// Upsert every row and move the cursor to the last one, in one transaction.
/// Rows must arrive in the engine's `(updated_at, user_id)` order — the last
/// row is the cursor. An empty slice is a no-op.
pub async fn apply_page(pool: &PgPool, rows: &[MirrorRow]) -> sqlx::Result<()> {
    let Some(last) = rows.last() else {
        return Ok(());
    };
    let mut tx = pool.begin().await?;
    for r in rows {
        upsert_row(&mut tx, r).await?;
    }
    sqlx::query(
        "UPDATE pothos.sync_cursor SET last_updated_at = $1, last_user_id = $2, synced_at = now() WHERE id = 1",
    )
    .bind(last.engine_updated_at)
    .bind(last.user_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

/// One ledger row, for tests.
#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Observed {
    pub dimension: String,
    pub phrase: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

#[cfg(test)]
pub async fn observed(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<Observed>> {
    sqlx::query_as(
        "SELECT dimension, phrase, first_seen_at, last_seen_at FROM pothos.observed_phrases \
         WHERE user_id = $1 ORDER BY dimension, phrase",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// The phrases of one dimension that the latest export carried, sorted.
#[cfg(test)]
pub async fn current(pool: &PgPool, user_id: Uuid, dimension: &str) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT o.phrase FROM pothos.observed_phrases o \
         JOIN pothos.insight_mirror m USING (user_id) \
         WHERE o.user_id = $1 AND o.dimension = $2 AND o.last_seen_at = m.engine_updated_at \
         ORDER BY o.phrase",
    )
    .bind(user_id)
    .bind(dimension)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;

    fn at(min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, 12, min, 0).unwrap()
    }

    fn row(id: Uuid, t: DateTime<Utc>, interests: &[&str]) -> MirrorRow {
        MirrorRow {
            user_id: id,
            interests: interests.iter().map(|s| s.to_string()).collect(),
            personality_traits: vec![],
            deal_breakers: vec![],
            age_min: None,
            age_max: None,
            engine_updated_at: t,
        }
    }

    fn obs(dimension: &str, phrase: &str, first: DateTime<Utc>, last: DateTime<Utc>) -> Observed {
        Observed {
            dimension: dimension.into(),
            phrase: phrase.into(),
            first_seen_at: first,
            last_seen_at: last,
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_phrase_the_engine_drops_stays_but_is_no_longer_current(pool: PgPool) {
        let a = Uuid::new_v4();
        upsert_one(&pool, &row(a, at(0), &["jazz", "tea"]))
            .await
            .unwrap();
        upsert_one(&pool, &row(a, at(5), &["jazz", "chess"]))
            .await
            .unwrap();
        assert_eq!(
            observed(&pool, a).await.unwrap(),
            vec![
                obs("interests", "chess", at(5), at(5)),
                obs("interests", "jazz", at(0), at(5)),
                obs("interests", "tea", at(0), at(0)),
            ]
        );
        assert_eq!(
            current(&pool, a, "interests").await.unwrap(),
            vec!["chess", "jazz"]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn re_applying_the_same_export_keeps_phrases_current_and_first_seen(pool: PgPool) {
        // The sweeper's overlap window re-fetches rows it already stored.
        let a = Uuid::new_v4();
        upsert_one(&pool, &row(a, at(0), &["jazz"])).await.unwrap();
        upsert_one(&pool, &row(a, at(0), &["jazz"])).await.unwrap();
        assert_eq!(
            observed(&pool, a).await.unwrap(),
            vec![obs("interests", "jazz", at(0), at(0))]
        );
        assert_eq!(current(&pool, a, "interests").await.unwrap(), vec!["jazz"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn phrases_are_canonical_and_each_dimension_is_kept_apart(pool: PgPool) {
        let a = Uuid::new_v4();
        let mut r = row(a, at(0), &[" Jazz ", "jazz", ""]);
        r.personality_traits = vec!["Jazz".into()];
        r.deal_breakers = vec!["Smoking".into()];
        upsert_one(&pool, &r).await.unwrap();
        assert_eq!(
            observed(&pool, a).await.unwrap(),
            vec![
                obs("deal_breakers", "smoking", at(0), at(0)),
                obs("interests", "jazz", at(0), at(0)),
                obs("personality_traits", "jazz", at(0), at(0)),
            ]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_export_with_empty_arrays_still_marks_the_user_exported(pool: PgPool) {
        let a = Uuid::new_v4();
        assert!(!exists(&pool, a).await.unwrap());
        upsert_one(&pool, &row(a, at(0), &[])).await.unwrap();
        assert!(exists(&pool, a).await.unwrap());
        assert!(observed(&pool, a).await.unwrap().is_empty());
    }
}
