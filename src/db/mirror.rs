//! `pothos.insight_mirror` — the five matching columns pulled from the engine.
//! Read-only from this service's point of view: rows are replaced wholesale
//! from the export, never edited.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

// consumed by Task 8
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MirrorRow {
    pub user_id: Uuid,
    pub interests: Vec<String>,
    pub personality_traits: Vec<String>,
    pub deal_breakers: Vec<String>,
    pub age_min: Option<i32>,
    pub age_max: Option<i32>,
    pub engine_updated_at: DateTime<Utc>,
}

// consumed by Task 8
#[allow(dead_code)]
pub async fn load(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Option<MirrorRow>> {
    sqlx::query_as::<_, MirrorRow>(
        "SELECT user_id, interests, personality_traits, deal_breakers, age_min, age_max, engine_updated_at \
         FROM pothos.insight_mirror WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

// consumed by Task 10
#[allow(dead_code)]
pub async fn upsert_one(pool: &PgPool, row: &MirrorRow) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO pothos.insight_mirror \
            (user_id, interests, personality_traits, deal_breakers, age_min, age_max, engine_updated_at, synced_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, now()) \
         ON CONFLICT (user_id) DO UPDATE SET \
            interests = EXCLUDED.interests, personality_traits = EXCLUDED.personality_traits, \
            deal_breakers = EXCLUDED.deal_breakers, age_min = EXCLUDED.age_min, age_max = EXCLUDED.age_max, \
            engine_updated_at = EXCLUDED.engine_updated_at, synced_at = now()",
    )
    .bind(row.user_id)
    .bind(&row.interests)
    .bind(&row.personality_traits)
    .bind(&row.deal_breakers)
    .bind(row.age_min)
    .bind(row.age_max)
    .bind(row.engine_updated_at)
    .execute(pool)
    .await?;
    Ok(())
}
