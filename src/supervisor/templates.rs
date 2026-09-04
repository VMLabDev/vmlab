//! Supervisor-side, lab-scoped template operations (PRD §6): list a lab's
//! `template {}` blocks and run builds as background tasks. Progress streams
//! as `template.op.*` events on the supervisor broadcast, with an in-memory
//! log ring per operation.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::Supervisor;
use crate::config::model::TemplateDef;
use crate::proto::{CommandError, Event};
use crate::sync::LockRecover;
use crate::template::TemplateStore;

/// Kept log lines per running operation (replayed via `template.op_status`).
const LOG_CAP: usize = 500;

struct OpState {
    kind: &'static str,
    started: chrono::DateTime<chrono::Utc>,
    log: VecDeque<String>,
    /// Structured playbook progress (`playbook.op.*` from the synthetic build
    /// lab, forwarded live as `template.op.step`), kept for resync.
    steps: VecDeque<Value>,
    console: Option<PathBuf>,
    cancel: tokio_util::sync::CancellationToken,
}

type OpKey = (String, String, String);

/// Registry of in-flight template operations, keyed by `(lab, arch, template)` —
/// one operation per template at a time (a push cannot race its own build),
/// different templates may run concurrently.
#[derive(Clone, Default)]
pub struct TemplateOps {
    inner: Arc<Mutex<HashMap<OpKey, OpState>>>,
}

impl TemplateOps {
    /// Claim `(lab, template)` for `kind`; the returned guard releases the
    /// claim on drop, so error and panic paths cannot wedge a template.
    pub(super) fn try_begin(
        &self,
        lab: &str,
        arch: &str,
        template: &str,
        kind: &'static str,
    ) -> Result<OpGuard, CommandError> {
        let key = (lab.to_string(), arch.to_string(), template.to_string());
        let mut ops = self.inner.lock_recover();
        if let Some(op) = ops.get(&key) {
            return Err(CommandError::conflict(format!(
                "{} already running for `{arch}/{template}`{} — stop it with \
                 `vmlab template stop {template}`",
                op.kind,
                if lab.is_empty() {
                    String::new()
                } else {
                    format!(" in lab `{lab}`")
                },
            )));
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        ops.insert(
            key.clone(),
            OpState {
                kind,
                started: chrono::Utc::now(),
                log: VecDeque::new(),
                steps: VecDeque::new(),
                console: None,
                cancel: cancel.clone(),
            },
        );
        Ok(OpGuard {
            ops: self.clone(),
            key,
            cancel,
        })
    }

    fn append_log(&self, lab: &str, arch: &str, template: &str, line: &str) {
        let mut ops = self.inner.lock_recover();
        if let Some(op) = ops.get_mut(&(lab.to_string(), arch.to_string(), template.to_string())) {
            if op.log.len() == LOG_CAP {
                op.log.pop_front();
            }
            op.log.push_back(line.to_string());
        }
    }

    fn append_step(&self, lab: &str, arch: &str, template: &str, step: Value) {
        let mut ops = self.inner.lock_recover();
        if let Some(op) = ops.get_mut(&(lab.to_string(), arch.to_string(), template.to_string())) {
            if op.steps.len() == LOG_CAP {
                op.steps.pop_front();
            }
            op.steps.push_back(step);
        }
    }

    fn set_console(&self, lab: &str, arch: &str, template: &str, path: PathBuf) {
        let mut ops = self.inner.lock_recover();
        if let Some(op) = ops.get_mut(&(lab.to_string(), arch.to_string(), template.to_string())) {
            op.console = Some(path);
        }
    }

    /// Cancel the operation claiming `(lab, arch, template)`, provided it is
    /// the `kind` the caller means to stop — stopping a build and stopping a
    /// push are different requests, and answering the wrong one would cancel
    /// work nobody asked about.
    pub(super) fn cancel(
        &self,
        lab: &str,
        arch: &str,
        template: &str,
        kind: &str,
    ) -> Result<(), CommandError> {
        let ops = self.inner.lock_recover();
        // The lab is where the operation is *filed*, not what identifies it:
        // a build claims the lab of the directory it was started from, and a
        // stop asks with the lab of the directory the user is standing in.
        // Those differ routinely — `just <t>-build` cd's into the template's
        // own directory, while the operator stops it from the repository root
        // — and the mismatch made `template stop` answer "no build running"
        // about the very build `template build` was refusing as already
        // running. Unstoppable except by restarting the supervisor.
        //
        // So fall back to the pair that does identify the work. Only when
        // exactly one operation matches: two labs building the same template
        // is the case the lab is there to tell apart, and guessing between
        // them would cancel work nobody asked about.
        let op = ops
            .get(&(lab.to_string(), arch.to_string(), template.to_string()))
            .or_else(|| {
                let mut hits = ops
                    .iter()
                    .filter(|((_, a, t), _)| a == arch && t == template);
                match (hits.next(), hits.next()) {
                    (Some((_, op)), None) => Some(op),
                    _ => None,
                }
            })
            .ok_or_else(|| {
                CommandError::not_found(format!("no {kind} running for `{arch}/{template}`"))
            })?;
        if op.kind != kind {
            return Err(CommandError::conflict(format!(
                "the operation running for `{arch}/{template}` is a {}",
                op.kind
            )));
        }
        op.cancel.cancel();
        Ok(())
    }

    /// The running operation for `(lab, template)`, as JSON for `template.list`.
    fn op_of(&self, lab: &str, arch: &str, template: &str) -> Value {
        let ops = self.inner.lock_recover();
        match ops.get(&(lab.to_string(), arch.to_string(), template.to_string())) {
            Some(op) => json!({"kind": op.kind, "started": op.started.to_rfc3339()}),
            None => Value::Null,
        }
    }
}

/// Releases a [`TemplateOps`] claim on drop.
pub(super) struct OpGuard {
    ops: TemplateOps,
    key: OpKey,
    cancel: tokio_util::sync::CancellationToken,
}

impl OpGuard {
    pub(super) fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel.clone()
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        self.ops.inner.lock_recover().remove(&self.key);
    }
}

