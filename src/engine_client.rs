//! The engine's `GET /svc/v1/insights/changes`, as this service consumes it:
//! the five matching columns out of the 21 the export returns, plus the
//! keyset cursor. Unknown fields are ignored on purpose.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::config::EngineConfig;

pub const EXPORT_SECRET_HEADER: &str = "x-insights-export-secret";
pub const PAGE_LIMIT: i64 = 1000;

#[derive(Debug, Clone, Deserialize)]
pub struct ExportRow {
    pub user_id: Uuid,
    pub interests: Vec<String>,
    pub personality_traits: Vec<String>,
    pub deal_breakers: Vec<String>,
    pub age_min: Option<i32>,
    pub age_max: Option<i32>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct Cursor {
    pub updated_at: DateTime<Utc>,
    pub user_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct ChangesPage {
    pub profiles: Vec<ExportRow>,
    pub next: Option<Cursor>,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("engine answered HTTP {0}")]
    Status(u16),
}

pub struct EngineClient {
    http: reqwest::Client,
    base_url: String,
    secret: reqwest::header::HeaderValue,
}

impl EngineClient {
    pub fn new(cfg: &EngineConfig) -> Self {
        let mut secret = reqwest::header::HeaderValue::from_str(&cfg.export_secret)
            .expect("export secret is valid header text");
        // Keeps the secret out of reqwest/hyper debug output.
        secret.set_sensitive(true);
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                // reqwest forwards custom headers on cross-host redirects; a
                // misconfigured `http://` base must not leak the secret.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("reqwest client"),
            base_url: cfg.base_url.clone(),
            secret,
        }
    }

    /// One page strictly after `(since, after_user)`. The sweeper decides the
    /// position: epoch + nil on first run, a few seconds behind the stored
    /// cursor on later runs, `next` verbatim within a walk.
    pub async fn changes(
        &self,
        since: DateTime<Utc>,
        after_user: Uuid,
    ) -> Result<ChangesPage, EngineError> {
        let res = self
            .http
            .get(format!("{}/svc/v1/insights/changes", self.base_url))
            .header(EXPORT_SECRET_HEADER, self.secret.clone())
            .query(&[
                // AutoSi keeps the engine's microsecond precision; a truncated
                // `since` would re-return same-second rows (harmless upserts,
                // but wasted pages). Whole-second values print without a
                // fraction, which is what the test matchers expect.
                (
                    "since",
                    since.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                ),
                ("after_user", after_user.to_string()),
                ("limit", PAGE_LIMIT.to_string()),
            ])
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(EngineError::Status(res.status().as_u16()));
        }
        Ok(res.json::<ChangesPage>().await?)
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use uuid::Uuid;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    fn cfg(server: &MockServer) -> EngineConfig {
        EngineConfig {
            base_url: server.uri(),
            export_secret: "s".repeat(32),
        }
    }

    fn row(id: Uuid, ts: &str) -> serde_json::Value {
        serde_json::json!({
            "user_id": id, "city": "Singapore", "location": null, "hometown": null,
            "nationality": null, "occupation": "dev", "mbti_guess": null,
            "love_values": null, "emotional_needs": null, "life_rhythm": null,
            "interests": ["jazz"], "personality_traits": ["calm"],
            "preferred_gender": "m", "age_min": 25, "age_max": 35,
            "deal_breakers": ["smoking"], "education": null, "family": null,
            "relationship_history": null, "social_pattern": null,
            "future_plans": null, "finance_status": null, "updated_at": ts
        })
    }

    #[tokio::test]
    async fn first_call_sends_epoch_nil_and_secret_and_parses_five_columns() {
        let server = MockServer::start().await;
        let id = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path("/svc/v1/insights/changes"))
            .and(header(EXPORT_SECRET_HEADER, "s".repeat(32).as_str()))
            .and(query_param("since", "1970-01-01T00:00:00Z"))
            .and(query_param(
                "after_user",
                "00000000-0000-0000-0000-000000000000",
            ))
            .and(query_param("limit", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [row(id, "2026-10-13T12:00:00Z")],
                "next": { "updated_at": "2026-10-13T12:00:00Z", "user_id": id }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let page = EngineClient::new(&cfg(&server))
            .changes(DateTime::<Utc>::UNIX_EPOCH, Uuid::nil())
            .await
            .unwrap();
        assert_eq!(page.profiles.len(), 1);
        let r = &page.profiles[0];
        assert_eq!(r.user_id, id);
        assert_eq!(r.interests, vec!["jazz"]);
        assert_eq!(r.personality_traits, vec!["calm"]);
        assert_eq!(r.deal_breakers, vec!["smoking"]);
        assert_eq!((r.age_min, r.age_max), (Some(25), Some(35)));
        let ts = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        assert_eq!(r.updated_at, ts);
        let next = page.next.unwrap();
        assert_eq!(next.user_id, id);
        assert_eq!(next.updated_at, ts);
    }

    #[tokio::test]
    async fn subsequent_call_passes_both_cursor_fields() {
        let server = MockServer::start().await;
        let id = Uuid::new_v4();
        let cur = Cursor {
            updated_at: Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap(),
            user_id: id,
        };
        Mock::given(method("GET"))
            .and(query_param("since", "2026-10-13T12:00:00Z"))
            .and(query_param("after_user", id.to_string().as_str()))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [], "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        let page = EngineClient::new(&cfg(&server))
            .changes(cur.updated_at, cur.user_id)
            .await
            .unwrap();
        assert!(page.profiles.is_empty());
        assert!(page.next.is_none());
    }

    #[tokio::test]
    async fn non_2xx_is_a_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403))
            .mount(&server)
            .await;
        let err = EngineClient::new(&cfg(&server))
            .changes(DateTime::<Utc>::UNIX_EPOCH, Uuid::nil())
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::Status(403)));
    }

    #[tokio::test]
    async fn since_keeps_microsecond_precision() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(query_param("since", "1970-01-01T00:00:00.123456Z"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "profiles": [], "next": null
            })))
            .expect(1)
            .mount(&server)
            .await;
        let since = DateTime::<Utc>::UNIX_EPOCH + chrono::Duration::microseconds(123_456);
        EngineClient::new(&cfg(&server))
            .changes(since, Uuid::nil())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn redirects_are_never_followed() {
        let target = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&target)
            .await;

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/svc/v1/insights/changes"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("location", format!("{}/elsewhere", target.uri()).as_str()),
            )
            .expect(1)
            .mount(&server)
            .await;

        let err = EngineClient::new(&cfg(&server))
            .changes(DateTime::<Utc>::UNIX_EPOCH, Uuid::nil())
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::Status(302)));
    }

    #[test]
    fn secret_header_is_marked_sensitive() {
        let server_cfg = EngineConfig {
            base_url: "http://engine.invalid".into(),
            export_secret: "s".repeat(32),
        };
        assert!(EngineClient::new(&server_cfg).secret.is_sensitive());
    }
}
