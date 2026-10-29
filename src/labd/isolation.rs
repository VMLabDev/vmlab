//! The **isolation plan**: what worktree isolation (PRD §21.4, ADR-0019)
//! changes about a lab's network, worked out once at lab start as one value.
//!
//! A frozen family's labs run clones of the same guests, with the same
//! hostnames, MACs, machine-ids and Windows SIDs, so no two of them may share
//! a wire or a route. Whether a lab is in a family is a fact about its root
//! ([`Family::of`]), read when the daemon starts and fixed for its lifetime:
//! a freeze or a thaw needs the main lab stopped, and a worktree lab built on
//! a freeze keeps it until destroyed.
//!
//! Under isolation:
//!
//! - NAT refuses every new flow to a host address ([`crate::net::nat::guard`]);
//!   the plan only says the guard is on.
//! - Port forwards bind [`forward_bind`]: `127.0.0.1`, not `0.0.0.0`.
//! - A worktree lab's `global = true` segments join `<segment>@<worktree>`
//!   ([`IsolationPlan::global_name`]), and a `connect {}` peer refuses `up`
//!   ([`IsolationPlan::refusal`]). The main lab keeps the plain names.
//!
//! Without a family there is no plan, and nothing changes.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::config::model::Lab;
use crate::freeze::Family;

/// Where the lab stands in its family.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// The main checkout's lab, frozen.
    Main,
    /// A linked worktree's lab, named by its worktree.
    Worktree { name: String },
}

/// Worktree isolation for one lab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationPlan {
    pub role: Role,
    /// The family's newest generation and its snapshot, as `worktree.json`
    /// names them.
    pub generation: u32,
    pub snapshot: String,
    /// Each `global = true` segment, by its declared name, and the name it
    /// joins in the supervisor.
    pub globals: BTreeMap<String, String>,
    /// The global segments declaring a `connect {}` peer — refused in a
    /// worktree lab, empty in the main lab.
    pub refused_peers: Vec<String>,
    /// Declared rules isolation will defeat, said once at lab start: a
    /// `redirect {}` onto a host address is a NAT flow like any other.
    pub warnings: Vec<String>,
    /// The main checkout's working data, which holds the family's port
    /// record (`worktree-ports.json`, §21.7).
    pub family_local: PathBuf,
}

/// The plan for `lab` at `root`, or `None` when no freeze applies to it.
pub fn decide(root: &Path, lab: &Lab) -> Result<Option<IsolationPlan>> {
    let Some(family) = Family::of(root)? else {
        return Ok(None);
    };
    let worktree = crate::instance::worktree_name(root);
    let family_local = match &family {
        Family::Worktree { main_lab_local, .. } => main_lab_local.clone(),
        Family::FrozenMain { .. } => crate::paths::lab_local_dir(root),
    };
    Ok(Some(plan(lab, &family, worktree.as_deref(), family_local)))
}

/// The plan for `lab` in `family`. `worktree` is the linked worktree's name
/// ([`crate::instance::worktree_name`]); a worktree lab without one, which
/// [`Family::of`] does not produce, falls back to the lab's own name.
/// `family_local` is the main checkout's working data.
pub fn plan(
    lab: &Lab,
    family: &Family,
    worktree: Option<&str>,
    family_local: PathBuf,
) -> IsolationPlan {
    let config = family.config();
    let role = match family {
        Family::FrozenMain { .. } => Role::Main,
        Family::Worktree { .. } => Role::Worktree {
            name: worktree.unwrap_or(&lab.name).to_string(),
        },
    };
    let global_segments = || lab.segments.iter().filter(|s| s.global);
    let globals = global_segments()
        .map(|s| (s.name.clone(), global_name(&role, &s.name)))
        .collect();
    let refused_peers = match role {
        Role::Main => Vec::new(),
        Role::Worktree { .. } => global_segments()
            .filter(|s| s.connect.is_some())
            .map(|s| s.name.clone())
            .collect(),
    };
    let warnings = lab
        .segments
        .iter()
        .flat_map(|s| s.redirect_rules.iter().map(move |r| (s, r)))
        .filter(|(_, r)| r.to.ip.is_loopback() || r.to.ip.is_unspecified())
        .map(|(s, r)| {
            format!(
                "segment \"{}\": redirect to {} is a host address, which worktree isolation \
                 refuses (PRD §21.4)",
                s.name, r.to.ip
            )
        })
        .collect();
    IsolationPlan {
        role,
        generation: config.generation,
        snapshot: config.snapshot.clone(),
        globals,
        refused_peers,
        warnings,
        family_local,
    }
}

