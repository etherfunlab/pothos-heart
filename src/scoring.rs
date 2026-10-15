//! Soft score. Pure: no I/O, no clock. Overlap coefficient per array
//! dimension — |A∩B| / min(|A|,|B|) — because array length tracks how much a
//! person has chatted, not how well they match; Jaccard would punish the
//! newcomer with two extracted interests. Geo credits the finest matching
//! level, and only when the country matches.

use std::collections::BTreeSet;

use serde::Serialize;
use utoipa::ToSchema;

use crate::config::Weights;

/// Finest geographic level at which two users match; present only when the country matches.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum GeoMatch {
    City,
    Region,
    Country,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Geo<'a> {
    pub country: Option<&'a str>,
    pub region: Option<&'a str>,
    pub city: Option<&'a str>,
}

pub struct Side<'a> {
    pub interests: &'a [String],
    pub traits: &'a [String],
    pub geo: Geo<'a>,
}

/// Per-dimension scores before weighting; `geo` is the configured credit for the matched level.
#[derive(Debug, Serialize, ToSchema)]
pub struct DimScores {
    pub interests: f64,
    pub personality_traits: f64,
    pub geo: f64,
}

/// A weighted match score with its per-dimension breakdown and the shared items behind it.
#[derive(Debug, Serialize, ToSchema)]
pub struct Scored {
    pub score: f64,
    pub scores: DimScores,
    pub shared_interests: Vec<String>,
    pub shared_traits: Vec<String>,
    pub geo_match: Option<GeoMatch>,
}

pub(crate) fn canon(s: &str) -> Option<String> {
    let c = s.trim().to_lowercase();
    (!c.is_empty()).then_some(c)
}

