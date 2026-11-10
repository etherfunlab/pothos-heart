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
             SELECT $1, $2, $3, $4, $5, $6 \
             WHERE NOT EXISTS (SELECT 1 FROM pothos.row_tombstones WHERE kind = 'memory' AND id = $1) \
             ON CONFLICT (id) DO NOTHING",
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

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;

    #[sqlx::test(migrations = "./migrations")]
    async fn a_tombstoned_id_is_not_mirrored(pool: PgPool) {
        let (gone, live, u) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let at = |min| Utc.with_ymd_and_hms(2026, 11, 10, 12, min, 0).unwrap();
        // A fact tombstone with the live memory's id is another stream's row.
        for (kind, id) in [("memory", gone), ("fact", live)] {
            sqlx::query(
                "INSERT INTO pothos.row_tombstones (kind, id, deleted_at) VALUES ($1, $2, now())",
            )
            .bind(kind)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
        }
        let row = |id, min| MemoryRow {
            id,
            user_id: u,
            content: "likes jazz".into(),
            category: "preference".into(),
            metadata: None,
            created_at: at(min),
        };

        apply_page(&pool, &[row(gone, 0), row(live, 1)])
            .await
            .unwrap();

        let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM pothos.memory_mirror")
            .fetch_all(&pool)
            .await
            .unwrap();
        assert_eq!(ids, vec![live]);
        let cur = load_cursor(&pool).await.unwrap().unwrap();
        assert_eq!((cur.created_at, cur.id), (at(1), live));
    }
}
