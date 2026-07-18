//! REST handlers. Each is a thin translation of an HTTP request into a daemon
//! proto call, returning the daemon's JSON (or an error mapped to a 4xx/5xx).

use actix_web::{HttpResponse, web};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

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
    let dir = match body
        .path
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
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

/// `DELETE /api/catalog/templates/{arch}/{name}/{version}` — remove one exact
/// local-store entry. The exact metadata match is resolved before constructing
/// any store path, so route parameters can never be used for path traversal.
pub async fn delete_catalog_template(path: web::Path<(String, String, String)>) -> HttpResponse {
    let (arch, name, version) = path.into_inner();
    let result = web::block(move || {
        let store = vmlab::template::TemplateStore::new(vmlab::paths::template_store_dir());
        let template = store
            .list()?
            .into_iter()
            .find(|t| t.arch == arch && t.name == name && t.version == version)
            .ok_or_else(|| anyhow::anyhow!("template {arch}/{name}@{version} not found"))?;
        store.remove(
            &template.arch,
            &template.name,
            &template.version,
            true,
            &|_| None,
        )?;
        Ok::<_, anyhow::Error>(template)
    })
    .await;

    match result {
        Ok(Ok(template)) => ok(json!({
            "removed": format!("{}/{}@{}", template.arch, template.name, template.version),
        })),
        Ok(Err(e)) if format!("{e:#}").contains("not found") => {
            HttpResponse::NotFound().json(json!({"error": format!("{e:#}")}))
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
        "restart_policies": ["no", "on-failure", "always"],
        // Schema defaults for `healthcheck {}` (seconds / count), so the
        // editor's placeholders can never drift from the parser's defaults.
        "healthcheck_defaults": {
            "interval": 10,
            "timeout": 5,
            "retries": 3,
            "start_period": 10,
        },
    }))
}

/// `GET /api/registries` — host-level OCI search settings shared with the CLI.
pub async fn list_registries() -> HttpResponse {
    match vmlab::template::registries::list() {
        Ok(entries) => {
            let rows: Vec<Value> = entries
                .into_iter()
                .map(|entry| {
                    let host = vmlab::template::registries::host_of(&entry.namespace).unwrap_or("");
                    json!({
                        "namespace": entry.namespace,
                        "vms": entry.use_for.vms(),
                        "containers": entry.use_for.containers(),
                        "authenticated": vmlab::template::oci_bridge::has_credentials(host),
                    })
                })
                .collect();
            let removed = vmlab::template::registries::removed().unwrap_or_default();
            ok(json!({"entries": rows, "removed": removed}))
        }
        Err(error) => {
            HttpResponse::InternalServerError().json(json!({"error": format!("{error:#}")}))
        }
    }
}

#[derive(Deserialize)]
pub struct RegistryBody {
    namespace: String,
    use_for: vmlab::template::registries::RegistryUse,
}

pub async fn add_registry(body: web::Json<RegistryBody>) -> HttpResponse {
    match vmlab::template::registries::add(&body.namespace, body.use_for) {
        Ok(entry) => HttpResponse::Created().json(entry),
        Err(error) => HttpResponse::BadRequest().json(json!({"error": format!("{error:#}")})),
    }
}

#[derive(Deserialize)]
pub struct RegistryRemoveBody {
    namespace: String,
}

pub async fn remove_registry(body: web::Json<RegistryRemoveBody>) -> HttpResponse {
    match vmlab::template::registries::remove(&body.namespace) {
        Ok(()) => HttpResponse::NoContent().finish(),
        Err(error) => HttpResponse::BadRequest().json(json!({"error": format!("{error:#}")})),
    }
}

#[derive(Deserialize)]
pub struct RegistryLoginBody {
    namespace: String,
    username: String,
    password: String,
}

pub async fn registry_login(body: web::Json<RegistryLoginBody>) -> HttpResponse {
    let namespace = match vmlab::template::registries::normalise_namespace(&body.namespace) {
        Ok(namespace) => namespace,
        Err(error) => {
            return HttpResponse::BadRequest().json(json!({"error": format!("{error:#}")}));
        }
    };
    let host = match vmlab::template::registries::host_of(&namespace) {
        Ok(host) => host,
        Err(error) => {
            return HttpResponse::BadRequest().json(json!({"error": format!("{error:#}")}));
        }
    };
    if body.username.is_empty() || body.password.is_empty() {
        return HttpResponse::BadRequest()
            .json(json!({"error": "username and password/token are required"}));
    }
    match vmlab::template::oci_bridge::login(host, &body.username, &body.password).await {
        Ok(()) => ok(json!({"authenticated": true})),
        Err(error) => HttpResponse::BadRequest().json(json!({"error": format!("{error:#}")})),
    }
}

