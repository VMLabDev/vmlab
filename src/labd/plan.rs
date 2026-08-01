//! The **LabPlan**: what an `up` or `down` is going to do, worked out in full
//! before anything is started or stopped.
//!
//! Ordering used to be decided inside the loop that executed it, so the rules
//! — which machines a subset drags in, which wave a machine lands in, which
//! configuration steps are in scope — could only be checked by bringing a real
//! lab up. Here they are a value, and the tests below assert on it.
//!
//! The two directions are mirror images, and saying so out loud fixed a real
//! defect. `up` expands **dependencies** (starting `srv01` needs `dc01`) and
//! gates forward. `down` expands **dependents** (stopping `dc01` must take
//! `srv01` with it) and gates in reverse. Before this, `down` did neither: it
//! stopped everything in parallel, so `vmlab down dc01` left domain members
//! running against a dead controller, and a full `down` could kill a domain
//! controller while its members were still shutting down.

use std::collections::{HashMap, HashSet};

use crate::config::model::{Lab, Playbook, Provision};

/// Which way through the dependency graph a plan runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Start machines: pull in what they depend on, dependencies first.
    Up,
    /// Stop machines: pull in what depends on them, dependents first.
    Down,
}

/// One `up`-phase configuration step: a `provision`/`playbook` block declared
/// inside `machine`, which is therefore its target.
#[derive(Debug, Clone)]
pub struct Step {
    pub machine: String,
    pub kind: StepKind,
}

#[derive(Debug, Clone)]
pub enum StepKind {
    Provision(Provision),
    Playbook(Playbook),
}

impl Step {
    /// How the step names itself in `up` output.
    pub fn describe(&self) -> String {
        match &self.kind {
            StepKind::Provision(p) => format!("provision {}", p.script.display()),
            StepKind::Playbook(p) => format!("playbook {} play {}", p.path.display(), p.play),
        }
    }
}

/// Something the plan deliberately leaves out, and why — announced rather than
/// dropped silently, so a partial `up` never looks like it ran everything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub what: String,
    pub why: String,
}

/// The ordered work a lifecycle operation will perform.
#[derive(Debug, Clone)]
pub struct LabPlan {
    /// Machines grouped into waves. Every machine in a wave may run
    /// concurrently; wave *n* only begins once wave *n-1* has finished.
    pub waves: Vec<Vec<String>>,
    /// Configuration steps to interleave, in declaration order. Empty for
    /// [`Direction::Down`].
    pub steps: Vec<Step>,
    pub skipped: Vec<Skip>,
}

impl LabPlan {
    /// Every machine the plan touches, in wave order.
    pub fn machines(&self) -> impl Iterator<Item = &String> {
        self.waves.iter().flatten()
    }
}

/// A machine's `depends_on`, whichever kind it is.
fn deps_of(lab: &Lab, name: &str) -> Vec<String> {
    lab.machine(name)
        .map(|m| m.depends_on().to_vec())
        .unwrap_or_default()
}

