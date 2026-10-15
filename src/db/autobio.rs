//! Autobio entries and the material they are written from. An entry is one
//! statement under a fixed section; revising it retires the row and adds one
//! that points back, so retired rows are the revision history.
//!
//! The material is the phrase ledger (array items and text-column versions)
//! and the memory mirror. `recorded_at` on both is when this service first
//! stored a row; `autobio_state.material_until` is how far a user's entries
//! have taken it in. The sweeper is the only writer of both tables and runs
//! its steps in sequence, so every row recorded before the autobio step reads
//! the clock is already committed.

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
    Observed(i64),
    Memory(Uuid),
}

impl SourceRef {
    pub fn tag(&self) -> String {
        match self {
            Self::Observed(id) => format!("o:{id}"),
            Self::Memory(id) => format!("m:{id}"),
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        let (kind, id) = s.split_once(':')?;
        match kind {
            "o" => id.parse().ok().map(Self::Observed),
            "m" => id.parse().ok().map(Self::Memory),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Material {
    pub source: SourceRef,
    /// The ledger dimension, or `memory/<category>`.
    pub kind: String,
    pub text: String,
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
            SELECT user_id, recorded_at FROM pothos.observed_phrases \
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

/// One user's material recorded after `after` (all of it when `None`).
pub async fn material(
    pool: &PgPool,
    user_id: Uuid,
    after: Option<DateTime<Utc>>,
) -> sqlx::Result<Vec<Material>> {
    let observed: Vec<(i64, String, String, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id, dimension, phrase, first_seen_at FROM pothos.observed_phrases \
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
    let mut out: Vec<Material> = observed
        .into_iter()
        .map(|(id, dimension, text, at)| Material {
            source: SourceRef::Observed(id),
            kind: dimension,
            text,
            metadata: None,
            at,
        })
        .chain(
            memories
                .into_iter()
                .map(|(id, category, text, metadata, at)| Material {
                    source: SourceRef::Memory(id),
                    kind: format!("memory/{category}"),
                    text,
                    metadata,
                    at,
                }),
        )
        .collect();
    out.sort_by(|a, b| {
        a.at.cmp(&b.at)
            .then_with(|| a.source.tag().cmp(&b.source.tag()))
    });
    Ok(out)
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
                active,
            },
        )
        .await
        .unwrap();
    }

    pub(crate) async fn export(pool: &PgPool, id: Uuid, t: DateTime<Utc>, texts: &[(&str, &str)]) {
        mirror::upsert_one(
            pool,
            &MirrorRow {
                user_id: id,
                interests: vec![],
                personality_traits: vec![],
                deal_breakers: vec![],
                texts: texts
                    .iter()
                    .map(|(f, v)| (f.to_string(), v.to_string()))
                    .collect(),
                age_min: None,
                age_max: None,
                engine_updated_at: t,
            },
        )
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
        for r in [SourceRef::Observed(12), SourceRef::Memory(m)] {
            assert_eq!(SourceRef::parse(&r.tag()), Some(r));
        }
        assert_eq!(SourceRef::Observed(12).tag(), "o:12");
        for junk in ["", "o:", "o:x", "m:not-a-uuid", "x:1", "12", "o:1:2"] {
            assert_eq!(SourceRef::parse(junk), None, "{junk}");
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn material_is_everything_first_then_only_what_arrived_after_the_watermark(pool: PgPool) {
        let a = Uuid::new_v4();
        export(&pool, a, at(0), &[("love_values", "slow to warm up")]).await;
        let m = memory(&pool, a, "likes hiking", at(1)).await;
        let other = Uuid::new_v4();
        memory(&pool, other, "not mine", at(1)).await;

        let all = material(&pool, a, None).await.unwrap();
        let o = observed_id(&pool, a, "love_values", "slow to warm up").await;
        assert_eq!(
            all.iter()
                .map(|x| (x.source, x.kind.as_str(), x.text.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (SourceRef::Observed(o), "love_values", "slow to warm up"),
                (SourceRef::Memory(m), "memory/preference", "likes hiking"),
            ]
        );
        assert_eq!(all[1].metadata.as_ref().unwrap()["domain"], "hobby");

        let watermark = db_now(&pool).await.unwrap();
        assert!(material(&pool, a, Some(watermark))
            .await
            .unwrap()
            .is_empty());
        export(
            &pool,
            a,
            at(5),
            &[("love_values", "slow to warm up; loyal")],
        )
        .await;
        let later = material(&pool, a, Some(watermark)).await.unwrap();
        assert_eq!(later.len(), 1);
        assert_eq!(later[0].text, "slow to warm up; loyal");
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
            export(&pool, id, at(0), &[("city", "Singapore")]).await;
        }
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
        assert!(due.iter().all(|d| d.material_until.is_none()));

        // The interval has passed for `waiting`, but nothing new has arrived: not due.
        let elapsed = db_now(&pool).await.unwrap();
        let idle = due_users(&pool, elapsed, 10).await.unwrap();
        assert!(
            !idle.iter().any(|d| d.user_id == waiting),
            "no new material, not due"
        );

        // New material for `waiting`: still not due inside the interval, due after it.
        export(&pool, waiting, at(5), &[("city", "Tokyo")]).await;
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
        export(&pool, a, at(0), &[("city", "Singapore")]).await;
        let o = observed_id(&pool, a, "city", "Singapore").await;
        let m = memory(&pool, a, "moved here in 2020", at(7)).await;
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
        assert_eq!(got[0].as_of, Some(at(7)));
        assert_eq!(
            got[0].sources,
            vec![SourceRef::Observed(o), SourceRef::Memory(m)]
        );
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
        export(
            &pool,
            a,
            at(0),
            &[("relationship_history", "single for two years")],
        )
        .await;
        let o = observed_id(&pool, a, "relationship_history", "single for two years").await;
        let m = memory(&pool, a, "seeing someone lately", at(9)).await;
        let until = db_now(&pool).await.unwrap();

        let done = apply(
            &pool,
            a,
            &[Op::Add {
                section: "love".into(),
                status: "current".into(),
                statement: "Has been single for two years".into(),
                sources: vec![SourceRef::Observed(o)],
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
            vec![SourceRef::Observed(o), SourceRef::Memory(m)],
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
    async fn nothing_is_written_for_a_user_who_left_the_pool(pool: PgPool) {
        let a = Uuid::new_v4();
        export(
            &pool,
            a,
            at(0),
            &[("relationship_history", "single for two years")],
        )
        .await;
        let o = observed_id(&pool, a, "relationship_history", "single for two years").await;
        // No match_profiles row: the user left while the model was answering.
        let until = db_now(&pool).await.unwrap();

        let done = apply(
            &pool,
            a,
            &[Op::Add {
                section: "love".into(),
                status: "current".into(),
                statement: "Has been single for two years".into(),
                sources: vec![SourceRef::Observed(o)],
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
