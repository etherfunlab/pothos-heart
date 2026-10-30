//! Autobio entries and the material they are written from. An entry is one
//! statement under a fixed section; revising it retires the row and adds one
//! that points back, so retired rows are the revision history.
//!
//! The material is the fact mirror and the memory mirror. `recorded_at` on
//! both is this service's clock (a fact's moves again when the engine row
//! changes); `autobio_state.material_until` is how far a user's entries have
//! taken it in. The sweeper is the only writer of both tables and runs its
//! steps in sequence, so every row recorded before the autobio step reads the
//! clock is already committed. Phrase-ledger rows are matching material, not
//! autobio material; `o:` refs survive only as the history of old entries.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

/// Fixed sections, in the order the owner sees them.
pub const SECTIONS: [&str; 7] = [
    "basics",
    "life",
    "personality",
    "love",
    "intimacy",
    "outlook",
    "roleplay",
];

pub const STATUSES: [&str; 3] = ["current", "past", "planned"];

/// A piece of material, as an entry cites it and the model sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceRef {
    /// A phrase-ledger row: history on pre-v0.4 entries, never new material.
    Observed(i64),
    Memory(Uuid),
    Fact(Uuid),
}

impl SourceRef {
    pub fn tag(&self) -> String {
        match self {
            Self::Observed(id) => format!("o:{id}"),
            Self::Memory(id) => format!("m:{id}"),
            Self::Fact(id) => format!("f:{id}"),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let (kind, id) = s.split_once(':')?;
        match kind {
            "o" => id.parse().ok().map(Self::Observed),
            "m" => id.parse().ok().map(Self::Memory),
            "f" => id.parse().ok().map(Self::Fact),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Material {
    pub source: SourceRef,
    /// `fact/<category>` or `memory/<category>`.
    pub kind: String,
    pub text: String,
    /// A memory's extraction hints verbatim; a fact's labels, counters,
    /// `last_seen_at` and `seen_again`, built here.
    pub metadata: Option<serde_json::Value>,
    /// When the engine first said it: `first_seen_at` / `engine_created_at`.
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub section: String,
    pub status: String,
    pub statement: String,
    /// The latest of its sources' times; `None` once every source is gone.
    pub as_of: Option<DateTime<Utc>>,
    pub sources: Vec<SourceRef>,
}

#[derive(Debug, Clone, PartialEq, sqlx::FromRow)]
pub struct Due {
    pub user_id: Uuid,
    pub material_until: Option<DateTime<Utc>>,
}

/// The database clock: every watermark compares with `recorded_at`, which
/// the database stamps.
pub async fn db_now(pool: &PgPool) -> sqlx::Result<DateTime<Utc>> {
    sqlx::query_scalar("SELECT now()").fetch_one(pool).await
}

/// Users whose autobio is due: they have a `match_profiles` row (any
/// `active`), material recorded after their watermark, and no run since
/// `ran_before`. Oldest pending material first.
pub async fn due_users(
    pool: &PgPool,
    ran_before: DateTime<Utc>,
    limit: i64,
) -> sqlx::Result<Vec<Due>> {
    sqlx::query_as(
        "WITH material AS ( \
            SELECT user_id, recorded_at FROM pothos.fact_mirror \
            UNION ALL \
            SELECT user_id, recorded_at FROM pothos.memory_mirror \
         ) \
         SELECT m.user_id, s.material_until \
         FROM material m \
         JOIN pothos.match_profiles p USING (user_id) \
         LEFT JOIN pothos.autobio_state s USING (user_id) \
         WHERE (s.material_until IS NULL OR m.recorded_at > s.material_until) \
           AND (s.ran_at IS NULL OR s.ran_at <= $1) \
         GROUP BY m.user_id, s.material_until \
         ORDER BY min(m.recorded_at), m.user_id \
         LIMIT $2",
    )
    .bind(ran_before)
    .bind(limit)
    .fetch_all(pool)
    .await
}

/// One user's material recorded after `after` (all of it when `None`),
/// oldest last said first: facts order by `last_seen_at`, memories by
/// `engine_created_at`, so an old fact said again reads as recent.
pub async fn material(
    pool: &PgPool,
    user_id: Uuid,
    after: Option<DateTime<Utc>>,
) -> sqlx::Result<Vec<Material>> {
    #[derive(sqlx::FromRow)]
    struct FactMaterial {
        id: Uuid,
        content: String,
        category: String,
        domain: String,
        evidence_type: String,
        temporality: String,
        persistence: String,
        confidence: String,
        occurrences: i32,
        session_count: i32,
        recurrence: String,
        first_seen_at: DateTime<Utc>,
        last_seen_at: DateTime<Utc>,
    }
    let facts: Vec<FactMaterial> = sqlx::query_as(
        "SELECT id, content, category, domain, evidence_type, temporality, persistence, \
                confidence, occurrences, session_count, recurrence, first_seen_at, last_seen_at \
         FROM pothos.fact_mirror \
         WHERE user_id = $1 AND ($2::timestamptz IS NULL OR recorded_at > $2)",
    )
    .bind(user_id)
    .bind(after)
    .fetch_all(pool)
    .await?;
    type MemoryRow = (
        Uuid,
        String,
        String,
        Option<serde_json::Value>,
        DateTime<Utc>,
    );
    let memories: Vec<MemoryRow> = sqlx::query_as(
        "SELECT id, category, content, metadata, engine_created_at FROM pothos.memory_mirror \
             WHERE user_id = $1 AND ($2::timestamptz IS NULL OR recorded_at > $2)",
    )
    .bind(user_id)
    .bind(after)
    .fetch_all(pool)
    .await?;
    let mut out: Vec<(DateTime<Utc>, Material)> = facts
        .into_iter()
        .map(|f| {
            // A fact first said before the watermark is back because it was
            // said again: it confirms, it is not news.
            let seen_again = after.is_some_and(|a| f.first_seen_at <= a);
            let metadata = serde_json::json!({
                "domain": f.domain,
                "evidence_type": f.evidence_type,
                "temporality": f.temporality,
                "persistence": f.persistence,
                "confidence": f.confidence,
                "recurrence": f.recurrence,
                "occurrences": f.occurrences,
                "session_count": f.session_count,
                "last_seen_at": f.last_seen_at
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                "seen_again": seen_again,
            });
            (
                f.last_seen_at,
                Material {
                    source: SourceRef::Fact(f.id),
                    kind: format!("fact/{}", f.category),
                    text: f.content,
                    metadata: Some(metadata),
                    at: f.first_seen_at,
                },
            )
        })
        .chain(
            memories
                .into_iter()
                .map(|(id, category, text, metadata, at)| {
                    (
                        at,
                        Material {
                            source: SourceRef::Memory(id),
                            kind: format!("memory/{category}"),
                            text,
                            metadata,
                            at,
                        },
                    )
                }),
        )
        .collect();
    out.sort_by(|(a_ts, a), (b_ts, b)| {
        a_ts.cmp(b_ts)
            .then_with(|| a.source.tag().cmp(&b.source.tag()))
    });
    Ok(out.into_iter().map(|(_, m)| m).collect())
}

/// A user's current entries with their sources, oldest first.
pub async fn current_entries(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<Entry>> {
    let rows: Vec<(i64, String, String, String)> = sqlx::query_as(
        "SELECT id, section, status, statement FROM pothos.autobio_entries \
         WHERE user_id = $1 AND retired_at IS NULL ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let observed: Vec<(i64, i64, DateTime<Utc>)> = sqlx::query_as(
        "SELECT s.entry_id, s.observed_id, o.first_seen_at \
         FROM pothos.autobio_observed_sources s \
         JOIN pothos.autobio_entries e ON e.id = s.entry_id \
         JOIN pothos.observed_phrases o ON o.id = s.observed_id \
         WHERE e.user_id = $1 AND e.retired_at IS NULL ORDER BY s.observed_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let memories: Vec<(i64, Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT s.entry_id, s.memory_id, m.engine_created_at \
         FROM pothos.autobio_memory_sources s \
         JOIN pothos.autobio_entries e ON e.id = s.entry_id \
         JOIN pothos.memory_mirror m ON m.id = s.memory_id \
         WHERE e.user_id = $1 AND e.retired_at IS NULL ORDER BY s.memory_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let facts: Vec<(i64, Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT s.entry_id, s.fact_id, f.first_seen_at \
         FROM pothos.autobio_fact_sources s \
         JOIN pothos.autobio_entries e ON e.id = s.entry_id \
         JOIN pothos.fact_mirror f ON f.id = s.fact_id \
         WHERE e.user_id = $1 AND e.retired_at IS NULL ORDER BY s.fact_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let mut sources: HashMap<i64, Vec<(SourceRef, DateTime<Utc>)>> = HashMap::new();
    for (entry, id, at) in observed {
        sources
            .entry(entry)
            .or_default()
            .push((SourceRef::Observed(id), at));
    }
    for (entry, id, at) in memories {
        sources
            .entry(entry)
            .or_default()
            .push((SourceRef::Memory(id), at));
    }
    for (entry, id, at) in facts {
        sources
            .entry(entry)
            .or_default()
            .push((SourceRef::Fact(id), at));
    }
    Ok(rows
        .into_iter()
        .map(|(id, section, status, statement)| {
            let cited = sources.remove(&id).unwrap_or_default();
            Entry {
                id,
                section,
                status,
                statement,
                as_of: cited.iter().map(|(_, at)| *at).max(),
                sources: cited.into_iter().map(|(s, _)| s).collect(),
            }
        })
        .collect())
}

/// Statements the owner deleted, oldest first.
pub async fn vetoed_statements(pool: &PgPool, user_id: Uuid) -> sqlx::Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT statement FROM pothos.autobio_entries \
         WHERE user_id = $1 AND retired_reason = 'vetoed' ORDER BY id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
}

/// The owner deletes one of their current entries. `false` if it is not
/// theirs or no longer current.
pub async fn veto(pool: &PgPool, user_id: Uuid, entry: i64) -> sqlx::Result<bool> {
    let n = sqlx::query(
        "UPDATE pothos.autobio_entries SET retired_at = now(), retired_reason = 'vetoed' \
         WHERE id = $1 AND user_id = $2 AND retired_at IS NULL",
    )
    .bind(entry)
    .bind(user_id)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(n > 0)
}

/// The autobio version of each listed user who has at least one current entry.
pub async fn versions(pool: &PgPool, users: &[Uuid]) -> sqlx::Result<HashMap<Uuid, DateTime<Utc>>> {
    let rows: Vec<(Uuid, DateTime<Utc>)> = sqlx::query_as(
        "SELECT user_id, version FROM pothos.autobio_versions \
         WHERE user_id = ANY($1) AND has_current",
    )
    .bind(users)
    .fetch_all(pool)
    .await?;
    Ok(rows.into_iter().collect())
}

/// One validated change from a model reply.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    Add {
        section: String,
        status: String,
        statement: String,
        sources: Vec<SourceRef>,
    },
    /// Keeps the entry's section and sources; `sources` are added to them.
    Revise {
        entry: i64,
        status: String,
        statement: String,
        sources: Vec<SourceRef>,
    },
    Retire {
        entry: i64,
    },
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Applied {
    pub added: usize,
    pub revised: usize,
    pub retired: usize,
}

/// Whether the user is still in the pool. Takes a share lock on the profile
/// row, so a concurrent leave waits until the caller's transaction ends.
async fn in_pool(conn: &mut PgConnection, user_id: Uuid) -> sqlx::Result<bool> {
    let row = sqlx::query("SELECT 1 FROM pothos.match_profiles WHERE user_id = $1 FOR SHARE")
        .bind(user_id)
        .fetch_optional(&mut *conn)
        .await?;
    Ok(row.is_some())
}

/// Whether the user is in the pool at this moment; no lock outlives the call.
pub async fn in_pool_now(pool: &PgPool, user_id: Uuid) -> sqlx::Result<bool> {
    in_pool(&mut *pool.acquire().await?, user_id).await
}

/// Whether every fact and memory source is still mirrored. An erasure
/// tombstone can remove them while the model is answering; an op citing a
/// vanished source is skipped, never a failed transaction.
async fn sources_exist(conn: &mut PgConnection, sources: &[SourceRef]) -> sqlx::Result<bool> {
    let ids = |f: fn(&SourceRef) -> Option<Uuid>| -> Vec<Uuid> {
        sources.iter().filter_map(f).collect()
    };
    for (table, column, wanted) in [
        (
            "fact_mirror",
            "id",
            ids(|s| match s {
                SourceRef::Fact(id) => Some(*id),
                _ => None,
            }),
        ),
        (
            "memory_mirror",
            "id",
            ids(|s| match s {
                SourceRef::Memory(id) => Some(*id),
                _ => None,
            }),
        ),
    ] {
        if wanted.is_empty() {
            continue;
        }
        let n: i64 = sqlx::query_scalar(&format!(
            "SELECT count(*) FROM pothos.{table} WHERE {column} = ANY($1)"
        ))
        .bind(&wanted)
        .fetch_one(&mut *conn)
        .await?;
        if n as usize != wanted.len() {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Apply one reply's operations and move the watermark, in one transaction.
/// An operation on an entry that is no longer current (the owner deleted it
/// while the model was answering) is dropped. A user who is no longer in
/// `match_profiles` (they left while the model was answering) gets nothing
/// written, not even the watermark; the row lock serialises it with a
/// concurrent leave.
pub async fn apply(
    pool: &PgPool,
    user_id: Uuid,
    ops: &[Op],
    material_until: DateTime<Utc>,
    model: &str,
) -> sqlx::Result<Applied> {
    let mut tx = pool.begin().await?;
    if !in_pool(&mut tx, user_id).await? {
        return Ok(Applied::default());
    }
    let mut done = Applied::default();
    for op in ops {
        match op {
            Op::Add {
                section,
                status,
                statement,
                sources,
            } => {
                if !sources_exist(&mut tx, sources).await? {
                    continue;
                }
                let id = insert_entry(&mut tx, user_id, section, status, statement, None).await?;
                link(&mut tx, id, sources).await?;
                done.added += 1;
            }
            Op::Revise {
                entry,
                status,
                statement,
                sources,
            } => {
                if !sources_exist(&mut tx, sources).await? {
                    continue;
                }
                let Some(section) = retire(&mut tx, user_id, *entry, "revised").await? else {
                    continue;
                };
                let id = insert_entry(&mut tx, user_id, &section, status, statement, Some(*entry))
                    .await?;
                carry_sources(&mut tx, *entry, id).await?;
                link(&mut tx, id, sources).await?;
                done.revised += 1;
            }
            Op::Retire { entry } => {
                if retire(&mut tx, user_id, *entry, "retracted")
                    .await?
                    .is_some()
                {
                    done.retired += 1;
                }
            }
        }
    }
    record(&mut tx, user_id, Some(material_until), model).await?;
    tx.commit().await?;
    Ok(done)
}

/// A run that wrote nothing: the time is stamped so the user waits a full
/// interval before the next try; the watermark stays. Writes nothing for a
/// user who is no longer in `match_profiles`.
pub async fn record_failure(pool: &PgPool, user_id: Uuid, model: &str) -> sqlx::Result<()> {
    let mut tx = pool.begin().await?;
    if !in_pool(&mut tx, user_id).await? {
        return Ok(());
    }
    record(&mut tx, user_id, None, model).await?;
    tx.commit().await
}

async fn record(
    conn: &mut PgConnection,
    user_id: Uuid,
    material_until: Option<DateTime<Utc>>,
    model: &str,
) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO pothos.autobio_state (user_id, material_until, ran_at, model) \
         VALUES ($1, $2, now(), $3) \
         ON CONFLICT (user_id) DO UPDATE SET \
            material_until = COALESCE(EXCLUDED.material_until, pothos.autobio_state.material_until), \
            ran_at = now(), model = EXCLUDED.model",
    )
    .bind(user_id)
    .bind(material_until)
    .bind(model)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Retire a current entry of this user; its section, or `None` if it was
/// not current.
async fn retire(
    conn: &mut PgConnection,
    user_id: Uuid,
    entry: i64,
    reason: &str,
) -> sqlx::Result<Option<String>> {
    sqlx::query_scalar(
        "UPDATE pothos.autobio_entries SET retired_at = now(), retired_reason = $3 \
         WHERE id = $1 AND user_id = $2 AND retired_at IS NULL RETURNING section",
    )
    .bind(entry)
    .bind(user_id)
    .bind(reason)
    .fetch_optional(&mut *conn)
    .await
}

async fn insert_entry(
    conn: &mut PgConnection,
    user_id: Uuid,
    section: &str,
    status: &str,
    statement: &str,
    supersedes: Option<i64>,
) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "INSERT INTO pothos.autobio_entries (user_id, section, status, statement, supersedes) \
         VALUES ($1, $2, $3, $4, $5) RETURNING id",
    )
    .bind(user_id)
    .bind(section)
    .bind(status)
    .bind(statement)
    .bind(supersedes)
    .fetch_one(&mut *conn)
    .await
}

async fn carry_sources(conn: &mut PgConnection, from: i64, to: i64) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO pothos.autobio_observed_sources (entry_id, observed_id) \
         SELECT $2, observed_id FROM pothos.autobio_observed_sources WHERE entry_id = $1",
    )
    .bind(from)
    .bind(to)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO pothos.autobio_memory_sources (entry_id, memory_id) \
         SELECT $2, memory_id FROM pothos.autobio_memory_sources WHERE entry_id = $1",
    )
    .bind(from)
    .bind(to)
    .execute(&mut *conn)
    .await?;
    sqlx::query(
        "INSERT INTO pothos.autobio_fact_sources (entry_id, fact_id) \
         SELECT $2, fact_id FROM pothos.autobio_fact_sources WHERE entry_id = $1",
    )
    .bind(from)
    .bind(to)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

async fn link(conn: &mut PgConnection, entry: i64, sources: &[SourceRef]) -> sqlx::Result<()> {
    for s in sources {
        match s {
            SourceRef::Observed(id) => {
                sqlx::query(
                    "INSERT INTO pothos.autobio_observed_sources (entry_id, observed_id) \
                     VALUES ($1, $2) ON CONFLICT DO NOTHING",
                )
                .bind(entry)
                .bind(id)
                .execute(&mut *conn)
                .await?;
            }
            SourceRef::Memory(id) => {
                sqlx::query(
                    "INSERT INTO pothos.autobio_memory_sources (entry_id, memory_id) \
                     VALUES ($1, $2) ON CONFLICT DO NOTHING",
                )
                .bind(entry)
                .bind(id)
                .execute(&mut *conn)
                .await?;
            }
            SourceRef::Fact(id) => {
                sqlx::query(
                    "INSERT INTO pothos.autobio_fact_sources (entry_id, fact_id) \
                     VALUES ($1, $2) ON CONFLICT DO NOTHING",
                )
                .bind(entry)
                .bind(id)
                .execute(&mut *conn)
                .await?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use chrono::{DateTime, TimeZone, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::db::mirror::{self, MirrorRow};
    use crate::db::profiles::{self, ProfileWrite};

    pub(crate) fn at(min: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 10, 15, 12, min, 0).unwrap()
    }

    pub(crate) async fn join_pool(pool: &PgPool, id: Uuid, active: bool) {
        profiles::upsert(
            pool,
            id,
            &ProfileWrite {
                gender: "f".into(),
                seeking_genders: vec!["m".into()],
                birth_date: None,
                country: None,
                region: None,
                city: None,
                age_min: None,
                age_max: None,
                active,
            },
        )
        .await
        .unwrap();
    }

    pub(crate) async fn export(pool: &PgPool, id: Uuid, t: DateTime<Utc>, interests: &[&str]) {
        mirror::upsert_one(
            pool,
            &MirrorRow {
                user_id: id,
                interests: interests.iter().map(|s| s.to_string()).collect(),
                personality_traits: vec![],
                deal_breakers: vec![],
                engine_updated_at: t,
            },
        )
        .await
        .unwrap();
    }

    /// One ledger fact mirrored as the sweeper would store it.
    pub(crate) async fn fact(
        pool: &PgPool,
        user: Uuid,
        content: &str,
        first: DateTime<Utc>,
        last: DateTime<Utc>,
    ) -> Uuid {
        let id = Uuid::new_v4();
        crate::db::facts::apply_page(
            pool,
            &[crate::engine_client::FactRow {
                id,
                user_id: user,
                content: content.into(),
                category: "preference".into(),
                domain: "hobby".into(),
                evidence_type: "explicit_statement".into(),
                temporality: "current".into(),
                persistence: "ongoing".into(),
                confidence: "medium".into(),
                occurrences: 1,
                session_count: 1,
                recurrence: "once".into(),
                first_seen_at: first,
                last_seen_at: last,
            }],
        )
        .await
        .unwrap();
        id
    }

    /// The same fact seen again: counters up, `last_seen_at` moved.
    pub(crate) async fn fact_seen_again(pool: &PgPool, id: Uuid, user: Uuid, last: DateTime<Utc>) {
        sqlx::query(
            "UPDATE pothos.fact_mirror SET occurrences = occurrences + 1, session_count = 2, \
                recurrence = 'repeated_across_sessions', last_seen_at = $2, recorded_at = now() \
             WHERE id = $1 AND user_id = $3",
        )
        .bind(id)
        .bind(last)
        .bind(user)
        .execute(pool)
        .await
        .unwrap();
    }

    pub(crate) async fn memory(pool: &PgPool, user: Uuid, content: &str, t: DateTime<Utc>) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO pothos.memory_mirror (id, user_id, content, category, metadata, engine_created_at) \
             VALUES ($1, $2, $3, 'preference', '{\"domain\":\"hobby\"}', $4)",
        )
        .bind(id)
        .bind(user)
        .bind(content)
        .bind(t)
        .execute(pool)
        .await
        .unwrap();
        id
    }

    pub(crate) async fn observed_id(
        pool: &PgPool,
        user: Uuid,
        dimension: &str,
        phrase: &str,
    ) -> i64 {
        sqlx::query_scalar(
            "SELECT id FROM pothos.observed_phrases WHERE user_id = $1 AND dimension = $2 AND phrase = $3",
        )
        .bind(user)
        .bind(dimension)
        .bind(phrase)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// An entry written straight to the table, for fixtures.
    pub(crate) async fn entry(pool: &PgPool, user: Uuid, section: &str, statement: &str) -> i64 {
        sqlx::query_scalar(
            "INSERT INTO pothos.autobio_entries (user_id, section, status, statement) \
             VALUES ($1, $2, 'current', $3) RETURNING id",
        )
        .bind(user)
        .bind(section)
        .bind(statement)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[test]
    fn source_refs_round_trip_and_reject_junk() {
        let m = Uuid::new_v4();
        for r in [
            SourceRef::Observed(12),
            SourceRef::Memory(m),
            SourceRef::Fact(m),
        ] {
            assert_eq!(SourceRef::parse(&r.tag()), Some(r));
        }
        assert_eq!(SourceRef::Observed(12).tag(), "o:12");
        assert_eq!(SourceRef::Fact(m).tag(), format!("f:{m}"));
        for junk in ["", "o:", "o:x", "m:not-a-uuid", "f:12", "x:1", "12", "o:1:2"] {
            assert_eq!(SourceRef::parse(junk), None, "{junk}");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn material_renders_a_fact_with_its_labels(pool: PgPool) {
        let a = Uuid::new_v4();
        let f = fact(&pool, a, "plays weekend jazz", at(0), at(3)).await;
        let other = Uuid::new_v4();
        fact(&pool, other, "not mine", at(0), at(0)).await;

        let all = material(&pool, a, None).await.unwrap();
        assert_eq!(all.len(), 1);
        let got = &all[0];
        assert_eq!(got.source, SourceRef::Fact(f));
        assert_eq!(got.kind, "fact/preference");
        assert_eq!(got.text, "plays weekend jazz");
        assert_eq!(got.at, at(0), "`at` is when it was first said");
        let meta = got.metadata.as_ref().unwrap();
        assert_eq!(meta["domain"], "hobby");
        assert_eq!(meta["evidence_type"], "explicit_statement");
        assert_eq!(meta["temporality"], "current");
        assert_eq!(meta["persistence"], "ongoing");
        assert_eq!(meta["confidence"], "medium");
        assert_eq!(meta["recurrence"], "once");
        assert_eq!(meta["occurrences"], 1);
        assert_eq!(meta["session_count"], 1);
        assert_eq!(
            meta["last_seen_at"],
            at(3).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
        );
        assert_eq!(meta["seen_again"], false, "no watermark: nothing is a repeat");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_fact_reseen_after_the_watermark_is_material_again_marked_seen_again(pool: PgPool) {
        let a = Uuid::new_v4();
        let f = fact(&pool, a, "plays weekend jazz", at(0), at(0)).await;
        let watermark = db_now(&pool).await.unwrap();
        assert!(material(&pool, a, Some(watermark)).await.unwrap().is_empty());

        fact_seen_again(&pool, f, a, at(5)).await;
        let later = material(&pool, a, Some(watermark)).await.unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].source, SourceRef::Fact(f));
        let meta = later[0].metadata.as_ref().unwrap();
        assert_eq!(meta["seen_again"], true);
        assert_eq!(meta["recurrence"], "repeated_across_sessions");
        assert_eq!(meta["occurrences"], 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn material_is_ordered_by_when_it_was_last_said(pool: PgPool) {
        let a = Uuid::new_v4();
        // An old fact re-seen at t9 sorts after a memory from t7, even though
        // the fact was first said at t0.
        let f = fact(&pool, a, "plays weekend jazz", at(0), at(9)).await;
        let m = memory(&pool, a, "likes hiking", at(7)).await;

        let all = material(&pool, a, None).await.unwrap();
        assert_eq!(
            all.iter().map(|x| x.source).collect::<Vec<_>>(),
            vec![SourceRef::Memory(m), SourceRef::Fact(f)]
        );
        assert_eq!(all[1].at, at(0), "`at` still shows the first time");
        assert_eq!(all[0].metadata.as_ref().unwrap()["domain"], "hobby");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn due_users_are_pool_members_with_new_material_past_the_interval(pool: PgPool) {
        let (fresh, waiting, outsider, inactive) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        for (id, active) in [(fresh, true), (waiting, true), (inactive, false)] {
            join_pool(&pool, id, active).await;
        }
        for id in [fresh, waiting, outsider, inactive] {
            fact(&pool, id, "plays weekend jazz", at(0), at(0)).await;
        }
        // A phrase-ledger row is matching material, not autobio material.
        let ledger_only = Uuid::new_v4();
        join_pool(&pool, ledger_only, true).await;
        export(&pool, ledger_only, at(0), &["steady walks"]).await;
        // `waiting` ran a moment ago and has taken in everything so far.
        let now = db_now(&pool).await.unwrap();
        sqlx::query(
            "INSERT INTO pothos.autobio_state (user_id, material_until, ran_at, model) VALUES ($1, $2, $2, 'm')",
        )
        .bind(waiting)
        .bind(now)
        .execute(&pool)
        .await
        .unwrap();

        // Oldest pending material first; an active:false row still counts as joined.
        let due = due_users(&pool, now - chrono::Duration::hours(1), 10)
            .await
            .unwrap();
        let ids: Vec<Uuid> = due.iter().map(|d| d.user_id).collect();
        assert_eq!(ids, vec![fresh, inactive]);
        assert!(
            !ids.contains(&ledger_only),
            "ledger-only material does not make a user due"
        );
        assert!(due.iter().all(|d| d.material_until.is_none()));

        // The interval has passed for `waiting`, but nothing new has arrived: not due.
        let elapsed = db_now(&pool).await.unwrap();
        let idle = due_users(&pool, elapsed, 10).await.unwrap();
        assert!(
            !idle.iter().any(|d| d.user_id == waiting),
            "no new material, not due"
        );

        // New material for `waiting`: still not due inside the interval, due after it.
        memory(&pool, waiting, "took up pottery", at(5)).await;
        let later = db_now(&pool).await.unwrap();
        let inside = due_users(&pool, now - chrono::Duration::hours(1), 10)
            .await
            .unwrap();
        assert!(!inside.iter().any(|d| d.user_id == waiting));
        let past = due_users(&pool, later, 10).await.unwrap();
        let w = past.iter().find(|d| d.user_id == waiting).unwrap();
        assert_eq!(w.material_until, Some(now));

        assert_eq!(due_users(&pool, later, 1).await.unwrap().len(), 1, "limit");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn current_entries_carry_sources_and_the_latest_source_time(pool: PgPool) {
        let a = Uuid::new_v4();
        export(&pool, a, at(0), &["singapore nights"]).await;
        let o = observed_id(&pool, a, "interests", "singapore nights").await;
        let m = memory(&pool, a, "moved here in 2020", at(7)).await;
        let f = fact(&pool, a, "works near the marina", at(9), at(9)).await;
        let e = entry(&pool, a, "basics", "Lives in Singapore").await;
        sqlx::query("INSERT INTO pothos.autobio_observed_sources VALUES ($1, $2)")
            .bind(e)
            .bind(o)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pothos.autobio_memory_sources VALUES ($1, $2)")
            .bind(e)
            .bind(m)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO pothos.autobio_fact_sources VALUES ($1, $2)")
            .bind(e)
            .bind(f)
            .execute(&pool)
            .await
            .unwrap();
        let bare = entry(&pool, a, "life", "No sources left").await;
        let gone = entry(&pool, a, "life", "Retired").await;
        sqlx::query(
            "UPDATE pothos.autobio_entries SET retired_at = now(), retired_reason = 'vetoed' WHERE id = $1",
        )
        .bind(gone)
        .execute(&pool)
        .await
        .unwrap();

        let got = current_entries(&pool, a).await.unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, e);
        assert_eq!(
            got[0].as_of,
            Some(at(9)),
            "the fact's first_seen_at is its anchor"
        );
        let mut sources = got[0].sources.clone();
        sources.sort_by_key(SourceRef::tag);
        let mut expected = vec![
            SourceRef::Observed(o),
            SourceRef::Memory(m),
            SourceRef::Fact(f),
        ];
        expected.sort_by_key(SourceRef::tag);
        assert_eq!(sources, expected);
        assert_eq!(
            (got[1].id, got[1].as_of, got[1].sources.len()),
            (bare, None, 0)
        );
        assert_eq!(vetoed_statements(&pool, a).await.unwrap(), vec!["Retired"]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn versions_cover_users_with_a_current_entry_and_move_on_every_change(pool: PgPool) {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let e = entry(&pool, a, "life", "x").await;
        let only = entry(&pool, b, "life", "y").await;
        sqlx::query(
            "UPDATE pothos.autobio_entries SET retired_at = now(), retired_reason = 'vetoed' WHERE id = $1",
        )
        .bind(only)
        .execute(&pool)
        .await
        .unwrap();

        let v = versions(&pool, &[a, b, c]).await.unwrap();
        assert_eq!(
            v.keys().copied().collect::<Vec<_>>(),
            vec![a],
            "b has no current entry, c none at all"
        );
        let before = v[&a];
        sqlx::query(
            "UPDATE pothos.autobio_entries SET retired_at = clock_timestamp(), retired_reason = 'vetoed' WHERE id = $1",
        )
        .bind(e)
        .execute(&pool)
        .await
        .unwrap();
        entry(&pool, a, "life", "z").await;
        let after = versions(&pool, &[a]).await.unwrap()[&a];
        assert!(after > before);
    }

    async fn state(pool: &PgPool, user: Uuid) -> (Option<DateTime<Utc>>, String) {
        sqlx::query_as("SELECT material_until, model FROM pothos.autobio_state WHERE user_id = $1")
            .bind(user)
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn apply_adds_revises_and_retires_and_moves_the_watermark(pool: PgPool) {
        let a = Uuid::new_v4();
        join_pool(&pool, a, true).await;
        let f = fact(&pool, a, "single for two years", at(0), at(0)).await;
        let m = memory(&pool, a, "seeing someone lately", at(9)).await;
        let until = db_now(&pool).await.unwrap();

        let done = apply(
            &pool,
            a,
            &[Op::Add {
                section: "love".into(),
                status: "current".into(),
                statement: "Has been single for two years".into(),
                sources: vec![SourceRef::Fact(f)],
            }],
            until,
            "m1",
        )
        .await
        .unwrap();
        assert_eq!(
            done,
            Applied {
                added: 1,
                ..Default::default()
            }
        );
        assert_eq!(state(&pool, a).await, (Some(until), "m1".to_string()));
        let first = current_entries(&pool, a).await.unwrap().remove(0);

        let doomed = entry(&pool, a, "life", "Misread").await;
        let until2 = db_now(&pool).await.unwrap();
        let done = apply(
            &pool,
            a,
            &[
                Op::Revise {
                    entry: first.id,
                    status: "past".into(),
                    statement: "Was single for two years; now seeing someone".into(),
                    sources: vec![SourceRef::Memory(m)],
                },
                Op::Retire { entry: doomed },
            ],
            until2,
            "m2",
        )
        .await
        .unwrap();
        assert_eq!(
            done,
            Applied {
                added: 0,
                revised: 1,
                retired: 1
            }
        );

        let now = current_entries(&pool, a).await.unwrap();
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].section, "love", "a revision keeps the section");
        assert_eq!(now[0].status, "past");
        assert_eq!(
            now[0].sources,
            vec![SourceRef::Memory(m), SourceRef::Fact(f)],
            "old sources carried, new one added"
        );
        let (supersedes, reason): (Option<i64>, Option<String>) = sqlx::query_as(
            "SELECT n.supersedes, o.retired_reason FROM pothos.autobio_entries n \
             JOIN pothos.autobio_entries o ON o.id = n.supersedes WHERE n.id = $1",
        )
        .bind(now[0].id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (supersedes, reason.as_deref()),
            (Some(first.id), Some("revised"))
        );
        let retracted: Option<String> =
            sqlx::query_scalar("SELECT retired_reason FROM pothos.autobio_entries WHERE id = $1")
                .bind(doomed)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(retracted.as_deref(), Some("retracted"));
        assert_eq!(state(&pool, a).await, (Some(until2), "m2".to_string()));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_revise_carries_pre_v04_observed_sources_to_the_new_entry(pool: PgPool) {
        let a = Uuid::new_v4();
        join_pool(&pool, a, true).await;
        export(&pool, a, at(0), &["jazz nights"]).await;
        let o = observed_id(&pool, a, "interests", "jazz nights").await;
        let e = entry(&pool, a, "life", "Out for jazz most weekends").await;
        sqlx::query("INSERT INTO pothos.autobio_observed_sources VALUES ($1, $2)")
            .bind(e)
            .bind(o)
            .execute(&pool)
            .await
            .unwrap();
        let m = memory(&pool, a, "prefers quiet bars now", at(5)).await;
        let until = db_now(&pool).await.unwrap();

        apply(
            &pool,
            a,
            &[Op::Revise {
                entry: e,
                status: "past".into(),
                statement: "Used to go out for jazz".into(),
                sources: vec![SourceRef::Memory(m)],
            }],
            until,
            "m",
        )
        .await
        .unwrap();

        let got = current_entries(&pool, a).await.unwrap();
        assert_eq!(got.len(), 1);
        let mut sources = got[0].sources.clone();
        sources.sort_by_key(SourceRef::tag);
        let mut expected = vec![SourceRef::Memory(m), SourceRef::Observed(o)];
        expected.sort_by_key(SourceRef::tag);
        assert_eq!(sources, expected, "the o: history rides the revision");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_revise_of_an_entry_vetoed_mid_call_adds_nothing(pool: PgPool) {
        let a = Uuid::new_v4();
        join_pool(&pool, a, true).await;
        let e = entry(&pool, a, "intimacy", "Something the owner removed").await;
        // The owner deletes it while the model is still answering.
        sqlx::query(
            "UPDATE pothos.autobio_entries SET retired_at = now(), retired_reason = 'vetoed' WHERE id = $1",
        )
        .bind(e)
        .execute(&pool)
        .await
        .unwrap();
        let until = db_now(&pool).await.unwrap();
        let done = apply(
            &pool,
            a,
            &[
                Op::Revise {
                    entry: e,
                    status: "current".into(),
                    statement: "The same thing, reworded".into(),
                    sources: vec![],
                },
                Op::Retire { entry: e },
            ],
            until,
            "m",
        )
        .await
        .unwrap();
        assert_eq!(done, Applied::default());
        assert!(current_entries(&pool, a).await.unwrap().is_empty());
        let reason: Option<String> =
            sqlx::query_scalar("SELECT retired_reason FROM pothos.autobio_entries WHERE id = $1")
                .bind(e)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(reason.as_deref(), Some("vetoed"), "the veto stands");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_failed_run_stamps_the_time_and_keeps_the_watermark(pool: PgPool) {
        let a = Uuid::new_v4();
        join_pool(&pool, a, true).await;
        record_failure(&pool, a, "m").await.unwrap();
        assert_eq!(state(&pool, a).await, (None, "m".to_string()));
        let until = db_now(&pool).await.unwrap();
        apply(&pool, a, &[], until, "m").await.unwrap();
        record_failure(&pool, a, "m2").await.unwrap();
        assert_eq!(state(&pool, a).await, (Some(until), "m2".to_string()));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn an_op_whose_source_was_erased_is_skipped_not_fatal(pool: PgPool) {
        let a = Uuid::new_v4();
        join_pool(&pool, a, true).await;
        let f = fact(&pool, a, "plays weekend jazz", at(0), at(0)).await;
        let m = memory(&pool, a, "likes hiking", at(1)).await;
        let until = db_now(&pool).await.unwrap();
        // A tombstone lands between the material read and the write.
        sqlx::query("DELETE FROM pothos.fact_mirror WHERE id = $1")
            .bind(f)
            .execute(&pool)
            .await
            .unwrap();

        let add = |statement: &str, source: SourceRef| Op::Add {
            section: "life".into(),
            status: "current".into(),
            statement: statement.into(),
            sources: vec![source],
        };
        let done = apply(
            &pool,
            a,
            &[
                add("Plays jazz on weekends", SourceRef::Fact(f)),
                add("Likes hiking", SourceRef::Memory(m)),
            ],
            until,
            "m",
        )
        .await
        .unwrap();
        assert_eq!(
            done,
            Applied {
                added: 1,
                ..Default::default()
            },
            "the erased source's op is skipped, its sibling lands"
        );
        let got = current_entries(&pool, a).await.unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].statement, "Likes hiking");
        assert_eq!(state(&pool, a).await, (Some(until), "m".to_string()));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn nothing_is_written_for_a_user_who_left_the_pool(pool: PgPool) {
        let a = Uuid::new_v4();
        let f = fact(&pool, a, "single for two years", at(0), at(0)).await;
        // No match_profiles row: the user left while the model was answering.
        let until = db_now(&pool).await.unwrap();

        let done = apply(
            &pool,
            a,
            &[Op::Add {
                section: "love".into(),
                status: "current".into(),
                statement: "Has been single for two years".into(),
                sources: vec![SourceRef::Fact(f)],
            }],
            until,
            "m",
        )
        .await
        .unwrap();
        assert_eq!(done, Applied::default());
        assert!(current_entries(&pool, a).await.unwrap().is_empty());
        let entries: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pothos.autobio_entries WHERE user_id = $1")
                .bind(a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(entries, 0);
        let states: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pothos.autobio_state WHERE user_id = $1")
                .bind(a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(states, 0, "apply leaves no watermark");

        record_failure(&pool, a, "m").await.unwrap();
        let states: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pothos.autobio_state WHERE user_id = $1")
                .bind(a)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(states, 0, "record_failure leaves no state row");
    }
}
