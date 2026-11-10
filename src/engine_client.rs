//! The engine's `/svc/v1/*` export streams, as this service consumes them:
//! the three concept arrays of the insights export, the extracted memories,
//! the fact ledger and the erasure tombstones, each with its keyset cursor.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use uuid::Uuid;

use crate::config::EngineConfig;

pub const EXPORT_SECRET_HEADER: &str = "x-insights-export-secret";
pub const FACTS_SECRET_HEADER: &str = "x-facts-export-secret";
pub const PAGE_LIMIT: i64 = 1000;

#[derive(Debug, Clone, Deserialize)]
pub struct ExportRow {
    pub user_id: Uuid,
    pub interests: Vec<String>,
    pub personality_traits: Vec<String>,
    pub deal_breakers: Vec<String>,
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

/// One extracted memory from `GET /svc/v1/memories/changes`.
#[derive(Debug, Clone, Deserialize)]
pub struct MemoryRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub content: String,
    pub category: String,
    /// The extractor's hints (`domain`, `evidence_type`, …), kept verbatim.
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct MemoryCursor {
    pub created_at: DateTime<Utc>,
    pub id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct MemoryPage {
    pub memories: Vec<MemoryRow>,
    pub next: Option<MemoryCursor>,
}

/// One ledger fact from `GET /svc/v1/facts/changes`. A repeat sighting moves
/// `last_seen_at` and the counters, and the row reappears in the stream.
#[derive(Debug, Clone, Deserialize)]
pub struct FactRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub content: String,
    pub category: String,
    pub domain: String,
    pub evidence_type: String,
    pub temporality: String,
    pub persistence: String,
    pub confidence: String,
    pub occurrences: i32,
    pub session_count: i32,
    pub recurrence: String,
    pub first_seen_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct FactCursor {
    pub last_seen_at: DateTime<Utc>,
    pub id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct FactsPage {
    pub facts: Vec<FactRow>,
    pub next: Option<FactCursor>,
}

/// One user-level tombstone from the erasure stream.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct ErasureRow {
    pub user_id: Uuid,
    pub erased_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct ErasureCursor {
    pub erased_at: DateTime<Utc>,
    pub user_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct ErasuresPage {
    pub erasures: Vec<ErasureRow>,
    pub next: Option<ErasureCursor>,
}

/// One row-level tombstone: a single memory or fact its owner deleted.
#[derive(Debug, Clone, Deserialize)]
pub struct RowDeletion {
    /// `memory` or `fact`; anything else is a kind this service does not mirror.
    pub kind: String,
    /// The id the row had in its changes stream.
    pub id: Uuid,
    pub deleted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct RowDeletionCursor {
    pub deleted_at: DateTime<Utc>,
    pub id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct RowDeletionsPage {
    pub deletions: Vec<RowDeletion>,
    pub next: Option<RowDeletionCursor>,
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("engine request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("engine answered HTTP {0}")]
    Status(u16),
    #[error("ENGINE_FACTS_SECRET is not configured")]
    MissingFactsSecret,
}

pub struct EngineClient {
    http: reqwest::Client,
    base_url: String,
    secret: reqwest::header::HeaderValue,
    facts_secret: Option<reqwest::header::HeaderValue>,
}

impl EngineClient {
    pub fn new(cfg: &EngineConfig) -> Self {
        let sensitive = |s: &str| {
            let mut v = reqwest::header::HeaderValue::from_str(s)
                .expect("export secret is valid header text");
            // Keeps the secret out of reqwest/hyper debug output.
            v.set_sensitive(true);
            v
        };
        let secret = sensitive(&cfg.export_secret);
        let facts_secret = cfg.facts_secret.as_deref().map(sensitive);
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
            facts_secret,
        }
    }

    /// Whether the facts export is configured; the sweeper skips the facts
    /// step when it is not.
    pub fn has_facts(&self) -> bool {
        self.facts_secret.is_some()
    }

    /// One page strictly after `(since, after_user)`. The sweeper decides the
    /// position: epoch + nil on first run, a few seconds behind the stored
    /// cursor on later runs, `next` verbatim within a walk.
    pub async fn changes(
        &self,
        since: DateTime<Utc>,
        after_user: Uuid,
    ) -> Result<ChangesPage, EngineError> {
        self.page(
            "/svc/v1/insights/changes",
            since,
            ("after_user", after_user),
        )
        .await
    }

    /// One page of extracted memories strictly after `(since, after_id)`. An
    /// engine without this export answers 404.
    pub async fn memory_changes(
        &self,
        since: DateTime<Utc>,
        after_id: Uuid,
    ) -> Result<MemoryPage, EngineError> {
        self.page("/svc/v1/memories/changes", since, ("after_id", after_id))
            .await
    }

    /// One page of ledger facts strictly after `(since, after_id)`. Needs the
    /// facts secret; an engine without the export answers 404.
    pub async fn facts_changes(
        &self,
        since: DateTime<Utc>,
        after_id: Uuid,
    ) -> Result<FactsPage, EngineError> {
        let secret = self
            .facts_secret
            .clone()
            .ok_or(EngineError::MissingFactsSecret)?;
        self.page_with(
            "/svc/v1/facts/changes",
            FACTS_SECRET_HEADER,
            secret,
            since,
            ("after_id", after_id),
        )
        .await
    }

    /// One page of user-level tombstones strictly after `(since, after_user)`.
    /// Rides the insights secret, like the other two mirrored streams.
    pub async fn deletions(
        &self,
        since: DateTime<Utc>,
        after_user: Uuid,
    ) -> Result<ErasuresPage, EngineError> {
        self.page(
            "/svc/v1/insights/deletions",
            since,
            ("after_user", after_user),
        )
        .await
    }

    /// One page of row-level tombstones strictly after `(since, after_id)`.
    /// Either read secret opens the stream; this uses the insights one, which
    /// a sweeper always has.
    pub async fn row_deletions(
        &self,
        since: DateTime<Utc>,
        after_id: Uuid,
    ) -> Result<RowDeletionsPage, EngineError> {
        self.page("/svc/v1/rows/deletions", since, ("after_id", after_id))
            .await
    }

    async fn page<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        since: DateTime<Utc>,
        after: (&str, Uuid),
    ) -> Result<T, EngineError> {
        self.page_with(
            path,
            EXPORT_SECRET_HEADER,
            self.secret.clone(),
            since,
            after,
        )
        .await
    }

    async fn page_with<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        header: &str,
        secret: reqwest::header::HeaderValue,
        since: DateTime<Utc>,
        (after_key, after): (&str, Uuid),
    ) -> Result<T, EngineError> {
        let res = self
            .http
            .get(format!("{}{path}", self.base_url))
            .header(header, secret)
            .query(&[
                // AutoSi keeps the engine's microsecond precision; a truncated
                // `since` would re-return same-second rows (harmless upserts,
                // but wasted pages). Whole-second values print without a
                // fraction, which is what the test matchers expect.
                (
                    "since",
                    since.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                ),
                (after_key, after.to_string()),
                ("limit", PAGE_LIMIT.to_string()),
            ])
            .send()
            .await?;
        if !res.status().is_success() {
            return Err(EngineError::Status(res.status().as_u16()));
        }
        Ok(res.json::<T>().await?)
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
            facts_secret: Some("f".repeat(32)),
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
    async fn first_call_sends_epoch_nil_and_secret_and_parses_the_matching_columns() {
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
        let ts = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        assert_eq!(r.updated_at, ts);
        let next = page.next.unwrap();
        assert_eq!(next.user_id, id);
        assert_eq!(next.updated_at, ts);
    }

    #[tokio::test]
    async fn memory_changes_sends_cursor_and_secret_and_parses_rows() {
        let server = MockServer::start().await;
        let (m, u) = (Uuid::new_v4(), Uuid::new_v4());
        Mock::given(method("GET"))
            .and(path("/svc/v1/memories/changes"))
            .and(header(EXPORT_SECRET_HEADER, "s".repeat(32).as_str()))
            .and(query_param("since", "1970-01-01T00:00:00Z"))
            .and(query_param(
                "after_id",
                "00000000-0000-0000-0000-000000000000",
            ))
            .and(query_param("limit", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "memories": [{
                    "id": m, "user_id": u, "content": "用户周末常去爬山",
                    "category": "preference",
                    "metadata": {"domain": "hobby", "evidence_type": "explicit_statement"},
                    "created_at": "2026-10-13T12:00:00Z"
                }],
                "next": { "created_at": "2026-10-13T12:00:00Z", "id": m }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let page = EngineClient::new(&cfg(&server))
            .memory_changes(DateTime::<Utc>::UNIX_EPOCH, Uuid::nil())
            .await
            .unwrap();
        assert_eq!(page.memories.len(), 1);
        assert_eq!(page.memories[0].id, m);
        assert_eq!(
            page.memories[0].metadata.as_ref().unwrap()["domain"],
            "hobby"
        );
        assert_eq!(page.next.unwrap().id, m);
    }

    fn fact(id: Uuid, user: Uuid, first: &str, last: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id, "user_id": user, "content": "plays weekend jazz",
            "category": "hobby", "domain": "hobby",
            "evidence_type": "explicit_statement", "temporality": "current",
            "persistence": "ongoing", "confidence": "high",
            "occurrences": 3, "session_count": 2,
            "recurrence": "repeated_across_sessions",
            "first_seen_at": first, "last_seen_at": last
        })
    }

    #[tokio::test]
    async fn facts_changes_sends_cursor_facts_secret_and_parses_rows() {
        let server = MockServer::start().await;
        let (f, u) = (Uuid::new_v4(), Uuid::new_v4());
        Mock::given(method("GET"))
            .and(path("/svc/v1/facts/changes"))
            .and(header(FACTS_SECRET_HEADER, "f".repeat(32).as_str()))
            .and(query_param("since", "1970-01-01T00:00:00Z"))
            .and(query_param(
                "after_id",
                "00000000-0000-0000-0000-000000000000",
            ))
            .and(query_param("limit", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "facts": [fact(f, u, "2026-10-01T08:00:00Z", "2026-10-13T12:00:00Z")],
                "next": { "last_seen_at": "2026-10-13T12:00:00Z", "id": f }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let page = EngineClient::new(&cfg(&server))
            .facts_changes(DateTime::<Utc>::UNIX_EPOCH, Uuid::nil())
            .await
            .unwrap();
        assert_eq!(page.facts.len(), 1);
        let r = &page.facts[0];
        assert_eq!((r.id, r.user_id), (f, u));
        assert_eq!(r.content, "plays weekend jazz");
        assert_eq!((r.category.as_str(), r.domain.as_str()), ("hobby", "hobby"));
        assert_eq!(r.evidence_type, "explicit_statement");
        assert_eq!(
            (
                r.temporality.as_str(),
                r.persistence.as_str(),
                r.confidence.as_str()
            ),
            ("current", "ongoing", "high")
        );
        assert_eq!((r.occurrences, r.session_count), (3, 2));
        assert_eq!(r.recurrence, "repeated_across_sessions");
        assert_eq!(
            r.first_seen_at,
            Utc.with_ymd_and_hms(2026, 10, 1, 8, 0, 0).unwrap()
        );
        let last = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        assert_eq!(r.last_seen_at, last);
        let next = page.next.unwrap();
        assert_eq!((next.last_seen_at, next.id), (last, f));
    }

    #[tokio::test]
    async fn facts_changes_without_a_secret_is_an_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let mut c = cfg(&server);
        c.facts_secret = None;
        let client = EngineClient::new(&c);
        assert!(!client.has_facts());
        let err = client
            .facts_changes(DateTime::<Utc>::UNIX_EPOCH, Uuid::nil())
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::MissingFactsSecret));
    }

    #[tokio::test]
    async fn row_deletions_use_the_insights_secret() {
        let server = MockServer::start().await;
        let id = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path("/svc/v1/rows/deletions"))
            .and(header(EXPORT_SECRET_HEADER, "s".repeat(32).as_str()))
            .and(query_param("since", "1970-01-01T00:00:00Z"))
            .and(query_param(
                "after_id",
                "00000000-0000-0000-0000-000000000000",
            ))
            .and(query_param("limit", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "deletions": [{ "kind": "fact", "id": id, "deleted_at": "2026-11-10T12:00:00Z" }],
                "next": { "deleted_at": "2026-11-10T12:00:00Z", "id": id }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let page = EngineClient::new(&cfg(&server))
            .row_deletions(DateTime::<Utc>::UNIX_EPOCH, Uuid::nil())
            .await
            .unwrap();
        assert_eq!(page.deletions.len(), 1);
        assert_eq!(
            (page.deletions[0].kind.as_str(), page.deletions[0].id),
            ("fact", id)
        );
        assert_eq!(page.next.unwrap().id, id);
    }

    #[tokio::test]
    async fn deletions_use_the_insights_secret() {
        let server = MockServer::start().await;
        let u = Uuid::new_v4();
        Mock::given(method("GET"))
            .and(path("/svc/v1/insights/deletions"))
            .and(header(EXPORT_SECRET_HEADER, "s".repeat(32).as_str()))
            .and(query_param("since", "1970-01-01T00:00:00Z"))
            .and(query_param(
                "after_user",
                "00000000-0000-0000-0000-000000000000",
            ))
            .and(query_param("limit", "1000"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "erasures": [{ "user_id": u, "erased_at": "2026-10-13T12:00:00Z" }],
                "next": { "erased_at": "2026-10-13T12:00:00Z", "user_id": u }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let page = EngineClient::new(&cfg(&server))
            .deletions(DateTime::<Utc>::UNIX_EPOCH, Uuid::nil())
            .await
            .unwrap();
        assert_eq!(page.erasures.len(), 1);
        assert_eq!(page.erasures[0].user_id, u);
        let t = Utc.with_ymd_and_hms(2026, 10, 13, 12, 0, 0).unwrap();
        assert_eq!(page.erasures[0].erased_at, t);
        let next = page.next.unwrap();
        assert_eq!((next.erased_at, next.user_id), (t, u));
    }

    #[tokio::test]
    async fn an_engine_without_the_memories_export_is_a_404_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        let err = EngineClient::new(&cfg(&server))
            .memory_changes(DateTime::<Utc>::UNIX_EPOCH, Uuid::nil())
            .await
            .unwrap_err();
        assert!(matches!(err, EngineError::Status(404)));
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
    fn secret_headers_are_marked_sensitive() {
        let server_cfg = EngineConfig {
            base_url: "http://engine.invalid".into(),
            export_secret: "s".repeat(32),
            facts_secret: Some("f".repeat(32)),
        };
        let client = EngineClient::new(&server_cfg);
        assert!(client.secret.is_sensitive());
        assert!(client.facts_secret.unwrap().is_sensitive());
    }
}
