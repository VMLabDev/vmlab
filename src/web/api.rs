//! REST handlers. Each is a thin translation of an HTTP request into a daemon
//! proto call, returning the daemon's JSON (or an error mapped to a 4xx/5xx).

use actix_web::{HttpResponse, web};
use serde::Deserialize;
use serde_json::{Value, json};

use super::state::AppState;

/// Map a daemon error string to an HTTP response.
pub(crate) fn fail(e: String) -> HttpResponse {
    // Unknown lab / vm is the client's fault; everything else is treated as a
    // bad gateway to the daemon.
    if e.contains("already running") {
        HttpResponse::Conflict().json(json!({"error": e}))
    } else if e.contains("invalid lab name")
        || e.contains("no push target")
        || e.contains("has no `registry`")
    {
        HttpResponse::BadRequest().json(json!({"error": e}))
    } else if e.contains("unknown lab")
        || e.contains("no such")
        || e.contains("not found")
        || e.contains("no template named")
    {
        HttpResponse::NotFound().json(json!({"error": e}))
    } else {
        HttpResponse::BadGateway().json(json!({"error": e}))
    }
}

fn ok(v: Value) -> HttpResponse {
    HttpResponse::Ok().json(v)
}

/// `GET /api/labs` — running labs (registry) merged with the cwd lab, labs
/// created this session (cached roots), and the managed labs home on disk.
pub async fn list_labs(state: web::Data<AppState>) -> HttpResponse {
    let mut labs = state
        .supervisor_call("status", Value::Null)
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    let push_stopped = |labs: &mut Vec<Value>, name: &str, root: &std::path::Path| {
        if !labs.iter().any(|l| l["name"].as_str() == Some(name)) {
            labs.push(json!({
                "name": name,
                "root": root.to_string_lossy(),
                "state": "stopped",
            }));
        }
    };

    // Ensure the cwd lab shows up even if its daemon isn't running yet.
    if let Some((name, root)) = &state.default_lab {
        push_stopped(&mut labs, name, root);
    }
    // Labs created through the web this session (covers custom-path labs).
    for (name, root) in state.known_roots().await {
        push_stopped(&mut labs, &name, &root);
    }
    // Labs on disk under the managed labs home (durable across restarts).
    if let Ok(mut dir) = tokio::fs::read_dir(vmlab::paths::labs_home()).await {
        while let Ok(Some(entry)) = dir.next_entry().await {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            let root = entry.path();
            if super::state::valid_name(&name)
                && tokio::fs::try_exists(root.join(vmlab::paths::LAB_FILE))
                    .await
                    .unwrap_or(false)
            {
                push_stopped(&mut labs, &name, &root);
            }
        }
    }
    ok(json!(labs))
}

#[derive(Deserialize)]
pub struct CreateLabBody {
    name: String,
    /// Absolute directory to create the lab in; omitted = the managed labs
    /// home (`~/.local/share/vmlab/labs/<name>`).
    #[serde(default)]
    path: Option<String>,
}