fn global_name(role: &Role, segment: &str) -> String {
    match role {
        Role::Main => segment.to_string(),
        Role::Worktree { name } => format!("{segment}@{name}"),
    }
}

/// The address every port forward binds: `127.0.0.1` under isolation, every
/// interface otherwise.
pub fn forward_bind(plan: Option<&IsolationPlan>) -> Ipv4Addr {
    match plan {
        Some(_) => Ipv4Addr::LOCALHOST,
        None => Ipv4Addr::UNSPECIFIED,
    }
}

impl IsolationPlan {
    /// The lab's name in the family's port record: its worktree's, `None`
    /// for the main lab (slot 0).
    pub fn port_owner(&self) -> Option<&str> {
        match &self.role {
            Role::Main => None,
            Role::Worktree { name } => Some(name),
        }
    }

    /// The name the supervisor knows the global segment `segment` by.
    pub fn global_name(&self, segment: &str) -> String {
        self.globals
            .get(segment)
            .cloned()
            .unwrap_or_else(|| global_name(&self.role, segment))
    }

    /// Why `up` refuses this lab, if it does: a worktree lab peering a
    /// global segment with another host would carry the clone onto a wire
    /// some other lab of the family may share.
    pub fn refusal(&self) -> Option<String> {
        if self.refused_peers.is_empty() {
            return None;
        }
        let segments = self
            .refused_peers
            .iter()
            .map(|s| format!("\"{s}\""))
            .collect::<Vec<_>>()
            .join(", ");
        Some(format!(
            "global segment {segments} declares a `connect {{}}` peer, which a worktree lab \
             may not have: its machines are clones of the main lab's (PRD §21.4)"
        ))
    }

    /// The line a lab says at start, and the event it records.
    pub fn describe(&self) -> String {
        let whose = match &self.role {
            Role::Main => "the frozen main lab".to_string(),
            Role::Worktree { name } => format!("worktree \"{name}\""),
        };
        format!(
            "worktree isolation: {whose}, generation {} at snapshot \"{}\" — NAT refuses host \
             addresses, port forwards bind 127.0.0.1",
            self.generation, self.snapshot
        )
    }