/// Parse a template file and return its `template {}` blocks. `file` is the
/// file to read — a shell may point at any of them — and defaults to the
/// lab's own `vmlab.wcl`, which is the only one the console knows about.
/// `root` stays the directory every root-relative path in the blocks resolves
/// against.
pub(super) fn load_defs(root: &Path, file: Option<&Path>) -> Result<Vec<TemplateDef>, String> {
    let path = match file {
        Some(file) => file.to_path_buf(),
        None => root.join(crate::paths::LAB_FILE),
    };
    let source = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let tf = crate::config::load_template_source(&source, &path.display().to_string(), root)
        .map_err(|e| format!("{:?}", miette::Report::new(e)))?;
    Ok(tf.templates)
}

fn find_def(
    root: &Path,
    file: Option<&Path>,
    template: &str,
    arch: Option<&str>,
) -> Result<TemplateDef, CommandError> {
    let mut matches = load_defs(root, file)?
        .into_iter()
        .filter(|d| d.name == template && arch.is_none_or(|a| d.arch == a));
    let first = matches.next().ok_or_else(|| {
        CommandError::not_found(match arch {
            Some(arch) => format!("no template named `{arch}/{template}` in the lab config"),
            None => format!("no template named `{template}` in the lab config"),
        })
    })?;
    if arch.is_none() && matches.next().is_some() {
        return Err(CommandError::invalid(format!(
            "template name `{template}` is ambiguous; specify its architecture"
        )));
    }
    Ok(first)
}

/// `template.list`: the lab's template definitions joined with their local
/// store versions (newest first) and any in-flight operation.
pub async fn list(
    lab: String,
    root: PathBuf,
    file: Option<PathBuf>,
    ops: TemplateOps,
) -> Result<Value, CommandError> {
    let entries = tokio::task::spawn_blocking(move || -> Result<Vec<Value>, CommandError> {
        let defs = load_defs(&root, file.as_deref())?;
        let store = TemplateStore::new(crate::paths::template_store_dir());
        Ok(defs
            .iter()
            .map(|def| {
                let mut versions = store.versions_of(&def.arch, &def.name).unwrap_or_default();
                versions.sort_by(|a, b| crate::template::store::compare_versions(b, a));
                json!({
                    "name": def.name,
                    "arch": def.arch,
                    "version_prefix": def.version,
                    "registry": def.registry,
                    "local_versions": versions,
                })
            })
            .collect())
    })
    .await
    .map_err(|e| e.to_string())??;

    let rows: Vec<Value> = entries
        .into_iter()
        .map(|mut e| {
            e["op"] = ops.op_of(
                &lab,
                e["arch"].as_str().unwrap_or_default(),
                e["name"].as_str().unwrap_or_default(),
            );
            e
        })
        .collect();
    Ok(Value::Array(rows))
}

