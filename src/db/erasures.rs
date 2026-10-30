//! The engine's erasure stream, honored locally. A tombstone removes
//! everything mirrored from the engine about that user and everything derived
//! from it here (autobio, verdicts, queued pairs). `match_profiles` is the
//! product layer's row and stays; the product layer deletes it through
//! `DELETE /v1/profiles` when the account goes.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::engine_client::{ErasureCursor, ErasureRow};

pub async fn load_cursor(pool: &PgPool) -> sqlx::Result<Option<ErasureCursor>> {
    let row: (Option<DateTime<Utc>>, Option<Uuid>) = sqlx::query_as(
        "SELECT last_erased_at, last_user_id FROM pothos.erasure_cursor WHERE id = 1",
    )
    .fetch_one(pool)
    .await?;
    Ok(match row {
        (Some(erased_at), Some(user_id)) => Some(ErasureCursor { erased_at, user_id }),
        _ => None,
    })
}

/// Apply every tombstone and move the cursor to the last one, in one
/// transaction. Rows must arrive in the engine's `(erased_at, user_id)`
/// order. Idempotent: a repeated or unknown tombstone deletes nothing and
/// still advances the cursor. An empty slice is a no-op.
pub async fn apply_page(pool: &PgPool, rows: &[ErasureRow]) -> sqlx::Result<()> {
    let Some(last) = rows.last() else {
        return Ok(());
    };
    let mut tx = pool.begin().await?;
    for r in rows {
        for sql in [
            "DELETE FROM pothos.insight_mirror WHERE user_id = $1",
            "DELETE FROM pothos.observed_phrases WHERE user_id = $1",
            "DELETE FROM pothos.memory_mirror WHERE user_id = $1",
            "DELETE FROM pothos.fact_mirror WHERE user_id = $1",
            "DELETE FROM pothos.autobio_entries WHERE user_id = $1",
            "DELETE FROM pothos.autobio_state WHERE user_id = $1",
            "DELETE FROM pothos.pair_verdicts WHERE user_lo = $1 OR user_hi = $1",
            "DELETE FROM pothos.judge_queue WHERE user_lo = $1 OR user_hi = $1",
        ] {
            sqlx::query(sql).bind(r.user_id).execute(&mut *tx).await?;
        }
    }
    sqlx::query(
        "UPDATE pothos.erasure_cursor SET last_erased_at = $1, last_user_id = $2, synced_at = now() WHERE id = 1",
    )
    .bind(last.erased_at)
    .bind(last.user_id)
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
    use crate::db::profiles::{self, ProfileWrite};

    async fn seed_user(pool: &PgPool) -> Uuid {
        let u = Uuid::new_v4();
        profiles::upsert(
            pool,
            u,
            &ProfileWrite {
                gender: "f".into(),
                seeking_genders: vec!["m".into()],
                birth_date: None,
                country: None,
                region: None,
                city: None,
                age_min: None,
                age_max: None,
                active: true,
            },
        )
        .await
        .unwrap();
        crate::db::mirror::upsert_one(
            pool,
            &crate::db::mirror::MirrorRow {
                user_id: u,
                interests: vec!["jazz".into()],
                personality_traits: vec![],
                deal_breakers: vec![],
                engine_updated_at: Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap(),
            },
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pothos.memory_mirror (id, user_id, content, category, engine_created_at) \
             VALUES ($1, $2, 'c', 'fact', now())",
        )
        .bind(Uuid::new_v4())
        .bind(u)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO pothos.fact_mirror \
                (id, user_id, content, category, domain, evidence_type, temporality, \
                 persistence, confidence, occurrences, session_count, recurrence, \
                 first_seen_at, last_seen_at) \
             VALUES ($1, $2, 'c', 'fact', 'other', 'explicit_statement', 'current', \
                     'stable', 'high', 1, 1, 'once', now(), now())",
        )
        .bind(Uuid::new_v4())
        .bind(u)
        .execute(pool)
        .await
        .unwrap();
        for retired in ["NULL", "now()"] {
            sqlx::query(&format!(
                "INSERT INTO pothos.autobio_entries (user_id, section, status, statement, retired_at, retired_reason) \
                 VALUES ($1, 'life', 'current', 'listens to jazz', {retired}, {})",
                if retired == "NULL" { "NULL" } else { "'vetoed'" }
            ))
            .bind(u)
            .execute(pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO pothos.autobio_state (user_id, ran_at, model) VALUES ($1, now(), 'm')",
        )
        .bind(u)
        .execute(pool)
        .await
        .unwrap();
        let other = Uuid::new_v4();
        let (lo, hi) = if u < other { (u, other) } else { (other, u) };
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
        u
    }

    async fn count(pool: &PgPool, table: &str, type_: &str, user: Uuid) -> i64 {
        let sql = match type_ {
            "user" => format!("SELECT count(*) FROM pothos.{table} WHERE user_id = $1"),
            _ => format!(
                "SELECT count(*) FROM pothos.{table} WHERE user_lo = $1 OR user_hi = $1"
            ),
        };
        sqlx::query_scalar(&sql).bind(user).fetch_one(pool).await.unwrap()
    }

    fn tombstone(user_id: Uuid) -> ErasureRow {
        ErasureRow {
            user_id,
            erased_at: Utc.with_ymd_and_hms(2026, 10, 16, 9, 0, 0).unwrap(),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_tombstone_erases_mirrors_and_derived_data_but_keeps_the_profile(pool: PgPool) {
        let u = seed_user(&pool).await;

        apply_page(&pool, &[tombstone(u)]).await.unwrap();

        for table in [
            "insight_mirror",
            "observed_phrases",
            "memory_mirror",
            "fact_mirror",
            "autobio_entries",
            "autobio_state",
        ] {
            assert_eq!(count(&pool, table, "user", u).await, 0, "{table}");
        }
        for table in ["pair_verdicts", "judge_queue"] {
            assert_eq!(count(&pool, table, "pair", u).await, 0, "{table}");
        }
        assert_eq!(
            count(&pool, "match_profiles", "user", u).await,
            1,
            "the product layer's row stays"
        );
        let cur = load_cursor(&pool).await.unwrap().unwrap();
        assert_eq!((cur.erased_at, cur.user_id), (tombstone(u).erased_at, u));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_repeated_or_unknown_tombstone_is_a_noop_that_advances_the_cursor(pool: PgPool) {
        let u = seed_user(&pool).await;
        apply_page(&pool, &[tombstone(u)]).await.unwrap();

        // The same user again, re-stamped later, plus a user never seen here.
        let later = ErasureRow {
            user_id: u,
            erased_at: Utc.with_ymd_and_hms(2026, 10, 17, 9, 0, 0).unwrap(),
        };
        let unknown = ErasureRow {
            user_id: Uuid::new_v4(),
            erased_at: Utc.with_ymd_and_hms(2026, 10, 17, 9, 5, 0).unwrap(),
        };
        apply_page(&pool, &[later, unknown]).await.unwrap();

        let cur = load_cursor(&pool).await.unwrap().unwrap();
        assert_eq!(
            (cur.erased_at, cur.user_id),
            (unknown.erased_at, unknown.user_id)
        );
        assert_eq!(count(&pool, "match_profiles", "user", u).await, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn erasures_page_and_cursor_are_one_transaction(pool: PgPool) {
        let u = seed_user(&pool).await;
        sqlx::query(
            "ALTER TABLE pothos.erasure_cursor ADD CONSTRAINT no_cursor CHECK (last_user_id IS NULL)",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert!(apply_page(&pool, &[tombstone(u)]).await.is_err());
        assert_eq!(
            count(&pool, "insight_mirror", "user", u).await,
            1,
            "deletes rolled back with the failed cursor update"
        );
        assert!(load_cursor(&pool).await.unwrap().is_none());
    }
}
