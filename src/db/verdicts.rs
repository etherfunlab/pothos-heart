//! The judge's verdicts, cached per pair and per autobio version, and the
//! queue of pairs waiting for one.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use sqlx::PgPool;
use utoipa::ToSchema;
use uuid::Uuid;

/// The stored key of an unordered pair: lower id first. Postgres orders
/// `uuid` bytewise, as `Uuid`'s `Ord` does, so this matches `user_lo < user_hi`.
#[cfg(test)]
pub(crate) fn pair(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
}

/// The judge's grade for a pair: a few absolute levels, never a number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum Grade {
    /// Concrete, complementary fit and no visible conflict.
    Strong,
    /// More fits than conflicts.
    Promising,
    /// Not enough to judge, or good and bad balance out.
    Unclear,
    /// A clear conflict.
    Poor,
}

impl Grade {
    pub const ALL: [Grade; 4] = [Grade::Strong, Grade::Promising, Grade::Unclear, Grade::Poor];

    pub fn as_str(self) -> &'static str {
        match self {
            Grade::Strong => "strong",
            Grade::Promising => "promising",
            Grade::Unclear => "unclear",
            Grade::Poor => "poor",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|g| g.as_str() == s)
    }

    /// Sort key for the candidate list. An unjudged pair ranks with
    /// `unclear`; `poor` goes last but stays in the list.
    pub fn rank(g: Option<Self>) -> u8 {
        match g {
            Some(Grade::Strong) => 0,
            Some(Grade::Promising) => 1,
            None | Some(Grade::Unclear) => 2,
            Some(Grade::Poor) => 3,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub grade: Grade,
    /// Both autobios are still at the versions the judge read.
    pub fresh: bool,
}

/// The verdicts between `me` and each of `others`, keyed by the other user.
pub async fn for_candidates(
    pool: &PgPool,
    me: Uuid,
    others: &[Uuid],
) -> sqlx::Result<HashMap<Uuid, Verdict>> {
    let rows: Vec<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT CASE WHEN p.user_lo = $1 THEN p.user_hi ELSE p.user_lo END, p.grade, \
                COALESCE(p.lo_version = vl.version AND p.hi_version = vh.version, false) \
         FROM pothos.pair_verdicts p \
         LEFT JOIN pothos.autobio_versions vl ON vl.user_id = p.user_lo \
         LEFT JOIN pothos.autobio_versions vh ON vh.user_id = p.user_hi \
         WHERE (p.user_lo = $1 AND p.user_hi = ANY($2)) \
            OR (p.user_hi = $1 AND p.user_lo = ANY($2))",
    )
    .bind(me)
    .bind(others)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|(other, grade, fresh)| {
            Grade::parse(&grade).map(|grade| (other, Verdict { grade, fresh }))
        })
        .collect())
}

/// Queue (me, other) pairs for the judge. A pair already queued keeps its
/// place; `me` itself is skipped. Only users in the pool are queued: the
/// profile rows of `me` and `others` are share-locked first, as `store` does,
/// so a concurrent leave either waits for this transaction and then clears the
/// rows, or has committed and is seen as gone. If `me` has no profile row,
/// nothing is queued; an other user without one is left out.
pub async fn enqueue(pool: &PgPool, me: Uuid, others: &[Uuid]) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    let mut ids = others.to_vec();
    ids.push(me);
    let present: Vec<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM pothos.match_profiles WHERE user_id = ANY($1) FOR SHARE",
    )
    .bind(ids)
    .fetch_all(&mut *tx)
    .await?;
    if present.contains(&me) {
        let others: Vec<Uuid> = others
            .iter()
            .copied()
            .filter(|o| present.contains(o))
            .collect();
        sqlx::query(
            "INSERT INTO pothos.judge_queue (user_lo, user_hi) \
             SELECT LEAST($1, o), GREATEST($1, o) FROM unnest($2::uuid[]) AS o WHERE o <> $1 \
             ON CONFLICT DO NOTHING",
        )
        .bind(me)
        .bind(others)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await
}

