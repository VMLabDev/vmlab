//! The three hand-written tables that steer the generator (G1b, G1c, G1d).

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SetsFile {
    selected: Vec<String>,
    set: BTreeMap<String, Set>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Set {
    features: Vec<String>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct OverridesFile {
    #[serde(default, rename = "override")]
    overrides: Vec<Override>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Override {
    pub winner: String,
    pub loser: String,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ConstraintsFile {
    #[serde(default, rename = "constraint")]
    constraints: Vec<Constraint>,
}

/// One extra condition on a format class (G1d). `require` is a Rust
/// expression of type `bool` over the raw fields of the class, each a `u32`
/// under its snake-case name. A record whose condition is false does not
/// match, and the decoder goes on to the next candidate.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Constraint {
    pub format: String,
    /// Empty: every record of the class.
    #[serde(default)]
    pub records: Vec<String>,
    pub require: String,
    /// Why the condition holds, and what found it.
    pub why: String,
}

pub struct Config {
    /// Feature names without the `Feature` prefix, in the order of sets.toml.
    pub features: Vec<String>,
    pub overrides: Vec<Override>,
    pub constraints: Vec<Constraint>,
}

impl Config {
    pub fn load(dir: &Path) -> Result<Self> {
        let read = |name: &str| {
            std::fs::read_to_string(dir.join(name))
                .with_context(|| format!("read {}", dir.join(name).display()))
        };
        let sets: SetsFile = toml::from_str(&read("sets.toml")?).context("sets.toml")?;
        let mut features = Vec::new();
        let mut seen = BTreeSet::new();
        for name in &sets.selected {
            let Some(set) = sets.set.get(name) else {
                bail!("sets.toml selects the set {name:?}, which it does not define");
            };
            for feature in &set.features {
                if seen.insert(feature.clone()) {
                    features.push(feature.clone());
                }
            }
        }
        if features.len() > 16 {
            bail!("more than 16 selected features: widen `Features` in the generator");
        }
        let overrides: OverridesFile =
            toml::from_str(&read("overrides.toml")?).context("overrides.toml")?;
        let constraints: ConstraintsFile =
            toml::from_str(&read("constraints.toml")?).context("constraints.toml")?;
        for constraint in &constraints.constraints {
            if constraint.why.trim().is_empty() {
                bail!(
                    "constraints.toml: the constraint on {} gives no reason",
                    constraint.format
                );
            }
        }
        Ok(Self {
            features,
            overrides: overrides.overrides,
            constraints: constraints.constraints,
        })
    }
}
