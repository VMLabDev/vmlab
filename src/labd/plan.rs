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
//!
//! Starting and provisioning are separate orders (§7.2). `depends_on` is start
//! order alone and builds the waves; `depends_on_provision` orders one
//! machine's configuration steps after another's and nothing else. The
//! provisioning half of the plan is one [`Provisioning`] per machine — its own
//! steps in file order and the machines whose steps must finish first — so
//! machines nothing orders provision in parallel. It replaces one file-ordered
//! queue that stalled at the first step whose machine had not started, which
//! ran a dependency's provisions *after* a dependent declared above it had
//! started.

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

/// One machine's share of `up`'s configuration (§7.2): its own steps, in the
/// order its blocks were written, run once it has started, and only after
/// every machine in `after` has finished all of its own.
#[derive(Debug, Clone)]
pub struct Provisioning {
    pub machine: String,
    pub steps: Vec<Step>,
    /// The machine's `depends_on_provision`. Every name is in the plan: a
    /// subset `up` pulls them in.
    pub after: Vec<String>,
}

/// The ordered work a lifecycle operation will perform.
#[derive(Debug, Clone)]
pub struct LabPlan {
    /// Machines grouped into waves by `depends_on` alone. Every machine in a
    /// wave may run concurrently; wave *n* only begins once wave *n-1* has
    /// finished starting. Provisioning never holds a wave back.
    pub waves: Vec<Vec<String>>,
    /// One entry per machine the plan starts, in wave order, steps or none.
    /// Empty for [`Direction::Down`].
    pub provisioning: Vec<Provisioning>,
    pub skipped: Vec<Skip>,
}

impl LabPlan {
    /// Every machine the plan touches, in wave order.
    pub fn machines(&self) -> impl Iterator<Item = &String> {
        self.waves.iter().flatten()
    }

    /// `machine`'s provisioning, if the plan starts it.
    pub fn provisioning_of(&self, machine: &str) -> Option<&Provisioning> {
        self.provisioning.iter().find(|p| p.machine == machine)
    }

    /// Whether some machine's steps wait for `machine`'s to finish — which
    /// makes `up` wait for `machine` to reach the point its steps run even
    /// when it has none.
    pub fn provision_awaited(&self, machine: &str) -> bool {
        self.provisioning
            .iter()
            .any(|p| p.after.iter().any(|a| a == machine))
    }
}

/// The two ordering keys (§7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Key {
    DependsOn,
    DependsOnProvision,
}

