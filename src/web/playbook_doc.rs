//! The playbook designer's data plane: config-weave's DocJson pipeline,
//! proxied for the Files tab's visual editor.
//!
//! config-weave owns its own WCL schema, so vmlab does not model
//! `playbook.wcl` at all. Its binary ships two hidden subcommands built for
//! exactly this — `__wcl-inspect` turns source into a structural document,
//! `__wcl-render` syncs an edited document back onto the file's AST (so
//! comments and constructs the editor doesn't understand survive) — and both
//! work on plain text, i.e. on the unsaved buffer the Files tab already
//! holds. Saving stays the Files tab's job (`files.rs`).
//!
//! The third endpoint reads the playbook's installed packages so the
//! properties panel knows what a resource accepts. It inspects each
//! `pkgs/<name>/package.wcl` rather than calling `config-weave list --json`:
//! `list` refuses to emit anything while the *playbook* has diagnostics,
//! which is the half-written state the designer exists for.

use std::path::{Path, PathBuf};
use std::time::Duration;

use actix_web::{HttpResponse, web};
use serde::Deserialize;
use serde_json::{Value, json};

use vmlab::weave_bin::{GuestOs, weave_binary};

use super::api::fail;
use super::fsops::plain_relative;
use super::state::AppState;

/// No network and no guest involved — this is a local parse, so a run that
/// takes this long is wedged, not slow.
const DOC_TIMEOUT: Duration = Duration::from_secs(15);

/// Refuse absurd payloads before spawning anything (the Files tab caps
/// files at 1 MiB; a playbook.wcl is orders of magnitude smaller).
const MAX_SOURCE_BYTES: usize = 1024 * 1024;

fn bad(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({"error": msg.into()}))
}

fn internal(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::InternalServerError().json(json!({"error": msg.into()}))
}

/// The document kinds config-weave's DocJson pipeline understands.
fn checked_kind(kind: Option<&str>) -> Result<&str, String> {
    match kind.unwrap_or("playbook") {
        k @ ("playbook" | "package") => Ok(k),
        other => Err(format!("unknown document kind \"{other}\"")),
    }
}

fn weave_bin(state: &AppState) -> Result<PathBuf, String> {
    weave_binary(&state.weave_bin_dir, GuestOs::Linux, "x86_64").map_err(|e| format!("{e:#}"))
}

/// Run one DocJson subcommand: JSON in on stdin, JSON out on stdout.
///
/// A non-zero exit with no parsable stdout is the interesting failure — it
/// is what an older config-weave (no `__wcl-*` subcommands at all) looks
/// like, so the message says so rather than leaving the UI blank.
async fn run_doc_cmd(bin: &Path, sub: &str, payload: Value) -> Result<Value, String> {
    use tokio::io::AsyncWriteExt;

    let mut child = tokio::process::Command::new(bin)
        .arg(sub)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("failed to run {}: {e}", bin.display()))?;
    let body = serde_json::to_vec(&payload).map_err(|e| e.to_string())?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| "config-weave stdin unavailable".to_string())?;
    let write = async move {
        stdin.write_all(&body).await?;
        stdin.shutdown().await
    };
    let out = tokio::time::timeout(DOC_TIMEOUT, async {
        let (write, out) = tokio::join!(write, child.wait_with_output());
        write.map_err(|e| e.to_string())?;
        out.map_err(|e| e.to_string())
    })
    .await
    .map_err(|_| {
        format!(
            "config-weave {sub} timed out after {}s",
            DOC_TIMEOUT.as_secs()
        )
    })??;

    let stdout = String::from_utf8_lossy(&out.stdout);
    if let Ok(v) = serde_json::from_str::<Value>(stdout.trim()) {
        return Ok(v);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let detail = stderr.trim().lines().next().unwrap_or("no output").trim();
    Err(format!(
        "config-weave {sub} produced no JSON ({detail}) — the playbook designer needs a \
         config-weave build with the {sub} subcommand"
    ))
}

// ---- inspect / render -------------------------------------------------------

#[derive(Deserialize)]
pub struct InspectBody {
    source: String,
    kind: Option<String>,
}