/// `POST /api/labs` `{name, path?}` — scaffold a new lab: create the
/// directory, write an initial `vmlab.wcl`, and register the root so every
/// other lab-addressed endpoint resolves it immediately.
pub async fn create_lab(
    state: web::Data<AppState>,
    body: web::Json<CreateLabBody>,
) -> HttpResponse {
    let name = body.name.trim().to_string();
    if !super::state::valid_name(&name) {
        return HttpResponse::BadRequest().json(json!({
            "error": format!("invalid lab name `{name}` — use a DNS label (letters, digits, hyphens)"),
        }));
    }
    if state.lab_root(&name).await.is_ok() {
        return HttpResponse::Conflict().json(json!({
            "error": format!("lab `{name}` already exists"),
        }));
    }
    let dir = match body.path.as_deref().map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => {
            let dir = std::path::PathBuf::from(p);
            if !dir.is_absolute()
                || dir
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return HttpResponse::BadRequest().json(json!({
                    "error": "custom location must be an absolute path without `..`",
                }));
            }
            dir
        }
        None => vmlab::paths::labs_home().join(&name),
    };

    let (create_name, create_dir) = (name.clone(), dir.clone());
    match web::block(move || vmlab::lab_init::create_lab_dir(&create_name, &create_dir)).await {
        Ok(Ok(())) => {
            state.register_root(&name, dir.clone()).await;
            HttpResponse::Created().json(json!({
                "name": name,
                "root": dir.to_string_lossy(),
            }))
        }
        Ok(Err(e)) => {
            let msg = format!("{e:#}");
            if msg.contains("already exists") {
                HttpResponse::Conflict().json(json!({"error": msg}))
            } else {
                HttpResponse::InternalServerError().json(json!({"error": msg}))
            }
        }
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

/// `GET /api/catalog/templates` — every template in the local store, for the
/// editor's template picker. Read in-process (same pattern as `get_config`:
/// the store belongs to the same host user as the daemons).
pub async fn catalog_templates() -> HttpResponse {
    let result = web::block(|| {
        vmlab::template::TemplateStore::new(vmlab::paths::template_store_dir()).list()
    })
    .await;
    match result {
        Ok(Ok(list)) => {
            let rows: Vec<Value> = list
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "arch": t.arch,
                        "version": t.version,
                        "profile": t.profile,
                        "cpus": t.cpus,
                        "memory": t.memory,
                        "disk": t.disk,
                        "firmware": t.firmware,
                        "tpm": t.tpm,
                        "secure_boot": t.secure_boot,
                        "display": t.display,
                        "created": t.created.to_rfc3339(),
                        "origin": t.origin,
                        "registry": t.registry,
                    })
                })
                .collect();
            ok(json!(rows))
        }
        Ok(Err(e)) => HttpResponse::InternalServerError().json(json!({"error": format!("{e:#}")})),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

