//! Postgres access. Everything under the `pothos` schema; nothing here refers
//! to any other schema, so the service runs against the engine's database or
//! its own.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

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
    sqlx::query("CREATE SCHEMA IF NOT EXISTS pothos")
        .execute(&pool)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    #[sqlx::test(migrations = "./migrations")]
    async fn init_creates_the_three_tables_with_gin_indexes_and_one_cursor_row(pool: PgPool) {
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT tablename FROM pg_tables WHERE schemaname = 'pothos' ORDER BY tablename",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            tables,
            vec!["insight_mirror", "match_profiles", "sync_cursor"]
        );

        let gin: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_indexes WHERE schemaname = 'pothos' \
               AND tablename = 'insight_mirror' AND indexdef LIKE '%USING gin%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(gin, 2, "GIN on interests and personality_traits");

        let fks: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_constraint c JOIN pg_namespace n ON n.oid = c.connamespace \
             WHERE n.nspname = 'pothos' AND c.contype = 'f'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(fks, 0, "spec §3.2: zero foreign keys by design");

        let cursor_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.sync_cursor")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(cursor_rows, 1);
    }
}