/// Work out what `direction` over `subset` will do.
///
/// An empty `subset` means the whole lab. A non-empty one is expanded
/// transitively — by dependencies going up, by dependents coming down — so a
/// partial operation never leaves the lab in a state its own `depends_on`
/// declarations say is impossible.
///
/// Returns `Err` naming any machine that is not in the lab, and any dependency
/// cycle, rather than discovering either mid-flight.
pub fn plan(lab: &Lab, subset: &[String], direction: Direction) -> Result<LabPlan, String> {
    let known: Vec<String> = lab.machines().map(|m| m.name().to_string()).collect();
    for name in subset {
        if !known.contains(name) {
            return Err(format!(
                "no vm or container \"{name}\" in lab \"{}\"",
                lab.name
            ));
        }
    }

    // Who points at whom. `dependents` is the reverse edge set, which is what
    // makes `down` the mirror of `up` rather than a special case.
    let dependencies: HashMap<String, Vec<String>> =
        known.iter().map(|n| (n.clone(), deps_of(lab, n))).collect();
    let mut dependents: HashMap<String, Vec<String>> =
        known.iter().map(|n| (n.clone(), Vec::new())).collect();
    for (name, deps) in &dependencies {
        for d in deps {
            if let Some(list) = dependents.get_mut(d) {
                list.push(name.clone());
            }
        }
    }

    // Following `dependencies` going up and `dependents` coming down is the
    // whole difference between the two directions.
    let edges = match direction {
        Direction::Up => &dependencies,
        Direction::Down => &dependents,
    };

    let mut skipped = Vec::new();
    let targets: Vec<String> = if subset.is_empty() {
        known.clone()
    } else {
        let mut wanted: HashSet<String> = HashSet::new();
        let mut stack: Vec<String> = subset.to_vec();
        while let Some(n) = stack.pop() {
            if wanted.insert(n.clone())
                && let Some(next) = edges.get(&n)
            {
                stack.extend(next.iter().cloned());
            }
        }
        for name in &wanted {
            if !subset.contains(name) {
                skipped.push(Skip {
                    what: name.clone(),
                    why: match direction {
                        Direction::Up => "pulled in — something asked for depends on it".into(),
                        Direction::Down => "pulled in — it depends on something asked for".into(),
                    },
                });
            }
        }
        known
            .iter()
            .filter(|n| wanted.contains(*n))
            .cloned()
            .collect()
    };

    // Waves: everything whose in-scope edges are already satisfied.
    let mut waves: Vec<Vec<String>> = Vec::new();
    let mut remaining = targets.clone();
    let mut done: HashSet<String> = HashSet::new();
    while !remaining.is_empty() {
        let wave: Vec<String> = remaining
            .iter()
            .filter(|n| {
                edges
                    .get(*n)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                    .iter()
                    .all(|d| done.contains(d) || !targets.contains(d))
            })
            .cloned()
            .collect();
        if wave.is_empty() {
            return Err(format!(
                "dependency deadlock among: {}",
                remaining.join(", ")
            ));
        }
        remaining.retain(|n| !wave.contains(n));
        done.extend(wave.iter().cloned());
        waves.push(wave);
    }

    // Configuration steps belong to their machine, so a partial `up` only runs
    // the steps of machines it started — anything else would stall the queue
    // on a machine that is never coming up.
    let mut steps = Vec::new();
    if direction == Direction::Up {
        for step in declared_steps(lab) {
            if targets.contains(&step.machine) {
                steps.push(step);
            } else {
                skipped.push(Skip {
                    what: step.describe(),
                    why: format!("\"{}\" is not in this `up`", step.machine),
                });
            }
        }
    }

    Ok(LabPlan {
        waves,
        steps,
        skipped,
    })
}

