//! Supervisor-side template operations for the web UI (PRD §6): list a lab's
//! `template {}` blocks, query registry status, and run builds/pushes as
//! background tasks. Progress streams as `template.op.*` events on the
//! supervisor broadcast (the web events channel forwards them verbatim), with
//! an in-memory log ring so a reconnecting UI can replay the tail.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::Supervisor;
use crate::config::model::TemplateDef;
use crate::proto::Event;
use crate::template::TemplateStore;

/// Kept log lines per running operation (replayed via `template.op_status`).
const LOG_CAP: usize = 500;

struct OpState {
    kind: &'static str,
    started: chrono::DateTime<chrono::Utc>,
    log: VecDeque<String>,
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
    fn try_begin(
        &self,
        lab: &str,
        arch: &str,
        template: &str,
        kind: &'static str,
    ) -> Result<OpGuard, String> {
        let key = (lab.to_string(), arch.to_string(), template.to_string());
        let mut ops = self.inner.lock().unwrap();
        if let Some(op) = ops.get(&key) {
            return Err(format!(
                "{} already running for `{arch}/{template}`",
                op.kind
            ));
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        ops.insert(
            key.clone(),
            OpState {
                kind,
                started: chrono::Utc::now(),
                log: VecDeque::new(),
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
        let mut ops = self.inner.lock().unwrap();
        if let Some(op) = ops.get_mut(&(lab.to_string(), arch.to_string(), template.to_string())) {
            if op.log.len() == LOG_CAP {
                op.log.pop_front();
            }
            op.log.push_back(line.to_string());
        }
    }

    fn set_console(&self, lab: &str, arch: &str, template: &str, path: PathBuf) {
        let mut ops = self.inner.lock().unwrap();
        if let Some(op) = ops.get_mut(&(lab.to_string(), arch.to_string(), template.to_string())) {
            op.console = Some(path);
        }
    }

    pub fn console_path(&self, lab: &str, arch: &str, template: &str) -> Result<PathBuf, String> {
        let ops = self.inner.lock().unwrap();
        let op = ops
            .get(&(lab.to_string(), arch.to_string(), template.to_string()))
            .ok_or_else(|| format!("no operation running for `{arch}/{template}`"))?;
        let path = op
            .console
            .as_ref()
            .ok_or_else(|| format!("console for `{arch}/{template}` is not ready"))?;
        if !path.exists() {
            return Err(format!(
                "console for `{arch}/{template}` is no longer available"
            ));
        }
        Ok(path.clone())
    }

    fn cancel_build(&self, lab: &str, arch: &str, template: &str) -> Result<(), String> {
        let ops = self.inner.lock().unwrap();
        let op = ops
            .get(&(lab.to_string(), arch.to_string(), template.to_string()))
            .ok_or_else(|| format!("no build running for `{arch}/{template}`"))?;
        if op.kind != "build" {
            return Err(format!(
                "the operation running for `{arch}/{template}` is a push"
            ));
        }
        op.cancel.cancel();
        Ok(())
    }

    /// The running operation for `(lab, template)`, as JSON for `template.list`.
    fn op_of(&self, lab: &str, arch: &str, template: &str) -> Value {
        let ops = self.inner.lock().unwrap();
        match ops.get(&(lab.to_string(), arch.to_string(), template.to_string())) {
            Some(op) => json!({"kind": op.kind, "started": op.started.to_rfc3339()}),
            None => Value::Null,
        }
    }

    /// All running operations for `lab` with their log tails
    /// (`template.op_status` — reconnecting UIs resync from this).
    pub fn status(&self, lab: &str) -> Value {
        let ops = self.inner.lock().unwrap();
        let mut rows: Vec<Value> = ops
            .iter()
            .filter(|((l, _, _), _)| l == lab)
            .map(|((_, arch, template), op)| {
                json!({
                    "arch": arch,
                    "template": template,
                    "kind": op.kind,
                    "started": op.started.to_rfc3339(),
                    "log_tail": op.log.iter().collect::<Vec<_>>(),
                    "console_ready": op.console.as_ref().is_some_and(|path| path.exists()),
                })
            })
            .collect();
        rows.sort_by(|a, b| {
            a["template"]
                .as_str()
                .cmp(&b["template"].as_str())
                .then_with(|| a["arch"].as_str().cmp(&b["arch"].as_str()))
        });
        Value::Array(rows)
    }
}

/// Releases a [`TemplateOps`] claim on drop.
struct OpGuard {
    ops: TemplateOps,
    key: OpKey,
    cancel: tokio_util::sync::CancellationToken,
}

impl OpGuard {
    fn cancel_token(&self) -> tokio_util::sync::CancellationToken {
        self.cancel.clone()
    }
}

impl Drop for OpGuard {
    fn drop(&mut self) {
        self.ops.inner.lock().unwrap().remove(&self.key);
    }
}

/// Parse the lab's `vmlab.wcl` and return its `template {}` blocks (the same
/// loader `vmlab template build` uses, so a lab-less template file works too).
fn load_defs(root: &Path) -> Result<Vec<TemplateDef>, String> {
    let path = root.join(crate::paths::LAB_FILE);
    let source = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let tf = crate::config::load_template_source(&source, &path.display().to_string(), root)
        .map_err(|e| format!("{:?}", miette::Report::new(e)))?;
    Ok(tf.templates)
}

fn find_def(root: &Path, template: &str, arch: Option<&str>) -> Result<TemplateDef, String> {
    let mut matches = load_defs(root)?
        .into_iter()
        .filter(|d| d.name == template && arch.is_none_or(|a| d.arch == a));
    let first = matches.next().ok_or_else(|| match arch {
        Some(arch) => format!("no template named `{arch}/{template}` in the lab config"),
        None => format!("no template named `{template}` in the lab config"),
    })?;
    if arch.is_none() && matches.next().is_some() {
        return Err(format!(
            "template name `{template}` is ambiguous; specify its architecture"
        ));
    }
    Ok(first)
}

/// `template.list`: the lab's template definitions joined with their local
/// store versions (newest first) and any in-flight operation.
pub async fn list(lab: String, root: PathBuf, ops: TemplateOps) -> Result<Value, String> {
    let entries = tokio::task::spawn_blocking(move || -> Result<Vec<Value>, String> {
        let defs = load_defs(&root)?;
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

/// `template.remote`: the concrete version tags (and their arches) published
/// under the template's registry, newest first.
pub async fn remote(
    root: PathBuf,
    template: String,
    arch: Option<String>,
) -> Result<Value, String> {
    use futures::StreamExt as _;

    let def = {
        let template = template.clone();
        tokio::task::spawn_blocking(move || find_def(&root, &template, arch.as_deref()))
            .await
            .map_err(|e| e.to_string())??
    };
    let Some(repo) = def.registry else {
        return Err(format!("template `{template}` has no `registry` set"));
    };
    let registry = crate::oci::Registry::new(&repo).map_err(|e| format!("{e:#}"))?;
    let tags = registry.list_tags().await.map_err(|e| format!("{e:#}"))?;
    // Concrete versions start with a digit; `latest`/`latest-prerelease` are
    // moving aliases of one of them.
    let mut versions: Vec<String> = tags
        .into_iter()
        .filter(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .collect();
    versions.sort_by(|a, b| crate::template::store::compare_versions(b, a));

    let registry = &registry;
    let rows: Vec<Value> = futures::stream::iter(versions.into_iter().map(|tag| async move {
        let arches = registry.index_arches(&tag).await.unwrap_or_default();
        json!({"tag": tag, "arches": arches})
    }))
    .buffered(8)
    .collect()
    .await;
    Ok(json!({"registry": repo, "tags": rows}))
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
) -> Result<Value, String> {
    let (def, profiles) = {
        let (root, template, arch) = (root.clone(), template.clone(), arch.clone());
        tokio::task::spawn_blocking(move || -> Result<_, String> {
            let def = find_def(&root, &template, arch.as_deref())?;
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
        let result = crate::template::build::build_template(
            &def,
            &root,
            &store,
            &profiles,
            log,
            None,
            crate::template::build::BuildControl {
                console_ready: Some(console_ready),
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
) -> Result<Value, String> {
    sup.template_ops.cancel_build(&lab, &arch, &template)?;
    Ok(json!({"stopping": true}))
}

/// `template.push`: kick off a background push of a locally stored version
/// (default: the newest) to the template's registry.
pub async fn start_push(
    sup: Arc<Supervisor>,
    lab: String,
    root: PathBuf,
    template: String,
    arch: Option<String>,
    version: Option<String>,
) -> Result<Value, String> {
    let (resolved, repo) = {
        let (root, template, arch) = (root.clone(), template.clone(), arch.clone());
        tokio::task::spawn_blocking(move || -> Result<_, String> {
            let def = find_def(&root, &template, arch.as_deref())?;
            let store = TemplateStore::new(crate::paths::template_store_dir());
            let resolved = store
                .resolve(&def.arch, &def.name, version.as_deref())
                .map_err(|e| format!("{e:#}"))?;
            let repo = resolved
                .meta
                .registry
                .clone()
                .or(def.registry)
                .ok_or("no push target — set `registry` in the template")?;
            Ok((resolved, repo))
        })
        .await
        .map_err(|e| e.to_string())??
    };
    let version = resolved.meta.version.clone();
    let arch = resolved.meta.arch.clone();
    let target = crate::oci::with_version_tag(&repo, &version).map_err(|e| format!("{e:#}"))?;

    let guard = sup.template_ops.try_begin(&lab, &arch, &template, "push")?;
    sup.emit(Event::new(
        "template.op.start",
        &*lab,
        json!({"template": template, "arch": arch, "kind": "push", "version": version}),
    ));

    let log = op_sink(
        sup.clone(),
        lab.clone(),
        arch.clone(),
        template.clone(),
        "push",
    );
    let started_version = version.clone();
    tokio::spawn(async move {
        let _guard = guard;
        let push_arch = resolved.meta.arch.clone();
        let host_cfg = crate::config::host::HostConfig::load_default().unwrap_or_default();
        log(format!(
            "pushing {push_arch}/{template}@{version} to {target}\n"
        ));
        // No source-repo annotation: the daemon's cwd says nothing about the
        // template's git origin (the CLI detects it from the caller's cwd).
        let result = crate::template::oci_bridge::push(
            &resolved.dir,
            &target,
            host_cfg.oci_chunk_size,
            &push_arch,
            None,
            Some("latest"),
        )
        .await;
        match result {
            Ok(()) => sup.emit(Event::new(
                "template.op.done",
                &*lab,
                json!({"template": template, "arch": arch, "kind": "push", "version": version}),
            )),
            Err(e) => {
                let mut error = format!("{e:#}");
                if error.contains("401") || error.to_lowercase().contains("unauthorized") {
                    error.push_str(" — run `vmlab template login <registry>` on the host");
                }
                sup.emit(Event::new(
                    "template.op.error",
                    &*lab,
                    json!({"template": template, "arch": arch, "kind": "push", "error": error}),
                ));
            }
        }
    });
    Ok(json!({"started": true, "version": started_version}))
}

/// An [`OutputSink`](crate::scripting::OutputSink) that appends to the op's
/// log ring and broadcasts each line as a `template.op.log` event.
fn op_sink(
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

    #[test]
    fn second_op_on_same_template_rejected() {
        let ops = TemplateOps::default();
        let _guard = ops.try_begin("lab1", "x86_64", "base", "build").unwrap();
        let Err(err) = ops.try_begin("lab1", "x86_64", "base", "push") else {
            panic!("second claim should be rejected");
        };
        assert!(err.contains("build already running"), "{err}");
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

        ops.cancel_build("lab1", "x86_64", "base").unwrap();
        assert!(x86.cancel_token().is_cancelled());
        assert!(!arm.cancel_token().is_cancelled());
    }

    #[test]
    fn log_ring_caps_and_status_reports_tail() {
        let ops = TemplateOps::default();
        let _guard = ops.try_begin("lab1", "x86_64", "base", "build").unwrap();
        for i in 0..(LOG_CAP + 10) {
            ops.append_log("lab1", "x86_64", "base", &format!("line {i}"));
        }
        let status = ops.status("lab1");
        let rows = status.as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["template"], "base");
        assert_eq!(rows[0]["arch"], "x86_64");
        assert_eq!(rows[0]["kind"], "build");
        let tail = rows[0]["log_tail"].as_array().unwrap();
        assert_eq!(tail.len(), LOG_CAP);
        assert_eq!(tail[0], "line 10");
        assert_eq!(tail[LOG_CAP - 1], format!("line {}", LOG_CAP + 9));
        // Logs to a template with no running op are dropped, not panics.
        ops.append_log("lab1", "x86_64", "ghost", "ignored");
        assert!(ops.status("lab2").as_array().unwrap().is_empty());
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

    #[test]
    fn console_is_exposed_only_while_ready_build_is_running() {
        let ops = TemplateOps::default();
        let guard = ops.try_begin("lab1", "x86_64", "base", "build").unwrap();
        assert!(ops.console_path("lab1", "x86_64", "base").is_err());
        assert_eq!(ops.status("lab1")[0]["console_ready"], false);

        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("vnc.sock");
        std::fs::write(&socket, "test").unwrap();
        ops.set_console("lab1", "x86_64", "base", socket.clone());
        assert_eq!(ops.console_path("lab1", "x86_64", "base").unwrap(), socket);
        assert_eq!(ops.status("lab1")[0]["console_ready"], true);

        drop(guard);
        assert!(ops.console_path("lab1", "x86_64", "base").is_err());
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
        let rows = list("lab1".into(), root.path().to_path_buf(), ops)
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
        assert!(find_def(root.path(), "base", None).is_ok());
        let err = find_def(root.path(), "nope", None).unwrap_err();
        assert!(err.contains("no template named"), "{err}");
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
        let err = find_def(root.path(), "base", None).unwrap_err();
        assert!(err.contains("ambiguous"), "{err}");
        assert_eq!(
            find_def(root.path(), "base", Some("aarch64")).unwrap().arch,
            "aarch64"
        );
    }
}
