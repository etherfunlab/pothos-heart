//! Boot-time configuration: env vars for wiring, a TOML file for weights.
//! Parsing is a pure function of a lookup closure so tests never touch the
//! process environment.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

pub const MIN_CALLER_SECRET_LEN: usize = 32;

#[derive(Clone)]
pub struct EngineConfig {
    pub base_url: String,
    pub export_secret: String,
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineConfig")
            .field("base_url", &self.base_url)
            .field("export_secret", &"<redacted>")
            .finish()
    }
}

/// An OpenAI-compatible chat-completions endpoint for the concept step.
#[derive(Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl std::fmt::Debug for LlmConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &"<redacted>")
            .field("model", &self.model)
            .finish()
    }
}

#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub bind_addr: String,
    pub caller_secret: String,
    pub engine: Option<EngineConfig>,
    pub sweep_interval: Duration,
    pub weights_path: PathBuf,
    pub llm: Option<LlmConfig>,
    pub concept_max_per_tick: i64,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind_addr", &self.bind_addr)
            .field("caller_secret", &"<redacted>")
            .field(
                "engine_base_url",
                &self.engine.as_ref().map(|e| &e.base_url),
            )
            .field("sweep_interval", &self.sweep_interval)
            .field("weights_path", &self.weights_path)
            .field("llm", &self.llm)
            .field("concept_max_per_tick", &self.concept_max_per_tick)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct GeoLevels {
    pub city: f64,
    pub region: f64,
    pub country: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Weights {
    pub interests: f64,
    pub personality_traits: f64,
    pub geo: f64,
    pub geo_levels: GeoLevels,
}

impl Default for Weights {
    fn default() -> Self {
        Self {
            interests: 0.5,
            personality_traits: 0.3,
            geo: 0.2,
            geo_levels: GeoLevels {
                city: 1.0,
                region: 0.6,
                country: 0.3,
            },
        }
    }
}

fn truthy(v: Option<String>) -> bool {
    matches!(
        v.as_deref().map(str::trim),
        Some("1") | Some("true") | Some("yes")
    )
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        Self::parse(|k| std::env::var(k).ok())
    }

    pub fn parse(get: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let nonblank = |k: &str| {
            get(k)
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        };

        let database_url = nonblank("DATABASE_URL").ok_or("DATABASE_URL is required")?;
        let caller_secret =
            nonblank("POTHOS_CALLER_SECRET").ok_or("POTHOS_CALLER_SECRET is required")?;
        if caller_secret.len() < MIN_CALLER_SECRET_LEN {
            return Err(format!(
                "POTHOS_CALLER_SECRET must be at least {MIN_CALLER_SECRET_LEN} bytes"
            ));
        }

        let engine = match (
            nonblank("ENGINE_BASE_URL"),
            nonblank("ENGINE_EXPORT_SECRET"),
        ) {
            (None, None) => None,
            (Some(base_url), Some(export_secret)) => Some(EngineConfig {
                base_url: base_url.trim_end_matches('/').to_string(),
                export_secret,
            }),
            (Some(_), None) => {
                return Err("ENGINE_BASE_URL is set but ENGINE_EXPORT_SECRET is not".into())
            }
            (None, Some(_)) => {
                return Err("ENGINE_EXPORT_SECRET is set but ENGINE_BASE_URL is not".into())
            }
        };

        let sweep_interval = if truthy(get("SWEEP_DISABLED")) {
            Duration::ZERO
        } else {
            let secs: u64 = match nonblank("SWEEP_INTERVAL_SECS") {
                None => 300,
                Some(s) => s
                    .parse()
                    .map_err(|_| format!("SWEEP_INTERVAL_SECS is not a number: {s}"))?,
            };
            Duration::from_secs(secs)
        };

        let llm = match (
            nonblank("LLM_BASE_URL"),
            nonblank("LLM_API_KEY"),
            nonblank("LLM_MODEL"),
        ) {
            (None, None, None) => None,
            (Some(base_url), Some(api_key), Some(model)) => Some(LlmConfig {
                base_url: base_url.trim_end_matches('/').to_string(),
                api_key,
                model,
            }),
            _ => {
                return Err(
                    "LLM_BASE_URL, LLM_API_KEY and LLM_MODEL are set together or not at all".into(),
                )
            }
        };

        let concept_max_per_tick = match nonblank("CONCEPT_MAX_PER_TICK") {
            None => 200,
            Some(s) => match s.parse::<i64>() {
                Ok(n) if n >= 1 => n,
                _ => {
                    return Err(format!(
                        "CONCEPT_MAX_PER_TICK must be a positive integer: {s}"
                    ))
                }
            },
        };

        Ok(Self {
            database_url,
            bind_addr: nonblank("BIND_ADDR").unwrap_or_else(|| "0.0.0.0:8090".into()),
            caller_secret,
            engine,
            sweep_interval,
            weights_path: nonblank("POTHOS_WEIGHTS")
                .map(PathBuf::from)
                .unwrap_or_else(|| "pothos.toml".into()),
            llm,
            concept_max_per_tick,
        })
    }
}

