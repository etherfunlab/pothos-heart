//! `pothos.match_profiles` — hard attributes pushed by the product layer.

use chrono::{DateTime, NaiveDate, Utc};
use sqlx::PgPool;
use uuid::Uuid;

// Whole-row read model: the match handler reads only the geo columns and `active`.
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct MatchProfile {
    pub user_id: Uuid,
    pub gender: String,
    pub seeking_genders: Vec<String>,
    pub birth_date: Option<NaiveDate>,
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub active: bool,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct ProfileWrite {
    pub gender: String,
    pub seeking_genders: Vec<String>,
    pub birth_date: Option<NaiveDate>,
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub active: bool,
}

/// Whole-row replace. PUT semantics: a field absent from the body becomes NULL.
pub async fn upsert(pool: &PgPool, user_id: Uuid, w: &ProfileWrite) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO pothos.match_profiles \
            (user_id, gender, seeking_genders, birth_date, country, region, city, active, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, now()) \
         ON CONFLICT (user_id) DO UPDATE SET \
            gender = EXCLUDED.gender, seeking_genders = EXCLUDED.seeking_genders, \
            birth_date = EXCLUDED.birth_date, country = EXCLUDED.country, \
            region = EXCLUDED.region, city = EXCLUDED.city, active = EXCLUDED.active, \
            updated_at = now()",
    )
    .bind(user_id)
    .bind(&w.gender)
    .bind(&w.seeking_genders)
    .bind(w.birth_date)
    .bind(&w.country)
    .bind(&w.region)
    .bind(&w.city)
    .bind(w.active)
    .execute(pool)
    .await?;
    Ok(())
}

/// Leave the pool. The user's autobio, its state, and every verdict or queued
/// pair naming them are deleted in the same transaction, whether or not a
/// profile row was there, so a repeated DELETE still clears leftovers. The
/// mirrors stay. `true` if a profile row was removed.
pub async fn delete(pool: &PgPool, user_id: Uuid) -> sqlx::Result<bool> {
    let mut tx = pool.begin().await?;
    let n = sqlx::query("DELETE FROM pothos.match_profiles WHERE user_id = $1")
        .bind(user_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    for sql in [
        "DELETE FROM pothos.autobio_entries WHERE user_id = $1",
        "DELETE FROM pothos.autobio_state WHERE user_id = $1",
        "DELETE FROM pothos.pair_verdicts WHERE user_lo = $1 OR user_hi = $1",
        "DELETE FROM pothos.judge_queue WHERE user_lo = $1 OR user_hi = $1",
    ] {
        sqlx::query(sql).bind(user_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(n > 0)
}

pub async fn load(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Option<MatchProfile>> {
    sqlx::query_as::<_, MatchProfile>(
        "SELECT user_id, gender, seeking_genders, birth_date, country, region, city, active, updated_at \
         FROM pothos.match_profiles WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_optional(pool)
    .await
}
