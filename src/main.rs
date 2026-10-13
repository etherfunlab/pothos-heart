//! pothos-heart — an open-source matchmaking engine.
//!
//! Subcommands: `serve` (default) · `migrate` · `print-openapi`.

mod auth;
mod config;
mod db;
mod engine_client;
mod error;
mod openapi;
mod routes;
mod scoring;
mod state;
mod sweeper;

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
        "print-openapi" => {
            run_print_openapi();
            Ok(())
        }
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

fn run_print_openapi() {
    use utoipa::OpenApi;
    let (_router, api) = routes::router_for_openapi().split_for_parts();
    let mut doc = openapi::ApiDoc::openapi();
    doc.merge(api);
    println!(
        "{}",
        serde_json::to_string_pretty(&doc).expect("openapi serialises")
    );
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
    match (&state.config.engine, state.config.sweep_interval) {
        (Some(engine), interval) if interval > std::time::Duration::ZERO => {
            let client = engine_client::EngineClient::new(engine);
            tokio::spawn(sweeper::run_loop(state.pool.clone(), client, interval));
            tracing::info!(every_secs = interval.as_secs(), engine = %engine.base_url, "mirror sweeper on");
        }
        _ => tracing::info!("mirror sweeper off (no ENGINE_* config or SWEEP_DISABLED)"),
    }
    let app = routes::build_app(state);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(addr = %bind_addr, "pothos-heart listening");
    axum::serve(listener, app).await?;
    Ok(())
}