#[derive(Deserialize)]
struct WeightsFile {
    weights: WeightsSection,
    geo: GeoLevels,
}

#[derive(Deserialize)]
struct WeightsSection {
    interests: f64,
    personality_traits: f64,
    geo: f64,
}

impl Weights {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read weights file {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    pub fn parse(toml_text: &str) -> Result<Self, String> {
        let f: WeightsFile = toml::from_str(toml_text).map_err(|e| format!("weights toml: {e}"))?;
        let w = Self {
            interests: f.weights.interests,
            personality_traits: f.weights.personality_traits,
            geo: f.weights.geo,
            geo_levels: f.geo,
        };
        let geo_levels = [
            ("geo.city", w.geo_levels.city),
            ("geo.region", w.geo_levels.region),
            ("geo.country", w.geo_levels.country),
        ];
        for (name, v) in [
            ("weights.interests", w.interests),
            ("weights.personality_traits", w.personality_traits),
            ("weights.geo", w.geo),
        ]
        .into_iter()
        .chain(geo_levels)
        {
            if !v.is_finite() || v < 0.0 {
                return Err(format!("{name} must be a non-negative number, got {v}"));
            }
        }
        // A level above 1.0 pushes the score past 1.0, breaking the [0, 1] contract.
        for (name, v) in geo_levels {
            if v > 1.0 {
                return Err(format!("{name} must be at most 1.0, got {v}"));
            }
        }
        if w.interests + w.personality_traits + w.geo <= 0.0 {
            return Err("weights sum must be > 0".into());
        }
        Ok(w)
    }