/// `POST /api/labs/{lab}/playbooks/doc/inspect` — source text → the
/// structural document the designer edits. A source that cannot be
/// represented answers `{"ok": false, "diags": […]}` with status 200: that
/// is a UI state (show the diagnostics, offer the code view), not a
/// transport error.
pub async fn inspect(
    state: web::Data<AppState>,
    _lab: web::Path<String>,
    body: web::Json<InspectBody>,
) -> HttpResponse {
    let body = body.into_inner();
    if body.source.len() > MAX_SOURCE_BYTES {
        return bad("document is too large for the designer");
    }
    let kind = match checked_kind(body.kind.as_deref()) {
        Ok(k) => k,
        Err(e) => return bad(e),
    };
    let bin = match weave_bin(&state) {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    match run_doc_cmd(
        &bin,
        "__wcl-inspect",
        json!({"kind": kind, "source": body.source}),
    )
    .await
    {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct RenderBody {
    base_source: String,
    doc: Value,
    kind: Option<String>,
}

/// `POST /api/labs/{lab}/playbooks/doc/render` — edited document + the
/// text it came from → new WCL source. `base_source` is what keeps
/// comments: config-weave syncs the document onto that AST instead of
/// printing a fresh file.
pub async fn render(
    state: web::Data<AppState>,
    _lab: web::Path<String>,
    body: web::Json<RenderBody>,
) -> HttpResponse {
    let body = body.into_inner();
    if body.base_source.len() > MAX_SOURCE_BYTES {
        return bad("document is too large for the designer");
    }
    let kind = match checked_kind(body.kind.as_deref()) {
        Ok(k) => k,
        Err(e) => return bad(e),
    };
    let bin = match weave_bin(&state) {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    let payload = json!({"kind": kind, "base_source": body.base_source, "doc": body.doc});
    match run_doc_cmd(&bin, "__wcl-render", payload).await {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => internal(e),
    }
}

// ---- resource catalogue -----------------------------------------------------

#[derive(Deserialize)]
pub struct CatalogQuery {
    /// Playbook folder, relative to the lab root.
    path: String,
}

/// The `pkgs/<name>/package.wcl` files of one playbook folder, in name
/// order. Undeclared folders are allowed for the same reason
/// `playbooks::list_plays` allows them: the designer opens whatever
/// playbook the Files tab is showing.
fn package_files(root: &Path, path: &str) -> Result<Vec<PathBuf>, String> {
    plain_relative(path, "playbook")?;
    let canonical_root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let dir = match std::fs::canonicalize(root.join(path)) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    if !dir.starts_with(&canonical_root) {
        return Err("playbook folder lies outside the lab root".into());
    }
    let entries = match std::fs::read_dir(dir.join("pkgs")) {
        Ok(entries) => entries,
        // No pkgs/ is legal — config-weave treats it as "no packages".
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.to_string()),
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| p.join("package.wcl"))
        .filter(|p| p.is_file())
        .collect();
    files.sort();
    Ok(files)
}

/// `GET /api/labs/{lab}/playbooks/catalog?path=…` — the resources and
/// gatherers the playbook's installed packages declare, with their
/// parameters, for the designer's resource picker and property form. A
/// package that fails to inspect is reported in `errors` and skipped, so
/// one broken package cannot empty the picker.
pub async fn catalog(
    state: web::Data<AppState>,
    _lab: web::Path<String>,
    query: web::Query<CatalogQuery>,
) -> HttpResponse {
    let lab = _lab.into_inner();
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let bin = match weave_bin(&state) {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    let path = query.into_inner().path;
    let files = match web::block(move || package_files(&root, &path)).await {
        Ok(Ok(files)) => files,
        Ok(Err(e)) => return bad(e),
        Err(e) => return internal(e.to_string()),
    };

    let mut packages = Vec::new();
    let mut errors = Vec::new();
    for file in files {
        let name = file
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let source = match tokio::fs::read_to_string(&file).await {
            Ok(s) => s,
            Err(e) => {
                errors.push(json!({"package": name, "error": e.to_string()}));
                continue;
            }
        };
        match run_doc_cmd(
            &bin,
            "__wcl-inspect",
            json!({"kind": "package", "source": source}),
        )
        .await
        {
            Ok(v) if v["ok"] == Value::Bool(true) => packages.push(v["doc"].clone()),
            Ok(v) => errors.push(json!({"package": name, "diags": v["diags"].clone()})),
            Err(e) => errors.push(json!({"package": name, "error": e})),
        }
    }
    HttpResponse::Ok().json(json!({"packages": packages, "errors": errors}))
}

#[cfg(test)]
mod tests {
    use super::super::state::AuthConfig;
    use super::*;
    use actix_web::{App, test};
    use std::os::unix::fs::PermissionsExt;

    /// A tempdir lab whose `playbooks/base` folder holds two packages.
    fn playbook_lab() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("vmlab.wcl"),
            "import <vmlab.wcl>\nlab \"lab\" { vm \"web01\" { template = \"x86_64/t\" } }\n",
        )
        .unwrap();
        for pkg in ["alpha", "beta"] {
            let dir = tmp.path().join("playbooks/base/pkgs").join(pkg);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("package.wcl"), format!("package \"{pkg}\" {{}}\n")).unwrap();
        }
        std::fs::write(tmp.path().join("playbooks/base/playbook.wcl"), "x").unwrap();
        tmp
    }

    /// A stub `config-weave-linux-x86_64` running `body` with the
    /// subcommand in `$1` and stdin still attached.
    fn stub_bin(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config-weave-linux-x86_64");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    fn state_for(root: &Path, stub: &Path) -> web::Data<AppState> {
        let mut state = AppState::new(
            AuthConfig {
                enabled: false,
                user: String::new(),
                password_hash: String::new(),
            },
            Some(("lab".into(), root.to_path_buf())),
            false,
        );
        state.weave_bin_dir = stub.to_path_buf();
        web::Data::new(state)
    }

    macro_rules! app {
        ($root:expr, $stub:expr) => {
            test::init_service(
                App::new()
                    .app_data(state_for($root, $stub))
                    .route(
                        "/api/labs/{lab}/playbooks/doc/inspect",
                        web::post().to(inspect),
                    )
                    .route(
                        "/api/labs/{lab}/playbooks/doc/render",
                        web::post().to(render),
                    )
                    .route("/api/labs/{lab}/playbooks/catalog", web::get().to(catalog)),
            )
            .await
        };
    }

    #[actix_web::test]
    async fn inspect_passes_source_on_stdin_and_returns_the_document() {
        let lab = playbook_lab();
        // Echo the payload back so the test can assert what was piped in.
        let stub = stub_bin(
            "cat > \"$(dirname \"$0\")/in.json\"; echo '{\"ok\":true,\"doc\":{\"name\":\"demo\"}}'",
        );
        let app = app!(lab.path(), stub.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/doc/inspect")
                .set_json(json!({"source": "playbook \"demo\" {}"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["ok"], true);
        assert_eq!(body["doc"]["name"], "demo");
        let piped: Value =
            serde_json::from_str(&std::fs::read_to_string(stub.path().join("in.json")).unwrap())
                .unwrap();
        assert_eq!(piped["kind"], "playbook");
        assert_eq!(piped["source"], "playbook \"demo\" {}");
    }

    #[actix_web::test]
    async fn inspect_forwards_diagnostics_as_a_200() {
        let lab = playbook_lab();
        let stub =
            stub_bin("cat >/dev/null; echo '{\"ok\":false,\"diags\":[{\"message\":\"boom\"}]}'");
        let app = app!(lab.path(), stub.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/doc/inspect")
                .set_json(json!({"source": "broken"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["ok"], false);
        assert_eq!(body["diags"][0]["message"], "boom");
    }

    #[actix_web::test]
    async fn render_pipes_base_source_and_doc() {
        let lab = playbook_lab();
        let stub = stub_bin(
            "cat > \"$(dirname \"$0\")/in.json\"; echo '{\"ok\":true,\"source\":\"rendered\"}'",
        );
        let app = app!(lab.path(), stub.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/doc/render")
                .set_json(json!({"base_source": "old", "doc": {"name": "demo"}}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["source"], "rendered");
        let piped: Value =
            serde_json::from_str(&std::fs::read_to_string(stub.path().join("in.json")).unwrap())
                .unwrap();
        assert_eq!(piped["base_source"], "old");
        assert_eq!(piped["doc"]["name"], "demo");
    }

    #[actix_web::test]
    async fn unknown_kind_is_rejected() {
        let lab = playbook_lab();
        let stub = stub_bin("exit 1");
        let app = app!(lab.path(), stub.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/doc/inspect")
                .set_json(json!({"source": "x", "kind": "repo"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn a_binary_without_the_subcommand_says_so() {
        let lab = playbook_lab();
        let stub =
            stub_bin("cat >/dev/null; echo \"error: unrecognized subcommand '$1'\" >&2; exit 2");
        let app = app!(lab.path(), stub.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/doc/inspect")
                .set_json(json!({"source": "x"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 500);
        let body: Value = test::read_body_json(resp).await;
        let msg = body["error"].as_str().unwrap();
        assert!(msg.contains("__wcl-inspect"), "{msg}");
    }

    #[actix_web::test]
    async fn catalog_inspects_every_installed_package() {
        let lab = playbook_lab();
        // Answer with the package name read off the piped source.
        let stub = stub_bin(
            "src=$(cat); name=$(printf '%s' \"$src\" | sed -n 's/.*package \\\\\"\\([a-z]*\\)\\\\\".*/\\1/p'); \
             printf '{\"ok\":true,\"doc\":{\"name\":\"%s\",\"resources\":[]}}' \"$name\"",
        );
        let app = app!(lab.path(), stub.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/catalog?path=playbooks%2Fbase")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        let names: Vec<&str> = body["packages"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|p| p["name"].as_str())
            .collect();
        assert_eq!(names, vec!["alpha", "beta"], "{body}");
        assert!(body["errors"].as_array().unwrap().is_empty(), "{body}");
    }

    #[actix_web::test]
    async fn catalog_is_empty_without_a_pkgs_folder_and_refuses_escapes() {
        let lab = playbook_lab();
        let stub = stub_bin("cat >/dev/null; echo '{\"ok\":true,\"doc\":{}}'");
        let app = app!(lab.path(), stub.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/catalog?path=playbooks%2Fmissing")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert!(body["packages"].as_array().unwrap().is_empty());

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/catalog?path=..%2Fescape")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);
    }
}
