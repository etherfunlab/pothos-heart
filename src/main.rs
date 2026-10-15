//! pothos-heart — an open-source matchmaking engine.
//!
//! Subcommands: `serve` (default) · `migrate` · `print-openapi`.

mod auth;
mod autobio;
mod concepts;
mod config;
mod db;
mod engine_client;
mod error;
mod judge;
mod llm;
mod openapi;
mod routes;
mod scoring;
mod state;
mod sweeper;

use std::sync::Arc;

/// Autobio and judge replies can be long; the concept step keeps the 60 s default.
const LONG_CALL: std::time::Duration = std::time::Duration::from_secs(300);

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();
    // Logs go to stderr so `print-openapi` keeps stdout clean.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
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
    let label_language = config::load_label_language(&config.weights_path)?;
    let autobio_language = config::load_autobio_language(&config.weights_path)?;
    let pool = db::connect(&config.database_url).await?;
    let bind_addr = config.bind_addr.clone();
    let state = state::AppState {
        pool,
        config: Arc::new(config),
        weights: Arc::new(weights),
    };
    let concept_task = state.config.llm.as_ref().map(|llm| concepts::ConceptTask {
        client: llm::LlmClient::new(llm),
        label_language,
        max_per_tick: state.config.concept_max_per_tick,
    });
    // A per-tick cap of 0 turns that step off.
    let autobio_task = state
        .config
        .llm
        .as_ref()
        .filter(|_| state.config.autobio_max_per_tick > 0)
        .map(|llm| autobio::AutobioTask {
            client: llm::LlmClient::with_timeout(
                &llm.with_model(state.config.autobio_model.as_deref()),
                LONG_CALL,
            ),
            language: autobio_language.clone(),
            max_per_tick: state.config.autobio_max_per_tick,
            min_interval: chrono::Duration::from_std(state.config.autobio_min_interval)
                .expect("interval fits"),
        });
    let judge_task = state
        .config
        .llm
        .as_ref()
        .filter(|_| state.config.judge_max_per_tick > 0)
        .map(|llm| judge::JudgeTask {
            client: llm::LlmClient::with_timeout(
                &llm.with_model(state.config.judge_model.as_deref()),
                LONG_CALL,
            ),
            language: autobio_language,
            max_per_tick: state.config.judge_max_per_tick,
        });
    match (&state.config.engine, state.config.sweep_interval) {
        (Some(engine), interval) if interval > std::time::Duration::ZERO => {
            let client = engine_client::EngineClient::new(engine);
            let models_on = concept_task.is_some();
            tokio::spawn(sweeper::run_loop(
                state.pool.clone(),
                client,
                concept_task,
                autobio_task,
                judge_task,
                interval,
            ));
            tracing::info!(every_secs = interval.as_secs(), engine = %engine.base_url, models = models_on, "mirror sweeper on");
        }
        _ => tracing::info!(
            "mirror sweeper off (no ENGINE_* config or SWEEP_DISABLED); the model steps run only with it"
        ),
    }
    let app = routes::build_app(state);
    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(addr = %bind_addr, "pothos-heart listening");
    axum::serve(listener, app).await?;
    Ok(())
}