impl Key {
    fn name(self) -> &'static str {
        match self {
            Key::DependsOn => "depends_on",
            Key::DependsOnProvision => "depends_on_provision",
        }
    }

    fn of(self, lab: &Lab, name: &str) -> Vec<String> {
        lab.machine(name)
            .map(|m| match self {
                Key::DependsOn => m.depends_on().to_vec(),
                Key::DependsOnProvision => m.depends_on_provision().to_vec(),
            })
            .unwrap_or_default()
    }
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
    // makes `down` the mirror of `up` rather than a special case. Both are
    // `depends_on` alone: it is the only key that orders starting and
    // stopping.
    let dependencies: HashMap<String, Vec<String>> = known
        .iter()
        .map(|n| (n.clone(), Key::DependsOn.of(lab, n)))
        .collect();
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
        // A subset `up` follows both keys — a machine that must provision
        // first is as necessary as one that must start first — and says
        // which key, and whose, pulled each machine in. `down` follows
        // `depends_on`'s reverse edges alone.
        let keys: &[Key] = match direction {
            Direction::Up => &[Key::DependsOn, Key::DependsOnProvision],
            Direction::Down => &[Key::DependsOn],
        };
        let mut wanted: HashSet<String> = subset.iter().cloned().collect();
        let mut queue: std::collections::VecDeque<String> = subset.iter().cloned().collect();
        while let Some(n) = queue.pop_front() {
            for key in keys {
                let next = match (direction, key) {
                    (Direction::Down, _) => edges.get(&n).cloned().unwrap_or_default(),
                    (Direction::Up, key) => key.of(lab, &n),
                };
                for m in next {
                    if wanted.insert(m.clone()) {
                        skipped.push(Skip {
                            what: m.clone(),
                            why: match direction {
                                Direction::Up => {
                                    format!("pulled in — \"{n}\" names it in {}", key.name())
                                }
                                Direction::Down => {
                                    format!("pulled in — it names \"{n}\" in {}", key.name())
                                }
                            },
                        });
                        queue.push_back(m);
                    }
                }
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
    // the steps of machines it starts, and announces the rest.
    let mut provisioning = Vec::new();
    if direction == Direction::Up {
        let mut by_machine: HashMap<String, Vec<Step>> = HashMap::new();
        for step in declared_steps(lab) {
            if targets.contains(&step.machine) {
                by_machine
                    .entry(step.machine.clone())
                    .or_default()
                    .push(step);
            } else {
                skipped.push(Skip {
                    what: step.describe(),
                    why: format!("\"{}\" is not in this `up`", step.machine),
                });
            }
        }
        for machine in waves.iter().flatten() {
            provisioning.push(Provisioning {
                machine: machine.clone(),
                steps: by_machine.remove(machine).unwrap_or_default(),
                after: Key::DependsOnProvision.of(lab, machine),
            });
        }
        provisioning_deadlock(&provisioning)?;
    }

    Ok(LabPlan {
        waves,
        provisioning,
        skipped,
    })
}

/// Refuse a `depends_on_provision` cycle before anything starts. Validation
/// already rejects one (§5.1); this keeps the plan from ever handing the
/// runtime a schedule that would wait forever.
fn provisioning_deadlock(provisioning: &[Provisioning]) -> Result<(), String> {
    let mut done: HashSet<&str> = HashSet::new();
    let mut remaining: Vec<&Provisioning> = provisioning.iter().collect();
    while !remaining.is_empty() {
        let ready: Vec<&str> = remaining
            .iter()
            .filter(|p| p.after.iter().all(|a| done.contains(a.as_str())))
            .map(|p| p.machine.as_str())
            .collect();
        if ready.is_empty() {
            let names: Vec<&str> = remaining.iter().map(|p| p.machine.as_str()).collect();
            return Err(format!(
                "depends_on_provision deadlock among: {}",
                names.join(", ")
            ));
        }
        remaining.retain(|p| !ready.contains(&p.machine.as_str()));
        done.extend(ready);
    }
    Ok(())
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

    /// Every configuration step in scope, machine by machine.
    fn steps(p: &LabPlan) -> impl Iterator<Item = &Step> {
        p.provisioning.iter().flat_map(|m| m.steps.iter())
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
        assert!(
            p.skipped.iter().any(
                |s| s.what == "dc01" && s.why == "pulled in — \"srv01\" names it in depends_on"
            ),
            "{:?}",
            p.skipped
        );
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

    /// Within a machine, steps run in the order its blocks were written; each
    /// machine carries its own, so no machine's steps queue behind another's.
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
        let labels: Vec<String> = steps(&p)
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
        assert!(
            p.provisioning.iter().all(|m| m.after.is_empty()),
            "nothing orders a's steps against b's: {:?}",
            p.provisioning
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
        assert_eq!(steps(&p).count(), 1, "only a's step runs");
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
        let p = plan(&lab, &[], Direction::Down).unwrap();
        assert!(p.provisioning.is_empty());
    }

    /// `depends_on` is start order alone (§7.2): a machine that only
    /// provisions after another boots with it, in the same wave.
    #[test]
    fn waves_come_from_depends_on_alone() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  vm "dc"     { template = "x86_64/t" provision "scripts/dc.ws" { } }
  vm "member" { template = "x86_64/t" depends_on_provision = ["dc"] provision "scripts/join.ws" { } }
  vm "app"    { template = "x86_64/t" depends_on = ["dc"] }
}"#,
        );
        let p = plan(&lab, &[], Direction::Up).unwrap();
        assert_eq!(wave_names(&p), vec![vec!["dc", "member"], vec!["app"]]);
        assert_eq!(p.provisioning_of("member").unwrap().after, vec!["dc"]);
        assert!(
            p.provisioning_of("app").unwrap().after.is_empty(),
            "depends_on orders starting, never provisioning"
        );
        assert!(p.provision_awaited("dc"));
        assert!(!p.provision_awaited("member") && !p.provision_awaited("app"));
    }

    /// The F12 defect: a dependent declared above its dependency. The old
    /// file-ordered queue ran `dc`'s provision after `member` had started;
    /// the plan now carries the order `depends_on_provision` asked for on the
    /// machine itself, whatever order the blocks were written in.
    #[test]
    fn a_provision_gate_does_not_depend_on_declaration_order() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  vm "member" {
    template = "x86_64/t"
    depends_on = ["dc"]
    depends_on_provision = ["dc"]
    provision "scripts/join.ws" { }
  }
  vm "dc" { template = "x86_64/t" provision "scripts/dc.ws" { } }
}"#,
        );
        let p = plan(&lab, &[], Direction::Up).unwrap();
        assert_eq!(wave_names(&p), vec![vec!["dc"], vec!["member"]]);
        let member = p.provisioning_of("member").unwrap();
        assert_eq!(member.after, vec!["dc"]);
        assert_eq!(member.steps.len(), 1);
        assert_eq!(p.provisioning_of("dc").unwrap().steps.len(), 1);
    }

    /// A subset follows both keys and says which key, and whose, pulled each
    /// machine in.
    #[test]
    fn up_subset_pulls_in_through_both_keys() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  vm "dc"     { template = "x86_64/t" }
  vm "db"     { template = "x86_64/t" depends_on = ["dc"] }
  vm "member" { template = "x86_64/t" depends_on_provision = ["db"] }
  vm "other"  { template = "x86_64/t" }
}"#,
        );
        let p = plan(&lab, &["member".into()], Direction::Up).unwrap();
        assert_eq!(wave_names(&p), vec![vec!["dc", "member"], vec!["db"]]);
        let why = |what: &str| {
            p.skipped
                .iter()
                .find(|s| s.what == what)
                .map(|s| s.why.clone())
                .unwrap_or_default()
        };
        assert_eq!(
            why("db"),
            "pulled in — \"member\" names it in depends_on_provision"
        );
        assert_eq!(why("dc"), "pulled in — \"db\" names it in depends_on");
        assert!(why("other").is_empty(), "other is unrelated and stays out");
    }

    /// `down` follows `depends_on` alone: provisioning order has nothing to
    /// say about stopping.
    #[test]
    fn down_ignores_depends_on_provision() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  vm "dc"     { template = "x86_64/t" }
  vm "member" { template = "x86_64/t" depends_on_provision = ["dc"] }
}"#,
        );
        let p = plan(&lab, &["dc".into()], Direction::Down).unwrap();
        assert_eq!(wave_names(&p), vec![vec!["dc"]]);
        let p = plan(&lab, &[], Direction::Down).unwrap();
        assert_eq!(wave_names(&p), vec![vec!["dc", "member"]]);
    }

    /// A cycle validation would have refused is still refused by the plan,
    /// not discovered as a hang.
    #[test]
    fn a_provision_cycle_is_refused() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  vm "a" { template = "x86_64/t" depends_on_provision = ["b"] }
  vm "b" { template = "x86_64/t" depends_on_provision = ["a"] }
}"#,
        );
        let err = plan(&lab, &[], Direction::Up).unwrap_err();
        assert!(err.contains("depends_on_provision deadlock"), "{err}");
    }
}
