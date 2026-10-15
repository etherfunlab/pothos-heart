//! `pothos.memory_mirror` — the engine's extracted memories, as exported.
//! Rows never change there, so a re-fetched row is left as it is (its
//! `recorded_at` is when this service first stored it).

use chrono::{DateTime, Utc};
use sqlx::PgPool;
use uuid::Uuid;

use crate::engine_client::{MemoryCursor, MemoryRow};

pub async fn load_cursor(pool: &PgPool) -> sqlx::Result<Option<MemoryCursor>> {
    let row: (Option<DateTime<Utc>>, Option<Uuid>) =
        sqlx::query_as("SELECT last_created_at, last_id FROM pothos.memory_cursor WHERE id = 1")
            .fetch_one(pool)
            .await?;
    Ok(match row {
        (Some(created_at), Some(id)) => Some(MemoryCursor { created_at, id }),
        _ => None,
    })
}

/// Store every row and move the cursor to the last one, in one transaction.
/// Rows must arrive in the engine's `(created_at, id)` order. An empty slice
/// is a no-op.
pub async fn apply_page(pool: &PgPool, rows: &[MemoryRow]) -> sqlx::Result<()> {
    let Some(last) = rows.last() else {
        return Ok(());
    };
    let mut tx = pool.begin().await?;
    for r in rows {
        sqlx::query(
            "INSERT INTO pothos.memory_mirror (id, user_id, content, category, metadata, engine_created_at) \
             VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
        )
        .bind(r.id)
        .bind(r.user_id)
        .bind(&r.content)
        .bind(&r.category)
        .bind(&r.metadata)
        .bind(r.created_at)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE pothos.memory_cursor SET last_created_at = $1, last_id = $2, synced_at = now() WHERE id = 1",
    )
    .bind(last.created_at)
    .bind(last.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await
}
