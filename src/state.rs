use std::sync::Arc;

use sqlx::PgPool;

use crate::config::{Config, Weights};

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub config: Arc<Config>,
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
            llm: None,
            concept_max_per_tick: 200,
            autobio_max_per_tick: 50,
            autobio_min_interval: std::time::Duration::from_secs(3600),
            autobio_model: None,
            judge_max_per_tick: 100,
            judge_top_k: 10,
            judge_model: None,
        }),
        weights: Arc::new(Weights::default()),
    }
}

/// `test_state` with a model configured, so the candidates endpoint queues
/// pairs for the judge. Nothing calls the model in route tests.
#[cfg(test)]
pub(crate) fn test_state_with_llm(pool: PgPool) -> AppState {
    let mut s = test_state(pool);
    let mut c = (*s.config).clone();
    c.llm = Some(crate::config::LlmConfig {
        base_url: "http://llm.invalid".into(),
        api_key: "k".into(),
        model: "m".into(),
    });
    s.config = Arc::new(c);
    s
}
