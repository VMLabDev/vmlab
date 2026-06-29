//! REST handlers. Each is a thin translation of an HTTP request into a daemon
//! proto call, returning the daemon's JSON (or an error mapped to a 4xx/5xx).

use actix_web::{HttpResponse, web};
use serde::Deserialize;
use serde_json::{Value, json};

use super::state::AppState;

/// Map a daemon error string to an HTTP response.
fn fail(e: String) -> HttpResponse {
    // Unknown lab / vm is the client's fault; everything else is treated as a
    // bad gateway to the daemon.
    if e.contains("unknown lab") || e.contains("no such") || e.contains("not found") {
        HttpResponse::NotFound().json(json!({"error": e}))
    } else {
        HttpResponse::BadGateway().json(json!({"error": e}))
    }
}

fn ok(v: Value) -> HttpResponse {
    HttpResponse::Ok().json(v)
}

/// `GET /api/labs` — running labs (registry) merged with the cwd lab.
pub async fn list_labs(state: web::Data<AppState>) -> HttpResponse {
    let mut labs = state
        .supervisor_call("status", Value::Null)
        .await
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default();

    // Ensure the cwd lab shows up even if its daemon isn't running yet.
    if let Some((name, root)) = &state.default_lab
        && !labs.iter().any(|l| l["name"].as_str() == Some(name))
    {
        labs.push(json!({
            "name": name,
            "root": root.to_string_lossy(),
            "state": "stopped",
        }));
    }
    ok(json!(labs))
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
    let out = std::env::temp_dir().join(format!("vmlab-web-{lab}-{vm}.png"));
    let out_str = out.to_string_lossy().to_string();
    if let Err(e) = state
        .lab_call(&lab, "vm.screenshot", json!({"vm": vm, "path": out_str}))
        .await
    {
        return fail(e);
    }
    match tokio::fs::read(&out).await {
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
