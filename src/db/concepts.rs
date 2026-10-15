//! The shared concept vocabulary and the phrase → concept decisions. A phrase
//! is decided once per dimension, for every user who wrote it; a decision with
//! no concepts marks a phrase that takes no part in matching.

use sqlx::PgPool;

/// A phrase in the ledger that has no decision yet.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct Pending {
    pub dimension: String,
    pub phrase: String,
}

/// What the model decided for one phrase; `concepts` are canonical labels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub dimension: String,
    pub phrase: String,
    pub concepts: Vec<String>,
}

/// Undecided array phrases, each once, oldest first. Text-column rows share
/// the ledger but never get concepts.
pub async fn undecided(pool: &PgPool, limit: i64) -> sqlx::Result<Vec<Pending>> {
    sqlx::query_as(
        "SELECT o.dimension, o.phrase FROM pothos.observed_phrases o \
         LEFT JOIN pothos.phrase_decisions d USING (dimension, phrase) \
         WHERE d.phrase IS NULL \
           AND o.dimension IN ('interests', 'personality_traits', 'deal_breakers') \
         GROUP BY o.dimension, o.phrase \
         ORDER BY min(o.first_seen_at), o.dimension, o.phrase \
         LIMIT $1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// Every concept label, sorted.
pub async fn vocabulary(pool: &PgPool) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar("SELECT label FROM pothos.concepts ORDER BY label")
        .fetch_all(pool)
        .await
}

/// Record decisions in one transaction. Returns how many concepts were new.
/// A phrase that already has a decision keeps it untouched.
pub async fn record(pool: &PgPool, model: &str, decisions: &[Decision]) -> sqlx::Result<u64> {
    let mut tx = pool.begin().await?;
    let mut created = 0;
    for d in decisions {
        let landed = sqlx::query(
            "INSERT INTO pothos.phrase_decisions (dimension, phrase, model) VALUES ($1, $2, $3) \
             ON CONFLICT DO NOTHING",
        )
        .bind(&d.dimension)
        .bind(&d.phrase)
        .bind(model)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if landed == 0 {
            continue;
        }
        for c in &d.concepts {
            created += sqlx::query(
                "INSERT INTO pothos.concepts (label) VALUES ($1) ON CONFLICT DO NOTHING",
            )
            .bind(c)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            sqlx::query(
                "INSERT INTO pothos.phrase_concepts (dimension, phrase, concept) VALUES ($1, $2, $3) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(&d.dimension)
            .bind(&d.phrase)
            .bind(c)
            .execute(&mut *tx)
            .await?;
        }
    }
    tx.commit().await?;
    Ok(created)
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::db::candidates::concept_sets;
    use crate::db::mirror::{self, MirrorRow};

    fn at(min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, 12, min, 0).unwrap()
    }

    async fn export(pool: &PgPool, id: Uuid, t: DateTime<Utc>, interests: &[&str], deal: &[&str]) {
        mirror::upsert_one(
            pool,
            &MirrorRow {
                user_id: id,
                interests: interests.iter().map(|s| s.to_string()).collect(),
                personality_traits: vec![],
                deal_breakers: deal.iter().map(|s| s.to_string()).collect(),
                age_min: None,
                age_max: None,
                texts: vec![],
                engine_updated_at: t,
            },
        )
        .await
        .unwrap();
    }

    fn decision(dimension: &str, phrase: &str, concepts: &[&str]) -> Decision {
        Decision {
            dimension: dimension.into(),
            phrase: phrase.into(),
            concepts: concepts.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn pending(dimension: &str, phrase: &str) -> Pending {
        Pending {
            dimension: dimension.into(),
            phrase: phrase.into(),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn undecided_lists_each_phrase_once_oldest_first(pool: PgPool) {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        export(&pool, a, at(10), &["jazz", "tea"], &[]).await;
        export(&pool, b, at(0), &["jazz", "chess"], &["smoking"]).await;
        record(&pool, "m", &[decision("interests", "chess", &["chess"])])
            .await
            .unwrap();

        // jazz and smoking first seen at 0 (ties broken by dimension), tea at 10;
        // chess is decided.
        assert_eq!(
            undecided(&pool, 10).await.unwrap(),
            vec![
                pending("deal_breakers", "smoking"),
                pending("interests", "jazz"),
                pending("interests", "tea"),
            ]
        );
        assert_eq!(
            undecided(&pool, 1).await.unwrap(),
            vec![pending("deal_breakers", "smoking")]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn record_coins_each_concept_once_and_counts_only_new_ones(pool: PgPool) {
        let n = record(
            &pool,
            "m",
            &[
                decision("interests", "listens to old jazz", &["jazz"]),
                decision("interests", "weekend jazz bars", &["jazz", "bars"]),
                decision("interests", "n/a", &[]),
            ],
        )
        .await
        .unwrap();
        assert_eq!(n, 2);
        assert_eq!(vocabulary(&pool).await.unwrap(), vec!["bars", "jazz"]);

        let n = record(
            &pool,
            "m",
            &[decision("personality_traits", "jazz head", &["jazz"])],
        )
        .await
        .unwrap();
        assert_eq!(n, 0);
        let decided: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pothos.phrase_decisions WHERE model = 'm'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(decided, 4, "the empty decision is recorded too");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn matching_sees_concepts_drops_empty_decisions_and_falls_back_to_the_phrase(
        pool: PgPool,
    ) {
        let a = Uuid::new_v4();
        export(
            &pool,
            a,
            at(0),
            &["listens to old jazz", "weekend jazz bars", "n/a", "tea"],
            &[],
        )
        .await;
        record(
            &pool,
            "m",
            &[
                decision("interests", "listens to old jazz", &["jazz"]),
                decision("interests", "weekend jazz bars", &["jazz", "bars"]),
                decision("interests", "n/a", &[]),
            ],
        )
        .await
        .unwrap();
        assert_eq!(
            concept_sets(&pool, a).await.unwrap().interests,
            vec!["bars", "jazz", "tea"]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn one_decision_serves_every_user_who_wrote_the_phrase(pool: PgPool) {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        export(&pool, a, at(0), &["weekend jazz bars"], &[]).await;
        export(&pool, b, at(5), &["weekend jazz bars"], &[]).await;
        assert_eq!(undecided(&pool, 10).await.unwrap().len(), 1);

        record(
            &pool,
            "m",
            &[decision("interests", "weekend jazz bars", &["jazz"])],
        )
        .await
        .unwrap();
        assert!(undecided(&pool, 10).await.unwrap().is_empty());
        for u in [a, b] {
            assert_eq!(
                concept_sets(&pool, u).await.unwrap().interests,
                vec!["jazz"]
            );
        }
    }
    #[sqlx::test(migrations = "./migrations")]
    async fn a_phrase_already_decided_keeps_its_decision_untouched(pool: PgPool) {
        record(
            &pool,
            "first",
            &[decision("personality_traits", "n/a", &[])],
        )
        .await
        .unwrap();
        let n = record(
            &pool,
            "second",
            &[decision("personality_traits", "n/a", &["smoking"])],
        )
        .await
        .unwrap();
        assert_eq!(n, 0, "no concept coined for a decision that did not land");
        assert!(vocabulary(&pool).await.unwrap().is_empty());
        let mapped: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.phrase_concepts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(mapped, 0);
        let model: String =
            sqlx::query_scalar("SELECT model FROM pothos.phrase_decisions WHERE phrase = 'n/a'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(model, "first");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn text_columns_never_reach_the_concept_step_or_matching(pool: PgPool) {
        let a = Uuid::new_v4();
        export(&pool, a, at(0), &["jazz"], &[]).await;
        sqlx::query(
            "INSERT INTO pothos.observed_phrases (user_id, dimension, phrase, first_seen_at, last_seen_at) \
             VALUES ($1, 'love_values', 'wants to be understood', $2, $2)",
        )
        .bind(a)
        .bind(at(0))
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            undecided(&pool, 10).await.unwrap(),
            vec![pending("interests", "jazz")]
        );
        let n: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pothos.user_concepts WHERE user_id = $1")
                .bind(a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(n, 1, "only the interest");
    }
}
