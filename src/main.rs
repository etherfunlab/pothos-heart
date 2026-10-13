//! pothos-heart — an open-source matchmaking engine.
//!
//! Subcommands: `serve` (default) · `migrate` · `print-openapi`.

#[allow(dead_code)]
mod config;
#[allow(dead_code)]
mod db;
#[allow(dead_code)]
mod routes;

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
        "serve" => {
            eprintln!("serve: not wired yet");
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

async fn run_migrate() -> Result<(), Box<dyn std::error::Error>> {
    let url = std::env::var("DATABASE_URL")?;
    db::migrate(&url).await?;
    tracing::info!("migrations applied");
    Ok(())
}
