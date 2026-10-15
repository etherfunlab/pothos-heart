//! Keeps each pool member's autobio: one model call per due user, reading
//! the current entries and the material recorded since the last run, and
//! answering with add / revise / retire operations. Every operation is
//! checked on its own; one bad operation never sinks the rest.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, SecondsFormat, Utc};
use serde_json::{json, Value};
use sqlx::PgPool;

use crate::db::autobio::{self as store, Entry, Material, Op, SourceRef, SECTIONS, STATUSES};
use crate::llm::{json_object, LlmClient};

/// Guards against runaway output; not a quality rule.
pub const MAX_STATEMENT_CHARS: usize = 300;

pub struct AutobioTask {
    pub client: LlmClient,
    pub language: String,
    pub max_per_tick: i64,
    pub min_interval: chrono::Duration,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct TickReport {
    pub users: usize,
    pub failed: usize,
    pub added: usize,
    pub revised: usize,
    pub retired: usize,
    pub skipped: usize,
    pub tokens: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum AutobioError {
    #[error(transparent)]
    Db(#[from] sqlx::Error),
}

pub fn system_prompt(language: &str) -> String {
    format!(
        "You keep a person's autobio for a matchmaking service: a list of short entries, each \
stating one thing about the person, filed under fixed sections. You receive the current entries \
and new material, and answer with operations that bring the entries up to date.

Sections:
- basics: where they live and come from, education, work, family
- life: daily rhythm, hobbies, social life
- personality: personality and how they communicate
- love: views on love, relationship history, emotional needs, what they will not accept in a partner
- intimacy: intimacy and sex
- outlook: plans, goals, money
- roleplay: tendencies shown in role-play with an AI companion

The input is JSON:
- `now`: the current time.
- `entries`: the current entries, each with `entry` (its id), `section`, `status`, `as_of` \
(year-month of its latest source, or null), `statement`, `sources` (refs), and `unsupported` \
(true when every source is gone).
- `material`: new material, each with `ref`, `kind`, `at` (when it was first said), `text` and, \
for memories, `metadata`. `kind` is the profile field or list it came from, or `memory/<category>`.
- `deleted_by_owner`: statements the person deleted from their autobio.

Operations:
- {{\"op\": \"add\", \"section\": ..., \"status\": ..., \"statement\": ..., \"sources\": [refs]}} \
adds an entry; it needs at least one source.
- {{\"op\": \"revise\", \"entry\": id, \"status\": ..., \"statement\": ..., \"sources\": [refs]}} \
rewrites an entry. It keeps its section and its sources; list only the sources you add.
- {{\"op\": \"retire\", \"entry\": id}} removes an entry the material contradicts or no longer supports.
Each entry takes at most one operation. A source is a `ref` from `material` or from a current \
entry's `sources`.

Rules:
- One thing per entry, in the third person, in {language}, at most {MAX_STATEMENT_CHARS} \
characters. Keep concrete detail, negation, degree and uncertainty as the material states them.
- Write only what the material supports. Do not infer causes, draw personality conclusions or \
diagnose. A cause the person stated may be recorded.
- When new material contradicts an entry, revise it (mark it `past`, or rewrite it) so two \
versions never stand side by side.
- `material` is oldest first; a later version of the same profile field replaces an earlier one.
- `status` is `current`, `past` or `planned`. A `planned` entry becomes `past` or `current` only \
when material says it happened. Never write relative time such as \"last year\" or \"recently\"; \
an entry's time comes from its sources.
- Material that repeats an entry: revise that entry with the new source instead of adding a \
duplicate. Two entries that say the same thing: revise one to cite the other's sources too, and \
retire the other.
- An `unsupported` entry is either supported again by new material (revise it with that source) \
or retired.
- Role-play material (metadata `evidence_type` is `roleplay_expression`, or the text says it \
happened in role-play) goes only to `roleplay`; nothing in `roleplay` is repeated in another section.
- Never write back a statement from `deleted_by_owner`, in any wording.
- Ignore material with nothing to say, such as \"none\" or \"unknown\".
- Record what the material says, sexual content included. Never refuse or soften.

Answer with JSON only: {{\"ops\": [...]}}. With nothing to change, answer {{\"ops\": []}}."
    )
}

fn month(t: DateTime<Utc>) -> String {
    t.format("%Y-%m").to_string()
}

pub fn user_prompt(
    now: DateTime<Utc>,
    entries: &[Entry],
    material: &[Material],
    vetoed: &[String],
) -> String {
    let entries: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "entry": e.id,
                "section": e.section,
                "status": e.status,
                "as_of": e.as_of.map(month),
                "statement": e.statement,
                "sources": e.sources.iter().map(SourceRef::tag).collect::<Vec<_>>(),
                "unsupported": e.sources.is_empty(),
            })
        })
        .collect();
    let material: Vec<Value> = material
        .iter()
        .map(|m| {
            let mut v = json!({
                "ref": m.source.tag(),
                "kind": m.kind,
                "at": m.at.to_rfc3339_opts(SecondsFormat::Secs, true),
                "text": m.text,
            });
            if let Some(meta) = &m.metadata {
                v["metadata"] = meta.clone();
            }
            v
        })
        .collect();
    json!({
        "now": now.to_rfc3339_opts(SecondsFormat::Secs, true),
        "entries": entries,
        "material": material,
        "deleted_by_owner": vetoed,
    })
    .to_string()
}

/// What a reply may refer to: the current entries, and the sources in play
/// (this run's material plus every current entry's sources).
pub struct Allowed {
    entries: HashSet<i64>,
    sources: HashSet<SourceRef>,
}

impl Allowed {
    pub fn new(entries: &[Entry], material: &[Material]) -> Self {
        Self {
            entries: entries.iter().map(|e| e.id).collect(),
            sources: material
                .iter()
                .map(|m| m.source)
                .chain(entries.iter().flat_map(|e| e.sources.iter().copied()))
                .collect(),
        }
    }
}

/// The operations a reply supports, and how many it held that broke a rule.
/// `Err` only when the reply holds no readable `ops` array.
pub fn parse(content: &str, allowed: &Allowed) -> Result<(Vec<Op>, usize), String> {
    let v = json_object(content)?;
    let items = v
        .get("ops")
        .and_then(Value::as_array)
        .ok_or("no ops array in the reply")?;
    let mut touched: HashMap<i64, usize> = HashMap::new();
    for e in items
        .iter()
        .filter_map(|i| i.get("entry").and_then(Value::as_i64))
    {
        *touched.entry(e).or_default() += 1;
    }
    let ops: Vec<Op> = items
        .iter()
        .filter_map(|item| op(item, allowed, &touched))
        .collect();
    let skipped = items.len() - ops.len();
    Ok((ops, skipped))
}

fn op(item: &Value, allowed: &Allowed, touched: &HashMap<i64, usize>) -> Option<Op> {
    match item.get("op")?.as_str()? {
        "add" => {
            let sources = sources(item.get("sources")?, allowed)?;
            if sources.is_empty() {
                return None;
            }
            Some(Op::Add {
                section: one_of(item.get("section")?, &SECTIONS)?,
                status: one_of(item.get("status")?, &STATUSES)?,
                statement: statement(item.get("statement")?)?,
                sources,
            })
        }
        "revise" => Some(Op::Revise {
            entry: entry(item, allowed, touched)?,
            status: one_of(item.get("status")?, &STATUSES)?,
            statement: statement(item.get("statement")?)?,
            sources: match item.get("sources") {
                None => Vec::new(),
                Some(v) => sources(v, allowed)?,
            },
        }),
        "retire" => Some(Op::Retire {
            entry: entry(item, allowed, touched)?,
        }),
        _ => None,
    }
}

/// A current entry that this reply touches exactly once.
fn entry(item: &Value, allowed: &Allowed, touched: &HashMap<i64, usize>) -> Option<i64> {
    let e = item.get("entry")?.as_i64()?;
    (allowed.entries.contains(&e) && touched.get(&e) == Some(&1)).then_some(e)
}

fn one_of(v: &Value, set: &[&str]) -> Option<String> {
    let s = v.as_str()?;
    set.contains(&s).then(|| s.to_string())
}

/// Trimmed, non-empty, within the cap, and free of NUL (Postgres text
/// rejects it, so the write would fail every tick).
fn statement(v: &Value) -> Option<String> {
    let s = v.as_str()?.trim();
    (!s.is_empty() && !s.contains('\0') && s.chars().count() <= MAX_STATEMENT_CHARS)
        .then(|| s.to_string())
}

/// Every ref parses and is in play; repeats dropped. `None` if any is not.
fn sources(v: &Value, allowed: &Allowed) -> Option<Vec<SourceRef>> {
    let mut out = Vec::new();
    for x in v.as_array()? {
        let r = SourceRef::parse(x.as_str()?)?;
        if !allowed.sources.contains(&r) {
            return None;
        }
        if !out.contains(&r) {
            out.push(r);
        }
    }
    Some(out)
}

/// Update every due user's autobio, one call each. An unreadable reply, or a
/// request the provider refuses, writes nothing for that user and stamps
/// them, and the tick goes on. Any other failed call stamps the user and ends
/// the tick with what it has done so far, so a dead endpoint costs one call
/// per tick, not one per user. `Err` only for a database error.
pub async fn run_once(pool: &PgPool, task: &AutobioTask) -> Result<TickReport, AutobioError> {
    let started = store::db_now(pool).await?;
    let due = store::due_users(pool, started - task.min_interval, task.max_per_tick).await?;
    let mut report = TickReport::default();
    let system = system_prompt(&task.language);
    let model = task.client.model();
    for d in due {
        // A user who left since `due_users` withdrew consent: nothing of
        // theirs is sent.
        if !store::in_pool_now(pool, d.user_id).await? {
            continue;
        }
        let entries = store::current_entries(pool, d.user_id).await?;
        let material = store::material(pool, d.user_id, d.material_until).await?;
        let vetoed = store::vetoed_statements(pool, d.user_id).await?;
        let user = user_prompt(started, &entries, &material, &vetoed);
        // Checked again right before the material goes out. The window that
        // remains is the model call itself; anything written back afterwards
        // is discarded by the row-locked guard in `apply`.
        if !store::in_pool_now(pool, d.user_id).await? {
            continue;
        }
        report.users += 1;
        let completion = match task.client.complete(&system, &user).await {
            Ok(c) => c,
            Err(e) => {
                store::record_failure(pool, d.user_id, model).await?;
                report.failed += 1;
                if e.rejects_request() {
                    tracing::warn!(error = %e, user = %d.user_id, "autobio request refused; nothing written");
                    continue;
                }
                tracing::warn!(error = ?e, "autobio call failed; the rest wait for the next tick");
                return Ok(report);
            }
        };
        report.tokens += completion.total_tokens.unwrap_or(0);
        match parse(&completion.content, &Allowed::new(&entries, &material)) {
            Ok((ops, skipped)) => {
                let done = store::apply(pool, d.user_id, &ops, started, model).await?;
                report.added += done.added;
                report.revised += done.revised;
                report.retired += done.retired;
                report.skipped += skipped;
            }
            Err(e) => {
                tracing::warn!(error = %e, user = %d.user_id, "unreadable autobio reply; nothing written");
                store::record_failure(pool, d.user_id, model).await?;
                report.failed += 1;
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;
    use wiremock::matchers::{body_string_contains, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::config::LlmConfig;
    use crate::db::autobio::{self as store, Entry, Material, SourceRef};

    fn mat(source: SourceRef, kind: &str, text: &str) -> Material {
        Material {
            source,
            kind: kind.into(),
            text: text.into(),
            metadata: None,
            at: Utc.with_ymd_and_hms(2026, 9, 14, 8, 0, 0).unwrap(),
        }
    }

    fn ent(id: i64, sources: Vec<SourceRef>) -> Entry {
        Entry {
            id,
            section: "love".into(),
            status: "current".into(),
            statement: "s".into(),
            as_of: None,
            sources,
        }
    }

    #[test]
    fn the_user_prompt_is_json_carrying_material_verbatim() {
        let odd = "说\"不\"的人\n还有\ttabs";
        let m = Uuid::new_v4();
        let mut with_meta = mat(SourceRef::Memory(m), "memory/preference", "x");
        with_meta.metadata = Some(serde_json::json!({"evidence_type": "roleplay_expression"}));
        let now = Utc.with_ymd_and_hms(2026, 10, 15, 12, 0, 0).unwrap();
        let v: serde_json::Value = serde_json::from_str(&user_prompt(
            now,
            &[ent(7, vec![SourceRef::Observed(3)]), ent(8, vec![])],
            &[mat(SourceRef::Observed(3), "love_values", odd), with_meta],
            &["deleted one".to_string()],
        ))
        .unwrap();
        assert_eq!(v["now"], "2026-10-15T12:00:00Z");
        assert_eq!(v["material"][0]["text"], odd);
        assert_eq!(v["material"][0]["ref"], "o:3");
        assert_eq!(v["material"][0]["at"], "2026-09-14T08:00:00Z");
        assert_eq!(
            v["material"][1]["metadata"]["evidence_type"],
            "roleplay_expression"
        );
        assert_eq!(v["entries"][0]["sources"], serde_json::json!(["o:3"]));
        assert_eq!(v["entries"][0]["unsupported"], false);
        assert_eq!(v["entries"][1]["unsupported"], true);
        assert_eq!(v["deleted_by_owner"], serde_json::json!(["deleted one"]));
    }

    #[test]
    fn the_system_prompt_names_every_section_the_language_and_the_cap() {
        let p = system_prompt("zh-Hans");
        for s in store::SECTIONS {
            assert!(p.contains(&format!("- {s}:")), "{s}");
        }
        assert!(p.contains("zh-Hans"));
        assert!(p.contains(&format!("{MAX_STATEMENT_CHARS} characters")));
        assert!(p.contains(
            "`material` is oldest first; a later version of the same profile field replaces an \
earlier one."
        ));
    }

    fn allowed() -> Allowed {
        Allowed::new(
            &[ent(7, vec![SourceRef::Observed(3)]), ent(8, vec![])],
            &[mat(SourceRef::Observed(4), "city", "x")],
        )
    }

    #[test]
    fn parse_reads_each_operation() {
        let content = r#"```json
{"ops":[
 {"op":"add","section":"basics","status":"current","statement":"  Lives in Singapore ","sources":["o:4"]},
 {"op":"revise","entry":7,"status":"past","statement":"Was single","sources":["o:4","o:4"]},
 {"op":"revise","entry":8,"status":"current","statement":"No new source"},
 {"op":"retire","entry":9999}
]}
```"#;
        let (ops, skipped) = parse(content, &allowed()).unwrap();
        assert_eq!(
            ops,
            vec![
                Op::Add {
                    section: "basics".into(),
                    status: "current".into(),
                    statement: "Lives in Singapore".into(),
                    sources: vec![SourceRef::Observed(4)],
                },
                Op::Revise {
                    entry: 7,
                    status: "past".into(),
                    statement: "Was single".into(),
                    sources: vec![SourceRef::Observed(4)],
                },
                Op::Revise {
                    entry: 8,
                    status: "current".into(),
                    statement: "No new source".into(),
                    sources: vec![],
                },
            ]
        );
        assert_eq!(skipped, 1, "retire of an entry not in play");
    }

    #[test]
    fn parse_rejects_sources_and_entries_not_in_play() {
        let stranger = Uuid::new_v4();
        let content = serde_json::json!({"ops": [
            {"op": "add", "section": "life", "status": "current", "statement": "a",
             "sources": [format!("m:{stranger}")]},
            {"op": "add", "section": "life", "status": "current", "statement": "b",
             "sources": ["o:999999"]},
            {"op": "add", "section": "life", "status": "current", "statement": "c",
             "sources": ["o:3"]},
            {"op": "add", "section": "life", "status": "current", "statement": "d", "sources": []},
            {"op": "add", "section": "hobbies", "status": "current", "statement": "e", "sources": ["o:4"]},
            {"op": "add", "section": "life", "status": "maybe", "statement": "f", "sources": ["o:4"]},
            {"op": "revise", "entry": 7, "status": "past", "statement": "g"},
            {"op": "retire", "entry": 7},
            {"op": "merge", "entry": 8}
        ]})
        .to_string();
        let (ops, skipped) = parse(&content, &allowed()).unwrap();
        // Only "c" survives: o:3 is a current entry's source, so it is in play.
        assert_eq!(ops.len(), 1);
        assert!(matches!(&ops[0], Op::Add { statement, .. } if statement == "c"));
        // a, b: not in play; d: no source; e, f: bad section/status;
        // revise + retire both touch 7: ambiguous; merge: unknown op.
        assert_eq!(skipped, 8);
    }

    #[test]
    fn parse_counts_statement_characters_not_bytes() {
        let at_cap = "爵".repeat(MAX_STATEMENT_CHARS);
        let over = "爵".repeat(MAX_STATEMENT_CHARS + 1);
        let content = serde_json::json!({"ops": [
            {"op": "add", "section": "life", "status": "current", "statement": at_cap, "sources": ["o:4"]},
            {"op": "add", "section": "life", "status": "current", "statement": over, "sources": ["o:4"]},
            {"op": "add", "section": "life", "status": "current", "statement": "   ", "sources": ["o:4"]}
        ]})
        .to_string();
        let (ops, skipped) = parse(&content, &allowed()).unwrap();
        assert_eq!((ops.len(), skipped), (1, 2));
    }

    #[test]
    fn parse_skips_a_statement_holding_a_nul() {
        let content = serde_json::json!({"ops": [
            {"op": "add", "section": "life", "status": "current", "statement": "a\u{0}b", "sources": ["o:4"]},
            {"op": "revise", "entry": 7, "status": "current", "statement": "\u{0}"},
            {"op": "add", "section": "life", "status": "current", "statement": "ok", "sources": ["o:4"]}
        ]})
        .to_string();
        assert!(content.contains("\\u0000"));
        let (ops, skipped) = parse(&content, &allowed()).unwrap();
        assert_eq!((ops.len(), skipped), (1, 2));
    }

    #[test]
    fn parse_fails_only_when_there_is_no_ops_array() {
        for bad in ["I can't help with that.", "{\"answer\": 1}", "{\"ops\": 3}"] {
            assert!(parse(bad, &allowed()).is_err(), "{bad}");
        }
        assert_eq!(parse("{\"ops\": []}", &allowed()).unwrap(), (vec![], 0));
    }

    fn task(server: &MockServer) -> AutobioTask {
        AutobioTask {
            client: crate::llm::LlmClient::for_test(&LlmConfig {
                base_url: server.uri(),
                api_key: "k".into(),
                model: "m".into(),
            }),
            language: "en".into(),
            max_per_tick: 50,
            min_interval: chrono::Duration::hours(1),
        }
    }

    fn reply(content: impl Into<String>) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": content.into()}}],
            "usage": {"total_tokens": 10}
        }))
    }

    async fn sent(server: &MockServer, n: usize) -> serde_json::Value {
        let reqs = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&reqs[n].body).unwrap();
        serde_json::from_str(body["messages"][1]["content"].as_str().unwrap()).unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_tick_writes_entries_from_the_reply_and_the_next_one_sees_only_new_material(
        pool: PgPool,
    ) {
        use crate::db::autobio::tests::{at, export, join_pool, observed_id};
        let server = MockServer::start().await;
        let a = Uuid::new_v4();
        join_pool(&pool, a, true).await;
        export(&pool, a, at(0), &[("city", "Singapore")]).await;
        let o = observed_id(&pool, a, "city", "Singapore").await;
        Mock::given(method("POST"))
            .respond_with(reply(
                serde_json::json!({"ops": [{"op": "add", "section": "basics", "status": "current",
                    "statement": "Lives in Singapore", "sources": [format!("o:{o}")]}]})
                .to_string(),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let r = run_once(&pool, &task(&server)).await.unwrap();
        assert_eq!((r.users, r.added, r.failed), (1, 1, 0));
        let first = sent(&server, 0).await;
        assert_eq!(first["material"].as_array().unwrap().len(), 1);
        assert_eq!(
            store::current_entries(&pool, a).await.unwrap()[0].statement,
            "Lives in Singapore"
        );

        // Inside the interval nothing is due, whatever arrives.
        export(&pool, a, at(5), &[("city", "Tokyo")]).await;
        assert_eq!(
            run_once(&pool, &task(&server)).await.unwrap(),
            TickReport::default()
        );

        // Past it, only the new version is material.
        let mut t = task(&server);
        t.min_interval = chrono::Duration::zero();
        server.reset().await;
        Mock::given(method("POST"))
            .respond_with(reply(r#"{"ops": []}"#))
            .expect(1)
            .mount(&server)
            .await;
        run_once(&pool, &t).await.unwrap();
        let second = sent(&server, 0).await;
        assert_eq!(second["material"].as_array().unwrap().len(), 1);
        assert_eq!(second["material"][0]["text"], "Tokyo");
        assert_eq!(second["entries"].as_array().unwrap().len(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_unreadable_reply_writes_nothing_and_keeps_the_watermark(pool: PgPool) {
        use crate::db::autobio::tests::{at, export, join_pool};
        let server = MockServer::start().await;
        let a = Uuid::new_v4();
        join_pool(&pool, a, true).await;
        export(&pool, a, at(0), &[("city", "Singapore")]).await;
        Mock::given(method("POST"))
            .respond_with(reply("sorry, no"))
            .mount(&server)
            .await;
        let r = run_once(&pool, &task(&server)).await.unwrap();
        assert_eq!((r.users, r.failed), (1, 1));
        assert!(store::current_entries(&pool, a).await.unwrap().is_empty());
        let until: Option<chrono::DateTime<Utc>> = sqlx::query_scalar(
            "SELECT material_until FROM pothos.autobio_state WHERE user_id = $1",
        )
        .bind(a)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(until.is_none());
        // ran_at moved, so the user is not due again inside the interval.
        assert_eq!(
            run_once(&pool, &task(&server)).await.unwrap(),
            TickReport::default()
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_llm_failure_stamps_the_user_and_ends_the_tick(pool: PgPool) {
        use crate::db::autobio::tests::{at, export, join_pool};
        let server = MockServer::start().await;
        for _ in 0..2 {
            let id = Uuid::new_v4();
            join_pool(&pool, id, true).await;
            export(&pool, id, at(0), &[("city", "Singapore")]).await;
        }
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .expect(3)
            .mount(&server)
            .await;
        let r = run_once(&pool, &task(&server)).await.unwrap();
        assert_eq!((r.users, r.failed), (1, 1));
        let stamped: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.autobio_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            stamped, 1,
            "one call, one user stamped; the other waits for the next tick"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_user_who_leaves_before_their_turn_gets_no_call(pool: PgPool) {
        use crate::db::autobio::tests::{at, export, join_pool};
        let server = MockServer::start().await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        for (id, city) in [(a, "Singapore"), (b, "Tokyo")] {
            join_pool(&pool, id, true).await;
            export(&pool, id, at(0), &[("city", city)]).await;
        }
        // Both are due; `b` leaves while `a`'s call is in flight.
        Mock::given(method("POST"))
            .respond_with(reply(r#"{"ops": []}"#).set_delay(std::time::Duration::from_millis(500)))
            .expect(1)
            .mount(&server)
            .await;
        let t = task(&server);
        let leave = async {
            while server.received_requests().await.unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            crate::db::profiles::delete(&pool, b).await.unwrap();
        };
        let (r, ()) = tokio::join!(run_once(&pool, &t), leave);
        let r = r.unwrap();
        assert_eq!((r.users, r.failed), (1, 0));
        assert_eq!(sent(&server, 0).await["material"][0]["text"], "Singapore");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_refused_request_stamps_that_user_and_the_tick_goes_on(pool: PgPool) {
        use crate::db::autobio::tests::{at, export, join_pool, observed_id};
        let server = MockServer::start().await;
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        for (id, city) in [(a, "Singapore"), (b, "Tokyo")] {
            join_pool(&pool, id, true).await;
            export(&pool, id, at(0), &[("city", city)]).await;
        }
        let o = observed_id(&pool, b, "city", "Tokyo").await;
        Mock::given(method("POST"))
            .and(body_string_contains("Singapore"))
            .respond_with(ResponseTemplate::new(403))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_string_contains("Tokyo"))
            .respond_with(reply(
                serde_json::json!({"ops": [{"op": "add", "section": "basics", "status": "current",
                    "statement": "Lives in Tokyo", "sources": [format!("o:{o}")]}]})
                .to_string(),
            ))
            .expect(1)
            .mount(&server)
            .await;

        let r = run_once(&pool, &task(&server)).await.unwrap();
        assert_eq!((r.users, r.failed, r.added), (2, 1, 1));
        assert_eq!(
            sent(&server, 0).await["material"][0]["text"],
            "Singapore",
            "the refused user went first"
        );
        assert!(store::current_entries(&pool, a).await.unwrap().is_empty());
        assert_eq!(
            store::current_entries(&pool, b).await.unwrap()[0].statement,
            "Lives in Tokyo"
        );
        let stamped: i64 = sqlx::query_scalar("SELECT count(*) FROM pothos.autobio_state")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(stamped, 2);
    }
}
