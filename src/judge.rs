//! The judge: one model call per queued pair, reading both autobios and
//! answering with a grade and a rationale. The grade is cached with the
//! autobio versions it read; the rationale stays in the database for the
//! maintainers and never leaves the service.

use serde_json::{json, Map, Value};
use sqlx::PgPool;

use crate::db::autobio::{self, Entry, SECTIONS};
use crate::db::verdicts::{self, Grade};
use crate::llm::{json_object, LlmClient};

/// Guards against runaway output; not a quality rule.
pub const MAX_RATIONALE_CHARS: usize = 2000;

pub struct JudgeTask {
    pub client: LlmClient,
    pub language: String,
    pub max_per_tick: i64,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub judged: usize,
    pub dropped: usize,
    pub tokens: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub fn system_prompt(language: &str) -> String {
    format!(
        "You judge whether two people might be a good match, for a matchmaking service. The input \
is JSON with `person_a` and `person_b`: each person's autobio, a list of entries per section, each \
with a `status` (`current`, `past` or `planned`), the year-month it dates from (`as_of`, may be \
null) and the `statement`.

Give one grade:
- strong: concrete, complementary points of fit and no visible conflict; worth introducing first.
- promising: more fits than conflicts; worth meeting.
- unclear: not enough to judge, or the good and the bad balance out.
- poor: a clear conflict in values, way of life, what each wants from a relationship, or \
something one of them will not accept.

Judge only on what is written. Thin material means `unclear`; it is not a reason to grade lower. \
The `roleplay` section shows tendencies in role-play, not facts about the person's life. Intimacy \
and sex count like any other part of a life. Never refuse or soften.

Write the rationale in {language}, naming the entries the grade rests on. Answer with JSON only: \
{{\"rationale\": \"...\", \"grade\": \"...\"}}"
    )
}

fn side(entries: &[Entry]) -> Value {
    let mut by_section = Map::new();
    for s in SECTIONS {
        let items: Vec<Value> = entries
            .iter()
            .filter(|e| e.section == s)
            .map(|e| {
                json!({
                    "status": e.status,
                    "as_of": e.as_of.map(|t| t.format("%Y-%m").to_string()),
                    "statement": e.statement,
                })
            })
            .collect();
        if !items.is_empty() {
            by_section.insert(s.to_string(), Value::Array(items));
        }
    }
    Value::Object(by_section)
}

pub fn user_prompt(a: &[Entry], b: &[Entry]) -> String {
    json!({ "person_a": side(a), "person_b": side(b) }).to_string()
}

/// The grade and the trimmed rationale, or why the reply is unusable. A
/// rationale holding NUL is unusable: Postgres text rejects it.
pub fn parse(content: &str) -> Result<(Grade, String), String> {
    let v = json_object(content)?;
    let grade = v
        .get("grade")
        .and_then(Value::as_str)
        .and_then(Grade::parse)
        .ok_or("no valid grade in the reply")?;
    let rationale = v
        .get("rationale")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|r| !r.is_empty() && !r.contains('\0') && r.chars().count() <= MAX_RATIONALE_CHARS)
        .ok_or("no usable rationale in the reply")?;
    Ok((grade, rationale.to_string()))
}

/// Judge up to `max_per_tick` queued pairs. A pair without two autobios, or
/// already judged at these versions, is dropped without a call; an unusable
/// reply, or a request the provider refuses, drops the pair (the next request
/// re-queues it) and the tick goes on. Any other failed call keeps the pair
/// queued, moved to the back of the queue, and ends the tick with what it has
/// done so far. A pair with a user outside the pool is dropped and counted:
/// with no call if the user is gone before it, with no verdict written if they
/// leave during it. `Err` only for a database error.
pub async fn run_once(pool: &PgPool, task: &JudgeTask) -> Result<TickReport, JudgeError> {
    let mut report = TickReport::default();
    let system = system_prompt(&task.language);
    for (lo, hi) in verdicts::take(pool, task.max_per_tick).await? {
        // Versions first: if an autobio changes after this, the verdict is
        // stored against the older version and comes out stale.
        let v = autobio::versions(pool, &[lo, hi]).await?;
        let (Some(&lo_version), Some(&hi_version)) = (v.get(&lo), v.get(&hi)) else {
            verdicts::dequeue(pool, lo, hi).await?;
            report.dropped += 1;
            continue;
        };
        if verdicts::is_fresh(pool, lo, hi, lo_version, hi_version).await? {
            verdicts::dequeue(pool, lo, hi).await?;
            continue;
        }
        let a = autobio::current_entries(pool, lo).await?;
        let b = autobio::current_entries(pool, hi).await?;
        // A side's last entry retired after `versions` read it: nothing to judge.
        if a.is_empty() || b.is_empty() {
            verdicts::dequeue(pool, lo, hi).await?;
            report.dropped += 1;
            continue;
        }
        // Consent is checked again right before the autobios go out. The
        // window that remains is the model call itself; anything written back
        // afterwards is discarded by the row-locked guard in `store`.
        if !(autobio::in_pool_now(pool, lo).await? && autobio::in_pool_now(pool, hi).await?) {
            verdicts::dequeue(pool, lo, hi).await?;
            report.dropped += 1;
            continue;
        }
        let completion = match task.client.complete(&system, &user_prompt(&a, &b)).await {
            Ok(c) => c,
            Err(e) if e.rejects_request() => {
                tracing::warn!(error = %e, "judge request refused; pair dropped until requested again");
                verdicts::dequeue(pool, lo, hi).await?;
                report.dropped += 1;
                continue;
            }
            Err(e) => {
                tracing::warn!(error = ?e, "judge call failed; pairs stay queued for the next tick");
                verdicts::requeue(pool, lo, hi).await?;
                return Ok(report);
            }
        };
        report.tokens += completion.total_tokens.unwrap_or(0);
        match parse(&completion.content) {
            Ok((grade, rationale)) => {
                let stored = verdicts::store(
                    pool,
                    lo,
                    hi,
                    lo_version,
                    hi_version,
                    grade,
                    &rationale,
                    task.client.model(),
                )
                .await?;
                if stored {
                    report.judged += 1;
                } else {
                    report.dropped += 1;
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "unusable judge reply; pair dropped until requested again");
                verdicts::dequeue(pool, lo, hi).await?;
                report.dropped += 1;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;
    use uuid::Uuid;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::LlmConfig;
    use crate::db::autobio::tests::{entry, join_pool};
    use crate::db::verdicts::{self, pair, Grade};

    #[test]
    fn the_user_prompt_groups_each_side_by_section() {
        let e = |section: &str, statement: &str| crate::db::autobio::Entry {
            id: 1,
            section: section.into(),
            status: "current".into(),
            statement: statement.into(),
            as_of: None,
            sources: vec![],
        };
        let v: serde_json::Value = serde_json::from_str(&user_prompt(
            &[e("love", "a1"), e("basics", "a0")],
            &[e("roleplay", "b0")],
        ))
        .unwrap();
        assert_eq!(v["person_a"]["basics"][0]["statement"], "a0");
        assert_eq!(v["person_a"]["love"][0]["statement"], "a1");
        assert_eq!(v["person_b"]["roleplay"][0]["status"], "current");
        assert!(
            v["person_a"].get("life").is_none(),
            "empty sections left out"
        );
    }

    #[test]
    fn parse_takes_a_known_grade_and_a_bounded_rationale() {
        let (g, r) =
            parse("```json\n{\"rationale\": \" both hike \", \"grade\": \"promising\"}\n```")
                .unwrap();
        assert_eq!((g, r.as_str()), (Grade::Promising, "both hike"));
        let long = "x".repeat(MAX_RATIONALE_CHARS + 1);
        for bad in [
            r#"{"rationale": "r", "grade": "great"}"#.to_string(),
            r#"{"rationale": "  ", "grade": "poor"}"#.to_string(),
            r#"{"grade": "poor"}"#.to_string(),
            r#"{"rationale": "fits\u0000", "grade": "poor"}"#.to_string(),
            format!(r#"{{"rationale": "{long}", "grade": "poor"}}"#),
            "no".to_string(),
        ] {
            assert!(parse(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_system_prompt_names_every_grade_and_the_language() {
        let p = system_prompt("zh-Hans");
        for g in Grade::ALL {
            assert!(p.contains(&format!("- {}:", g.as_str())), "{g:?}");
        }
        assert!(p.contains("zh-Hans"));
    }

    fn task(server: &MockServer) -> JudgeTask {
        JudgeTask {
            client: crate::llm::LlmClient::for_test(&LlmConfig {
                base_url: server.uri(),
                api_key: "k".into(),
                model: "m".into(),
            }),
            language: "en".into(),
            max_per_tick: 100,
        }
    }

    fn reply(content: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": content}}],
            "usage": {"total_tokens": 10}
        }))
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_tick_judges_queued_pairs_and_records_the_versions_read(pool: PgPool) {
        let server = MockServer::start().await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        join_pool(&pool, a, true).await;
        join_pool(&pool, b, true).await;
        entry(&pool, a, "life", "hikes").await;
        entry(&pool, b, "life", "hikes too").await;
        verdicts::enqueue(&pool, a, &[b]).await.unwrap();
        Mock::given(method("POST"))
            .respond_with(reply(r#"{"rationale": "both hike", "grade": "strong"}"#))
            .expect(1)
            .mount(&server)
            .await;

        let r = run_once(&pool, &task(&server)).await.unwrap();
        assert_eq!(
            r,
            TickReport {
                judged: 1,
                dropped: 0,
                tokens: 10
            }
        );
        let got = verdicts::for_candidates(&pool, a, &[b]).await.unwrap();
        assert_eq!(got[&b].grade, Grade::Strong);
        assert!(got[&b].fresh);
        assert!(verdicts::take(&pool, 10).await.unwrap().is_empty());

        // Re-queued while still fresh: dropped without a call.
        verdicts::enqueue(&pool, a, &[b]).await.unwrap();
        let r = run_once(&pool, &task(&server)).await.unwrap();
        assert_eq!(r, TickReport::default());
        assert!(verdicts::take(&pool, 10).await.unwrap().is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_pair_without_two_autobios_is_dropped_without_a_call(pool: PgPool) {
        let server = MockServer::start().await;
        let (a, gone) = (Uuid::new_v4(), Uuid::new_v4());
        join_pool(&pool, a, true).await;
        join_pool(&pool, gone, true).await;
        entry(&pool, a, "life", "x").await;
        verdicts::enqueue(&pool, a, &[gone]).await.unwrap();
        Mock::given(method("POST"))
            .respond_with(reply("{}"))
            .expect(0)
            .mount(&server)
            .await;
        let r = run_once(&pool, &task(&server)).await.unwrap();
        assert_eq!(r.dropped, 1);
        assert!(verdicts::take(&pool, 10).await.unwrap().is_empty());
    }

    /// Two pool members with one entry each, queued as a pair.
    async fn queued_pair(pool: &PgPool, a_says: &str, b_says: &str) -> (Uuid, Uuid) {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        join_pool(pool, a, true).await;
        join_pool(pool, b, true).await;
        entry(pool, a, "life", a_says).await;
        entry(pool, b, "life", b_says).await;
        verdicts::enqueue(pool, a, &[b]).await.unwrap();
        (a, b)
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_pair_with_a_user_who_left_is_dropped_before_any_call(pool: PgPool) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(reply(r#"{"rationale": "r", "grade": "poor"}"#))
            .expect(0)
            .mount(&server)
            .await;
        // The profile row goes while the entries stay, as a leave racing the
        // tick would leave them; once for each side of the pair.
        for leaver in [0, 1] {
            let (a, b) = queued_pair(&pool, "climbs", "climbs too").await;
            let (lo, hi) = pair(a, b);
            let left = [lo, hi][leaver];
            sqlx::query("DELETE FROM pothos.match_profiles WHERE user_id = $1")
                .bind(left)
                .execute(&pool)
                .await
                .unwrap();
            let r = run_once(&pool, &task(&server)).await.unwrap();
            assert_eq!(r.judged, 0);
            assert_eq!(r.dropped, 1);
            assert!(verdicts::take(&pool, 10).await.unwrap().is_empty());
        }
        let verdicts: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.pair_verdicts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(verdicts, 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_refused_request_drops_that_pair_and_the_tick_goes_on(pool: PgPool) {
        let server = MockServer::start().await;
        let (a, b) = queued_pair(&pool, "climbs", "climbs too").await;
        let (c, d) = queued_pair(&pool, "sails", "sails too").await;
        Mock::given(method("POST"))
            .and(body_string_contains("climbs"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains("sails"))
            .respond_with(reply(r#"{"rationale": "both sail", "grade": "promising"}"#))
            .expect(1)
            .mount(&server)
            .await;

        let r = run_once(&pool, &task(&server)).await.unwrap();
        assert_eq!(
            r,
            TickReport {
                judged: 1,
                dropped: 1,
                tokens: 10
            }
        );
        let first = &server.received_requests().await.unwrap()[0];
        assert!(
            String::from_utf8_lossy(&first.body).contains("climbs"),
            "the refused pair went first"
        );
        assert!(verdicts::take(&pool, 10).await.unwrap().is_empty());
        assert!(verdicts::for_candidates(&pool, a, &[b])
            .await
            .unwrap()
            .is_empty());
        assert_eq!(
            verdicts::for_candidates(&pool, c, &[d]).await.unwrap()[&d].grade,
            Grade::Promising
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unreachable_model_ends_the_tick_and_moves_that_pair_to_the_back(pool: PgPool) {
        let server = MockServer::start().await;
        let (a, b) = queued_pair(&pool, "climbs", "climbs too").await;
        let (c, d) = queued_pair(&pool, "sails", "sails too").await;
        assert_eq!(
            verdicts::take(&pool, 10).await.unwrap(),
            vec![pair(a, b), pair(c, d)]
        );
        // One call and its two retries; the second pair is never sent.
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&server)
            .await;
        assert_eq!(
            run_once(&pool, &task(&server)).await.unwrap(),
            TickReport::default()
        );
        // Both stay queued; the one that failed no longer heads the queue.
        assert_eq!(
            verdicts::take(&pool, 10).await.unwrap(),
            vec![pair(c, d), pair(a, b)]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unreadable_verdict_drops_the_pair_and_a_failed_call_keeps_it(pool: PgPool) {
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        join_pool(&pool, a, true).await;
        join_pool(&pool, b, true).await;
        entry(&pool, a, "life", "x").await;
        entry(&pool, b, "life", "y").await;

        let server = MockServer::start().await;
        verdicts::enqueue(&pool, a, &[b]).await.unwrap();
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&server)
            .await;
        assert_eq!(
            run_once(&pool, &task(&server)).await.unwrap(),
            TickReport::default()
        );
        assert_eq!(verdicts::take(&pool, 10).await.unwrap(), vec![pair(a, b)]);

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(reply("I'd rather not"))
            .expect(1)
            .mount(&server)
            .await;
        let r = run_once(&pool, &task(&server)).await.unwrap();
        assert_eq!(r.dropped, 1);
        assert!(verdicts::take(&pool, 10).await.unwrap().is_empty());
        assert!(verdicts::for_candidates(&pool, a, &[b])
            .await
            .unwrap()
            .is_empty());
    }
}