/// `template.build`: kick off a background build of `template` from the lab's
/// config, streaming progress as `template.op.*` events. Returns as soon as
/// the build is claimed and spawned.
pub async fn start_build(
    sup: Arc<Supervisor>,
    lab: String,
    root: PathBuf,
    template: String,
    arch: Option<String>,
    version: Option<String>,
    file: Option<PathBuf>,
) -> Result<Value, CommandError> {
    let (def, profiles) = {
        let (root, template, arch, file) =
            (root.clone(), template.clone(), arch.clone(), file.clone());
        tokio::task::spawn_blocking(move || -> Result<_, CommandError> {
            let def = find_def(&root, file.as_deref(), &template, arch.as_deref())?;
            let profiles = crate::profiles::ProfileSet::load_default()
                .map_err(|e| format!("loading profiles: {e:#}"))?;
            Ok((def, profiles))
        })
        .await
        .map_err(|e| e.to_string())??
    };
    let arch = def.arch.clone();

    let guard = sup
        .template_ops
        .try_begin(&lab, &arch, &template, "build")?;
    let cancel = guard.cancel_token();
    sup.emit(Event::new(
        "template.op.start",
        &*lab,
        json!({"template": template, "arch": arch, "kind": "build"}),
    ));

    let log = op_sink(
        sup.clone(),
        lab.clone(),
        arch.clone(),
        template.clone(),
        "build",
    );
    tokio::spawn(async move {
        let _guard = guard;
        let store = TemplateStore::new(crate::paths::template_store_dir());
        let ready_sup = sup.clone();
        let ready_lab = lab.clone();
        let ready_arch = arch.clone();
        let ready_template = template.clone();
        let console_ready: crate::template::build::ConsoleReady = Arc::new(move |path| {
            ready_sup
                .template_ops
                .set_console(&ready_lab, &ready_arch, &ready_template, path);
            ready_sup.emit(Event::new(
                "template.op.console",
                &*ready_lab,
                json!({"template": ready_template, "arch": ready_arch, "kind": "build"}),
            ));
        });
        // Forward the synthetic build lab's playbook step stream as
        // `template.op.step` (live) and into the resync ring.
        let step_sup = sup.clone();
        let (step_lab, step_arch, step_template) = (lab.clone(), arch.clone(), template.clone());
        let on_event: crate::template::build::BuildEvent = Arc::new(move |ev| {
            let payload = json!({
                "template": step_template,
                "arch": step_arch,
                "kind": "build",
                "event": ev.event,
                "data": ev.data,
            });
            step_sup.template_ops.append_step(
                &step_lab,
                &step_arch,
                &step_template,
                payload.clone(),
            );
            step_sup.emit(Event::new("template.op.step", &*step_lab, payload));
        });
        let result = crate::template::build::build_template(
            &def,
            &root,
            &store,
            &profiles,
            log,
            version.as_deref(),
            crate::template::build::BuildControl {
                console_ready: Some(console_ready),
                on_event: Some(on_event),
                cancel: cancel.clone(),
            },
        )
        .await;
        match result {
            Ok(meta) => sup.emit(Event::new(
                "template.op.done",
                &*lab,
                json!({"template": template, "arch": arch, "kind": "build", "version": meta.version}),
            )),
            Err(_) if cancel.is_cancelled() => sup.emit(Event::new(
                "template.op.cancelled",
                &*lab,
                json!({"template": template, "arch": arch, "kind": "build"}),
            )),
            Err(e) => sup.emit(Event::new(
                "template.op.error",
                &*lab,
                json!({"template": template, "arch": arch, "kind": "build", "error": format!("{e:#}")}),
            )),
        }
    });
    Ok(json!({"started": true}))
}

/// Stop the active build for one exact architecture/template pair.
pub fn stop_build(
    sup: Arc<Supervisor>,
    lab: String,
    arch: String,
    template: String,
) -> Result<Value, CommandError> {
    sup.template_ops.cancel(&lab, &arch, &template, "build")?;
    Ok(json!({"stopping": true}))
}

