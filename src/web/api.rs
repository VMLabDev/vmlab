//! REST handlers. Each is a thin translation of an HTTP request into a daemon
//! proto call, returning the daemon's JSON (or an error mapped to a 4xx/5xx).

use actix_web::http::StatusCode;
use actix_web::{HttpResponse, web};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

use vmlab::config::projection::SchemaProjection;
use vmlab::proto::{CommandError, ErrorCode, LabRequest, SupRequest};

use super::fsops::{ensure_safe_parent, plain_relative};
use super::state::AppState;
use vmlab::status::LabStatus;

/// Map a daemon failure to an HTTP response.
///
/// The daemon says why it failed (ADR-0007) and this reads the code, so the
/// wording of an error is free to change without moving a status code.
pub(crate) fn fail(e: CommandError) -> HttpResponse {
    let status = status_for(e.code);
    HttpResponse::build(status).json(json!({"error": e.message, "code": e.code.as_str()}))
}

/// One code, one status — the published mapping, in actix's terms.
pub(crate) fn status_for(code: ErrorCode) -> StatusCode {
    StatusCode::from_u16(code.http_status()).unwrap_or(StatusCode::BAD_GATEWAY)
}

fn ok(v: Value) -> HttpResponse {
    HttpResponse::Ok().json(v)
}

/// `GET /api/labs` — running labs (registry) merged with the cwd lab, labs
/// created this session (cached roots), and the managed labs home on disk.
pub async fn list_labs(state: web::Data<AppState>) -> HttpResponse {
    let mut labs = state
        .supervisor_call(SupRequest::Status {})
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
    /// What the lab starts out as: `empty` (default) or `starter`.
    #[serde(default)]
    preset: vmlab::lab_init::LabPreset,
}

