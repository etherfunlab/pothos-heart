//! `pothos.insight_mirror` — the five matching columns pulled from the engine.
//! Read-only from this service's point of view: rows are replaced wholesale
//! from the export, never edited.

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::engine_client::Cursor;

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

pub async fn load(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Option<MirrorRow>> {
    sqlx::query_as::<_, MirrorRow>(
        "SELECT user_id, interests, personality_traits, deal_breakers, age_min, age_max, engine_updated_at \
         FROM pothos.insight_mirror WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

/// The one upsert statement; `upsert_one` and `apply_page` both run it.
async fn upsert_row<'e>(exec: impl sqlx::PgExecutor<'e>, row: &MirrorRow) -> sqlx::Result<()> {
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
    .execute(exec)
    .await?;
    Ok(())
}

/// Single-row form of the upsert, for test fixtures.
#[cfg(test)]
pub async fn upsert_one(pool: &PgPool, row: &MirrorRow) -> sqlx::Result<()> {
    upsert_row(pool, row).await
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
        upsert_row(&mut *tx, r).await?;
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