/// `GET /api/catalog/profiles` — guest OS profile names for the editor's
/// profile picker.
pub async fn catalog_profiles() -> HttpResponse {
    let result = web::block(|| {
        vmlab::profiles::ProfileSet::load_default().map(|set| {
            let mut names: Vec<String> = set.names().map(str::to_string).collect();
            names.sort();
            names
        })
    })
    .await;
    match result {
        Ok(Ok(names)) => ok(json!(names)),
        Ok(Err(e)) => HttpResponse::InternalServerError().json(json!({"error": format!("{e:#}")})),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

/// `GET /api/catalog/meta` — schema enums the editor renders as pickers,
/// sourced from the Rust constants so they can never drift from the code.
pub async fn catalog_meta() -> HttpResponse {
    ok(json!({
        "arches": vmlab::config::model::KNOWN_ARCHES,
        "events": vmlab::config::model::EVENT_NAMES,
        "firmware": ["ovmf", "seabios"],
        "gpu_modes": ["passthrough", "virgl", "vulkan"],
        "sinkhole_modes": ["nxdomain", "zero"],
        "forward_protos": ["tcp", "udp", "both"],
        "l4_protos": ["tcp", "udp", "icmp"],
        "media_kinds": ["iso", "floppy"],
    }))
}

/// `GET /api/labs/{lab}` — full lab status (vms + segments).
pub async fn lab_status(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    match state.lab_call(&lab, "status", Value::Null).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `POST /api/labs/{lab}/{action}` where action ∈ up|down|destroy.
pub async fn lab_action(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, action) = path.into_inner();
    let cmd = match action.as_str() {
        "up" | "down" | "destroy" => action.as_str(),
        _ => return HttpResponse::NotFound().json(json!({"error": "unknown lab action"})),
    };
    match state.lab_call(&lab, cmd, json!({})).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `POST /api/labs/{lab}/vms/{vm}/{action}` where action ∈ start|stop|restart|destroy.
pub async fn vm_action(
    state: web::Data<AppState>,
    path: web::Path<(String, String, String)>,
) -> HttpResponse {
    let (lab, vm, action) = path.into_inner();
    let cmd = match action.as_str() {
        "start" => "vm.start",
        "stop" => "vm.stop",
        "restart" => "vm.restart",
        "destroy" => "vm.destroy",
        _ => return HttpResponse::NotFound().json(json!({"error": "unknown vm action"})),
    };
    match state.lab_call(&lab, cmd, json!({"vm": vm})).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub struct SendKeys {
    keys: String,
}

/// `POST /api/labs/{lab}/vms/{vm}/sendkeys` `{keys}`.
pub async fn vm_sendkeys(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    body: web::Json<SendKeys>,
) -> HttpResponse {
    let (lab, vm) = path.into_inner();
    match state
        .lab_call(&lab, "vm.sendkeys", json!({"vm": vm, "keys": body.keys}))
        .await
    {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/vms/{vm}/screenshot.png` — capture and stream a PNG.
/// A non-VNC fallback (the live view uses the WebSocket bridge).
pub async fn vm_screenshot(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, vm) = path.into_inner();
    // `lab` is checked by lab_call's root lookup; `vm` lands in a filename.
    if !super::state::valid_name(&vm) {
        return HttpResponse::BadRequest().json(json!({"error": "invalid vm name"}));
    }
    // A unique file under the lab's private runtime dir (not the shared
    // system temp dir), removed once streamed.
    static SHOT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SHOT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let out = vmlab::paths::lab_runtime_dir(&lab)
        .join(format!("web-shot-{vm}-{}-{seq}.png", std::process::id()));
    let out_str = out.to_string_lossy().to_string();
    if let Err(e) = state
        .lab_call(&lab, "vm.screenshot", json!({"vm": vm, "path": out_str}))
        .await
    {
        return fail(e);
    }
    let bytes = tokio::fs::read(&out).await;
    let _ = tokio::fs::remove_file(&out).await;
    match bytes {
        Ok(bytes) => HttpResponse::Ok()
            .content_type("image/png")
            .insert_header(("Cache-Control", "no-store"))
            .body(bytes),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

/// `GET /api/labs/{lab}/vms/{vm}/snapshots` — list a VM's snapshots.
pub async fn vm_snapshots(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, vm) = path.into_inner();
    match state
        .lab_call(&lab, "snapshot.list", json!({"vm": vm}))
        .await
    {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub struct SnapshotBody {
    name: String,
    /// Optional single VM; omitted = lab-wide.
    #[serde(default)]
    vm: Option<String>,
}

/// `POST /api/labs/{lab}/snapshots` `{name, vm?}` — take a snapshot.
pub async fn snapshot_take(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<SnapshotBody>,
) -> HttpResponse {
    let mut args = json!({"name": body.name});
    if let Some(vm) = &body.vm {
        args["vm"] = json!(vm);
    }
    match state.lab_call(&lab, "snapshot.take", args).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `DELETE /api/labs/{lab}/vms/{vm}/snapshots/{name}` — delete one VM snapshot.
pub async fn snapshot_delete(
    state: web::Data<AppState>,
    path: web::Path<(String, String, String)>,
) -> HttpResponse {
    let (lab, vm, name) = path.into_inner();
    match state
        .lab_call(&lab, "snapshot.delete", json!({"vm": vm, "name": name}))
        .await
    {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/config` — read the lab's `vmlab.wcl`.
pub async fn get_config(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let path = root.join(vmlab::paths::LAB_FILE);
    match tokio::fs::read_to_string(&path).await {
        Ok(content) => ok(json!({"path": path.to_string_lossy(), "content": content})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HttpResponse::NotFound()
            .json(json!({"error": format!("{}: not found", path.display())})),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

#[derive(Deserialize)]
pub struct ConfigBody {
    content: String,
    /// When true, validate only and don't write the file (the "Validate"
    /// button); the on-disk config is left untouched either way.
    #[serde(default)]
    validate_only: bool,
}

/// `POST /api/labs/{lab}/config` `{content, validate_only?}` — validate then
/// (unless `validate_only`) write `vmlab.wcl`. On validation failure responds
/// 422 with the issues and leaves the on-disk file untouched, so a running
/// daemon never inherits a broken config.
pub async fn save_config(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<ConfigBody>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let body = body.into_inner();
    let content = body.content;

    // WCL parse + the §5.1 host checks are blocking; the server runs a single
    // worker, so keep them off the async runtime thread.
    let validate_root = root.clone();
    let validate_content = content.clone();
    let result = web::block(move || {
        vmlab::cli::validate::validate_source(&validate_content, &validate_root)
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(issues)) => {
            let issues: Vec<Value> = issues
                .into_iter()
                .map(|i| json!({"message": i.message, "line": i.line}))
                .collect();
            return HttpResponse::UnprocessableEntity().json(json!({"issues": issues}));
        }
        Err(e) => {
            return HttpResponse::InternalServerError().json(json!({"error": e.to_string()}));
        }
    }

    if body.validate_only {
        return ok(json!({"ok": true}));
    }

    let path = root.join(vmlab::paths::LAB_FILE);
    match tokio::fs::write(&path, content).await {
        Ok(()) => ok(json!({"ok": true})),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

/// `POST /api/labs/{lab}/reload` — restart the lab daemon so it re-reads
/// `vmlab.wcl`. Requires the lab to be down (the daemon can't re-adopt running
/// VMs across a restart); responds 409 if any VM is still running.
pub async fn reload_lab(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    // Only block on running VMs if the daemon is actually up. If it isn't,
    // there's nothing running to lose and the restart just starts it fresh.
    if let Ok(status) = state.lab_call(&lab, "status", Value::Null).await {
        let running = status["vms"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|v| v["state"].as_str() != Some("stopped"));
        if running {
            return HttpResponse::Conflict()
                .json(json!({"error": "stop all VMs before reloading the lab"}));
        }
    }

    let root = match state.lab_root(&lab).await {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let args = json!({"name": lab.as_str(), "root": root.to_string_lossy()});
    match state.supervisor_call("lab.restart", args).await {
        Ok(_) => {
            // The old socket is gone; force a reconnect to the fresh daemon.
            state.drop_lab_client(&lab).await;
            ok(json!({"ok": true}))
        }
        Err(e) => fail(e),
    }
}

/// Forward a `template.*` command to the supervisor with the lab's `lab` +
/// `root` filled in (the supervisor loads `vmlab.wcl` from the root itself).
/// Template names are NOT `valid_name`-checked: they may contain dots
/// (`ubuntu-24.04`) and are only equality-matched against the parsed config,
/// never used as paths.
async fn template_call(state: &AppState, lab: &str, cmd: &str, mut args: Value) -> HttpResponse {
    let root = match state.lab_root(lab).await {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    args["lab"] = json!(lab);
    args["root"] = json!(root.to_string_lossy());
    match state.supervisor_call(cmd, args).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/templates` — the lab's `template {}` definitions with
/// local store versions and any in-flight operation. `[]` when the lab file
/// defines none (the UI hides the Templates page then).
pub async fn list_templates(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    template_call(&state, &lab, "template.list", json!({})).await
}

/// `GET /api/labs/{lab}/templates/ops` — running build/push operations with
/// their log tails, for reconnecting UIs.
pub async fn template_ops(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    template_call(&state, &lab, "template.op_status", json!({})).await
}

/// `GET /api/labs/{lab}/templates/{tpl}/remote` — published tags/arches on
/// the template's registry.
pub async fn template_remote(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, tpl) = path.into_inner();
    template_call(&state, &lab, "template.remote", json!({"template": tpl})).await
}

/// `POST /api/labs/{lab}/templates/{tpl}/build` — start a background build;
/// progress arrives as `template.op.*` events. 409 while one is running.
pub async fn template_build(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, tpl) = path.into_inner();
    template_call(&state, &lab, "template.build", json!({"template": tpl})).await
}

#[derive(Deserialize)]
pub struct PublishBody {
    /// Local store version to push; omitted = newest.
    #[serde(default)]
    version: Option<String>,
}

/// `POST /api/labs/{lab}/templates/{tpl}/publish` `{version?}` — start a
/// background push of a stored version to the template's registry.
pub async fn template_publish(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    body: web::Json<PublishBody>,
) -> HttpResponse {
    let (lab, tpl) = path.into_inner();
    let mut args = json!({"template": tpl});
    if let Some(v) = &body.version {
        args["version"] = json!(v);
    }
    template_call(&state, &lab, "template.push", args).await
}

#[derive(Deserialize)]
pub struct RestoreBody {
    #[serde(default)]
    vm: Option<String>,
}

/// `POST /api/labs/{lab}/snapshots/{name}/restore` `{vm?}` — restore a snapshot.
pub async fn snapshot_restore(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    body: web::Json<RestoreBody>,
) -> HttpResponse {
    let (lab, name) = path.into_inner();
    let mut args = json!({"name": name});
    if let Some(vm) = &body.vm {
        args["vm"] = json!(vm);
    }
    match state.lab_call(&lab, "snapshot.restore", args).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}