/// `POST /api/labs` `{name, path?, preset?}` — scaffold a new lab: create the
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

    let (create_name, create_dir, preset) = (name.clone(), dir.clone(), body.preset);
    match web::block(move || vmlab::lab_init::create_lab_dir(&create_name, &create_dir, preset))
        .await
    {
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

/// `POST /api/catalog/templates/{arch}/{name}/{version}/verify` — hash the
/// stored `disk.qcow2` and compare it with the digest its registry publishes
/// (the Templates page's Check button). Templates with no registry are
/// checked against the digest recorded when they were built or installed.
///
/// Hashing reads the whole image, so a big Windows template takes minutes;
/// the response is the result, there is no progress stream.
pub async fn verify_catalog_template(path: web::Path<(String, String, String)>) -> HttpResponse {
    let (arch, name, version) = path.into_inner();
    // Resolve through the store's own listing, so route parameters never
    // reach a path (same reasoning as delete_catalog_template).
    let resolved = web::block({
        let (arch, name, version) = (arch.clone(), name.clone(), version.clone());
        move || {
            let store = vmlab::template::TemplateStore::new(vmlab::paths::template_store_dir());
            let found = store
                .list()?
                .into_iter()
                .find(|t| t.arch == arch && t.name == name && t.version == version)
                .ok_or_else(|| anyhow::anyhow!("template {arch}/{name}@{version} not found"))?;
            store.resolve(&found.arch, &found.name, Some(&found.version))
        }
    })
    .await;
    let resolved = match resolved {
        Ok(Ok(r)) => r,
        Ok(Err(e)) if format!("{e:#}").contains("not found") => {
            return HttpResponse::NotFound().json(json!({"error": format!("{e:#}")}));
        }
        Ok(Err(e)) => {
            return HttpResponse::InternalServerError().json(json!({"error": format!("{e:#}")}));
        }
        Err(e) => return HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    };

    // Ask the registry what it publishes for this exact version. A registry
    // that can't be reached is reported, not fatal: the local hash is still
    // worth comparing against the recorded one.
    let mut remote: Option<String> = None;
    let mut remote_error: Option<String> = None;
    if let Some(repo) = resolved.meta.registry.clone() {
        match vmlab::oci_registry::with_version_tag(&repo, &resolved.meta.version)
            .and_then(|reference| vmlab::oci_registry::Registry::new(&reference))
        {
            Ok(registry) => match registry.published_disk_digest(Some(&arch)).await {
                Ok(Some(digest)) => remote = Some(strip_sha256(&digest)),
                Ok(None) => {
                    remote_error = Some("the published manifest records no image digest".into());
                }
                Err(e) => remote_error = Some(format!("{e:#}")),
            },
            Err(e) => remote_error = Some(format!("{e:#}")),
        }
    }

    let disk = resolved.disk_path.clone();
    let local = match web::block(move || vmlab::template::store::sha256_file(&disk)).await {
        Ok(Ok(hex)) => hex,
        Ok(Err(e)) => {
            return HttpResponse::InternalServerError().json(json!({"error": format!("{e:#}")}));
        }
        Err(e) => return HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    };
    let recorded = resolved.meta.sha256.as_deref().map(strip_sha256);
    // Registry digest first: it is the published truth. Without one, the
    // recorded digest still catches local corruption.
    let (expected, source) = match (&remote, &recorded) {
        (Some(digest), _) => (Some(digest.clone()), "registry"),
        (None, Some(digest)) => (Some(digest.clone()), "recorded"),
        (None, None) => (None, "none"),
    };
    ok(json!({
        "template": format!("{arch}/{name}@{version}"),
        "local": local,
        "expected": expected,
        "source": source,
        "registry": resolved.meta.registry,
        "recorded": recorded,
        "remote_error": remote_error,
        "matches": expected.as_deref().map(|e| e.eq_ignore_ascii_case(&local)),
    }))
}

/// Digests travel as `sha256:<hex>` on the wire and bare hex in template
/// metadata; compare them in one form.
fn strip_sha256(digest: &str) -> String {
    digest
        .strip_prefix("sha256:")
        .unwrap_or(digest)
        .to_ascii_lowercase()
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

/// Which kind of machine [`catalog_inherited`] is being asked about.
#[derive(serde::Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum MachineKindQuery {
    #[default]
    Vm,
    Container,
}

/// Query for [`catalog_inherited`]: the layers below a machine's own block.
#[derive(serde::Deserialize)]
pub struct InheritedQuery {
    #[serde(default)]
    kind: MachineKindQuery,
    /// `<arch>/<name>[@version]` store reference, for a VM's template layer.
    template: Option<String>,
    profile: Option<String>,
    /// Emulator arch; defaults to x86_64 (also inferred from `template`).
    arch: Option<String>,
}

/// `GET /api/catalog/inherited` — the hardware a machine would boot with if
/// its own block declared nothing, straight from the resolver.
///
/// The designer shows this behind every unset hardware field. It asks the
/// resolver rather than approximating it, so the inherited display is the
/// value the machine actually boots with; a field with no layer to inherit
/// from comes back null (ADR-0008).
pub async fn catalog_inherited(q: web::Query<InheritedQuery>) -> HttpResponse {
    let q = q.into_inner();
    let result = web::block(move || -> anyhow::Result<Value> {
        let profiles = vmlab::profiles::ProfileSet::load_default()?;
        let template_ref = q
            .template
            .as_deref()
            .and_then(|r| vmlab::config::model::parse_template_ref(r).ok());
        let meta = match &template_ref {
            Some(vmlab::config::model::TemplateRef::Store {
                arch,
                name,
                version,
            }) => vmlab::template::TemplateStore::new(vmlab::paths::template_store_dir())
                .resolve(arch, name, version.as_deref())
                .ok()
                .map(|r| r.meta),
            _ => None,
        };
        // A store reference carries the arch; an explicit one wins. A draft
        // that has named neither is mid-edit, so assume the common case
        // rather than refuse to answer.
        let arch = q
            .arch
            .clone()
            .or_else(|| meta.as_ref().map(|m| m.arch.clone()))
            .unwrap_or_else(|| "x86_64".to_string());
        let profile = q.profile.as_deref();

        if q.kind == MachineKindQuery::Container {
            // No layer sizing the micro-VM means nothing to inherit, which
            // is a fact about the config, not an error to report here.
            let r = vmlab::hardware::inherited_container(&arch, profile, &profiles).ok();
            return Ok(json!({
                "cpus": r.as_ref().map(|r| r.cpus),
                "memory": r.as_ref().map(|r| r.memory),
                "profile": profile,
            }));
        }
        // The template's profile backs the VM's own — but that fallback is
        // the resolver's to apply, and `ResolvedVm::profile` reports which
        // one it landed on. Restating it here is the mirror ADR-0008 removes.
        //
        // A refusal here is config-shaped, not a server fault: an unknown
        // profile, or one asking for secure boot on SeaBIOS, which resolution
        // will not answer for (§5.2). Validation reports those against the
        // source span, so the honest answer for the form behind an unset
        // field is that there is nothing to inherit.
        let Ok(r) = vmlab::hardware::inherited_vm(&arch, profile, meta.as_ref(), &profiles) else {
            return Ok(json!({ "cpus": null, "memory": null, "profile": profile }));
        };
        Ok(json!({
            "cpus": r.cpus,
            "memory": r.memory,
            "machine": r.machine,
            "firmware": r.firmware.map(|f| match f {
                vmlab::profiles::FirmwareKind::Ovmf => "ovmf",
                vmlab::profiles::FirmwareKind::Seabios => "seabios",
            }),
            "secure_boot": r.secure_boot,
            "tpm": r.tpm,
            "display": r.display_device,
            "profile": r.profile,
        }))
    })
    .await;
    match result {
        Ok(Ok(v)) => ok(v),
        Ok(Err(e)) => HttpResponse::InternalServerError().json(json!({"error": format!("{e:#}")})),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

/// `GET /api/catalog/meta` — the option lists the editor renders as pickers.
///
/// Every closed set comes from the Schema projection (ADR-0005), so a picker
/// offers exactly what `schema.wcl` declares and the validator accepts.
/// `arches` and `events` are the two that are not schema facts: they are host
/// and runtime vocabularies, and stay sourced from their Rust constants.
pub async fn catalog_meta() -> HttpResponse {
    let schema = SchemaProjection::get();
    let healthcheck_default = |field: &str| schema.default_number("healthcheck", field);
    ok(json!({
        "arches": vmlab::config::model::KNOWN_ARCHES,
        "events": vmlab::config::model::EVENT_NAMES,
        "firmware": schema.options("vm", "firmware"),
        "gpu_modes": schema.options("gpu", "mode"),
        "sinkhole_modes": schema.options("sinkhole", "mode"),
        "forward_protos": schema.options("forward", "proto"),
        "l4_protos": schema.options("block", "proto"),
        "media_kinds": schema.options("media", "kind"),
        // The `healthcheck {}` defaults, in the seconds/counts the editor
        // edits in — reflected from the schema's `@default`s.
        "healthcheck_defaults": {
            "interval": healthcheck_default("interval"),
            "timeout": healthcheck_default("timeout"),
            "retries": healthcheck_default("retries"),
            "start_period": healthcheck_default("start_period"),
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
    match vmlab::template::catalog::search_catalog(query, registry, arch, containers).await {
        Ok(found) => ok(json!(found.rows)),
        Err(e) => HttpResponse::BadGateway().json(json!({"error": format!("{e:#}")})),
    }
}

/// `GET /api/labs/{lab}` — the lab status projection (ADR-0004): machines,
/// segments and in-flight downloads, verbatim from the daemon.
pub async fn lab_status(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    match state.lab_call(&lab, LabRequest::Status {}).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/dns` — live per-segment DNS zone snapshots
/// (auto-registered guest records tagged `dynamic`, statics, sinkholes).
pub async fn lab_dns_table(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    match state.lab_call(&lab, LabRequest::DnsTable {}).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// The request behind `POST /api/labs/{lab}/{action}`, or `None` for a
/// segment this endpoint does not serve.
fn lab_request(action: &str, force: bool) -> Option<LabRequest> {
    Some(match action {
        "up" => LabRequest::Up {
            machines: Vec::new(),
        },
        "pull" => LabRequest::Pull {
            machines: Vec::new(),
        },
        "down" => LabRequest::Down {
            machines: Vec::new(),
            force,
        },
        "destroy" => LabRequest::Destroy {},
        _ => return None,
    })
}

/// As [`lab_request`], for
/// `POST /api/labs/{lab}/machines/{machine}/{action}`.
fn machine_request(action: &str, machine: String, force: bool) -> Option<LabRequest> {
    Some(match action {
        "start" => LabRequest::MachineStart { machine },
        "stop" => LabRequest::MachineStop { machine, force },
        "restart" => LabRequest::MachineRestart { machine, force },
        "destroy" => LabRequest::MachineDestroy { machine },
        _ => return None,
    })
}

/// Optional `?force=true` on the stop-shaped actions: force-kill instead of
/// the graceful ladder (`down`, `*.stop`, and the stop half of `*.restart`).
#[derive(Deserialize)]
pub struct ForceQuery {
    #[serde(default)]
    force: bool,
}

/// `POST /api/labs/{lab}/{action}` where action ∈ up|down|destroy|pull.
/// `pull` downloads any missing templates/images without starting machines;
/// like `up`, the response blocks until done while `template.pull.*` events
/// drive the UI.
pub async fn lab_action(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    q: web::Query<ForceQuery>,
) -> HttpResponse {
    let (lab, action) = path.into_inner();
    let Some(req) = lab_request(&action, q.force) else {
        return HttpResponse::NotFound().json(json!({"error": "unknown lab action"}));
    };
    match state.lab_call(&lab, req).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `POST /api/labs/{lab}/pulls/{machine}/cancel` — abort the download running
/// for one machine (the Templates page's Cancel button). `{"cancelled": false}`
/// when it wasn't downloading; whatever was waiting on the download fails.
pub async fn cancel_pull(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, machine) = path.into_inner();
    match state
        .lab_call(&lab, LabRequest::PullCancel { machine })
        .await
    {
        Ok(v) => ok(json!({"cancelled": v.as_bool().unwrap_or(false)})),
        Err(e) => fail(e),
    }
}

/// `POST /api/labs/{lab}/machines/{machine}/{action}` where action ∈
/// start|stop|restart|destroy. One endpoint for both kinds — the console
/// still shows VMs and containers separately, but the plumbing behind each
/// button is the same.
pub async fn machine_action(
    state: web::Data<AppState>,
    path: web::Path<(String, String, String)>,
    q: web::Query<ForceQuery>,
) -> HttpResponse {
    let (lab, machine, action) = path.into_inner();
    let Some(req) = machine_request(&action, machine, q.force) else {
        return HttpResponse::NotFound().json(json!({"error": "unknown machine action"}));
    };
    match state.lab_call(&lab, req).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/machines/{machine}/capabilities` — what this machine
/// can do (display, console log, in-place reboot, agent features). The
/// console drives its affordances from this rather than from the kind.
pub async fn machine_capabilities(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, machine) = path.into_inner();
    match state
        .lab_call(&lab, LabRequest::MachineCapabilities { machine })
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

/// `POST /api/labs/{lab}/machines/{machine}/sendkeys` `{keys}`.
pub async fn machine_sendkeys(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    body: web::Json<SendKeys>,
) -> HttpResponse {
    let (lab, vm) = path.into_inner();
    match state
        .lab_call(
            &lab,
            LabRequest::MachineSendKeys {
                machine: vm,
                keys: body.keys.clone(),
            },
        )
        .await
    {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/machines/{machine}/screenshot.png` — capture and stream a
/// PNG. Machines with no display answer with the daemon's "no display" error.
/// A non-VNC fallback (the live view uses the WebSocket bridge).
pub async fn machine_screenshot(
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
        .lab_call(
            &lab,
            LabRequest::MachineScreenshot {
                machine: vm.clone(),
                path: out_str,
            },
        )
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

/// `GET /api/labs/{lab}/machines/{machine}/snapshots` — list a machine's snapshots.
pub async fn machine_snapshots(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, vm) = path.into_inner();
    match state
        .lab_call(&lab, LabRequest::SnapshotList { machine: vm })
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
    let req = LabRequest::SnapshotTake {
        name: body.name.clone(),
        machine: body.vm.clone(),
    };
    match state.lab_call(&lab, req).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `DELETE /api/labs/{lab}/machines/{machine}/snapshots/{name}` — delete one.
pub async fn snapshot_delete(
    state: web::Data<AppState>,
    path: web::Path<(String, String, String)>,
) -> HttpResponse {
    let (lab, vm, name) = path.into_inner();
    match state
        .lab_call(&lab, LabRequest::SnapshotDelete { machine: vm, name })
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
///
/// The relative-shape check is [`plain_relative`] — one implementation of the
/// path sandbox, shared with the Files tab — plus the `.ws` extension rule.
fn lab_script_path(root: &Path, requested: &str) -> Result<PathBuf, String> {
    let relative = plain_relative(requested, "script path")
        .map_err(|_| "script path must be a relative .ws file inside the lab".to_string())?;
    if relative.extension().and_then(|e| e.to_str()) != Some("ws") {
        return Err("script path must be a relative .ws file inside the lab".into());
    }
    Ok(root.join(relative))
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
        // Same symlink-refusing walk the Files tab uses — one implementation.
        let canonical_parent = match ensure_safe_parent(&root, parent) {
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

/// Whether a lab `status` payload *proves* nothing is running — the only
/// evidence that lets a reload restart the daemon safely.
///
/// The payload is parsed into the projection the daemon produces
/// ([`LabStatus`], ADR-0004) rather than read key by key, so a producer-side
/// rename fails this at compile time instead of quietly waving running machines
/// through, which is how this guard came to be disarmed once already.
///
/// A payload that will not parse proves nothing, so it answers false. The
/// affordable failure is a spurious 409 the user clears by stopping the lab.
fn all_machines_stopped(status: &Value) -> bool {
    serde_json::from_value::<LabStatus>(status.clone()).is_ok_and(|status| status.all_stopped())
}

/// `POST /api/labs/{lab}/reload` — restart the lab daemon so it re-reads
/// `vmlab.wcl`. Requires the lab to be down (the daemon can't re-adopt running
/// VMs across a restart); responds 409 if any machine is still running.
pub async fn reload_lab(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    // A `status` that never answered is not a veto: `lab_call` also fails when
    // the daemon can't be started at all — a lab whose `vmlab.wcl` no longer
    // parses — and that is exactly the state reload exists to recover from.
    // Blocking there would make the button useless when it's needed most.
    if let Ok(status) = state.lab_call(&lab, LabRequest::Status {}).await
        && !all_machines_stopped(&status)
    {
        return HttpResponse::Conflict()
            .json(json!({"error": "stop all VMs and containers before reloading the lab"}));
    }

    let root = match state.lab_root(&lab).await {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    let req = SupRequest::LabRestart {
        name: lab.to_string(),
        root,
    };
    match state.supervisor_call(req).await {
        Ok(_) => {
            // The old socket is gone; force a reconnect to the fresh daemon.
            state.drop_lab_client(&lab).await;
            ok(json!({"ok": true}))
        }
        Err(e) => fail(e),
    }
}

/// Forward a `template.*` request to the supervisor, handing `build` the
/// lab's name and root (the supervisor loads `vmlab.wcl` from the root
/// itself). Template names are NOT `valid_name`-checked: they may contain
/// dots (`ubuntu-24.04`) and are only equality-matched against the parsed
/// config, never used as paths.
async fn template_call(
    state: &AppState,
    lab: &str,
    build: impl FnOnce(String, PathBuf) -> SupRequest,
) -> HttpResponse {
    let root = match state.lab_root(lab).await {
        Ok(r) => r,
        Err(e) => return fail(e),
    };
    match state.supervisor_call(build(lab.to_string(), root)).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/templates` — the lab's `template {}` definitions with
/// local store versions and any in-flight operation. `[]` when the lab file
/// defines none (the UI hides the Templates page then).
pub async fn list_templates(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    template_call(&state, &lab, |lab, root| SupRequest::TemplateList {
        lab,
        root,
        // The console knows one file per lab: the lab's own.
        file: None,
    })
    .await
}

/// `GET /api/labs/{lab}/templates/ops` — running build/push operations with
/// their log tails, for reconnecting UIs.
pub async fn template_ops(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    template_call(&state, &lab, |lab, _root| SupRequest::TemplateOpStatus {
        lab,
    })
    .await
}

/// `GET /api/labs/{lab}/templates/{tpl}/remote` — published tags/arches on
/// the template's registry.
pub async fn template_remote(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    query: web::Query<TemplateSelector>,
) -> HttpResponse {
    let (lab, tpl) = path.into_inner();
    let arch = query.arch.clone();
    template_call(&state, &lab, |lab, root| SupRequest::TemplateRemote {
        lab,
        root,
        template: tpl,
        arch,
    })
    .await
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
    let arch = body.arch.clone();
    template_call(&state, &lab, |lab, root| SupRequest::TemplateBuild {
        lab,
        root,
        template: tpl,
        arch,
        // The console builds the lab's declared templates, version and all.
        version: None,
        file: None,
    })
    .await
}

/// `POST /api/labs/{lab}/templates/{tpl}/stop` — cancel the active build for
/// the selected architecture.
pub async fn template_stop(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    body: web::Json<TemplateSelector>,
) -> HttpResponse {
    let (lab, tpl) = path.into_inner();
    let Some(arch) = body.arch.clone() else {
        return fail(CommandError::invalid("missing arch"));
    };
    template_call(&state, &lab, |lab, _root| SupRequest::TemplateStopBuild {
        lab,
        arch,
        template: tpl,
    })
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
    let (arch, version) = (body.arch.clone(), body.version.clone());
    template_call(&state, &lab, |lab, root| SupRequest::TemplatePush {
        lab,
        root,
        template: tpl,
        arch,
        version,
    })
    .await
}

#[derive(Deserialize)]
pub struct RestoreBody {
    #[serde(default)]
    vm: Option<String>,
    /// §19.6's explicit discard flag: restore a machine whose workspace is
    /// halted, throwing the guest copy of every conflicting path away. The
    /// console does not offer it — resolution is host-side and per path, and
    /// a checkbox is the wrong shape for a decision about a developer's own
    /// working copy — but the API carries it so the surface is one vocabulary.
    #[serde(default)]
    discard: bool,
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
    match state.supervisor_call(SupRequest::FastPath {}).await {
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
    let req = LabRequest::SnapshotRestore {
        name,
        machine: body.vm.clone(),
        discard: body.discard,
    };
    match state.lab_call(&lab, req).await {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/machines/{machine}/stats` — latest guest metrics from the
/// vmlab-agent (CPU/memory/disks; 404-ish conflict for agent-less guests).
pub async fn machine_stats(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, vm) = path.into_inner();
    match state
        .lab_call(&lab, LabRequest::MachineStats { machine: vm })
        .await
    {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// `GET /api/labs/{lab}/machines/{machine}/logs?lines=` — the machine's
/// console log. 404-shaped error on machines that keep none (VMs); the
/// console asks `capabilities` first.
pub async fn machine_logs(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    q: web::Query<LogsQuery>,
) -> HttpResponse {
    let (lab, machine) = path.into_inner();
    match state
        .lab_call(
            &lab,
            LabRequest::MachineLogs {
                machine,
                lines: q.lines,
                follow: false,
            },
        )
        .await
    {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

#[derive(Deserialize)]
pub struct LogsQuery {
    #[serde(default = "default_log_lines")]
    lines: usize,
}

fn default_log_lines() -> usize {
    200
}

#[derive(serde::Deserialize)]
pub struct ClipboardBody {
    pub text: String,
}

/// `GET /api/labs/{lab}/machines/{machine}/clipboard` — read the guest clipboard
/// (agent `clipboard` feature; needs a logged-on desktop session).
pub async fn machine_clipboard_get(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
) -> HttpResponse {
    let (lab, vm) = path.into_inner();
    match state
        .lab_call(&lab, LabRequest::MachineClipboardGet { machine: vm })
        .await
    {
        Ok(v) => ok(json!({"text": v})),
        Err(e) => fail(e),
    }
}

/// `POST /api/labs/{lab}/machines/{machine}/clipboard` `{text}` — set the guest
/// clipboard.
pub async fn machine_clipboard_set(
    state: web::Data<AppState>,
    path: web::Path<(String, String)>,
    body: web::Json<ClipboardBody>,
) -> HttpResponse {
    let (lab, vm) = path.into_inner();
    match state
        .lab_call(
            &lab,
            LabRequest::MachineClipboardSet {
                machine: vm,
                text: body.text.clone(),
            },
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
    use vmlab::status::{
        ContainerStatus, MachineDetail, MachineLabel, MachineStatus, PowerState, VmStatus,
    };

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

    /// The HTTP contract of the daemon surface, in full. This used to be
    /// substring matching on the daemon's prose, so rewording an error moved a
    /// status code; now the code decides and the wording is free.
    #[actix_web::test]
    async fn every_error_code_has_its_status() {
        let cases = [
            (ErrorCode::UnknownCommand, 400),
            (ErrorCode::InvalidArgument, 400),
            (ErrorCode::NotFound, 404),
            (ErrorCode::Conflict, 409),
            (ErrorCode::Unsupported, 501),
            (ErrorCode::Failed, 502),
            (ErrorCode::Internal, 500),
        ];
        assert_eq!(
            cases.len(),
            ErrorCode::ALL.len(),
            "a new code needs a status here"
        );
        for (code, want) in cases {
            assert_eq!(status_for(code).as_u16(), want, "{code}");
        }
    }

    /// The response body carries both halves: prose for a person, and the code
    /// the console branches on.
    #[actix_web::test]
    async fn a_failure_body_carries_the_code_beside_the_message() {
        let resp = fail(CommandError::conflict("dc01 is already running"));
        assert_eq!(resp.status().as_u16(), 409);
        let bytes = actix_web::body::to_bytes(resp.into_body()).await.unwrap();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["error"], "dc01 is already running");
        assert_eq!(body["code"], "conflict");
    }

    /// The console's action unions and the protocol report are generated from
    /// one table; this is what holds that table to the endpoints it claims to
    /// describe. A segment the endpoint stops serving fails here rather than
    /// 404-ing in the browser.
    #[actix_web::test]
    async fn action_segments_build_the_requests_the_report_documents() {
        use vmlab::proto::WireRequest;
        use vmlab::proto::report::{LAB_ACTIONS, MACHINE_ACTIONS};

        for (segment, cmd) in LAB_ACTIONS {
            let req = lab_request(segment, false).unwrap_or_else(|| panic!("`{segment}`"));
            assert_eq!(req.cmd(), *cmd);
        }
        for (segment, cmd) in MACHINE_ACTIONS {
            let req = machine_request(segment, "dc01".into(), false)
                .unwrap_or_else(|| panic!("`{segment}`"));
            assert_eq!(req.cmd(), *cmd);
        }
        assert!(lab_request("teleport", false).is_none());
        assert!(machine_request("teleport", "dc01".into(), false).is_none());
    }

    /// `?force=true` is not decoration: it is the difference between the
    /// graceful ladder and a kill, and it has to reach the request.
    #[actix_web::test]
    async fn force_reaches_the_stop_shaped_requests() {
        assert_eq!(
            machine_request("stop", "dc01".into(), true),
            Some(LabRequest::MachineStop {
                machine: "dc01".into(),
                force: true
            })
        );
        assert_eq!(
            machine_request("restart", "dc01".into(), true),
            Some(LabRequest::MachineRestart {
                machine: "dc01".into(),
                force: true
            })
        );
        assert_eq!(
            lab_request("down", true),
            Some(LabRequest::Down {
                machines: Vec::new(),
                force: true
            })
        );
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

    /// One lab status payload in exactly the form the daemon emits it: the
    /// projection, serialised.
    ///
    /// Built here rather than from `vmlab::status::fixtures`, which this binary
    /// cannot reach — it links the ordinary library, not the library's own test
    /// build.
    fn status_payload(machines: Vec<(&str, PowerState)>) -> Value {
        let machines = machines
            .into_iter()
            .enumerate()
            .map(|(i, (name, state))| {
                // Alternate the kinds so a guard that only understood one of
                // them would have to skip a machine to pass.
                let detail = if i.is_multiple_of(2) {
                    MachineDetail::Vm(VmStatus {
                        template: "x86_64/win11".into(),
                        arch: Some("x86_64".into()),
                        cpus: Some(4),
                        memory: Some(8 << 30),
                        agent_version: None,
                    })
                } else {
                    MachineDetail::Container(ContainerStatus {
                        image: "docker.io/library/nginx:latest".into(),
                        digest: None,
                        health: None,
                        exit_code: None,
                    })
                };
                MachineStatus {
                    name: name.into(),
                    label: MachineLabel::derive(state, false, &detail),
                    state,
                    ready: false,
                    ip: None,
                    nics: Vec::new(),
                    web: Vec::new(),
                    cached: true,
                    dev: None,
                    attachable: false,
                    agent_diverged: false,
                    detail,
                }
            })
            .collect();
        json!(LabStatus {
            lab: "demo".into(),
            machines,
            segments: Vec::new(),
            provisioned: true,
            pulls: Vec::new(),
        })
    }

    /// The reload guard's whole job: the daemon cannot re-adopt a live QEMU
    /// process across a restart, so a reload needs proof that there is none.
    /// The guard reads the daemon's own projection, one entry per VM *and*
    /// container — reading keys the daemon no longer emitted made it wave
    /// everything through, and that is the bug this test exists to keep fixed.
    #[actix_web::test]
    async fn only_an_all_stopped_lab_may_reload() {
        assert!(all_machines_stopped(&status_payload(vec![
            ("dc01", PowerState::Stopped),
            ("web", PowerState::Stopped),
        ])));
        assert!(all_machines_stopped(&status_payload(Vec::new())));

        // Every non-stopped state blocks, not just `Running`: a machine
        // mid-boot has a QEMU process the restart would orphan too.
        for state in [
            PowerState::Running,
            PowerState::Starting,
            PowerState::Stopping,
        ] {
            let status = status_payload(vec![("dc01", PowerState::Stopped), ("web", state)]);
            assert!(!all_machines_stopped(&status), "state {state}");
        }
    }

    /// A payload the guard can't read is not evidence that nothing is running
    /// — it means the daemon answered in a shape this guard doesn't
    /// understand, which is precisely how it came to wave running machines
    /// through. Withhold the permission; the user can still stop the lab.
    #[actix_web::test]
    async fn an_unrecognised_status_payload_withholds_permission() {
        // The pre-`3117cff` shape — separate `vms`/`containers` lists — which
        // this guard went on reading after the daemon stopped emitting it.
        let old = json!({"vms": [{"name": "dc01", "state": "stopped"}], "containers": []});
        assert!(!all_machines_stopped(&old));
        assert!(!all_machines_stopped(&json!({})));
        assert!(!all_machines_stopped(&json!({"machines": "not-a-list"})));
        // A machine with no kind is not a machine this guard can vouch for.
        assert!(!all_machines_stopped(
            &json!({"lab": "demo", "machines": [{"name": "dc01", "state": "stopped"}],
                    "segments": [], "provisioned": true, "pulls": []})
        ));
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
