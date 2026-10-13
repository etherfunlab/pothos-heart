use std::sync::Arc;

use sqlx::PgPool;

use crate::config::{Config, Weights};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
    // read by scoring in Task 8
    #[allow(dead_code)]
    pub weights: Arc<Weights>,
}

#[cfg(test)]
pub(crate) const TEST_SECRET: &str = "0123456789abcdef0123456789abcdef";

#[cfg(test)]
pub(crate) fn test_state(pool: PgPool) -> AppState {
    AppState {
        pool,
        config: Arc::new(Config {
            database_url: String::new(),
            bind_addr: "127.0.0.1:0".into(),
            caller_secret: TEST_SECRET.into(),
            engine: None,
            sweep_interval: std::time::Duration::ZERO,
            weights_path: "pothos.toml".into(),
        }),
        weights: Arc::new(Weights::default()),
    }
}
