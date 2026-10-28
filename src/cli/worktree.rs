//! `vmlab worktree` — freeze the main checkout's lab for worktree labs to
//! clone from, thaw it, and say where the family stands (PRD §21).
//!
//! `freeze` and `thaw` go to the lab daemon, as the snapshot verbs do: it
//! owns the lab's state and its machines. `status` reads the files alone —
//! the freeze, `worktree.json` and every worktree disk's backing chain — so it
//! answers in a worktree whose main lab has no daemon running, or none at all.

use anyhow::Result;
use clap::Subcommand;
use serde_json::Value;

use super::daemon::remote;
use super::lab::{lab_client_for, rt};
use crate::freeze::inspect::FreezeStatus;
use crate::proto::LabRequest;

/// Worktree labs (PRD §21).
#[derive(Subcommand)]
pub enum WorktreeCmd {
    /// Freeze the stopped main lab at an offline snapshot every machine has,
    /// for linked git worktrees to clone their machines from
    ///
    /// The machines' disks move into `.vmlab/freeze/<n>/` and become
    /// read-only; the main lab carries on in overlays over them, in exactly
    /// the state it had. Snapshots taken before the freeze are locked until
    /// `vmlab worktree thaw`. Freezing again adds a generation.
    Freeze {
        /// The offline snapshot to freeze at
        snapshot: String,
        /// Emit the raw JSON reply instead of a report
        #[arg(long)]
        json: bool,
    },
    /// Fold the freeze back into the stopped main lab's disks and unlock its
    /// earlier snapshots
    ///
    /// Refused while any worktree lab's disk backs onto the freeze: `vmlab
    /// destroy` there releases it. The main lab keeps its current state.
    Thaw {
        /// Give up the online snapshots taken since a freeze, whose machine
        /// state cannot be carried into the thawed disks, rather than refuse
        #[arg(long)]
        drop_online_snapshots: bool,
        /// Emit the raw JSON reply instead of a report
        #[arg(long)]
        json: bool,
    },
    /// Each generation of the main lab's freeze, its snapshot, and the
    /// worktrees whose disks back onto it
    Status {
        /// Emit the raw JSON instead of a table
        #[arg(long)]
        json: bool,
    },
}

pub fn cmd_worktree(cmd: WorktreeCmd) -> Result<()> {
    match cmd {
        WorktreeCmd::Freeze { snapshot, json } => cmd_freeze(snapshot, json),
        WorktreeCmd::Thaw {
            drop_online_snapshots,
            json,
        } => cmd_thaw(drop_online_snapshots, json),
        WorktreeCmd::Status { json } => cmd_status(json),
    }
}

fn cmd_freeze(snapshot: String, json: bool) -> Result<()> {
    rt()?.block_on(async {
        let (_name, client) = lab_client_for(None).await?;
        let reply = client
            .send(LabRequest::WorktreeFreeze { snapshot })
            .await
            .map_err(remote)?;
        super::emit(json, &reply, render_freeze)
    })
}

fn render_freeze(reply: &Value) -> String {
    let machines: Vec<&str> = reply["machines"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut out = format!(
        "froze generation {} at \"{}\": {}\n\
         snapshots taken before it are locked until `vmlab worktree thaw`; linked worktrees \
         now clone these machines on `vmlab up`\n",
        reply["generation"],
        reply["snapshot"].as_str().unwrap_or("?"),
        machines.join(", ")
    );
    push_warnings(&mut out, reply);
    out
}

fn cmd_thaw(drop_online_snapshots: bool, json: bool) -> Result<()> {
    rt()?.block_on(async {
        let (_name, client) = lab_client_for(None).await?;
        let reply = client
            .send(LabRequest::WorktreeThaw {
                drop_online_snapshots,
            })
            .await
            .map_err(remote)?;
        super::emit(json, &reply, render_thaw)
    })
}

fn render_thaw(reply: &Value) -> String {
    let mut out = String::from("thawed: the freeze is gone and every snapshot is unlocked\n");
    for d in reply["dropped"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "dropped snapshot \"{}\" of {}\n",
            d["name"].as_str().unwrap_or("?"),
            d["machine"].as_str().unwrap_or("?")
        ));
    }
    push_warnings(&mut out, reply);
    out
}