    /// The status projection (ADR-0004).
    pub fn status(&self) -> crate::status::IsolationStatus {
        crate::status::IsolationStatus {
            role: match self.role {
                Role::Main => "main",
                Role::Worktree { .. } => "worktree",
            }
            .to_string(),
            worktree: match &self.role {
                Role::Main => None,
                Role::Worktree { name } => Some(name.clone()),
            },
            generation: self.generation,
            snapshot: self.snapshot.clone(),
            forward_bind: forward_bind(Some(self)).to_string(),
            globals: self
                .globals
                .iter()
                .map(|(segment, joined)| crate::status::IsolatedGlobal {
                    segment: segment.clone(),
                    joined: joined.clone(),
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::freeze::WorktreeConfig;

    fn lab_of(src: &str) -> Lab {
        crate::config::load_lab_source(src, "<test>", Path::new("/tmp"))
            .expect("parse")
            .lab
    }

    fn config() -> WorktreeConfig {
        WorktreeConfig {
            generation: 2,
            snapshot: "base".into(),
        }
    }

    fn main() -> Family {
        Family::FrozenMain { config: config() }
    }

    fn worktree() -> Family {
        Family::Worktree {
            main_root: "/src/main".into(),
            main_lab_local: "/src/main/.vmlab".into(),
            config: config(),
        }
    }

    fn globals_lab() -> Lab {
        lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" nat = true }
  segment "shared" { global = true }
  segment "corp" { global = true }
}"#,
        )
    }

    #[test]
    fn without_a_family_forwards_bind_every_interface() {
        assert_eq!(forward_bind(None), Ipv4Addr::UNSPECIFIED);
    }

    #[test]
    fn the_main_lab_keeps_its_global_names_and_binds_loopback() {
        let p = plan(&globals_lab(), &main(), None, "/src/main/.vmlab".into());
        assert_eq!(p.role, Role::Main);
        assert_eq!(forward_bind(Some(&p)), Ipv4Addr::LOCALHOST);
        assert_eq!(p.global_name("shared"), "shared");
        assert_eq!(p.global_name("corp"), "corp");
        assert_eq!(p.globals.len(), 2, "only global segments: {:?}", p.globals);
        assert_eq!(p.refusal(), None);
        assert_eq!((p.generation, p.snapshot.as_str()), (2, "base"));
    }

    /// A worktree lab's global segments are its own, named by worktree, so
    /// two lab files of the worktree meet there and nobody else does.
    #[test]
    fn a_worktree_lab_joins_its_globals_by_worktree() {
        let p = plan(
            &globals_lab(),
            &worktree(),
            Some("feat-x"),
            "/src/main/.vmlab".into(),
        );
        assert_eq!(
            p.role,
            Role::Worktree {
                name: "feat-x".into()
            }
        );
        assert_eq!(p.global_name("shared"), "shared@feat-x");
        assert_eq!(p.global_name("corp"), "corp@feat-x");
        assert_eq!(forward_bind(Some(&p)), Ipv4Addr::LOCALHOST);
        assert_eq!(p.refusal(), None);
        assert_eq!(p.port_owner(), Some("feat-x"));
    }

    #[test]
    fn a_worktree_lab_with_a_peer_is_refused_naming_the_section() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "shared" { global = true connect { host = "otherhost:13947" } }
  segment "corp" { global = true }
}"#,
        );
        let p = plan(&lab, &worktree(), Some("feat-x"), "/src/main/.vmlab".into());
        assert_eq!(p.refused_peers, vec!["shared".to_string()]);
        let why = p.refusal().expect("refused");
        assert!(why.contains("\"shared\""), "{why}");
        assert!(why.contains("§21.4"), "{why}");
        // The frozen main lab keeps its peer.
        assert_eq!(
            plan(&lab, &main(), None, "/src/main/.vmlab".into()).refusal(),
            None
        );
    }

    #[test]
    fn a_redirect_onto_the_host_is_warned_about() {
        let lab = lab_of(
            r#"import <vmlab.wcl>
lab "l" {
  segment "lan" { subnet = "10.0.0.0/24" nat = true
    redirect { from = "10.0.0.1:8080" to = "127.0.0.1:8080" }
    redirect { from = "10.0.0.1:53" to = "1.1.1.1:53" }
  }
}"#,
        );
        let p = plan(&lab, &main(), None, "/src/main/.vmlab".into());
        assert_eq!(p.warnings.len(), 1, "{:?}", p.warnings);
        assert!(p.warnings[0].contains("127.0.0.1"), "{:?}", p.warnings);
    }

    #[test]
    fn the_status_projection_names_the_worktree_and_its_globals() {
        let s = plan(
            &globals_lab(),
            &worktree(),
            Some("feat-x"),
            "/src/main/.vmlab".into(),
        )
        .status();
        assert_eq!(s.role, "worktree");
        assert_eq!(s.worktree.as_deref(), Some("feat-x"));
        assert_eq!(s.forward_bind, "127.0.0.1");
        assert!(
            s.globals
                .iter()
                .any(|g| g.segment == "shared" && g.joined == "shared@feat-x")
        );
    }

    /// `decide` reads the freeze record from the lab's own working data.
    #[test]
    fn decide_reads_worktree_json() {
        let tmp = tempfile::tempdir().unwrap();
        let lab = globals_lab();
        assert_eq!(decide(tmp.path(), &lab).unwrap(), None);
        config()
            .save(&crate::paths::lab_local_dir(tmp.path()))
            .unwrap();
        let p = decide(tmp.path(), &lab).unwrap().expect("frozen main");
        assert_eq!(p.role, Role::Main);
        assert_eq!(p.port_owner(), None, "slot 0");
        assert_eq!(p.family_local, crate::paths::lab_local_dir(tmp.path()));
    }
}
