//! The engine's row-deletion stream, honored locally. A tombstone names one
//! memory or fact its owner deleted: the mirrored row goes, the id is kept so
//! a late changes page cannot bring it back, and an autobio entry left without
//! any source is retired. A user who lost an entry loses their verdicts too:
//! a rationale quotes both autobios.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::engine_client::{RowDeletion, RowDeletionCursor};

pub async fn load_cursor(pool: &PgPool) -> sqlx::Result<Option<RowDeletionCursor>> {
    let row: (Option<DateTime<Utc>>, Option<Uuid>) = sqlx::query_as(
        "SELECT last_deleted_at, last_id FROM pothos.row_deletion_cursor WHERE id = 1",
    )
    .fetch_one(pool)
    .await?;
    Ok(match row {
        (Some(deleted_at), Some(id)) => Some(RowDeletionCursor { deleted_at, id }),
        _ => None,
    })
}

/// Apply every tombstone and move the cursor to the last one, in one
/// transaction. Rows must arrive in the engine's `(deleted_at, id)` order.
/// Idempotent: a repeated tombstone, or one for a row never mirrored, deletes
/// nothing and still advances the cursor. A kind other than `memory` or
/// `fact` is skipped. An empty slice is a no-op.
pub async fn apply_page(pool: &PgPool, rows: &[RowDeletion]) -> sqlx::Result<()> {
    let Some(last) = rows.last() else {
        return Ok(());
    };
    let mut tx = pool.begin().await?;
    // Entries that cited a row deleted in this page.
    let mut cut: Vec<i64> = Vec::new();
    for r in rows {
        let (mirror, sources, column) = match r.kind.as_str() {
            "memory" => ("memory_mirror", "autobio_memory_sources", "memory_id"),
            "fact" => ("fact_mirror", "autobio_fact_sources", "fact_id"),
            _ => continue,
        };
        let entries: Vec<i64> = sqlx::query_scalar(&format!(
            "SELECT entry_id FROM pothos.{sources} WHERE {column} = $1"
        ))
        .bind(r.id)
        .fetch_all(&mut *tx)
        .await?;
        cut.extend(entries);
        // The source links go with the mirror row (ON DELETE CASCADE).
        sqlx::query(&format!("DELETE FROM pothos.{mirror} WHERE id = $1"))
            .bind(r.id)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO pothos.row_tombstones (kind, id, deleted_at) VALUES ($1, $2, $3) \
             ON CONFLICT (kind, id) DO NOTHING",
        )
        .bind(&r.kind)
        .bind(r.id)
        .bind(r.deleted_at)
        .execute(&mut *tx)
        .await?;
    }
    if !cut.is_empty() {
        let users: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE pothos.autobio_entries e \
             SET retired_at = now(), retired_reason = 'source_deleted' \
             WHERE e.id = ANY($1) AND e.retired_at IS NULL \
               AND NOT EXISTS (SELECT 1 FROM pothos.autobio_fact_sources s WHERE s.entry_id = e.id) \
               AND NOT EXISTS (SELECT 1 FROM pothos.autobio_memory_sources s WHERE s.entry_id = e.id) \
               AND NOT EXISTS (SELECT 1 FROM pothos.autobio_observed_sources s WHERE s.entry_id = e.id) \
             RETURNING e.user_id",
        )
        .bind(&cut)
        .fetch_all(&mut *tx)
        .await?;
        if !users.is_empty() {
            sqlx::query(
                "DELETE FROM pothos.pair_verdicts WHERE user_lo = ANY($1) OR user_hi = ANY($1)",
            )
            .bind(&users)
            .execute(&mut *tx)
            .await?;
        }
    }
    sqlx::query(
        "UPDATE pothos.row_deletion_cursor SET last_deleted_at = $1, last_id = $2, synced_at = now() WHERE id = 1",
    )
    .bind(last.deleted_at)
    .bind(last.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::db::autobio::tests::{entry, export, fact, join_pool, memory, observed_id};
    use crate::db::autobio::{current_entries, veto, vetoed_statements};

    fn at(min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 10, 12, min, 0).unwrap()
    }

    fn tomb(kind: &str, id: Uuid, min: u32) -> RowDeletion {
        RowDeletion {
            kind: kind.into(),
            id,
            deleted_at: at(min),
        }
    }

    async fn cite(pool: &PgPool, table: &str, entry: i64, id: Uuid) {
        sqlx::query(&format!("INSERT INTO pothos.{table} VALUES ($1, $2)"))
            .bind(entry)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn exists(pool: &PgPool, table: &str, id: Uuid) -> bool {
        sqlx::query_scalar(&format!(
            "SELECT EXISTS (SELECT 1 FROM pothos.{table} WHERE id = $1)"
        ))
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn tombstones(pool: &PgPool) -> Vec<(String, Uuid)> {
        sqlx::query_as("SELECT kind, id FROM pothos.row_tombstones ORDER BY kind, id")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn retirement(pool: &PgPool, entry: i64) -> (Option<String>, Option<DateTime<Utc>>) {
        sqlx::query_as(
            "SELECT retired_reason, retired_at FROM pothos.autobio_entries WHERE id = $1",
        )
        .bind(entry)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn version(pool: &PgPool, user: Uuid) -> DateTime<Utc> {
        sqlx::query_scalar("SELECT version FROM pothos.autobio_versions WHERE user_id = $1")
            .bind(user)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn verdict(pool: &PgPool, a: Uuid, b: Uuid) {
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        sqlx::query(
            "INSERT INTO pothos.pair_verdicts (user_lo, user_hi, lo_version, hi_version, grade, rationale, model) \
             VALUES ($1, $2, now(), now(), 'promising', 'r', 'm')",
        )
        .bind(lo)
        .bind(hi)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO pothos.judge_queue (user_lo, user_hi) VALUES ($1, $2)")
            .bind(lo)
            .bind(hi)
            .execute(pool)
            .await
            .unwrap();
    }

    async fn pairs(pool: &PgPool, table: &str, user: Uuid) -> i64 {
        sqlx::query_scalar(&format!(
            "SELECT count(*) FROM pothos.{table} WHERE user_lo = $1 OR user_hi = $1"
        ))
        .bind(user)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_tombstone_drops_the_mirror_row_and_keeps_its_id(pool: PgPool) {
        let u = Uuid::new_v4();
        let m = memory(&pool, u, "likes jazz", at(0)).await;
        let f = fact(&pool, u, "likes jazz", at(0), at(0)).await;
        let kept = fact(&pool, u, "hikes", at(0), at(0)).await;

        apply_page(&pool, &[tomb("memory", m, 1), tomb("fact", f, 2)])
            .await
            .unwrap();

        assert!(!exists(&pool, "memory_mirror", m).await);
        assert!(!exists(&pool, "fact_mirror", f).await);
        assert!(exists(&pool, "fact_mirror", kept).await);
        let mut want = vec![("fact".to_string(), f), ("memory".to_string(), m)];
        want.sort();
        assert_eq!(tombstones(&pool).await, want);
        let cur = load_cursor(&pool).await.unwrap().unwrap();
        assert_eq!((cur.deleted_at, cur.id), (at(2), f));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_id_stores_the_tombstone_and_moves_the_cursor(pool: PgPool) {
        let id = Uuid::new_v4();
        apply_page(&pool, &[tomb("fact", id, 1)]).await.unwrap();
        assert_eq!(tombstones(&pool).await, vec![("fact".to_string(), id)]);
        assert_eq!(load_cursor(&pool).await.unwrap().unwrap().id, id);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unknown_kind_is_skipped_and_not_stored(pool: PgPool) {
        let u = Uuid::new_v4();
        // The same id as a mirrored fact: a foreign kind must not touch it.
        let f = fact(&pool, u, "hikes", at(0), at(0)).await;
        apply_page(&pool, &[tomb("journal", f, 1)]).await.unwrap();
        assert!(exists(&pool, "fact_mirror", f).await);
        assert!(tombstones(&pool).await.is_empty());
        assert_eq!(load_cursor(&pool).await.unwrap().unwrap().id, f);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_entry_losing_its_only_source_retires_as_source_deleted(pool: PgPool) {
        let u = Uuid::new_v4();
        join_pool(&pool, u, true).await;
        let f = fact(&pool, u, "likes jazz", at(0), at(0)).await;
        let m = memory(&pool, u, "hikes on weekends", at(0)).await;
        let by_fact = entry(&pool, u, "life", "Likes jazz.").await;
        let by_memory = entry(&pool, u, "life", "Hikes on weekends.").await;
        let other = entry(&pool, u, "life", "Works nights.").await;
        cite(&pool, "autobio_fact_sources", by_fact, f).await;
        cite(&pool, "autobio_memory_sources", by_memory, m).await;
        cite(
            &pool,
            "autobio_fact_sources",
            other,
            fact(&pool, u, "works nights", at(0), at(0)).await,
        )
        .await;
        let before = version(&pool, u).await;

        apply_page(&pool, &[tomb("fact", f, 1), tomb("memory", m, 2)])
            .await
            .unwrap();

        let current: Vec<i64> = current_entries(&pool, u)
            .await
            .unwrap()
            .iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(current, vec![other]);
        for e in [by_fact, by_memory] {
            assert_eq!(
                retirement(&pool, e).await.0.as_deref(),
                Some("source_deleted")
            );
        }
        assert!(
            version(&pool, u).await > before,
            "the autobio version moved"
        );
        assert!(
            vetoed_statements(&pool, u).await.unwrap().is_empty(),
            "a source deletion is not the owner's veto"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_entry_with_another_source_stays(pool: PgPool) {
        let u = Uuid::new_v4();
        join_pool(&pool, u, true).await;
        export(&pool, u, at(0), &["jazz"]).await;
        let phrase = observed_id(&pool, u, "interests", "jazz").await;
        let (f1, f2, f3) = (
            fact(&pool, u, "likes jazz", at(0), at(0)).await,
            fact(&pool, u, "goes to jazz bars", at(0), at(0)).await,
            fact(&pool, u, "plays the sax", at(0), at(0)).await,
        );
        let two_facts = entry(&pool, u, "life", "Likes jazz.").await;
        cite(&pool, "autobio_fact_sources", two_facts, f1).await;
        cite(&pool, "autobio_fact_sources", two_facts, f2).await;
        let with_phrase = entry(&pool, u, "life", "Plays the sax.").await;
        cite(&pool, "autobio_fact_sources", with_phrase, f3).await;
        sqlx::query("INSERT INTO pothos.autobio_observed_sources VALUES ($1, $2)")
            .bind(with_phrase)
            .bind(phrase)
            .execute(&pool)
            .await
            .unwrap();
        let other = Uuid::new_v4();
        verdict(&pool, u, other).await;
        let before = version(&pool, u).await;

        apply_page(&pool, &[tomb("fact", f1, 1), tomb("fact", f3, 2)])
            .await
            .unwrap();

        assert_eq!(current_entries(&pool, u).await.unwrap().len(), 2);
        assert_eq!(version(&pool, u).await, before);
        assert_eq!(
            pairs(&pool, "pair_verdicts", u).await,
            1,
            "no entry retired"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn two_sources_of_one_entry_in_one_page_retire_it_once(pool: PgPool) {
        let u = Uuid::new_v4();
        join_pool(&pool, u, true).await;
        let f = fact(&pool, u, "likes jazz", at(0), at(0)).await;
        let m = memory(&pool, u, "likes jazz", at(0)).await;
        let e = entry(&pool, u, "life", "Likes jazz.").await;
        cite(&pool, "autobio_fact_sources", e, f).await;
        cite(&pool, "autobio_memory_sources", e, m).await;

        apply_page(&pool, &[tomb("fact", f, 1), tomb("memory", m, 2)])
            .await
            .unwrap();
        let first = retirement(&pool, e).await;
        assert_eq!(first.0.as_deref(), Some("source_deleted"));

        // The page again: nothing left to cut, nothing changes.
        apply_page(&pool, &[tomb("fact", f, 1), tomb("memory", m, 2)])
            .await
            .unwrap();
        assert_eq!(retirement(&pool, e).await, first);
        assert_eq!(tombstones(&pool).await.len(), 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_retired_entry_keeps_its_reason(pool: PgPool) {
        let u = Uuid::new_v4();
        join_pool(&pool, u, true).await;
        let f = fact(&pool, u, "likes jazz", at(0), at(0)).await;
        let e = entry(&pool, u, "life", "Likes jazz.").await;
        cite(&pool, "autobio_fact_sources", e, f).await;
        assert!(veto(&pool, u, e).await.unwrap());
        let before = retirement(&pool, e).await;

        apply_page(&pool, &[tomb("fact", f, 1)]).await.unwrap();

        assert_eq!(retirement(&pool, e).await, before);
        assert_eq!(before.0.as_deref(), Some("vetoed"));
        assert_eq!(
            vetoed_statements(&pool, u).await.unwrap(),
            vec!["Likes jazz.".to_string()]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_retirement_drops_the_users_verdicts_and_spares_others(pool: PgPool) {
        let (a, b, c, d) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        join_pool(&pool, a, true).await;
        let f = fact(&pool, a, "likes jazz", at(0), at(0)).await;
        let e = entry(&pool, a, "life", "Likes jazz.").await;
        cite(&pool, "autobio_fact_sources", e, f).await;
        verdict(&pool, a, b).await;
        // The user on each side of a pair, whatever the random ids' order.
        verdict(&pool, a, Uuid::nil()).await;
        verdict(&pool, a, Uuid::max()).await;
        verdict(&pool, c, d).await;

        apply_page(&pool, &[tomb("fact", f, 1)]).await.unwrap();

        assert_eq!(pairs(&pool, "pair_verdicts", a).await, 0);
        assert_eq!(
            pairs(&pool, "judge_queue", a).await,
            3,
            "the queue is left alone"
        );
        assert_eq!(pairs(&pool, "pair_verdicts", c).await, 1);
    }
}