    /// Divisor that maps the weighted sum onto [0, 1].
    pub fn total(&self) -> f64 {
        self.interests + self.personality_traits + self.geo
    }
}

#[derive(Deserialize, Default)]
struct ConceptsFile {
    #[serde(default)]
    concepts: ConceptsSection,
}

#[derive(Deserialize)]
struct ConceptsSection {
    #[serde(default = "default_label_language")]
    label_language: String,
}

impl Default for ConceptsSection {
    fn default() -> Self {
        Self {
            label_language: default_label_language(),
        }
    }
}

fn default_label_language() -> String {
    "en".into()
}

/// `[concepts] label_language` from the weights file: the language new
/// concept labels are written in. `en` when absent.
pub fn label_language(toml_text: &str) -> Result<String, String> {
    let f: ConceptsFile = toml::from_str(toml_text).map_err(|e| format!("pothos toml: {e}"))?;
    let lang = f.concepts.label_language.trim().to_string();
    if lang.is_empty() {
        return Err("concepts.label_language must not be blank".into());
    }
    Ok(lang)
}

pub fn load_label_language(path: &Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read weights file {}: {e}", path.display()))?;
    label_language(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    #[test]
    fn minimal_config_defaults_bind_and_disables_sweeper() {
        let c = Config::parse(env(&[
            ("DATABASE_URL", "postgres://x"),
            ("POTHOS_CALLER_SECRET", SECRET),
        ]))
        .unwrap();
        assert_eq!(c.bind_addr, "0.0.0.0:8090");
        assert!(c.engine.is_none());
        assert_eq!(c.sweep_interval, Duration::from_secs(300));
        assert_eq!(c.weights_path, PathBuf::from("pothos.toml"));
    }

    #[test]
    fn caller_secret_is_required_and_at_least_32_bytes() {
        let err = Config::parse(env(&[("DATABASE_URL", "postgres://x")])).unwrap_err();
        assert!(err.contains("POTHOS_CALLER_SECRET"));
        let err = Config::parse(env(&[
            ("DATABASE_URL", "postgres://x"),
            ("POTHOS_CALLER_SECRET", "short"),
        ]))
        .unwrap_err();
        assert!(err.contains("32"));
    }

    #[test]
    fn engine_vars_are_both_or_neither() {
        let err = Config::parse(env(&[
            ("DATABASE_URL", "postgres://x"),
            ("POTHOS_CALLER_SECRET", SECRET),
            ("ENGINE_BASE_URL", "http://engine:8080"),
        ]))
        .unwrap_err();
        assert!(err.contains("ENGINE_EXPORT_SECRET"));

        let c = Config::parse(env(&[
            ("DATABASE_URL", "postgres://x"),
            ("POTHOS_CALLER_SECRET", SECRET),
            ("ENGINE_BASE_URL", "http://engine:8080/"),
            ("ENGINE_EXPORT_SECRET", SECRET),
        ]))
        .unwrap();
        let e = c.engine.unwrap();
        assert_eq!(e.base_url, "http://engine:8080", "trailing slash trimmed");
    }

    #[test]
    fn sweep_disabled_or_zero_interval_turns_sweeper_off() {
        for pairs in [
            vec![("SWEEP_DISABLED", "1")],
            vec![("SWEEP_INTERVAL_SECS", "0")],
        ] {
            let mut all = vec![
                ("DATABASE_URL", "postgres://x"),
                ("POTHOS_CALLER_SECRET", SECRET),
            ];
            all.extend(pairs);
            let c = Config::parse(env(&all)).unwrap();
            assert_eq!(c.sweep_interval, Duration::ZERO);
        }
    }

    #[test]
    fn weights_parse_and_reject_negatives() {
        let w = Weights::parse(
            "[weights]\ninterests = 0.5\npersonality_traits = 0.3\ngeo = 0.2\n\
             [geo]\ncity = 1.0\nregion = 0.6\ncountry = 0.3\n",
        )
        .unwrap();
        assert_eq!(w, Weights::default());

        let err = Weights::parse(
            "[weights]\ninterests = -1\npersonality_traits = 0.3\ngeo = 0.2\n\
             [geo]\ncity = 1.0\nregion = 0.6\ncountry = 0.3\n",
        )
        .unwrap_err();
        assert!(err.contains("negative"));

        let err = Weights::parse(
            "[weights]\ninterests = 0\npersonality_traits = 0\ngeo = 0\n\
             [geo]\ncity = 1.0\nregion = 0.6\ncountry = 0.3\n",
        )
        .unwrap_err();
        assert!(err.contains("sum"));
    }

    #[test]
    fn geo_levels_above_one_are_rejected_by_name() {
        for key in ["city", "region", "country"] {
            let mut levels = [("city", "1.0"), ("region", "0.6"), ("country", "0.3")];
            levels.iter_mut().find(|(k, _)| *k == key).unwrap().1 = "1.5";
            let geo: String = levels.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
            let err = Weights::parse(&format!(
                "[weights]\ninterests = 0.5\npersonality_traits = 0.3\ngeo = 0.2\n[geo]\n{geo}"
            ))
            .unwrap_err();
            assert!(err.contains(&format!("geo.{key}")), "{err}");
        }
        // Exactly 1.0 is the ceiling, not over it.
        assert!(Weights::parse(
            "[weights]\ninterests = 0.5\npersonality_traits = 0.3\ngeo = 0.2\n\
             [geo]\ncity = 1.0\nregion = 1.0\ncountry = 1.0\n"
        )
        .is_ok());
    }
    #[test]
    fn llm_config_debug_redacts_the_key() {
        let c = LlmConfig {
            base_url: "http://llm".into(),
            api_key: "sk-very-secret".into(),
            model: "m".into(),
        };
        let shown = format!("{c:?}");
        assert!(!shown.contains("sk-very-secret"), "{shown}");
        assert!(
            shown.contains("http://llm") && shown.contains("\"m\""),
            "{shown}"
        );
    }
    #[test]
    fn llm_vars_are_all_or_none_and_the_base_url_loses_its_trailing_slash() {
        let base = [
            ("DATABASE_URL", "postgres://x"),
            ("POTHOS_CALLER_SECRET", SECRET),
        ];
        for partial in [
            vec![("LLM_BASE_URL", "http://llm/v1")],
            vec![("LLM_BASE_URL", "http://llm/v1"), ("LLM_API_KEY", "k")],
            vec![("LLM_MODEL", "m")],
        ] {
            let mut all = base.to_vec();
            all.extend(partial);
            let err = Config::parse(env(&all)).unwrap_err();
            assert!(
                err.contains("LLM_BASE_URL, LLM_API_KEY and LLM_MODEL"),
                "{err}"
            );
        }

        assert!(Config::parse(env(&base)).unwrap().llm.is_none());

        let mut all = base.to_vec();
        all.extend([
            ("LLM_BASE_URL", "http://llm/v1/"),
            ("LLM_API_KEY", "k"),
            ("LLM_MODEL", "m"),
        ]);
        let llm = Config::parse(env(&all)).unwrap().llm.unwrap();
        assert_eq!(llm.base_url, "http://llm/v1");
        assert_eq!((llm.api_key.as_str(), llm.model.as_str()), ("k", "m"));
    }

    #[test]
    fn concept_max_per_tick_defaults_to_200_and_must_be_positive() {
        let base = [
            ("DATABASE_URL", "postgres://x"),
            ("POTHOS_CALLER_SECRET", SECRET),
        ];
        assert_eq!(Config::parse(env(&base)).unwrap().concept_max_per_tick, 200);
        for bad in ["0", "-3", "many"] {
            let mut all = base.to_vec();
            all.push(("CONCEPT_MAX_PER_TICK", bad));
            let err = Config::parse(env(&all)).unwrap_err();
            assert!(err.contains("CONCEPT_MAX_PER_TICK"), "{bad}: {err}");
        }
        let mut all = base.to_vec();
        all.push(("CONCEPT_MAX_PER_TICK", "50"));
        assert_eq!(Config::parse(env(&all)).unwrap().concept_max_per_tick, 50);
    }

    #[test]
    fn the_llm_key_never_shows_in_config_debug() {
        let c = Config::parse(env(&[
            ("DATABASE_URL", "postgres://x"),
            ("POTHOS_CALLER_SECRET", SECRET),
            ("LLM_BASE_URL", "http://llm/v1"),
            ("LLM_API_KEY", "sk-very-secret"),
            ("LLM_MODEL", "m"),
        ]))
        .unwrap();
        assert!(!format!("{c:?}").contains("sk-very-secret"));
    }

    #[test]
    fn label_language_defaults_to_en_and_rejects_blank() {
        assert_eq!(
            label_language("[weights]\ninterests = 0.5\n").unwrap(),
            "en"
        );
        assert_eq!(
            label_language("[concepts]\nlabel_language = \"zh-Hans\"\n").unwrap(),
            "zh-Hans"
        );
        let err = label_language("[concepts]\nlabel_language = \"  \"\n").unwrap_err();
        assert!(err.contains("label_language"), "{err}");
        // The shipped file serves both readers.
        let shipped = include_str!("../pothos.toml");
        assert!(Weights::parse(shipped).is_ok());
        assert_eq!(label_language(shipped).unwrap(), "en");
    }
}
