//! Files observed phrases under the shared concept vocabulary with an LLM.
//! For each phrase the model makes a discrete choice — reuse a label, coin
//! one, or none — and the engine's arithmetic does the rest. Runs as the
//! sweeper's second step; meanwhile an undecided phrase matches as itself.

use std::collections::HashMap;

use sqlx::PgPool;

use crate::db::concepts::{self as store, Decision, Pending};
use crate::llm::{LlmClient, LlmError};
use crate::scoring;

/// Phrases per model call.
pub const BATCH: usize = 40;
/// Concepts one phrase may carry; zero is allowed.
pub const MAX_CONCEPTS: usize = 3;
/// Guards against runaway output; not a quality rule.
pub const MAX_LABEL_CHARS: usize = 24;

pub struct ConceptTask {
    pub client: LlmClient,
    pub label_language: String,
    pub max_per_tick: i64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub decided: usize,
    pub skipped: usize,
    pub new_concepts: u64,
    pub vocabulary: usize,
    pub tokens: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ConceptError {
    #[error(transparent)]
    Llm(#[from] LlmError),
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub fn system_prompt(label_language: &str) -> String {
    format!(
        "You file short profile phrases under a shared vocabulary of concepts, so that \
two people's profiles can be compared.

The input is JSON with `vocabulary` (the existing concept labels) and `phrases` \
(each with an index `i`, a `dimension` and the `phrase`).

For every phrase, give 0 to {MAX_CONCEPTS} concept labels:
- Reuse a vocabulary label whenever it means the same thing, whatever language the phrase is in.
- Coin a new label only when no existing label fits. Write new labels in {label_language}, \
at most {MAX_LABEL_CHARS} characters.
- Pick the level at which two people would say \"we both like X\": \"jazz\", not \"music\" \
and not \"old-school jazz\".
- A `deal_breakers` phrase names what the person rejects. Label the rejected thing itself and \
keep its polarity: \"can't stand smoking\" -> \"smoking\"; \"quit smoking three years ago\" -> \
\"quitting smoking\", which is not \"smoking\".
- Give an empty list for a phrase with nothing to match on, such as \"none\" or \"unknown\".
- Record what the phrase says, sexual preferences included. Never refuse or soften.

Answer with JSON only, one entry per phrase: \
{{\"results\": [{{\"i\": 0, \"concepts\": [\"...\"]}}]}}"
    )
}

pub fn user_prompt(vocabulary: &[String], batch: &[Pending]) -> String {
    let phrases: Vec<serde_json::Value> = batch
        .iter()
        .enumerate()
        .map(|(i, p)| serde_json::json!({"i": i, "dimension": p.dimension, "phrase": p.phrase}))
        .collect();
    serde_json::json!({ "vocabulary": vocabulary, "phrases": phrases }).to_string()
}

/// The decisions a reply supports, and how many phrases of the batch it left
/// undecided. Each entry is validated alone; `Err` only when the reply holds
/// no readable `results` object at all.
pub fn parse(content: &str, batch: &[Pending]) -> Result<(Vec<Decision>, usize), String> {
    let v = crate::llm::json_object(content)?;
    let items = v
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or("no results array in the reply")?;

    let index = |item: &serde_json::Value| item.get("i").and_then(|i| i.as_u64());
    let mut seen: HashMap<u64, usize> = HashMap::new();
    for i in items.iter().filter_map(index) {
        *seen.entry(i).or_default() += 1;
    }
    let mut decisions = Vec::new();
    for item in items {
        let Some(i) = index(item) else { continue };
        if seen[&i] > 1 {
            continue; // ambiguous: the model answered this phrase twice
        }
        let Some(p) = usize::try_from(i).ok().and_then(|i| batch.get(i)) else {
            continue;
        };
        let Some(concepts) = labels(item.get("concepts")) else {
            continue;
        };
        decisions.push(Decision {
            dimension: p.dimension.clone(),
            phrase: p.phrase.clone(),
            concepts,
        });
    }
    let skipped = batch.len() - decisions.len();
    Ok((decisions, skipped))
}

/// Up to `MAX_CONCEPTS` canonical labels within the length cap, repeats
/// dropped; `None` if the entry breaks any rule.
fn labels(v: Option<&serde_json::Value>) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for x in v?.as_array()? {
        let c = scoring::canon(x.as_str()?)?;
        if c.chars().count() > MAX_LABEL_CHARS {
            return None;
        }
        if !out.contains(&c) {
            out.push(c);
        }
    }
    (out.len() <= MAX_CONCEPTS).then_some(out)
}

/// Decide up to `max_per_tick` undecided phrases, batch by batch, each batch
/// seeing the concepts the batches before it coined. A failed model call ends
/// the tick; an unreadable reply skips only its batch.
pub async fn run_once(pool: &PgPool, task: &ConceptTask) -> Result<TickReport, ConceptError> {
    let pending = store::undecided(pool, task.max_per_tick).await?;
    let mut report = TickReport::default();
    if pending.is_empty() {
        return Ok(report);
    }
    let system = system_prompt(&task.label_language);
    for batch in pending.chunks(BATCH) {
        let vocabulary = store::vocabulary(pool).await?;
        let completion = task
            .client
            .complete(&system, &user_prompt(&vocabulary, batch))
            .await?;
        report.tokens += completion.total_tokens.unwrap_or(0);
        match parse(&completion.content, batch) {
            Ok((decisions, skipped)) => {
                report.new_concepts += store::record(pool, task.client.model(), &decisions).await?;
                report.decided += decisions.len();
                report.skipped += skipped;
            }
            Err(e) => {
                tracing::warn!(error = %e, phrases = batch.len(), "unreadable concept reply; batch left for the next tick");
                report.skipped += batch.len();
            }
        }
    }
    report.vocabulary = store::vocabulary(pool).await?.len();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::LlmConfig;
    use crate::db::candidates::concept_sets;
    use crate::db::mirror::{self, MirrorRow};

    fn pending(dimension: &str, phrase: &str) -> Pending {
        Pending {
            dimension: dimension.into(),
            phrase: phrase.into(),
        }
    }

    fn decision(dimension: &str, phrase: &str, concepts: &[&str]) -> Decision {
        Decision {
            dimension: dimension.into(),
            phrase: phrase.into(),
            concepts: concepts.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn parse_reads_fenced_or_wrapped_json_and_maps_indices_to_phrases() {
        let batch = [
            pending("interests", "listens to old jazz"),
            pending("interests", "n/a"),
        ];
        let json = r#"{"results":[{"i":0,"concepts":["jazz"]},{"i":1,"concepts":[]}]}"#;
        for content in [
            json.to_string(),
            format!("```json\n{json}\n```"),
            format!("Here you go:\n{json}\nDone."),
        ] {
            let (decisions, skipped) = parse(&content, &batch).unwrap();
            assert_eq!(skipped, 0, "{content}");
            assert_eq!(
                decisions,
                vec![
                    decision("interests", "listens to old jazz", &["jazz"]),
                    decision("interests", "n/a", &[]),
                ],
                "{content}"
            );
        }
    }

    #[test]
    fn parse_skips_only_the_entries_that_break_a_rule() {
        let batch: Vec<Pending> = (0..8)
            .map(|i| pending("interests", &format!("p{i}")))
            .collect();
        let too_long = "x".repeat(MAX_LABEL_CHARS + 1);
        let cjk_at_cap = "爵".repeat(MAX_LABEL_CHARS);
        let content = serde_json::json!({"results": [
            {"i": 0, "concepts": ["a", "b", "c", "d"]},
            {"i": 1, "concepts": [too_long]},
            {"i": 2, "concepts": ["   "]},
            {"i": 3, "concepts": "jazz"},
            {"i": "4", "concepts": ["jazz"]},
            {"i": 5, "concepts": ["x"]},
            {"i": 5, "concepts": ["y"]},
            {"i": 6, "concepts": [" Jazz ", "jazz", "BARS"]},
            {"i": 7, "concepts": [cjk_at_cap.clone()]},
            {"i": 99, "concepts": ["z"]}
        ]})
        .to_string();
        let (decisions, skipped) = parse(&content, &batch).unwrap();
        assert_eq!(
            decisions,
            vec![
                decision("interests", "p6", &["jazz", "bars"]),
                decision("interests", "p7", &[cjk_at_cap.as_str()]),
            ]
        );
        // p0 too many, p1 too long, p2 blank, p3 not a list, p4 index not a
        // number, p5 ambiguous.
        assert_eq!(skipped, 6);
    }

    #[test]
    fn parse_rejects_a_reply_with_no_results_object() {
        let batch = [pending("interests", "jazz")];
        for content in ["I can't help with that.", "{\"answer\": 1}", "} {"] {
            assert!(parse(content, &batch).is_err(), "{content}");
        }
    }

    #[test]
    fn the_user_prompt_is_json_carrying_phrases_verbatim() {
        let odd = "说\"不\"的人\n还有\ttabs";
        let batch = [pending("deal_breakers", odd), pending("interests", "jazz")];
        let v: serde_json::Value =
            serde_json::from_str(&user_prompt(&["爵士乐".to_string()], &batch)).unwrap();
        assert_eq!(v["vocabulary"], serde_json::json!(["爵士乐"]));
        assert_eq!(
            v["phrases"][0],
            serde_json::json!({"i": 0, "dimension": "deal_breakers", "phrase": odd})
        );
        assert_eq!(v["phrases"][1]["i"], 1);
    }

    #[test]
    fn the_system_prompt_carries_the_label_language_and_the_limits() {
        let p = system_prompt("zh-Hans");
        assert!(p.contains("zh-Hans"));
        assert!(p.contains(&format!("0 to {MAX_CONCEPTS}")));
        assert!(p.contains(&format!("at most {MAX_LABEL_CHARS} characters")));
    }

    fn task(server: &MockServer, max_per_tick: i64) -> ConceptTask {
        ConceptTask {
            client: LlmClient::for_test(&LlmConfig {
                base_url: server.uri(),
                api_key: "k".into(),
                model: "m".into(),
            }),
            label_language: "en".into(),
            max_per_tick,
        }
    }

    fn reply(content: impl Into<String>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": content.into()}}],
            "usage": {"total_tokens": 10}
        }))
    }

    async fn export(pool: &PgPool, id: Uuid, interests: &[&str]) {
        mirror::upsert_one(
            pool,
            &MirrorRow {
                user_id: id,
                interests: interests.iter().map(|s| s.to_string()).collect(),
                personality_traits: vec![],
                deal_breakers: vec![],
                age_min: None,
                age_max: None,
                texts: vec![],
                engine_updated_at: Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap(),
            },
        )
        .await
        .unwrap();
    }

    /// The user message of the n-th request the fake model received, parsed.
    async fn sent(server: &MockServer, n: usize) -> serde_json::Value {
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[n].body).unwrap();
        serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap()
    }

    async fn decided(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM pothos.phrase_decisions")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_tick_reuses_coins_and_records_concepts(pool: PgPool) {
        let server = MockServer::start().await;
        store::record(
            &pool,
            "old-model",
            &[decision("interests", "jazz", &["jazz"])],
        )
        .await
        .unwrap();
        let a = Uuid::new_v4();
        export(
            &pool,
            a,
            &["listens to old jazz", "n/a", "weekend jazz bars"],
        )
        .await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(reply(
                r#"{"results":[{"i":0,"concepts":["jazz"]},{"i":1,"concepts":[]},{"i":2,"concepts":[" Jazz ","bars"]}]}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let r = run_once(&pool, &task(&server, 200)).await.unwrap();
        assert_eq!(
            r,
            TickReport {
                decided: 3,
                skipped: 0,
                new_concepts: 1,
                vocabulary: 2,
                tokens: 10
            }
        );
        let prompt = sent(&server, 0).await;
        assert_eq!(prompt["vocabulary"], serde_json::json!(["jazz"]));
        assert_eq!(prompt["phrases"].as_array().unwrap().len(), 3);
        assert_eq!(
            concept_sets(&pool, a).await.unwrap().interests,
            vec!["bars", "jazz"]
        );
        let models: Vec<String> =
            sqlx::query_scalar("SELECT DISTINCT model FROM pothos.phrase_decisions ORDER BY model")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(models, vec!["m", "old-model"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn later_batches_see_concepts_coined_by_earlier_ones(pool: PgPool) {
        let server = MockServer::start().await;
        let phrases: Vec<String> = (0..=BATCH).map(|i| format!("phrase {i:02}")).collect();
        let refs: Vec<&str> = phrases.iter().map(String::as_str).collect();
        export(&pool, Uuid::new_v4(), &refs).await;
        let first: Vec<serde_json::Value> = (0..BATCH)
            .map(|i| serde_json::json!({"i": i, "concepts": ["coined"]}))
            .collect();
        Mock::given(method("POST"))
            .respond_with(reply(serde_json::json!({ "results": first }).to_string()))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(reply(r#"{"results":[{"i":0,"concepts":["coined"]}]}"#))
            .expect(1)
            .mount(&server)
            .await;

        let r = run_once(&pool, &task(&server, 200)).await.unwrap();
        assert_eq!((r.decided, r.new_concepts, r.vocabulary), (BATCH + 1, 1, 1));
        assert_eq!(sent(&server, 0).await["vocabulary"], serde_json::json!([]));
        let second = sent(&server, 1).await;
        assert_eq!(second["vocabulary"], serde_json::json!(["coined"]));
        assert_eq!(second["phrases"][0]["phrase"], format!("phrase {BATCH:02}"));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_tick_decides_at_most_max_per_tick(pool: PgPool) {
        let server = MockServer::start().await;
        export(&pool, Uuid::new_v4(), &["a", "b", "c", "d", "e"]).await;
        Mock::given(method("POST"))
            .respond_with(reply(
                r#"{"results":[{"i":0,"concepts":["x"]},{"i":1,"concepts":["x"]}]}"#,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let r = run_once(&pool, &task(&server, 2)).await.unwrap();
        assert_eq!(r.decided, 2);
        assert_eq!(
            sent(&server, 0).await["phrases"].as_array().unwrap().len(),
            2
        );
        assert_eq!(store::undecided(&pool, 10).await.unwrap().len(), 3);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn skipped_and_unreadable_answers_leave_phrases_for_the_next_tick(pool: PgPool) {
        let server = MockServer::start().await;
        export(&pool, Uuid::new_v4(), &["jazz", "tea"]).await;
        // Tick 1 answers only jazz; tick 2 is unreadable; tick 3 answers tea.
        Mock::given(method("POST"))
            .respond_with(reply(r#"{"results":[{"i":0,"concepts":["jazz"]}]}"#))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(reply("sorry, no"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(reply(r#"{"results":[{"i":0,"concepts":["tea"]}]}"#))
            .mount(&server)
            .await;
        let t = task(&server, 200);

        let r1 = run_once(&pool, &t).await.unwrap();
        assert_eq!((r1.decided, r1.skipped), (1, 1));
        let r2 = run_once(&pool, &t).await.unwrap();
        assert_eq!((r2.decided, r2.skipped), (0, 1));
        assert_eq!(
            decided(&pool).await,
            1,
            "an unreadable answer writes nothing"
        );
        let r3 = run_once(&pool, &t).await.unwrap();
        assert_eq!((r3.decided, r3.skipped), (1, 0));
        assert_eq!(sent(&server, 2).await["phrases"][0]["phrase"], "tea");
        assert!(store::undecided(&pool, 10).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_llm_failure_ends_the_tick_and_decides_nothing(pool: PgPool) {
        let server = MockServer::start().await;
        export(&pool, Uuid::new_v4(), &["jazz"]).await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&server)
            .await;

        let err = run_once(&pool, &task(&server, 200)).await.unwrap_err();
        assert!(
            matches!(err, ConceptError::Llm(LlmError::Status(500))),
            "{err:?}"
        );
        assert_eq!(decided(&pool).await, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn nothing_undecided_means_no_call(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(reply("{}"))
            .expect(0)
            .mount(&server)
            .await;
        let r = run_once(&pool, &task(&server, 200)).await.unwrap();
        assert_eq!(r, TickReport::default());
    }
}
