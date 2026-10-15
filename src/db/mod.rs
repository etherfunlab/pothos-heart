//! Postgres access. Everything under the `pothos` schema; nothing here refers
//! to any other schema, so the service runs against the engine's database or
//! its own.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

pub mod candidates;
#[allow(dead_code)] // wired in Task 4
pub mod concepts;
pub mod mirror;
pub mod profiles;

pub async fn connect(url: &str) -> Result<PgPool, sqlx::Error> {
    PgPoolOptions::new().max_connections(8).connect(url).await
}

/// Run embedded migrations, tracking them in `pothos._sqlx_migrations`.
///
/// The service may share a database with another service that already owns
/// `public._sqlx_migrations`. The migrator creates its bookkeeping table in the
/// first schema of the connection's `search_path`, so a one-connection pool
/// with `pothos` first keeps ours separate. The migration SQL itself is
/// schema-qualified, so the serve path needs no `search_path`.
pub async fn migrate(url: &str) -> Result<(), Box<dyn std::error::Error>> {
    use sqlx::postgres::PgConnectOptions;
    use std::str::FromStr;

    let opts = PgConnectOptions::from_str(url)?.options([("search_path", "pothos,public")]);
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await?;
    // Load-bearing: the migrator creates its bookkeeping table before migration
    // 0001 runs. Without the `pothos` schema, Postgres skips the missing
    // `search_path` entry and the table lands in `public`.
    sqlx::query("CREATE SCHEMA IF NOT EXISTS pothos")
        .execute(&pool)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;

    #[sqlx::test(migrations = "./migrations")]
    async fn schema_has_the_ledger_the_vocabulary_and_two_cascading_fks(pool: PgPool) {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename FROM pg_tables WHERE schemaname = 'pothos' ORDER BY tablename",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            tables,
            vec![
                "concepts",
                "insight_mirror",
                "match_profiles",
                "observed_phrases",
                "phrase_concepts",
                "phrase_decisions",
                "sync_cursor"
            ]
        );

        let views: Vec<String> = sqlx::query_scalar(
            "SELECT viewname FROM pg_views WHERE schemaname = 'pothos' ORDER BY viewname",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(views, vec!["user_concepts"]);

        let gin: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_indexes WHERE schemaname = 'pothos' \
               AND tablename = 'insight_mirror' AND indexdef LIKE '%USING gin%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(gin, 0, "the arrays and their GIN indexes left the mirror");

        // (child, parent, on delete, on update)
        let fks: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT c.conrelid::regclass::text, c.confrelid::regclass::text, \
                    c.confdeltype::text, c.confupdtype::text \
             FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace \
             WHERE n.nspname = 'pothos' AND c.contype = 'f' ORDER BY 2",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let fk = |p: &str, d: &str, u: &str| {
            (
                "pothos.phrase_concepts".to_string(),
                p.to_string(),
                d.to_string(),
                u.to_string(),
            )
        };
        assert_eq!(
            fks,
            vec![
                fk("pothos.concepts", "c", "c"),
                fk("pothos.phrase_decisions", "c", "a")
            ]
        );

        let concept_fk_index: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_indexes WHERE schemaname = 'pothos' \
               AND indexname = 'idx_phrase_concepts_concept'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(concept_fk_index, 1);

        let cursor_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.sync_cursor")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(cursor_rows, 1);
    }

    #[sqlx::test(migrations = false)]
    async fn migration_0002_moves_mirror_arrays_into_the_ledger(pool: PgPool) {
        sqlx::raw_sql(include_str!("../../migrations/0001_init.sql"))
            .execute(&pool)
            .await
            .unwrap();
        let a = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO pothos.insight_mirror \
                (user_id, interests, personality_traits, deal_breakers, age_min, engine_updated_at) \
             VALUES ($1, '{jazz,tea}', '{calm}', '{smoking}', 25, '2026-10-13T12:00:00Z')",
        )
        .bind(a)
        .execute(&pool)
        .await
        .unwrap();

        sqlx::raw_sql(include_str!(
            "../../migrations/0002_phrase_ledger_and_concepts.sql"
        ))
        .execute(&pool)
        .await
        .unwrap();

        let t: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        let rows: Vec<(String, String, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
            "SELECT dimension, phrase, first_seen_at, last_seen_at \
             FROM pothos.observed_phrases WHERE user_id = $1 ORDER BY dimension, phrase",
        )
        .bind(a)
        .fetch_all(&pool)
        .await
        .unwrap();
        let r = |d: &str, p: &str| (d.to_string(), p.to_string(), t, t);
        assert_eq!(
            rows,
            vec![
                r("deal_breakers", "smoking"),
                r("interests", "jazz"),
                r("interests", "tea"),
                r("personality_traits", "calm")
            ]
        );

        let cols: Vec<String> = sqlx::query_scalar(
            "SELECT column_name::text FROM information_schema.columns \
             WHERE table_schema = 'pothos' AND table_name = 'insight_mirror' ORDER BY column_name",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            cols,
            vec![
                "age_max",
                "age_min",
                "engine_updated_at",
                "synced_at",
                "user_id"
            ]
        );
        let age: Option<i32> =
            sqlx::query_scalar("SELECT age_min FROM pothos.insight_mirror WHERE user_id = $1")
                .bind(a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(age, Some(25), "scalars survive the migration");
    }
}
