//! The hard filter: one statement over the two mirrors (spec §4.1).

use sqlx::PgPool;
use uuid::Uuid;

// consumed by Task 8
#[allow(dead_code)]
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct CandidateRow {
    pub user_id: Uuid,
    pub country: Option<String>,
    pub region: Option<String>,
    pub city: Option<String>,
    pub interests: Vec<String>,
    pub personality_traits: Vec<String>,
}

/// Everyone in the pool who survives the hard filter against `me`. Missing
/// data never excludes: an age rule only fires when the preference and the
/// birth date both exist; a candidate without a mirror row is kept with empty
/// arrays. Geo is not a filter. Returns nothing if `me` has no profile row.
// consumed by Task 8
#[allow(dead_code)]
pub async fn hard_filtered(pool: &PgPool, me: Uuid) -> sqlx::Result<Vec<CandidateRow>> {
    sqlx::query_as::<_, CandidateRow>(
        "WITH me AS ( \
            SELECT p.user_id, p.gender, p.seeking_genders, p.birth_date, \
                   COALESCE(m.interests, '{}')          AS interests, \
                   COALESCE(m.personality_traits, '{}') AS personality_traits, \
                   COALESCE(m.deal_breakers, '{}')      AS deal_breakers, \
                   m.age_min, m.age_max \
            FROM pothos.match_profiles p \
            LEFT JOIN pothos.insight_mirror m USING (user_id) \
            WHERE p.user_id = $1 \
         ), pool AS ( \
            SELECT c.user_id, c.gender, c.seeking_genders, c.birth_date, \
                   c.country, c.region, c.city, \
                   COALESCE(cm.interests, '{}')          AS interests, \
                   COALESCE(cm.personality_traits, '{}') AS personality_traits, \
                   COALESCE(cm.deal_breakers, '{}')      AS deal_breakers, \
                   cm.age_min, cm.age_max \
            FROM pothos.match_profiles c \
            LEFT JOIN pothos.insight_mirror cm USING (user_id) \
            WHERE c.active \
         ) \
         SELECT c.user_id, c.country, c.region, c.city, c.interests, c.personality_traits \
         FROM pool c, me \
         WHERE c.user_id <> me.user_id \
           AND c.gender = ANY(me.seeking_genders) \
           AND me.gender = ANY(c.seeking_genders) \
           AND (me.age_min IS NULL OR c.birth_date IS NULL \
                OR extract(year from age(current_date, c.birth_date)) >= me.age_min) \
           AND (me.age_max IS NULL OR c.birth_date IS NULL \
                OR extract(year from age(current_date, c.birth_date)) <= me.age_max) \
           AND (c.age_min IS NULL OR me.birth_date IS NULL \
                OR extract(year from age(current_date, me.birth_date)) >= c.age_min) \
           AND (c.age_max IS NULL OR me.birth_date IS NULL \
                OR extract(year from age(current_date, me.birth_date)) <= c.age_max) \
           AND NOT (me.deal_breakers && c.interests) \
           AND NOT (me.deal_breakers && c.personality_traits) \
           AND NOT (c.deal_breakers && me.interests) \
           AND NOT (c.deal_breakers && me.personality_traits) \
         ORDER BY c.user_id",
    )
    .bind(me)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use chrono::{Datelike, NaiveDate, Utc};
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::hard_filtered;
    use crate::db::mirror::{self, MirrorRow};
    use crate::db::profiles::{self, ProfileWrite};

    fn years_ago(n: i32) -> NaiveDate {
        let today = Utc::now().date_naive();
        NaiveDate::from_ymd_opt(today.year() - n, 6, 15).unwrap()
    }

    async fn profile(
        pool: &PgPool,
        gender: &str,
        seeking: &[&str],
        birth: Option<NaiveDate>,
        active: bool,
    ) -> Uuid {
        let id = Uuid::new_v4();
        profiles::upsert(
            pool,
            id,
            &ProfileWrite {
                gender: gender.into(),
                seeking_genders: seeking.iter().map(|s| s.to_string()).collect(),
                birth_date: birth,
                country: Some("SG".into()),
                region: None,
                city: None,
                active,
            },
        )
        .await
        .unwrap();
        id
    }

    async fn mirror(
        pool: &PgPool,
        id: Uuid,
        interests: &[&str],
        traits: &[&str],
        deal: &[&str],
        age: Option<(i32, i32)>,
    ) {
        mirror::upsert_one(
            pool,
            &MirrorRow {
                user_id: id,
                interests: interests.iter().map(|s| s.to_string()).collect(),
                personality_traits: traits.iter().map(|s| s.to_string()).collect(),
                deal_breakers: deal.iter().map(|s| s.to_string()).collect(),
                age_min: age.map(|a| a.0),
                age_max: age.map(|a| a.1),
                engine_updated_at: Utc::now(),
            },
        )
        .await
        .unwrap();
    }

    async fn ids(pool: &PgPool, me: Uuid) -> Vec<Uuid> {
        let mut v: Vec<Uuid> = hard_filtered(pool, me)
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.user_id)
            .collect();
        v.sort();
        v
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn gender_is_bidirectional_and_self_and_inactive_are_excluded(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], None, true).await;
        let wants_f = profile(&pool, "m", &["f"], None, true).await;
        let wants_m_only = profile(&pool, "m", &["m"], None, true).await;
        let other_f = profile(&pool, "f", &["m"], None, true).await;
        let inactive = profile(&pool, "m", &["f"], None, false).await;

        let got = ids(&pool, me).await;
        assert_eq!(got, vec![wants_f]);
        for excluded in [me, wants_m_only, other_f, inactive] {
            assert!(!got.contains(&excluded));
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn age_filters_both_ways_only_when_both_sides_have_data(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], Some(years_ago(30)), true).await;
        mirror(&pool, me, &[], &[], &[], Some((25, 35))).await;

        let in_range = profile(&pool, "m", &["f"], Some(years_ago(28)), true).await;
        let too_old = profile(&pool, "m", &["f"], Some(years_ago(40)), true).await;
        let no_birth_date = profile(&pool, "m", &["f"], None, true).await;
        let his_prefs_exclude_me = profile(&pool, "m", &["f"], Some(years_ago(28)), true).await;
        mirror(&pool, his_prefs_exclude_me, &[], &[], &[], Some((18, 24))).await;
        let his_prefs_include_me = profile(&pool, "m", &["f"], Some(years_ago(28)), true).await;
        mirror(&pool, his_prefs_include_me, &[], &[], &[], Some((25, 40))).await;

        let got = ids(&pool, me).await;
        let mut expected = vec![in_range, no_birth_date, his_prefs_include_me];
        expected.sort();
        assert_eq!(got, expected);
        assert!(!got.contains(&too_old));
        assert!(!got.contains(&his_prefs_exclude_me));
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn me_without_age_prefs_does_not_filter_on_age(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], Some(years_ago(30)), true).await;
        mirror(&pool, me, &["jazz"], &[], &[], None).await;
        let any_age = profile(&pool, "m", &["f"], Some(years_ago(60)), true).await;
        assert_eq!(ids(&pool, me).await, vec![any_age]);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn deal_breakers_apply_both_ways_to_interests_and_traits(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], None, true).await;
        mirror(&pool, me, &["jazz"], &["calm"], &["smoking"], None).await;

        let smokes_interest = profile(&pool, "m", &["f"], None, true).await;
        mirror(&pool, smokes_interest, &["smoking"], &[], &[], None).await;
        let smokes_trait = profile(&pool, "m", &["f"], None, true).await;
        mirror(&pool, smokes_trait, &[], &["smoking"], &[], None).await;
        let hates_jazz = profile(&pool, "m", &["f"], None, true).await;
        mirror(&pool, hates_jazz, &[], &[], &["jazz"], None).await;
        let hates_calm = profile(&pool, "m", &["f"], None, true).await;
        mirror(&pool, hates_calm, &[], &[], &["calm"], None).await;
        let fine = profile(&pool, "m", &["f"], None, true).await;
        mirror(&pool, fine, &["tea"], &["loud"], &["gambling"], None).await;
        let no_mirror_at_all = profile(&pool, "m", &["f"], None, true).await;

        let got = ids(&pool, me).await;
        let mut expected = vec![fine, no_mirror_at_all];
        expected.sort();
        assert_eq!(got, expected);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn mirror_only_users_never_surface(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], None, true).await;
        let ghost = Uuid::new_v4(); // synced from the engine, never pushed by the product layer
        mirror(&pool, ghost, &["jazz"], &[], &[], None).await;
        assert!(ids(&pool, me).await.is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn candidate_rows_carry_geo_and_arrays_for_scoring(pool: PgPool) {
        let me = profile(&pool, "f", &["m"], None, true).await;
        let him = profile(&pool, "m", &["f"], None, true).await;
        mirror(&pool, him, &["jazz", "tea"], &["calm"], &[], None).await;
        let rows = hard_filtered(&pool, me).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].country.as_deref(), Some("SG"));
        assert_eq!(rows[0].interests, vec!["jazz", "tea"]);
        assert_eq!(rows[0].personality_traits, vec!["calm"]);
    }
}