/// Canonical labels (`canon`), repeats dropped, first occurrence wins. The SQL
/// hard filter compares stored phrases with exact `&&`, so everything stored
/// takes this form.
pub(crate) fn canonical(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(values.len());
    for c in values.iter().filter_map(|v| canon(v)) {
        if !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

fn as_set(items: &[String]) -> BTreeSet<String> {
    items.iter().filter_map(|s| canon(s)).collect()
}

/// Overlap coefficient and the shared items (canonical lowercase, sorted).
pub fn overlap(a: &[String], b: &[String]) -> (f64, Vec<String>) {
    let (sa, sb) = (as_set(a), as_set(b));
    let min = sa.len().min(sb.len());
    if min == 0 {
        return (0.0, Vec::new());
    }
    let shared: Vec<String> = sa.intersection(&sb).cloned().collect();
    (shared.len() as f64 / min as f64, shared)
}

fn same(a: Option<&str>, b: Option<&str>) -> bool {
    match (a.and_then(canon), b.and_then(canon)) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// Finest matching level; `None` unless the country matches on both sides.
/// A city name only counts as a city match when it is not contradicted by the
/// regions: equal regions, or at least one side without a region. Two
/// "Springfield"s in different regions fall through to `Region` / `Country`.
pub fn geo_match(a: &Geo, b: &Geo) -> Option<GeoMatch> {
    if !same(a.country, b.country) {
        return None;
    }
    let regions_differ = matches!(
        (a.region.and_then(canon), b.region.and_then(canon)),
        (Some(x), Some(y)) if x != y
    );
    if same(a.city, b.city) && !regions_differ {
        Some(GeoMatch::City)
    } else if same(a.region, b.region) {
        Some(GeoMatch::Region)
    } else {
        Some(GeoMatch::Country)
    }
}

pub fn score(w: &Weights, me: &Side, other: &Side) -> Scored {
    let (oc_i, shared_interests) = overlap(me.interests, other.interests);
    let (oc_t, shared_traits) = overlap(me.traits, other.traits);
    let gm = geo_match(&me.geo, &other.geo);
    let geo_level = match gm {
        Some(GeoMatch::City) => w.geo_levels.city,
        Some(GeoMatch::Region) => w.geo_levels.region,
        Some(GeoMatch::Country) => w.geo_levels.country,
        None => 0.0,
    };
    let total = (w.interests * oc_i + w.personality_traits * oc_t + w.geo * geo_level) / w.total();
    Scored {
        score: total,
        scores: DimScores {
            interests: oc_i,
            personality_traits: oc_t,
            geo: geo_level,
        },
        shared_interests,
        shared_traits,
        geo_match: gm,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn overlap_is_coefficient_over_min_not_jaccard() {
        // newcomer with 2 interests vs veteran with 10, both share the 2
        let a = v(&["jazz", "hiking"]);
        let b = v(&["jazz", "hiking", "c", "d", "e", "f", "g", "h", "i", "j"]);
        let (oc, shared) = overlap(&a, &b);
        assert_eq!(oc, 1.0, "min-normalised: the newcomer is not punished");
        assert_eq!(shared, v(&["hiking", "jazz"]), "shared items, sorted");

        let (oc_rev, shared_rev) = overlap(&b, &a);
        assert_eq!(oc_rev, oc, "argument order does not change the coefficient");
        assert_eq!(
            shared_rev, shared,
            "argument order does not change shared items"
        );
    }

    #[test]
    fn overlap_dedupes_and_ignores_case_and_whitespace() {
        let a = v(&["Jazz", "jazz", " hiking "]);
        let b = v(&["JAZZ", "Tea"]);
        let (oc, shared) = overlap(&a, &b);
        // a as a set = {jazz, hiking} (2); b = {jazz, tea} (2); ∩ = {jazz}
        assert_eq!(oc, 0.5);
        assert_eq!(shared, v(&["jazz"]), "canonical lowercase form");
    }

    #[test]
    fn overlap_with_an_empty_side_is_zero() {
        assert_eq!(overlap(&[], &v(&["jazz"])).0, 0.0);
        assert_eq!(overlap(&v(&["jazz"]), &[]).0, 0.0);
        assert!(overlap(&[], &[]).1.is_empty());
    }

    #[test]
    fn geo_credits_finest_level_only_when_country_matches() {
        let sg_city = Geo {
            country: Some("SG"),
            region: None,
            city: Some("Singapore"),
        };
        let sg_city2 = Geo {
            country: Some("sg"),
            region: Some("Central"),
            city: Some("singapore"),
        };
        assert_eq!(geo_match(&sg_city, &sg_city2), Some(GeoMatch::City));

        let jp_tokyo = Geo {
            country: Some("JP"),
            region: Some("Kanto"),
            city: Some("Tokyo"),
        };
        let jp_yoko = Geo {
            country: Some("JP"),
            region: Some("Kanto"),
            city: Some("Yokohama"),
        };
        assert_eq!(geo_match(&jp_tokyo, &jp_yoko), Some(GeoMatch::Region));

        let jp_osaka = Geo {
            country: Some("JP"),
            region: Some("Kansai"),
            city: Some("Osaka"),
        };
        assert_eq!(geo_match(&jp_tokyo, &jp_osaka), Some(GeoMatch::Country));

        let us_springfield = Geo {
            country: Some("US"),
            region: None,
            city: Some("Springfield"),
        };
        let uk_springfield = Geo {
            country: Some("GB"),
            region: None,
            city: Some("Springfield"),
        };
        assert_eq!(
            geo_match(&us_springfield, &uk_springfield),
            None,
            "same city name, different country"
        );

        let no_country = Geo {
            country: None,
            region: None,
            city: Some("Singapore"),
        };
        assert_eq!(
            geo_match(&no_country, &sg_city),
            None,
            "no country ⇒ no credit"
        );
    }

    #[test]
    fn a_city_match_is_not_credited_across_conflicting_regions() {
        let springfield = |region| Geo {
            country: Some("US"),
            region,
            city: Some("Springfield"),
        };
        assert_eq!(
            geo_match(&springfield(Some("IL")), &springfield(Some("MO"))),
            Some(GeoMatch::Country),
            "same city name, different regions"
        );
        assert_eq!(
            geo_match(&springfield(Some("IL")), &springfield(None)),
            Some(GeoMatch::City),
            "an unknown region does not veto the city"
        );
        assert_eq!(
            geo_match(&springfield(None), &springfield(Some("MO"))),
            Some(GeoMatch::City)
        );
        assert_eq!(
            geo_match(&springfield(Some("il")), &springfield(Some(" IL "))),
            Some(GeoMatch::City),
            "regions compare in canonical form"
        );
    }

    #[test]
    fn score_is_weighted_and_normalised_to_unit_interval() {
        let w = Weights::default(); // 0.5 / 0.3 / 0.2, city 1.0
        let me = Side {
            interests: &v(&["jazz", "hiking"]),
            traits: &v(&["calm"]),
            geo: Geo {
                country: Some("SG"),
                region: None,
                city: Some("Singapore"),
            },
        };
        let other = Side {
            interests: &v(&["jazz", "tea"]),
            traits: &v(&["loud"]),
            geo: Geo {
                country: Some("SG"),
                region: None,
                city: Some("Singapore"),
            },
        };
        let s = score(&w, &me, &other);
        // interests 0.5 * 0.5 + traits 0.3 * 0 + geo 0.2 * 1.0 = 0.45 ; / 1.0
        assert!((s.score - 0.45).abs() < 1e-9, "{}", s.score);
        assert_eq!(s.scores.interests, 0.5);
        assert_eq!(s.scores.personality_traits, 0.0);
        assert_eq!(s.scores.geo, 1.0);
        assert_eq!(s.shared_interests, v(&["jazz"]));
        assert!(s.shared_traits.is_empty());
        assert_eq!(s.geo_match, Some(GeoMatch::City));

        let perfect = score(&w, &me, &me);
        assert!((perfect.score - 1.0).abs() < 1e-9);
    }

    fn sg_home() -> Geo<'static> {
        Geo {
            country: Some("SG"),
            region: Some("Central"),
            city: Some("Singapore"),
        }
    }

    #[test]
    fn geo_levels_flow_through_score() {
        let w = Weights::default(); // geo levels 1.0 / 0.6 / 0.3
        let ints = v(&["jazz", "hiking"]);
        let trs = v(&["calm"]);
        let me = Side {
            interests: &ints,
            traits: &trs,
            geo: sg_home(),
        };
        let cases = [
            (
                "region only",
                Geo {
                    country: Some("SG"),
                    region: Some("Central"),
                    city: Some("Jurong"),
                },
                0.6,
                Some(GeoMatch::Region),
            ),
            (
                "country only",
                Geo {
                    country: Some("SG"),
                    region: Some("East"),
                    city: Some("Changi"),
                },
                0.3,
                Some(GeoMatch::Country),
            ),
            (
                "no country match",
                Geo {
                    country: Some("MY"),
                    region: Some("Central"),
                    city: Some("Singapore"),
                },
                0.0,
                None,
            ),
        ];
        for (name, geo, level, gm) in cases {
            let other = Side {
                interests: &ints,
                traits: &trs,
                geo,
            };
            let s = score(&w, &me, &other);
            assert!(
                (s.scores.geo - level).abs() < 1e-9,
                "{name}: geo {}",
                s.scores.geo
            );
            assert_eq!(s.geo_match, gm, "{name}");
            // identical interests and traits: 0.5 * 1.0 + 0.3 * 1.0 + 0.2 * level
            let expected = 0.8 + 0.2 * level;
            assert!(
                (s.score - expected).abs() < 1e-9,
                "{name}: score {}",
                s.score
            );
        }
    }

    #[test]
    fn score_normalises_by_weight_total_not_by_one() {
        let ints_a = v(&["jazz", "hiking"]);
        let ints_b = v(&["jazz", "tea"]);
        let trs_a = v(&["calm"]);
        let trs_b = v(&["loud"]);
        let me = Side {
            interests: &ints_a,
            traits: &trs_a,
            geo: sg_home(),
        };
        let other = Side {
            interests: &ints_b,
            traits: &trs_b,
            geo: sg_home(),
        };
        let default = Weights::default();
        let custom = Weights {
            interests: 2.0,
            personality_traits: 1.2,
            geo: 0.8,
            geo_levels: crate::config::GeoLevels {
                city: 1.0,
                region: 0.6,
                country: 0.3,
            },
        };
        let s_default = score(&default, &me, &other).score;
        let s_custom = score(&custom, &me, &other).score;
        // (2.0 * 0.5 + 1.2 * 0 + 0.8 * 1.0) / 4.0 = 0.45, same as the default
        assert!(
            (s_custom - s_default).abs() < 1e-9,
            "{s_custom} vs {s_default}"
        );
        assert!((s_custom - 0.45).abs() < 1e-9);
        assert!((score(&custom, &me, &me).score - 1.0).abs() < 1e-9);
    }

    #[test]
    fn overlap_denominator_uses_deduped_size() {
        let (oc, shared) = overlap(&v(&["Jazz", "jazz"]), &v(&["jazz", "tea"]));
        assert_eq!(oc, 1.0, "deduped a has size 1; raw length would give 0.5");
        assert_eq!(shared, v(&["jazz"]));
    }

    #[test]
    fn overlap_trims_and_drops_blank_entries() {
        assert_eq!(overlap(&v(&[" jazz "]), &v(&["jazz"])), (1.0, v(&["jazz"])));
        assert_eq!(
            overlap(&v(&["", "  "]), &v(&["", "jazz"])),
            (0.0, Vec::new()),
            "blank entries are never shared"
        );
    }

    #[test]
    fn serde_output_uses_lowercase_geo_match_and_null_when_no_country() {
        assert_eq!(serde_json::to_value(GeoMatch::City).unwrap(), "city");
        assert_eq!(serde_json::to_value(GeoMatch::Region).unwrap(), "region");
        assert_eq!(serde_json::to_value(GeoMatch::Country).unwrap(), "country");

        let w = Weights::default();
        let ints = v(&["jazz"]);
        let trs = v(&["calm"]);
        let me = Side {
            interests: &ints,
            traits: &trs,
            geo: sg_home(),
        };
        let other = Side {
            interests: &ints,
            traits: &trs,
            geo: Geo {
                country: Some("MY"),
                region: Some("Central"),
                city: Some("Singapore"),
            },
        };
        let json = serde_json::to_value(score(&w, &me, &other)).unwrap();
        assert!(json["geo_match"].is_null(), "{json}");
    }
}