/// An [`OutputSink`](crate::scripting::OutputSink) that appends to the op's
/// log ring and broadcasts each line as a `template.op.log` event.
pub(super) fn op_sink(
    sup: Arc<Supervisor>,
    lab: String,
    arch: String,
    template: String,
    kind: &'static str,
) -> crate::scripting::OutputSink {
    Arc::new(move |text: String| {
        for line in text.split('\n').filter(|l| !l.trim().is_empty()) {
            sup.template_ops.append_log(&lab, &arch, &template, line);
            sup.emit(Event::new(
                "template.op.log",
                &*lab,
                json!({"template": template, "arch": arch, "kind": kind, "line": line}),
            ));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A build is filed under the lab it was started from, and a stop asks
    /// with the lab the operator is standing in. Those differ routinely — the
    /// justfile cd's into each template's own directory — and when the lookup
    /// insisted on an exact match, `template stop` reported "no build running"
    /// about the build `template build` was refusing as already running.
    #[test]
    fn a_build_can_be_stopped_from_another_lab() {
        let ops = TemplateOps::default();
        let _guard = ops
            .try_begin("built-here", "x86_64", "base", "build")
            .unwrap();
        ops.cancel("standing-there", "x86_64", "base", "build")
            .expect("the arch/template pair identifies the work");
    }

    /// Except when two labs are building the same template: that is precisely
    /// what the lab distinguishes, and cancelling a guess would stop work
    /// nobody asked about.
    #[test]
    fn an_ambiguous_stop_is_refused() {
        let ops = TemplateOps::default();
        let _a = ops.try_begin("lab-a", "x86_64", "base", "build").unwrap();
        let _b = ops.try_begin("lab-b", "x86_64", "base", "build").unwrap();
        assert!(ops.cancel("lab-c", "x86_64", "base", "build").is_err());
        ops.cancel("lab-a", "x86_64", "base", "build")
            .expect("an exact lab still resolves");
    }

    #[test]
    fn second_op_on_same_template_rejected() {
        let ops = TemplateOps::default();
        let _guard = ops.try_begin("lab1", "x86_64", "base", "build").unwrap();
        let Err(err) = ops.try_begin("lab1", "x86_64", "base", "push") else {
            panic!("second claim should be rejected");
        };
        assert_eq!(err.code, crate::proto::ErrorCode::Conflict);
        assert!(err.message.contains("build already running"), "{err}");
        // Other templates and other labs are unaffected.
        ops.try_begin("lab1", "aarch64", "base", "build").unwrap();
        ops.try_begin("lab1", "x86_64", "other", "build").unwrap();
        ops.try_begin("lab2", "x86_64", "base", "build").unwrap();
    }

    #[test]
    fn guard_drop_releases_claim() {
        let ops = TemplateOps::default();
        drop(ops.try_begin("lab1", "x86_64", "base", "build").unwrap());
        ops.try_begin("lab1", "x86_64", "base", "push").unwrap();
    }

    #[test]
    fn cancellation_targets_one_build_architecture() {
        let ops = TemplateOps::default();
        let x86 = ops.try_begin("lab1", "x86_64", "base", "build").unwrap();
        let arm = ops.try_begin("lab1", "aarch64", "base", "build").unwrap();

        ops.cancel("lab1", "x86_64", "base", "build").unwrap();
        assert!(x86.cancel_token().is_cancelled());
        assert!(!arm.cancel_token().is_cancelled());
    }

    #[test]
    fn op_of_reflects_running_state() {
        let ops = TemplateOps::default();
        assert_eq!(ops.op_of("lab1", "x86_64", "base"), Value::Null);
        let _guard = ops.try_begin("lab1", "x86_64", "base", "push").unwrap();
        let op = ops.op_of("lab1", "x86_64", "base");
        assert_eq!(op["kind"], "push");
        assert!(op["started"].as_str().is_some());
    }

    #[tokio::test]
    async fn list_joins_defs_store_and_ops() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(crate::paths::LAB_FILE),
            r#"import <vmlab.wcl>
template "base" {
  arch    = "x86_64"
  version = "1.0"
  registry = "ghcr.io/acme/base"
  source "scratch" { }
}
"#,
        )
        .unwrap();
        let ops = TemplateOps::default();
        let _guard = ops.try_begin("lab1", "x86_64", "base", "build").unwrap();
        let rows = list("lab1".into(), root.path().to_path_buf(), None, ops)
            .await
            .unwrap();
        let rows = rows.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["name"], "base");
        assert_eq!(rows[0]["arch"], "x86_64");
        assert_eq!(rows[0]["version_prefix"], "1.0");
        assert_eq!(rows[0]["registry"], "ghcr.io/acme/base");
        assert_eq!(rows[0]["local_versions"], json!([]));
        assert_eq!(rows[0]["op"]["kind"], "build");
    }

    #[test]
    fn find_def_reports_missing_template() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(crate::paths::LAB_FILE),
            "import <vmlab.wcl>\ntemplate \"base\" { arch = \"x86_64\" version = \"1\" source \"scratch\" { } }\n",
        )
        .unwrap();
        assert!(find_def(root.path(), None, "base", None).is_ok());
        let err = find_def(root.path(), None, "nope", None).unwrap_err();
        assert_eq!(err.code, crate::proto::ErrorCode::NotFound);
        assert!(err.message.contains("no template named"), "{err}");
    }

    #[test]
    fn find_def_requires_arch_for_duplicate_names() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join(crate::paths::LAB_FILE),
            r#"import <vmlab.wcl>
template "base" { arch = "x86_64" version = "1" source "scratch" { } }
template "base" { arch = "aarch64" version = "1" source "scratch" { } }
"#,
        )
        .unwrap();
        let err = find_def(root.path(), None, "base", None).unwrap_err();
        assert_eq!(err.code, crate::proto::ErrorCode::InvalidArgument);
        assert!(err.message.contains("ambiguous"), "{err}");
        assert_eq!(
            find_def(root.path(), None, "base", Some("aarch64"))
                .unwrap()
                .arch,
            "aarch64"
        );
    }
}
