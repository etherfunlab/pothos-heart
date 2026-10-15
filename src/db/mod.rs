//! Postgres access. Everything under the `pothos` schema; nothing here refers
//! to any other schema, so the service runs against the engine's database or
//! its own.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

pub mod autobio;
pub mod candidates;
pub mod concepts;
pub mod memories;
pub mod mirror;
pub mod profiles;
pub mod verdicts;

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
    async fn schema_has_the_v03_tables_views_and_foreign_keys(pool: PgPool) {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename FROM pg_tables WHERE schemaname = 'pothos' ORDER BY tablename",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            tables,
            vec![
                "autobio_entries",
                "autobio_memory_sources",
                "autobio_observed_sources",
                "autobio_state",
                "concepts",
                "insight_mirror",
                "judge_queue",
                "match_profiles",
                "memory_cursor",
                "memory_mirror",
                "observed_phrases",
                "pair_verdicts",
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
        assert_eq!(views, vec!["autobio_versions", "user_concepts"]);

        // (child, parent, on delete, on update)
        let fks: Vec<(String, String, String, String)> = sqlx::query_as(
            "SELECT c.conrelid::regclass::text, c.confrelid::regclass::text, \
                    c.confdeltype::text, c.confupdtype::text \
             FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace \
             WHERE n.nspname = 'pothos' AND c.contype = 'f' ORDER BY 1, 2",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        let fk = |c: &str, p: &str, d: &str, u: &str| {
            (
                format!("pothos.{c}"),
                format!("pothos.{p}"),
                d.to_string(),
                u.to_string(),
            )
        };
        assert_eq!(
            fks,
            vec![
                fk("autobio_entries", "autobio_entries", "n", "a"),
                fk("autobio_memory_sources", "autobio_entries", "c", "a"),
                fk("autobio_memory_sources", "memory_mirror", "c", "a"),
                fk("autobio_observed_sources", "autobio_entries", "c", "a"),
                fk("autobio_observed_sources", "observed_phrases", "c", "a"),
                fk("phrase_concepts", "concepts", "c", "c"),
                fk("phrase_concepts", "phrase_decisions", "c", "a"),
            ]
        );

        // Every FK column is indexed (alone or as an index's first column).
        for (table, column) in [
            ("autobio_entries", "supersedes"),
            ("autobio_memory_sources", "memory_id"),
            ("autobio_observed_sources", "observed_id"),
            ("phrase_concepts", "concept"),
        ] {
            let n: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pg_index i \
                 JOIN pg_class t ON t.oid = i.indrelid \
                 JOIN pg_namespace n ON n.oid = t.relnamespace \
                 JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = i.indkey[0] \
                 WHERE n.nspname = 'pothos' AND t.relname = $1 AND a.attname = $2",
            )
            .bind(table)
            .bind(column)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert!(n >= 1, "{table}.{column} has no leading index");
        }

        for cursor in ["sync_cursor", "memory_cursor"] {
            let rows: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM pothos.{cursor}"))
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(rows, 1, "{cursor}");
        }
    }

    #[sqlx::test(migrations = false)]
    async fn migration_0003_numbers_the_ledger_and_resets_the_insights_cursor(pool: PgPool) {
        for sql in [
            include_str!("../../migrations/0001_init.sql"),
            include_str!("../../migrations/0002_phrase_ledger_and_concepts.sql"),
        ] {
            sqlx::raw_sql(sql).execute(&pool).await.unwrap();
        }
        let a = Uuid::new_v4();
        let t: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        sqlx::query(
            "INSERT INTO pothos.observed_phrases (user_id, dimension, phrase, first_seen_at, last_seen_at) \
             VALUES ($1, 'interests', 'jazz', $2, $2), ($1, 'interests', 'tea', $2, $2)",
        )
        .bind(a)
        .bind(t)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("UPDATE pothos.sync_cursor SET last_updated_at = $1, last_user_id = $2")
            .bind(t)
            .bind(a)
            .execute(&pool)
            .await
            .unwrap();

        sqlx::raw_sql(include_str!("../../migrations/0003_autobio_and_judge.sql"))
            .execute(&pool)
            .await
            .unwrap();

        let ids: Vec<(i64, DateTime<Utc>)> =
            sqlx::query_as("SELECT id, recorded_at FROM pothos.observed_phrases ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(ids.len(), 2);
        assert_ne!(ids[0].0, ids[1].0, "every ledger row gets its own id");
        let cursor: (Option<DateTime<Utc>>, Option<Uuid>) =
            sqlx::query_as("SELECT last_updated_at, last_user_id FROM pothos.sync_cursor")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(cursor, (None, None), "the next sweep re-walks the export");

        let accepted = sqlx::query(
            "INSERT INTO pothos.observed_phrases (user_id, dimension, phrase, first_seen_at, last_seen_at) \
             VALUES ($1, 'love_values', 'x', $2, $2)",
        )
        .bind(a)
        .bind(t)
        .execute(&pool)
        .await;
        assert!(accepted.is_ok(), "text columns are ledger dimensions");
        let rejected = sqlx::query(
            "INSERT INTO pothos.observed_phrases (user_id, dimension, phrase, first_seen_at, last_seen_at) \
             VALUES ($1, 'preferred_gender', 'm', $2, $2)",
        )
        .bind(a)
        .bind(t)
        .execute(&pool)
        .await;
        assert!(
            rejected.is_err(),
            "preferred_gender is not a ledger dimension"
        );
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