#[derive(Deserialize)]
pub struct OciSearchQuery {
    registry: String,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    arch: Option<String>,
    #[serde(default)]
    kind: Option<String>,
}

/// `GET /api/catalog/oci?registry=host/namespace&q=…&arch=…` — search a
/// configured OCI namespace for VM templates or container images.
pub async fn catalog_oci(q: web::Query<OciSearchQuery>) -> HttpResponse {
    let registry = q.registry.trim().trim_end_matches('/').to_string();
    if registry.is_empty() {
        return HttpResponse::BadRequest().json(json!({"error": "registry is required"}));
    }
    let query =
        q.q.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    let arch = q
        .arch
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let containers = q.kind.as_deref() == Some("container");
    match vmlab::template::cli::search_catalog(query, registry, arch, containers).await {
        Ok(rows) => ok(json!(rows)),
        Err(e) => HttpResponse::BadGateway().json(json!({"error": format!("{e:#}")})),
    }
}

/// `GET /api/labs/{lab}` — full lab status (vms + segments).
pub async fn lab_status(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    match state.lab_call(&lab, "status", Value::Null).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/dns` — live per-segment DNS zone snapshots
/// (auto-registered guest records tagged `dynamic`, statics, sinkholes).
pub async fn lab_dns_table(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    match state.lab_call(&lab, "dns.table", Value::Null).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// Optional `?force=true` on the stop-shaped actions: force-kill instead of
/// the graceful ladder (`down`, `*.stop`, and the stop half of `*.restart`).
#[derive(Deserialize)]
pub struct ForceQuery {
    #[serde(default)]
    force: bool,
}

/// `POST /api/labs/{lab}/{action}` where action ∈ up|down|destroy|pull.
/// `pull` downloads any missing templates/images without starting machines
/// (the overview's "Download templates" button); like `up`, the response
/// blocks until done while `template.pull.*` events drive the UI.
pub async fn lab_action(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    q: web::Query<ForceQuery>,
) -> HttpResponse {
    let (lab, action) = path.into_inner();
    let cmd = match action.as_str() {
        "up" | "down" | "destroy" | "pull" => action.as_str(),
        _ => return HttpResponse::NotFound().json(json!({"error": "unknown lab action"})),
    };
    let args = if cmd == "down" {
        json!({"force": q.force})
    } else {
        json!({})
    };
    match state.lab_call(&lab, cmd, args).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `POST /api/labs/{lab}/vms/{vm}/{action}` where action ∈ start|stop|restart|destroy.
pub async fn vm_action(
    state: web::Data<AppState>,
    path: web::Path<(String, String, String)>,
    q: web::Query<ForceQuery>,
) -> HttpResponse {
    let (lab, vm, action) = path.into_inner();
    let cmd = match action.as_str() {
        "start" => "vm.start",
        "stop" => "vm.stop",
        "restart" => "vm.restart",
        "destroy" => "vm.destroy",
        _ => return HttpResponse::NotFound().json(json!({"error": "unknown vm action"})),
    };
    match state
        .lab_call(&lab, cmd, json!({"vm": vm, "force": q.force}))
        .await
    {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `POST /api/labs/{lab}/containers/{container}/{action}` where action ∈
/// start|stop|restart|destroy — the container mirror of [`vm_action`], proxied
/// to the labd `container.*` commands (arg key `container`, not `vm`).
pub async fn container_action(
    state: web::Data<AppState>,
    path: web::Path<(String, String, String)>,
    q: web::Query<ForceQuery>,
) -> HttpResponse {
    let (lab, container, action) = path.into_inner();
    let cmd = match action.as_str() {
        "start" => "container.start",
        "stop" => "container.stop",
        "restart" => "container.restart",
        "destroy" => "container.destroy",
        _ => return HttpResponse::NotFound().json(json!({"error": "unknown container action"})),
    };
    match state
        .lab_call(&lab, cmd, json!({"container": container, "force": q.force}))
        .await
    {
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

const MAX_SCRIPT_BYTES: usize = 1024 * 1024;

#[derive(Deserialize)]
pub struct ScriptQuery {
    path: String,
}

#[derive(Deserialize)]
pub struct ScriptBody {
    path: String,
    content: String,
    /// SHA-256 returned by `GET`; `None` means create without overwriting.
    base_rev: Option<String>,
}

fn script_rev(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

/// Resolve a script path lexically beneath a lab root. Canonicalisation is
/// intentionally not used: a newly-created file and its parent may not exist.
fn lab_script_path(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let relative = Path::new(requested);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative.extension().and_then(|e| e.to_str()) != Some("ws")
        || relative
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("script path must be a relative .ws file inside the lab".into());
    }
    Ok(root.join(relative))
}

fn ensure_safe_script_parent(root: &Path, parent: &Path) -> Result<PathBuf, String> {
    let canonical_root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let relative = parent
        .strip_prefix(root)
        .map_err(|_| "script path escapes the lab".to_string())?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err("script path escapes the lab".into());
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("script directories cannot be symbolic links".into());
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(format!("{} is not a directory", current.display()));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&current).map_err(|e| e.to_string())?;
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    let canonical_parent = std::fs::canonicalize(current).map_err(|e| e.to_string())?;
    if !canonical_parent.starts_with(canonical_root) {
        return Err("script path escapes the lab".into());
    }
    Ok(canonical_parent)
}

/// `GET /api/labs/{lab}/scripts?path=...` — read a lab-relative WScript.
pub async fn get_script(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    query: web::Query<ScriptQuery>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let requested = query.path.clone();
    let path = match lab_script_path(&root, &requested) {
        Ok(path) => path,
        Err(error) => return HttpResponse::BadRequest().json(json!({"error": error})),
    };
    let canonical_root = match tokio::fs::canonicalize(&root).await {
        Ok(path) => path,
        Err(e) => return HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    };
    let canonical_path = match tokio::fs::canonicalize(&path).await {
        Ok(path) if path.starts_with(&canonical_root) => path,
        Ok(_) => {
            return HttpResponse::BadRequest()
                .json(json!({"error": "script path escapes the lab"}));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return HttpResponse::NotFound()
                .json(json!({"error": format!("{}: not found", path.display())}));
        }
        Err(e) => return HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    };
    match tokio::fs::read_to_string(&canonical_path).await {
        Ok(content) if content.len() <= MAX_SCRIPT_BYTES => ok(json!({
            "path": requested,
            "rev": script_rev(&content),
            "content": content,
        })),
        Ok(_) => HttpResponse::PayloadTooLarge()
            .json(json!({"error": "script exceeds the 1 MiB editor limit"})),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HttpResponse::NotFound()
            .json(json!({"error": format!("{}: not found", path.display())})),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

enum ScriptSave {
    Saved { rev: String },
    Stale { rev: Option<String> },
    Invalid(String),
    Error(String),
}

/// `PUT /api/labs/{lab}/scripts` — revision-aware create/update. New files
/// use `base_rev: null`; existing files are never overwritten that way.
pub async fn save_script(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<ScriptBody>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let body = body.into_inner();
    if body.content.len() > MAX_SCRIPT_BYTES {
        return HttpResponse::PayloadTooLarge()
            .json(json!({"error": "script exceeds the 1 MiB editor limit"}));
    }
    let path = match lab_script_path(&root, &body.path) {
        Ok(path) => path,
        Err(error) => return HttpResponse::BadRequest().json(json!({"error": error})),
    };
    let content = body.content;
    let base_rev = body.base_rev;
    let outcome = web::block(move || {
        let Some(parent) = path.parent() else {
            return ScriptSave::Error("script has no parent directory".into());
        };
        let canonical_parent = match ensure_safe_script_parent(&root, parent) {
            Ok(path) => path,
            Err(e) => return ScriptSave::Invalid(e),
        };
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return ScriptSave::Invalid("script path cannot be a symbolic link".into());
        }
        let safe_path = canonical_parent.join(path.file_name().expect("validated script filename"));
        let existing = match std::fs::read_to_string(&safe_path) {
            Ok(source) => Some(source),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return ScriptSave::Error(e.to_string()),
        };
        let current_rev = existing.as_deref().map(script_rev);
        if current_rev != base_rev {
            return ScriptSave::Stale { rev: current_rev };
        }
        let creating = base_rev.is_none();
        let mut temp = match tempfile::NamedTempFile::new_in(&canonical_parent) {
            Ok(file) => file,
            Err(e) => return ScriptSave::Error(e.to_string()),
        };
        if let Err(e) = temp
            .write_all(content.as_bytes())
            .and_then(|_| temp.flush())
        {
            return ScriptSave::Error(e.to_string());
        }
        let persisted = if creating {
            temp.persist_noclobber(&safe_path)
        } else {
            temp.persist(&safe_path)
        };
        if let Err(e) = persisted {
            if creating && e.error.kind() == std::io::ErrorKind::AlreadyExists {
                return ScriptSave::Stale {
                    rev: std::fs::read_to_string(&safe_path)
                        .ok()
                        .as_deref()
                        .map(script_rev),
                };
            }
            return ScriptSave::Error(e.error.to_string());
        }
        ScriptSave::Saved {
            rev: script_rev(&content),
        }
    })
    .await;
    match outcome {
        Ok(ScriptSave::Saved { rev }) => ok(json!({"ok": true, "rev": rev})),
        Ok(ScriptSave::Stale { rev }) => {
            HttpResponse::Conflict().json(json!({"error": "script changed on disk", "rev": rev}))
        }
        Ok(ScriptSave::Invalid(error)) => HttpResponse::BadRequest().json(json!({"error": error})),
        Ok(ScriptSave::Error(error)) => {
            HttpResponse::InternalServerError().json(json!({"error": error}))
        }
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
        let running = ["vms", "containers"].iter().any(|kind| {
            status[kind]
                .as_array()
                .into_iter()
                .flatten()
                .any(|v| v["state"].as_str() != Some("stopped"))
        });
        if running {
            return HttpResponse::Conflict()
                .json(json!({"error": "stop all VMs and containers before reloading the lab"}));
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
    query: web::Query<TemplateSelector>,
) -> HttpResponse {
    let (lab, tpl) = path.into_inner();
    let mut args = json!({"template": tpl});
    if let Some(arch) = &query.arch {
        args["arch"] = json!(arch);
    }
    template_call(&state, &lab, "template.remote", args).await
}

#[derive(Deserialize)]
pub struct TemplateSelector {
    #[serde(default)]
    arch: Option<String>,
}

/// `POST /api/labs/{lab}/templates/{tpl}/build` — start a background build;
/// progress arrives as `template.op.*` events. 409 while one is running.
pub async fn template_build(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    body: web::Json<TemplateSelector>,
) -> HttpResponse {
    let (lab, tpl) = path.into_inner();
    let mut args = json!({"template": tpl});
    if let Some(arch) = &body.arch {
        args["arch"] = json!(arch);
    }
    template_call(&state, &lab, "template.build", args).await
}

/// `POST /api/labs/{lab}/templates/{tpl}/stop` — cancel the active build for
/// the selected architecture.
pub async fn template_stop(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    body: web::Json<TemplateSelector>,
) -> HttpResponse {
    let (lab, tpl) = path.into_inner();
    let Some(arch) = &body.arch else {
        return fail("missing arch".to_string());
    };
    template_call(
        &state,
        &lab,
        "template.stop_build",
        json!({"template": tpl, "arch": arch}),
    )
    .await
}

#[derive(Deserialize)]
pub struct PublishBody {
    #[serde(default)]
    arch: Option<String>,
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
    if let Some(arch) = &body.arch {
        args["arch"] = json!(arch);
    }
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

/// `GET /api/host` — host capacity (CPU cores + total RAM) for the editor's
/// hardware sliders, plus the DNS suffix guest names register under (feeds
/// the DNS tab's expected-registrations view when no lab daemon is up).
pub async fn host_info() -> HttpResponse {
    let cpus = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let memory = tokio::fs::read_to_string("/proc/meminfo")
        .await
        .ok()
        .and_then(|s| parse_mem_total(&s))
        .unwrap_or(0);
    let kvm = vmlab::kvm_available();
    let dns_suffix = vmlab::config::host::HostConfig::load_default()
        .map(|c| c.dns_suffix)
        .unwrap_or_else(|_| "vmlab.internal".to_string());
    ok(json!({
        "cpus": cpus,
        "memory": memory,
        "acceleration": if kvm { "kvm" } else { "tcg" },
        "arch": std::env::consts::ARCH,
        "dns_suffix": dns_suffix,
    }))
}

/// `GET /api/fastpath` — the network fast-path tier the supervisor selected
/// (PRD §9.1) plus why the skipped kernel tiers were unavailable; drives the
/// Topbar badge.
pub async fn fastpath(state: web::Data<AppState>) -> HttpResponse {
    match state.supervisor_call("fastpath", Value::Null).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// Total RAM in bytes from `/proc/meminfo` (`MemTotal:  16384000 kB`).
fn parse_mem_total(meminfo: &str) -> Option<u64> {
    let rest = meminfo.lines().find_map(|l| l.strip_prefix("MemTotal:"))?;
    let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
    Some(kb * 1024)
}

#[derive(Deserialize)]
pub struct FsQuery {
    path: String,
}

/// `GET /api/host/fs?path=<abs dir>` — list one directory for the editor's
/// server-side file picker (the ISO browser). Hidden entries are skipped;
/// directories sort first. Auth-gated like every other `/api` route.
pub async fn host_fs(q: web::Query<FsQuery>) -> HttpResponse {
    let path = std::path::PathBuf::from(&q.path);
    if !path.is_absolute() {
        return HttpResponse::BadRequest().json(json!({"error": "path must be absolute"}));
    }
    // Normalise `..`/symlinks so the breadcrumb the UI shows is canonical.
    let path = match tokio::fs::canonicalize(&path).await {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return HttpResponse::NotFound()
                .json(json!({"error": format!("{}: not found", path.display())}));
        }
        Err(e) => return HttpResponse::Forbidden().json(json!({"error": e.to_string()})),
    };
    let mut dir = match tokio::fs::read_dir(&path).await {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => {
            return HttpResponse::BadRequest().json(json!({"error": "not a directory"}));
        }
        Err(e) => return HttpResponse::Forbidden().json(json!({"error": e.to_string()})),
    };
    let mut entries: Vec<(bool, String, Option<u64>)> = Vec::new();
    while let Ok(Some(entry)) = dir.next_entry().await {
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if name.starts_with('.') {
            continue;
        }
        // Follow symlinks so a linked ISO directory still browses.
        let Ok(meta) = tokio::fs::metadata(entry.path()).await else {
            continue;
        };
        if meta.is_dir() {
            entries.push((true, name, None));
        } else if meta.is_file() {
            entries.push((false, name, Some(meta.len())));
        }
    }
    entries.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| a.1.to_lowercase().cmp(&b.1.to_lowercase()))
    });
    let rows: Vec<Value> = entries
        .into_iter()
        .map(|(dir, name, size)| json!({"name": name, "dir": dir, "size": size}))
        .collect();
    ok(json!({
        "path": path.to_string_lossy(),
        "parent": path.parent().map(|p| p.to_string_lossy().into_owned()),
        "entries": rows,
    }))
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

/// `GET /api/labs/{lab}/vms/{vm}/stats` — latest guest metrics from the
/// vmlab-agent (CPU/memory/disks; 404-ish conflict for agent-less guests).
pub async fn vm_stats(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, vm) = path.into_inner();
    match state.lab_call(&lab, "vm.stats", json!({"vm": vm})).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/containers/{container}/stats` — micro-VM metrics.
pub async fn container_stats(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, container) = path.into_inner();
    match state
        .lab_call(&lab, "container.stats", json!({"container": container}))
        .await
    {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

#[derive(serde::Deserialize)]
pub struct ClipboardBody {
    pub text: String,
}

/// `GET /api/labs/{lab}/vms/{vm}/clipboard` — read the guest clipboard
/// (agent `clipboard` feature; needs a logged-on desktop session).
pub async fn vm_clipboard_get(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, vm) = path.into_inner();
    match state
        .lab_call(&lab, "vm.clipboard_get", json!({"vm": vm}))
        .await
    {
        Ok(v) => ok(json!({"text": v})),
        Err(e) => fail(e),
    }
}

/// `POST /api/labs/{lab}/vms/{vm}/clipboard` `{text}` — set the guest
/// clipboard.
pub async fn vm_clipboard_set(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    body: web::Json<ClipboardBody>,
) -> HttpResponse {
    let (lab, vm) = path.into_inner();
    match state
        .lab_call(
            &lab,
            "vm.clipboard_set",
            json!({"vm": vm, "text": body.text}),
        )
        .await
    {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, test};

    fn script_test_state(root: &Path) -> web::Data<AppState> {
        web::Data::new(AppState::new(
            super::super::state::AuthConfig {
                enabled: false,
                user: String::new(),
                password_hash: String::new(),
            },
            Some(("lab".into(), root.to_path_buf())),
            false,
        ))
    }

    #[actix_web::test]
    async fn mem_total_parses_meminfo() {
        let s = "MemTotal:       65670920 kB\nMemFree:        1234 kB\n";
        assert_eq!(parse_mem_total(s), Some(65670920 * 1024));
        assert_eq!(parse_mem_total("MemFree: 1 kB\n"), None);
        assert_eq!(parse_mem_total("MemTotal: garbage kB\n"), None);
    }

    #[actix_web::test]
    async fn host_fs_rejects_relative_paths() {
        let app =
            test::init_service(App::new().route("/api/host/fs", web::get().to(host_fs))).await;
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/host/fs?path=relative/dir")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn host_fs_lists_dirs_first_and_skips_hidden() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("a.iso"), b"x").unwrap();
        std::fs::write(tmp.path().join(".hidden"), b"x").unwrap();

        let app =
            test::init_service(App::new().route("/api/host/fs", web::get().to(host_fs))).await;
        let uri = format!("/api/host/fs?path={}", tmp.path().display());
        let resp = test::call_service(&app, test::TestRequest::get().uri(&uri).to_request()).await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["name"], "sub");
        assert_eq!(entries[0]["dir"], true);
        assert_eq!(entries[1]["name"], "a.iso");
        assert_eq!(entries[1]["size"], 1);
        assert!(body["parent"].as_str().is_some());
    }

    #[actix_web::test]
    async fn host_info_reports_capacity() {
        let app = test::init_service(App::new().route("/api/host", web::get().to(host_info))).await;
        let resp =
            test::call_service(&app, test::TestRequest::get().uri("/api/host").to_request()).await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert!(body["cpus"].as_u64().unwrap() >= 1);
        assert!(body["memory"].as_u64().unwrap() > 0);
        assert!(matches!(body["acceleration"].as_str(), Some("kvm" | "tcg")));
        assert_eq!(body["arch"], std::env::consts::ARCH);
    }

    #[actix_web::test]
    async fn provision_script_create_read_and_stale_update() {
        let tmp = tempfile::tempdir().unwrap();
        let state = script_test_state(tmp.path());
        let app = test::init_service(
            App::new()
                .app_data(state)
                .route("/api/labs/{lab}/scripts", web::get().to(get_script))
                .route("/api/labs/{lab}/scripts", web::put().to(save_script)),
        )
        .await;

        let create = test::TestRequest::put()
            .uri("/api/labs/lab/scripts")
            .set_json(json!({
                "path": "scripts/provision-1.ws",
                "content": "use vmlab\n",
                "base_rev": null,
            }))
            .to_request();
        let response = test::call_service(&app, create).await;
        assert_eq!(response.status(), 200);
        let created: Value = test::read_body_json(response).await;
        let rev = created["rev"].as_str().unwrap().to_string();

        let read = test::TestRequest::get()
            .uri("/api/labs/lab/scripts?path=scripts%2Fprovision-1.ws")
            .to_request();
        let response = test::call_service(&app, read).await;
        assert_eq!(response.status(), 200);
        let document: Value = test::read_body_json(response).await;
        assert_eq!(document["content"], "use vmlab\n");
        assert_eq!(document["rev"], rev);

        let stale = test::TestRequest::put()
            .uri("/api/labs/lab/scripts")
            .set_json(json!({
                "path": "scripts/provision-1.ws",
                "content": "changed",
                "base_rev": "not-the-current-revision",
            }))
            .to_request();
        let response = test::call_service(&app, stale).await;
        assert_eq!(response.status(), 409);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("scripts/provision-1.ws")).unwrap(),
            "use vmlab\n"
        );
    }

    #[actix_web::test]
    async fn provision_script_rejects_unsafe_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let state = script_test_state(tmp.path());
        let app = test::init_service(
            App::new()
                .app_data(state)
                .route("/api/labs/{lab}/scripts", web::put().to(save_script)),
        )
        .await;
        for path in ["../outside.ws", "/tmp/outside.ws", "scripts/not-wscript.sh"] {
            let request = test::TestRequest::put()
                .uri("/api/labs/lab/scripts")
                .set_json(json!({"path": path, "content": "", "base_rev": null}))
                .to_request();
            let response = test::call_service(&app, request).await;
            assert_eq!(response.status(), 400, "path {path}");
        }
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(outside.path(), tmp.path().join("linked")).unwrap();
            let request = test::TestRequest::put()
                .uri("/api/labs/lab/scripts")
                .set_json(json!({
                    "path": "linked/escape.ws",
                    "content": "",
                    "base_rev": null,
                }))
                .to_request();
            let response = test::call_service(&app, request).await;
            assert_eq!(response.status(), 400);
            assert!(!outside.path().join("escape.ws").exists());
        }
    }
}