/// Every machine's configuration steps in file declaration order — within a
/// machine that is the order its blocks were written, and across machines the
/// order the machine blocks appear. Block byte spans recover both, since the
/// model keeps provisions and playbooks in separate vecs.
fn declared_steps(lab: &Lab) -> Vec<Step> {
    let mut steps: Vec<(usize, Step)> = Vec::new();
    for m in lab.machines() {
        for p in m.provisions() {
            steps.push((
                p.span.0,
                Step {
                    machine: m.name().to_string(),
                    kind: StepKind::Provision(p.clone()),
                },
            ));
        }
        for p in m.playbooks() {
            steps.push((
                p.span.0,
                Step {
                    machine: m.name().to_string(),
                    kind: StepKind::Playbook(p.clone()),
                },
            ));
        }
    }
    steps.sort_by_key(|(at, _)| *at);
    steps.into_iter().map(|(_, s)| s).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lab_of(src: &str) -> Lab {
        crate::config::load_lab_source(src, "<test>", std::path::Path::new("/tmp"))
            .expect("parse")
            .lab
    }

    /// dc01 ← srv01 ← app01, plus an unrelated container.
    fn chain() -> Lab {
        lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  vm "dc01"  { template = "x86_64/t" }
  vm "srv01" { template = "x86_64/t" depends_on = ["dc01"] }
  vm "app01" { template = "x86_64/t" depends_on = ["srv01"] }
  container "cache" { image = "redis" }
}"#,
        )
    }

    fn wave_names(p: &LabPlan) -> Vec<Vec<&str>> {
        p.waves
            .iter()
            .map(|w| w.iter().map(String::as_str).collect())
            .collect()
    }

    #[test]
    fn up_gates_forward_on_dependencies() {
        let p = plan(&chain(), &[], Direction::Up).unwrap();
        assert_eq!(
            wave_names(&p),
            vec![vec!["dc01", "cache"], vec!["srv01"], vec!["app01"]],
            "independents go first and together"
        );
    }

    /// The mirror image, and the fix: stopping a lab must take dependents down
    /// before the things they depend on.
    #[test]
    fn down_gates_in_reverse() {
        let p = plan(&chain(), &[], Direction::Down).unwrap();
        assert_eq!(
            wave_names(&p),
            vec![vec!["app01", "cache"], vec!["srv01"], vec!["dc01"]],
            "leaves first, the domain controller last"
        );
    }

    #[test]
    fn up_subset_pulls_in_dependencies() {
        let p = plan(&chain(), &["app01".into()], Direction::Up).unwrap();
        assert_eq!(
            wave_names(&p),
            vec![vec!["dc01"], vec!["srv01"], vec!["app01"]],
            "cache is unrelated and stays out"
        );
        assert!(p.skipped.iter().any(|s| s.what == "dc01"));
    }

    /// `vmlab down dc01` used to stop only dc01, leaving srv01 and app01
    /// running against a dead dependency.
    #[test]
    fn down_subset_pulls_in_dependents() {
        let p = plan(&chain(), &["dc01".into()], Direction::Down).unwrap();
        assert_eq!(
            wave_names(&p),
            vec![vec!["app01"], vec!["srv01"], vec!["dc01"]],
            "everything that depends on dc01 comes down with it, leaves first"
        );
        let pulled: Vec<&str> = p.skipped.iter().map(|s| s.what.as_str()).collect();
        assert!(pulled.contains(&"srv01") && pulled.contains(&"app01"));
    }

    #[test]
    fn a_cycle_is_reported_not_discovered_mid_flight() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" depends_on = ["b"] }
  vm "b" { template = "x86_64/t" depends_on = ["a"] }
}"#,
        );
        let err = plan(&lab, &[], Direction::Up).unwrap_err();
        assert!(err.contains("deadlock"), "{err}");
    }

    #[test]
    fn an_unknown_machine_is_named() {
        let err = plan(&chain(), &["nope".into()], Direction::Up).unwrap_err();
        assert!(err.contains("nope"), "{err}");
    }

    /// Within a machine, steps run in the order its blocks were written;
    /// across machines, in the order the machine blocks appear.
    #[test]
    fn steps_follow_declaration_order() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" {
    template = "x86_64/t"
    provision "scripts/one.ws" { }
    playbook "pb/base" { play = "base" }
    provision "scripts/two.ws" { }
  }
  vm "b" {
    template = "x86_64/t"
    provision "scripts/three.ws" { }
  }
}"#,
        );
        let p = plan(&lab, &[], Direction::Up).unwrap();
        let labels: Vec<String> = p
            .steps
            .iter()
            .map(|s| format!("{} → {}", s.describe(), s.machine))
            .collect();
        assert_eq!(
            labels,
            vec![
                "provision scripts/one.ws → a",
                "playbook pb/base play base → a",
                "provision scripts/two.ws → a",
                "provision scripts/three.ws → b",
            ]
        );
    }

    /// A partial `up` announces the steps it is not running rather than
    /// dropping them silently.
    #[test]
    fn out_of_scope_steps_are_announced() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" provision "scripts/a.ws" { } }
  vm "b" { template = "x86_64/t" provision "scripts/b.ws" { } }
}"#,
        );
        let p = plan(&lab, &["a".into()], Direction::Up).unwrap();
        assert_eq!(p.steps.len(), 1, "only a's step runs");
        assert!(
            p.skipped
                .iter()
                .any(|s| s.what.contains("b.ws") && s.why.contains("\"b\"")),
            "b's step is announced as skipped: {:?}",
            p.skipped
        );
    }

    /// `down` carries no configuration steps — there is nothing to converge on
    /// a machine that is stopping.
    #[test]
    fn down_carries_no_steps() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" provision "scripts/a.ws" { } }
}"#,
        );
        assert!(plan(&lab, &[], Direction::Down).unwrap().steps.is_empty());
    }
}