/// The oldest queued pairs; they stay queued until stored or dequeued.
pub async fn take(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<(Uuid, Uuid)>> {
    sqlx::query_as(
        "SELECT user_lo, user_hi FROM pothos.judge_queue \
         ORDER BY requested_at, user_lo, user_hi LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

pub async fn dequeue(pool: &PgPool, lo: Uuid, hi: Uuid) -> sqlx::Result<()> {
    sqlx::query("DELETE FROM pothos.judge_queue WHERE user_lo = $1 AND user_hi = $2")
        .bind(lo)
        .bind(hi)
        .execute(pool)
        .await?;
    Ok(())
}

/// Move a queued pair to the back, so a pair whose call keeps failing does
/// not head the queue every tick.
pub async fn requeue(pool: &PgPool, lo: Uuid, hi: Uuid) -> sqlx::Result<()> {
    sqlx::query(
        "UPDATE pothos.judge_queue SET requested_at = now() WHERE user_lo = $1 AND user_hi = $2",
    )
    .bind(lo)
    .bind(hi)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn is_fresh(
    pool: &PgPool,
    lo: Uuid,
    hi: Uuid,
    lo_version: DateTime<Utc>,
    hi_version: DateTime<Utc>,
) -> sqlx::Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM pothos.pair_verdicts \
         WHERE user_lo = $1 AND user_hi = $2 AND lo_version = $3 AND hi_version = $4)",
    )
    .bind(lo)
    .bind(hi)
    .bind(lo_version)
    .bind(hi_version)
    .fetch_one(pool)
    .await
}

/// Record a verdict with the versions it read and drop the pair from the
/// queue, in one transaction. Both users' profile rows are share-locked
/// first, as `autobio::apply` does, so a concurrent leave waits. If either
/// user has left the pool, no verdict is written (its rationale quotes both
/// autobios), the queue row is dropped, and this returns `false`.
#[allow(clippy::too_many_arguments)] // one row's columns
pub async fn store(
    pool: &PgPool,
    lo: Uuid,
    hi: Uuid,
    lo_version: DateTime<Utc>,
    hi_version: DateTime<Utc>,
    grade: Grade,
    rationale: &str,
    model: &str,
) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let present: Vec<Uuid> = sqlx::query_scalar(
        "SELECT user_id FROM pothos.match_profiles WHERE user_id = ANY($1) FOR SHARE",
    )
    .bind(vec![lo, hi])
    .fetch_all(&mut *tx)
    .await?;
    if present.len() < 2 {
        sqlx::query("DELETE FROM pothos.judge_queue WHERE user_lo = $1 AND user_hi = $2")
            .bind(lo)
            .bind(hi)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO pothos.pair_verdicts \
            (user_lo, user_hi, lo_version, hi_version, grade, rationale, model) \
         VALUES ($1, $2, $3, $4, $5, $6, $7) \
         ON CONFLICT (user_lo, user_hi) DO UPDATE SET \
            lo_version = EXCLUDED.lo_version, hi_version = EXCLUDED.hi_version, \
            grade = EXCLUDED.grade, rationale = EXCLUDED.rationale, \
            model = EXCLUDED.model, judged_at = now()",
    )
    .bind(lo)
    .bind(hi)
    .bind(lo_version)
    .bind(hi_version)
    .bind(grade.as_str())
    .bind(rationale)
    .bind(model)
    .execute(&mut *tx)
    .await?;
    sqlx::query("DELETE FROM pothos.judge_queue WHERE user_lo = $1 AND user_hi = $2")
        .bind(lo)
        .bind(hi)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::db::autobio;
    use crate::db::autobio::tests::{entry, join_pool};

    #[test]
    fn grades_parse_and_rank_with_unjudged_beside_unclear() {
        for g in Grade::ALL {
            assert_eq!(Grade::parse(g.as_str()), Some(g));
        }
        assert_eq!(Grade::parse("Strong"), None);
        let order: Vec<u8> = [
            Some(Grade::Strong),
            Some(Grade::Promising),
            None,
            Some(Grade::Unclear),
            Some(Grade::Poor),
        ]
        .into_iter()
        .map(Grade::rank)
        .collect();
        assert_eq!(order, vec![0, 1, 2, 2, 3]);
        assert_eq!(serde_json::to_value(Grade::Promising).unwrap(), "promising");
    }

    #[test]
    fn pair_orders_both_ways_the_same() {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(pair(a, b), pair(b, a));
        let (lo, hi) = pair(a, b);
        assert!(lo < hi);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_verdict_is_fresh_until_either_autobio_changes(pool: PgPool) {
        let (me, him, other) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for id in [me, him] {
            join_pool(&pool, id, true).await;
            entry(&pool, id, "life", "x").await;
        }
        let v = autobio::versions(&pool, &[me, him]).await.unwrap();
        let (lo, hi) = pair(me, him);
        assert!(
            store(&pool, lo, hi, v[&lo], v[&hi], Grade::Strong, "r", "m")
                .await
                .unwrap()
        );

        let got = for_candidates(&pool, me, &[him, other]).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[&him],
            Verdict {
                grade: Grade::Strong,
                fresh: true
            }
        );
        assert!(is_fresh(&pool, lo, hi, v[&lo], v[&hi]).await.unwrap());
        // Seen from the other side, the same verdict.
        assert_eq!(
            for_candidates(&pool, him, &[me]).await.unwrap()[&me].grade,
            Grade::Strong
        );

        entry(&pool, him, "life", "y").await;
        assert!(!for_candidates(&pool, me, &[him]).await.unwrap()[&him].fresh);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_queue_dedupes_pairs_and_store_clears_its_row(pool: PgPool) {
        let (me, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        for id in [me, a, b] {
            join_pool(&pool, id, true).await;
        }
        enqueue(&pool, me, &[a, b, me]).await.unwrap();
        enqueue(&pool, a, &[me]).await.unwrap();
        let queued = take(&pool, 10).await.unwrap();
        assert_eq!(queued.len(), 2, "self skipped, (a, me) is (me, a)");
        assert!(queued.contains(&pair(me, a)));
        assert_eq!(take(&pool, 1).await.unwrap().len(), 1);

        let (lo, hi) = pair(me, a);
        let t = autobio::db_now(&pool).await.unwrap();
        assert!(store(&pool, lo, hi, t, t, Grade::Poor, "r", "m")
            .await
            .unwrap());
        assert!(store(&pool, lo, hi, t, t, Grade::Unclear, "r2", "m2")
            .await
            .unwrap());
        assert_eq!(take(&pool, 10).await.unwrap(), vec![pair(me, b)]);
        let (grade, rationale): (String, String) = sqlx::query_as(
            "SELECT grade, rationale FROM pothos.pair_verdicts WHERE user_lo = $1 AND user_hi = $2",
        )
        .bind(lo)
        .bind(hi)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((grade.as_str(), rationale.as_str()), ("unclear", "r2"));

        let (lo, hi) = pair(me, b);
        dequeue(&pool, lo, hi).await.unwrap();
        assert!(take(&pool, 10).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enqueue_names_only_users_who_are_in_the_pool(pool: PgPool) {
        let (me, inside, outside) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        join_pool(&pool, me, true).await;
        join_pool(&pool, inside, true).await;
        enqueue(&pool, me, &[inside, outside]).await.unwrap();
        assert_eq!(take(&pool, 10).await.unwrap(), vec![pair(me, inside)]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn enqueue_from_a_user_outside_the_pool_queues_nothing(pool: PgPool) {
        let (me, inside) = (Uuid::new_v4(), Uuid::new_v4());
        join_pool(&pool, inside, true).await;
        enqueue(&pool, me, &[inside]).await.unwrap();
        assert!(take(&pool, 10).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_pair_with_a_departed_user_is_dequeued_without_a_verdict(pool: PgPool) {
        let (me, gone) = (Uuid::new_v4(), Uuid::new_v4());
        join_pool(&pool, me, true).await;
        join_pool(&pool, gone, true).await;
        entry(&pool, me, "life", "x").await;
        entry(&pool, gone, "life", "y").await;
        enqueue(&pool, me, &[gone]).await.unwrap();
        sqlx::query("DELETE FROM pothos.match_profiles WHERE user_id = $1")
            .bind(gone)
            .execute(&pool)
            .await
            .unwrap();
        let (lo, hi) = pair(me, gone);
        let t = autobio::db_now(&pool).await.unwrap();

        assert!(!store(&pool, lo, hi, t, t, Grade::Strong, "r", "m")
            .await
            .unwrap());
        let verdicts: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.pair_verdicts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(verdicts, 0, "no verdict naming a departed user");
        assert!(take(&pool, 10).await.unwrap().is_empty(), "queue row gone");
    }
}