fn push_warnings(out: &mut String, reply: &Value) {
    for w in reply["warnings"].as_array().into_iter().flatten() {
        if let Some(w) = w.as_str() {
            out.push_str(&format!("WARNING: {w}\n"));
        }
    }
}

fn cmd_status(json: bool) -> Result<()> {
    rt()?.block_on(async {
        let root = crate::paths::find_lab_root(&std::env::current_dir()?)?;
        let status = crate::freeze::inspect::status(&root).await?;
        super::emit(json, &serde_json::to_value(&status)?, |_| {
            render_status(&status)
        })
    })
}

fn render_status(s: &FreezeStatus) -> String {
    let mut out = format!("main checkout: {}\n", s.main_root.display());
    if let Some(wt) = &s.worktree {
        out.push_str(&format!("this worktree: {}\n", wt.display()));
    }
    if s.generations.is_empty() {
        out.push_str(
            "not frozen — `vmlab worktree freeze <snapshot>` in the main checkout freezes it\n",
        );
        return out;
    }
    let rows: Vec<[String; 4]> = s
        .generations
        .iter()
        .map(|g| {
            let worktrees: Vec<String> = g
                .worktrees
                .iter()
                .map(|w| w.display().to_string())
                .collect();
            [
                if g.newest {
                    format!("{} (newest)", g.generation)
                } else {
                    g.generation.to_string()
                },
                g.snapshot.clone().unwrap_or_else(|| "?".into()),
                g.machines.join(", "),
                if worktrees.is_empty() {
                    "-".into()
                } else {
                    worktrees.join(", ")
                },
            ]
        })
        .collect();
    let headers = ["GENERATION", "SNAPSHOT", "MACHINES", "WORKTREES"];
    let widths: Vec<usize> = (0..3)
        .map(|i| {
            rows.iter()
                .map(|r| r[i].len())
                .max()
                .unwrap_or(0)
                .max(headers[i].len())
        })
        .collect();
    let line = |cols: [&str; 4]| {
        format!(
            "{:<w0$}  {:<w1$}  {:<w2$}  {}\n",
            cols[0],
            cols[1],
            cols[2],
            cols[3],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2]
        )
    };
    out.push_str(&line(headers));
    for r in &rows {
        out.push_str(&line([&r[0], &r[1], &r[2], &r[3]]));
    }
    if s.config.is_none() {
        out.push_str(
            "WARNING: generations are on disk but worktree.json is missing, so worktree labs \
             do not clone from them\n",
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::freeze::WorktreeConfig;
    use crate::freeze::inspect::GenerationStatus;

    #[test]
    fn status_tabulates_each_generation_and_its_worktrees() {
        let s = FreezeStatus {
            main_root: "/m/labs/demo".into(),
            worktree: None,
            config: Some(WorktreeConfig {
                generation: 2,
                snapshot: "two".into(),
            }),
            generations: vec![
                GenerationStatus {
                    generation: 1,
                    snapshot: Some("base".into()),
                    machines: vec!["dc".into(), "ws".into()],
                    newest: false,
                    worktrees: vec!["/wt/a/labs/demo".into()],
                },
                GenerationStatus {
                    generation: 2,
                    snapshot: Some("two".into()),
                    machines: vec!["dc".into()],
                    newest: true,
                    worktrees: vec![],
                },
            ],
        };
        let out = render_status(&s);
        assert!(
            out.contains("1           base      dc, ws    /wt/a/labs/demo"),
            "{out}"
        );
        assert!(out.contains("2 (newest)  two       dc        -"), "{out}");
    }

    #[test]
    fn an_unfrozen_lab_says_how_to_freeze() {
        let s = FreezeStatus {
            main_root: "/m".into(),
            worktree: Some("/wt".into()),
            config: None,
            generations: vec![],
        };
        let out = render_status(&s);
        assert!(out.contains("this worktree: /wt") && out.contains("worktree freeze"));
    }

    #[test]
    fn thaw_names_what_it_dropped() {
        let out = render_thaw(&serde_json::json!({
            "dropped": [{"machine": "dc", "name": "live"}],
            "warnings": ["litter"],
        }));
        assert!(out.contains("dropped snapshot \"live\" of dc"));
        assert!(out.contains("WARNING: litter"));
    }
}
