//! An OpenAI-compatible chat-completions client: one system and one user
//! message in, the first choice's text out. Any provider that serves
//! `POST {base}/chat/completions` works.

use std::time::Duration;

use reqwest::header::{HeaderValue, AUTHORIZATION};
use serde::Deserialize;

use crate::config::LlmConfig;

/// The call plus at most two retries.
const ATTEMPTS: usize = 3;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("llm request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("llm answered HTTP {0}")]
    Status(u16),
    #[error("llm reply has no message content")]
    NoContent,
}

impl LlmError {
    /// Worth retrying within the same call: a 5xx or a timeout.
    fn transient(&self) -> bool {
        match self {
            Self::Status(s) => *s >= 500,
            Self::Http(e) => e.is_timeout(),
            Self::NoContent => false,
        }
    }
}

#[derive(Debug)]
pub struct Completion {
    pub content: String,
    pub total_tokens: Option<u64>,
}

pub struct LlmClient {
    http: reqwest::Client,
    url: String,
    key: HeaderValue,
    model: String,
    retry_delay: Duration,
}

impl LlmClient {
    pub fn new(cfg: &LlmConfig) -> Self {
        Self::build(cfg, Duration::from_secs(60), Duration::from_secs(2))
    }

    /// A short timeout and no wait between retries, for tests across the crate.
    #[cfg(test)]
    pub(crate) fn for_test(cfg: &LlmConfig) -> Self {
        Self::build(cfg, Duration::from_secs(5), Duration::ZERO)
    }

    fn build(cfg: &LlmConfig, timeout: Duration, retry_delay: Duration) -> Self {
        let mut key = HeaderValue::from_str(&format!("Bearer {}", cfg.api_key))
            .expect("api key is valid header text");
        // Keeps the key out of reqwest/hyper debug output.
        key.set_sensitive(true);
        Self {
            http: reqwest::Client::builder()
                .timeout(timeout)
                // reqwest forwards custom headers on cross-host redirects; a
                // misconfigured base must not leak the key.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client"),
            url: format!("{}/chat/completions", cfg.base_url),
            key,
            model: cfg.model.clone(),
            retry_delay,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// One completion at temperature 0. Retries only 5xx and timeouts, at most
    /// twice, inside this call; anything else is returned at once.
    pub async fn complete(&self, system: &str, user: &str) -> Result<Completion, LlmError> {
        let mut attempt = 1;
        loop {
            match self.send(system, user).await {
                Err(e) if e.transient() && attempt < ATTEMPTS => {
                    tracing::warn!(error = ?e, attempt, "llm call failed; retrying");
                    tokio::time::sleep(self.retry_delay).await;
                    attempt += 1;
                }
                done => return done,
            }
        }
    }

    async fn send(&self, system: &str, user: &str) -> Result<Completion, LlmError> {
        let res = self
            .http
            .post(&self.url)
            .header(AUTHORIZATION, self.key.clone())
            .json(&serde_json::json!({
                "model": self.model,
                "temperature": 0,
                "messages": [
                    {"role": "system", "content": system},
                    {"role": "user", "content": user}
                ]
            }))
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(LlmError::Status(res.status().as_u16()));
        }
        let body: ChatResponse = res.json().await?;
        let content = body
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or(LlmError::NoContent)?;
        Ok(Completion {
            content,
            total_tokens: body.usage.and_then(|u| u.total_tokens),
        })
    }
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<Choice>,
    usage: Option<Usage>,
}

#[derive(Deserialize)]
struct Choice {
    message: Message,
}

#[derive(Deserialize)]
struct Message {
    content: Option<String>,
}

#[derive(Deserialize)]
struct Usage {
    total_tokens: Option<u64>,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use wiremock::matchers::{body_partial_json, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn cfg(server: &MockServer) -> LlmConfig {
        LlmConfig {
            base_url: format!("{}/v1", server.uri()),
            api_key: "k".into(),
            model: "m".into(),
        }
    }

    fn reply(content: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": content}}],
            "usage": {"prompt_tokens": 30, "completion_tokens": 12, "total_tokens": 42}
        }))
    }

    #[tokio::test]
    async fn posts_an_openai_shaped_request_and_returns_content_and_tokens() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer k"))
            .and(body_partial_json(serde_json::json!({
                "model": "m",
                "temperature": 0,
                "messages": [
                    {"role": "system", "content": "S"},
                    {"role": "user", "content": "U"}
                ]
            })))
            .respond_with(reply("hi"))
            .expect(1)
            .mount(&server)
            .await;

        let c = LlmClient::for_test(&cfg(&server))
            .complete("S", "U")
            .await
            .unwrap();
        assert_eq!(c.content, "hi");
        assert_eq!(c.total_tokens, Some(42));
    }

    #[tokio::test]
    async fn a_5xx_is_retried_twice_then_returned() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&server)
            .await;
        let err = LlmClient::for_test(&cfg(&server))
            .complete("S", "U")
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Status(500)), "{err:?}");
    }

    #[tokio::test]
    async fn a_retry_that_succeeds_returns_the_answer() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(reply("ok"))
            .expect(1)
            .mount(&server)
            .await;
        let c = LlmClient::for_test(&cfg(&server))
            .complete("S", "U")
            .await
            .unwrap();
        assert_eq!(c.content, "ok");
    }

    #[tokio::test]
    async fn a_4xx_is_not_retried() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&server)
            .await;
        let err = LlmClient::for_test(&cfg(&server))
            .complete("S", "U")
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Status(401)), "{err:?}");
    }

    #[tokio::test]
    async fn a_timeout_is_retried_twice_then_returned() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(reply("late").set_delay(Duration::from_millis(500)))
            .expect(3)
            .mount(&server)
            .await;
        let client = LlmClient::build(&cfg(&server), Duration::from_millis(100), Duration::ZERO);
        let err = client.complete("S", "U").await.unwrap_err();
        assert!(
            matches!(err, LlmError::Http(ref e) if e.is_timeout()),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn redirects_are_never_followed() {
        let target = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(reply("leaked"))
            .expect(0)
            .mount(&target)
            .await;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/elsewhere", target.uri()).as_str()),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = LlmClient::for_test(&cfg(&server))
            .complete("S", "U")
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::Status(302)), "{err:?}");
    }

    #[tokio::test]
    async fn a_reply_without_content_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"choices": []})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let err = LlmClient::for_test(&cfg(&server))
            .complete("S", "U")
            .await
            .unwrap_err();
        assert!(matches!(err, LlmError::NoContent), "{err:?}");
    }

    #[test]
    fn the_key_header_is_sensitive() {
        let c = LlmClient::new(&LlmConfig {
            base_url: "http://llm.invalid".into(),
            api_key: "k".into(),
            model: "m".into(),
        });
        assert!(c.key.is_sensitive());
        assert_eq!(c.model(), "m");
    }
}
