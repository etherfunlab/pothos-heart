//! pothos-heart — an open-source matchmaking engine.
//!
//! Subcommands: `serve` (default) · `migrate` · `print-openapi`.

mod auth;
mod config;
mod db;
mod error;
mod routes;
#[allow(dead_code)] // consumed by Task 8
mod scoring;
mod state;

use std::sync::Arc;

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cmd = std::env::args().nth(1).unwrap_or_else(|| "serve".into());
    let result = match cmd.as_str() {
        "migrate" => run_migrate().await,
        "serve" => run_serve().await,
        other => {
            eprintln!(
                "unknown subcommand {other}; usage: pothos-heart [serve|migrate|print-openapi]"
            );
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        tracing::error!(error = %e, "fatal");
        std::process::exit(1);
    }
}

async fn run_migrate() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL")?;
    db::migrate(&url).await?;
    tracing::info!("migrations applied");
    Ok(())
}

async fn run_serve() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::from_env()?;
    let weights = config::Weights::load(&config.weights_path)?;
    let pool = db::connect(&config.database_url).await?;
    let bind_addr = config.bind_addr.clone();
    let state = state::AppState {
        pool,
        config: Arc::new(config),
        weights: Arc::new(weights),
    };
    let app = routes::build_app(state);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(addr = %bind_addr, "pothos-heart listening");
    axum::serve(listener, app).await?;
    Ok(())
}
