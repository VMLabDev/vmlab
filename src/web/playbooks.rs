//! Playbook endpoints: run config-weave check/apply against a machine
//! (proxied to the lab daemon's `playbook.*` commands, progress via the
//! `playbook.op.*` events), plus the declaration list and folder
//! scaffolding. Playbook files themselves are edited through the lab
//! Files tab (`files.rs`); package management lives in `pkgs.rs`, gated
//! by [`playbook_dir`] below.
//!
//! Sandbox contract: only folders that appear as `playbook "…"` blocks in
//! the lab file (re-derived per request — the declarations are the sole
//! authority) are touched. Playbooks declared outside the lab root work at
//! run time but are not editable or manageable here. The two endpoints the
//! designer uses on brand-new folders — [`list_plays`] and [`scaffold`] —
//! are the exception: they accept any lexically clean path inside the lab
//! root, because the designer creates a playbook before declaring it.

use std::path::{Path, PathBuf};
use std::time::Duration;

use actix_web::{HttpResponse, web};
use serde::Deserialize;
use serde_json::json;

use super::api::fail;
use super::fsops::{FsError as PbDirError, plain_relative};
use super::state::AppState;

/// How long a run request waits for a fast verdict (validation errors,
/// already-running conflicts) before detaching to the event stream.
const RUN_DETACH_AFTER: Duration = Duration::from_millis(800);

// ---- declared playbooks (the sandbox authority) -----------------------------

struct PlaybookDecl {
    path: String,
    play: String,
    vms: Vec<String>,
    all_machines: bool,
}

/// Parse the lab's `vmlab.wcl` and return its playbook declarations. Works
/// with the lab daemon down — file editing must not require a running lab.
fn declared_playbooks(root: &Path) -> Result<Vec<PlaybookDecl>, String> {
    let file =
        vmlab::config::load_lab_root(root).map_err(|e| format!("{:?}", miette::Report::new(e)))?;
    Ok(file
        .lab
        .playbooks
        .iter()
        .map(|p| PlaybookDecl {
            path: p.path.display().to_string(),
            play: p.play.clone(),
            vms: p.vms.clone(),
            all_machines: p.all_machines,
        })
        .collect())
}

/// Validate the `playbook` param against the declared set and resolve the
/// existing folder to its canonical path (prefix-checked under the lab
/// root). `NotFound` = declared but the folder doesn't exist yet — the
/// editor offers scaffolding for that case.
pub(crate) fn playbook_dir(root: &Path, playbook: &str) -> Result<PathBuf, PbDirError> {
    plain_relative(playbook, "playbook").map_err(PbDirError::BadRequest)?;
    let declared = declared_playbooks(root).map_err(PbDirError::Forbidden)?;
    if !declared.iter().any(|d| d.path == playbook) {
        return Err(PbDirError::Forbidden(format!(
            "{playbook} is not a declared playbook folder — declare it in vmlab.wcl first"
        )));
    }
    let canonical_root = std::fs::canonicalize(root).map_err(|e| PbDirError::Io(e.to_string()))?;
    let dir = match std::fs::canonicalize(root.join(playbook)) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(PbDirError::NotFound(format!(
                "playbook folder {playbook} does not exist yet"
            )));
        }
        Err(e) => return Err(PbDirError::Io(e.to_string())),
    };
    if !dir.starts_with(&canonical_root) {
        // Declared with enough parent hops to leave the lab — runnable, but
        // outside the web editor's sandbox.
        return Err(PbDirError::Forbidden(
            "playbook folder lies outside the lab root".into(),
        ));
    }
    if !dir.is_dir() {
        return Err(PbDirError::BadRequest(format!(
            "{playbook} is not a directory"
        )));
    }
    Ok(dir)
}

// ---- listing ----------------------------------------------------------------

/// `GET /api/labs/{lab}/playbooks` — the lab's playbook declarations.
pub async fn list_playbooks(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let decls = web::block(move || declared_playbooks(&root)).await;
    match decls {
        Ok(Ok(decls)) => HttpResponse::Ok().json(
            decls
                .iter()
                .map(|d| {
                    json!({
                        "path": d.path, "play": d.play, "vms": d.vms,
                        "all_machines": d.all_machines,
                    })
                })
                .collect::<Vec<_>>(),
        ),
        Ok(Err(e)) => HttpResponse::UnprocessableEntity().json(json!({"error": e})),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

#[derive(Deserialize)]
pub struct PlaysQuery {
    path: String,
}

/// Syntactic scan of a folder's `playbook.wcl` for its `play` blocks.
/// Deliberately schema-free (`parse_for_edit`) — config-weave semantics
/// stay in the guest; the designer only needs the play names. Unlike
/// [`playbook_dir`] this does not require the folder to be declared in
/// vmlab.wcl: the designer asks about draft nodes before any save.
fn enumerate_plays(root: &Path, path: &str) -> Result<serde_json::Value, PbDirError> {
    plain_relative(path, "playbook").map_err(PbDirError::BadRequest)?;
    let missing = json!({"exists": false, "playbook": null, "plays": [], "error": null});
    let canonical_root = std::fs::canonicalize(root).map_err(|e| PbDirError::Io(e.to_string()))?;
    let dir = match std::fs::canonicalize(root.join(path)) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(missing),
        Err(e) => return Err(PbDirError::Io(e.to_string())),
    };
    if !dir.starts_with(&canonical_root) {
        return Err(PbDirError::Forbidden(
            "playbook folder lies outside the lab root".into(),
        ));
    }
    let source = match std::fs::read_to_string(dir.join("playbook.wcl")) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(missing),
        Err(e) => return Err(PbDirError::Io(e.to_string())),
    };
    let src = match wcl_lang::parse_for_edit(&source, "playbook.wcl") {
        Ok(s) => s,
        Err(e) => {
            return Ok(json!({
                "exists": true, "playbook": null, "plays": [],
                "error": e.to_string(),
            }));
        }
    };

    fn str_label(b: &wcl_lang::ast::Block) -> Option<&str> {
        match b.labels.first() {
            Some(wcl_lang::ast::Expr::Utf8(s)) => Some(s),
            _ => None,
        }
    }
    let mut playbook_name = None;
    let mut plays = Vec::new();
    for item in &src.items {
        let wcl_lang::ast::Item::Block(b) = item else {
            continue;
        };
        if b.kind != "playbook" {
            continue;
        }
        playbook_name = playbook_name.or_else(|| str_label(b).map(str::to_string));
        for inner in &b.items {
            let wcl_lang::ast::Item::Block(p) = inner else {
                continue;
            };
            if p.kind != "play" {
                continue;
            }
            let Some(name) = str_label(p) else { continue };
            let description = p.items.iter().find_map(|i| match i {
                wcl_lang::ast::Item::Field(f) if f.name == "description" => match &f.expr {
                    wcl_lang::ast::Expr::Utf8(s) => Some(s.clone()),
                    _ => None,
                },
                _ => None,
            });
            plays.push(json!({"name": name, "description": description}));
        }
    }
    Ok(json!({
        "exists": true, "playbook": playbook_name, "plays": plays,
        "error": null,
    }))
}

/// `GET /api/labs/{lab}/playbooks/plays?path=…` — the plays defined in a
/// folder's `playbook.wcl`, for the designer's per-play cards.
pub async fn list_plays(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    query: web::Query<PlaysQuery>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let q = query.into_inner();
    let out = web::block(move || enumerate_plays(&root, &q.path)).await;
    match out {
        Ok(Ok(v)) => HttpResponse::Ok().json(v),
        Ok(Err(e)) => e.respond(),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

/// `GET /api/labs/{lab}/playbooks/ops` — in-flight runs with log tails
/// (the reconnect resync source, mirroring `template.op_status`).
pub async fn playbook_ops(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    match state.lab_call(&lab, "playbook.op_status", json!({})).await {
        Ok(v) => HttpResponse::Ok().json(v),
        Err(e) => fail(e),
    }
}

// ---- check / apply ------------------------------------------------------------

#[derive(Deserialize, Default)]
pub struct RunBody {
    /// Playbook folder path, to disambiguate when several target the machine.
    path: Option<String>,
    /// Play name, same purpose.
    play: Option<String>,
}

/// `POST /api/labs/{lab}/{vms|containers}/{machine}/playbook/{check|apply}`.
/// Fast failures (unknown machine, ambiguous playbook, already running)
/// return synchronously; anything still going after a short grace detaches
/// with 202 and finishes via the `playbook.op.*` events.
pub async fn run_playbook(
    state: web::Data<AppState>,
    path: web::Path<(String, String, String)>,
    body: Option<web::Json<RunBody>>,
) -> HttpResponse {
    let (lab, machine, action) = path.into_inner();
    let cmd = match action.as_str() {
        "check" => "playbook.check",
        "apply" => "playbook.apply",
        _ => return HttpResponse::NotFound().json(json!({"error": "unknown playbook action"})),
    };
    let body = body.map(web::Json::into_inner).unwrap_or_default();
    let args = json!({"machine": machine, "playbook": body.path, "play": body.play});

    let state = state.into_inner();
    let task = tokio::spawn(async move { state.lab_call(&lab, cmd, args).await });
    match tokio::time::timeout(RUN_DETACH_AFTER, task).await {
        Ok(Ok(Ok(v))) => HttpResponse::Ok().json(v),
        Ok(Ok(Err(e))) => fail(e),
        Ok(Err(e)) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
        // Still running: the op is live, progress rides the event stream.
        Err(_) => HttpResponse::Accepted().json(json!({"started": true})),
    }
}

// ---- scaffolding ------------------------------------------------------------

#[derive(Deserialize)]
pub struct ScaffoldBody {
    playbook: String,
    /// Play name for the starter file; falls back to the declared block's
    /// play, then `main`.
    play: Option<String>,
}

/// Folder segment and play names are interpolated into the skeleton's WCL
/// string literals, so keep them boring.
fn plain_name(value: &str, what: &str) -> Result<(), PbDirError> {
    if value.is_empty()
        || !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
    {
        return Err(PbDirError::BadRequest(format!(
            "{what} \"{value}\" must be letters, digits, dot, dash or underscore"
        )));
    }
    Ok(())
}

/// Resolve a not-yet-existing folder inside the lab: the nearest existing
/// ancestor is canonicalized and prefix-checked, so symlinked parents can't
/// carry the scaffold outside the lab root.
fn new_dir_in_lab(root: &Path, rel: &str) -> Result<PathBuf, PbDirError> {
    let canonical_root = std::fs::canonicalize(root).map_err(|e| PbDirError::Io(e.to_string()))?;
    let target = root.join(rel);
    let mut existing = target.as_path();
    while !existing.exists() {
        existing = existing
            .parent()
            .ok_or_else(|| PbDirError::BadRequest("playbook path escapes the lab".into()))?;
    }
    let anchor = std::fs::canonicalize(existing).map_err(|e| PbDirError::Io(e.to_string()))?;
    if !anchor.starts_with(&canonical_root) {
        return Err(PbDirError::Forbidden(
            "playbook folder lies outside the lab root".into(),
        ));
    }
    Ok(target)
}

/// `POST /api/labs/{lab}/playbooks/scaffold` — create the folder with a
/// starter `playbook.wcl` and an empty `pkgs/` (config-weave ignores
/// non-package entries there, and the Files tab hangs its package buttons off
/// that row). Unlike the editing endpoints this does not require the folder
/// to be declared in vmlab.wcl: the designer scaffolds a new playbook
/// *before* writing its block (and `list_plays` already answers for
/// undeclared paths for the same reason). The write power is the same as the
/// Files tab's, and stays inside the lab root.
pub async fn scaffold(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<ScaffoldBody>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let body = body.into_inner();
    let outcome = web::block(move || -> Result<(String, bool), PbDirError> {
        plain_relative(&body.playbook, "playbook").map_err(PbDirError::BadRequest)?;
        let name = body
            .playbook
            .rsplit('/')
            .next()
            .unwrap_or(&body.playbook)
            .to_string();
        plain_name(&name, "playbook folder name")?;
        let play = match body.play.clone() {
            Some(play) => play,
            None => declared_playbooks(&root)
                .map_err(PbDirError::Forbidden)?
                .iter()
                .find(|d| d.path == body.playbook)
                .map(|d| d.play.clone())
                .unwrap_or_else(|| "main".to_string()),
        };
        plain_name(&play, "play name")?;
        let dir = new_dir_in_lab(&root, &body.playbook)?;
        std::fs::create_dir_all(&dir).map_err(|e| PbDirError::Io(e.to_string()))?;
        let target = dir.join("playbook.wcl");
        if target.exists() {
            return Ok((body.playbook, false));
        }
        let skeleton = format!(
            "playbook \"{name}\" {{\n  description = \"Describe what this playbook converges\"\n  version = \"0.1.0\"\n\n  play \"{play}\" {{\n    description = \"A starter play\"\n  }}\n}}\n"
        );
        std::fs::write(&target, skeleton).map_err(|e| PbDirError::Io(e.to_string()))?;
        // Only for a folder we just scaffolded: an existing playbook that
        // keeps its packages elsewhere shouldn't grow a stray empty dir.
        std::fs::create_dir_all(dir.join("pkgs")).map_err(|e| PbDirError::Io(e.to_string()))?;
        Ok((body.playbook, true))
    })
    .await;
    match outcome {
        Ok(Ok((playbook, created))) => {
            HttpResponse::Ok().json(json!({"ok": true, "playbook": playbook, "created": created}))
        }
        Ok(Err(e)) => e.respond(),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, test};
    use serde_json::Value;

    /// A tempdir lab named `lab` with one VM and one declared playbook
    /// (`playbooks/base`, folder present with a playbook.wcl).
    fn playbook_lab() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("vmlab.wcl"),
            r#"import <vmlab.wcl>
lab "lab" {
  vm "web01" { template = "x86_64/t" }
  playbook "playbooks/base" { play = "base" vms = ["web01"] }
  playbook "playbooks/ghost" { play = "base" }
}
"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("playbooks/base/pkgs/example")).unwrap();
        std::fs::write(
            tmp.path().join("playbooks/base/playbook.wcl"),
            r#"playbook "base" {
  description = "test playbook"
  play "base" { description = "converge the base" }
  play "extra" {}
}
"#,
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("playbooks/base/pkgs/example/package.wcl"),
            "y",
        )
        .unwrap();
        std::fs::write(tmp.path().join("playbooks/base/.hidden"), "z").unwrap();
        tmp
    }

    fn state_for(root: &Path) -> web::Data<AppState> {
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

    macro_rules! app {
        ($root:expr) => {
            test::init_service(
                App::new()
                    .app_data(state_for($root))
                    .route("/api/labs/{lab}/playbooks", web::get().to(list_playbooks))
                    .route("/api/labs/{lab}/playbooks/plays", web::get().to(list_plays))
                    .route(
                        "/api/labs/{lab}/playbooks/scaffold",
                        web::post().to(scaffold),
                    ),
            )
            .await
        };
    }

    #[actix_web::test]
    async fn list_returns_declarations() {
        let tmp = playbook_lab();
        let app = app!(tmp.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body[0]["path"], "playbooks/base");
        assert_eq!(body[0]["play"], "base");
        assert_eq!(body[0]["vms"][0], "web01");
        assert_eq!(body[0]["all_machines"], false);
        assert_eq!(body[1]["vms"].as_array().unwrap().len(), 0);
        assert_eq!(body[1]["all_machines"], false);
    }

    #[actix_web::test]
    async fn plays_enumerates_folder_file() {
        let tmp = playbook_lab();
        let app = app!(tmp.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/plays?path=playbooks/base")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["exists"], true);
        assert_eq!(body["playbook"], "base");
        assert_eq!(body["error"], Value::Null);
        assert_eq!(body["plays"][0]["name"], "base");
        assert_eq!(body["plays"][0]["description"], "converge the base");
        assert_eq!(body["plays"][1]["name"], "extra");
        assert_eq!(body["plays"][1]["description"], Value::Null);
    }

    #[actix_web::test]
    async fn plays_missing_and_undeclared_folders() {
        let tmp = playbook_lab();
        let app = app!(tmp.path());
        // Declared but the folder doesn't exist yet.
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/plays?path=playbooks/ghost")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["exists"], false);
        assert_eq!(body["plays"].as_array().unwrap().len(), 0);

        // Undeclared draft paths are fine too — same missing shape.
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/plays?path=playbooks/draft")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["exists"], false);
    }

    #[actix_web::test]
    async fn plays_unparsable_file_reports_error() {
        let tmp = playbook_lab();
        std::fs::write(
            tmp.path().join("playbooks/base/playbook.wcl"),
            "playbook \"broken {",
        )
        .unwrap();
        let app = app!(tmp.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/plays?path=playbooks/base")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["exists"], true);
        assert!(!body["error"].as_str().unwrap().is_empty());
        assert_eq!(body["plays"].as_array().unwrap().len(), 0);
    }

    #[actix_web::test]
    async fn plays_rejects_escaping_paths() {
        let tmp = playbook_lab();
        let app = app!(tmp.path());
        for path in ["../outside", "/etc", "a/../.."] {
            let resp = test::call_service(
                &app,
                test::TestRequest::get()
                    .uri(&format!(
                        "/api/labs/lab/playbooks/plays?path={}",
                        urlencoding(path)
                    ))
                    .to_request(),
            )
            .await;
            assert_eq!(resp.status(), 400, "{path}");
        }
    }

    fn urlencoding(s: &str) -> String {
        s.replace('/', "%2F").replace("..", "%2E%2E")
    }

    #[actix_web::test]
    async fn scaffold_creates_declared_missing_folder() {
        let tmp = playbook_lab();
        let app = app!(tmp.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/scaffold")
                .set_json(json!({"playbook": "playbooks/ghost"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let content =
            std::fs::read_to_string(tmp.path().join("playbooks/ghost/playbook.wcl")).unwrap();
        assert!(content.contains("play \"base\""), "{content}");
    }

    /// The designer scaffolds before it declares, so an undeclared path with
    /// an explicit play name is the normal "Add playbook" case.
    #[actix_web::test]
    async fn scaffold_creates_undeclared_folder_with_named_play() {
        let tmp = playbook_lab();
        let app = app!(tmp.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/scaffold")
                .set_json(json!({"playbook": "playbooks/fresh", "play": "bootstrap"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["created"], true);
        let content =
            std::fs::read_to_string(tmp.path().join("playbooks/fresh/playbook.wcl")).unwrap();
        assert!(content.contains("playbook \"fresh\""), "{content}");
        assert!(content.contains("play \"bootstrap\""), "{content}");
        // Ready for `Add package` in the Files tab.
        assert!(tmp.path().join("playbooks/fresh/pkgs").is_dir());

        // Idempotent: a second call leaves the file alone.
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/scaffold")
                .set_json(json!({"playbook": "playbooks/fresh", "play": "other"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["created"], false);
        let again =
            std::fs::read_to_string(tmp.path().join("playbooks/fresh/playbook.wcl")).unwrap();
        assert_eq!(again, content);
    }

    #[actix_web::test]
    async fn scaffold_rejects_escaping_paths_and_odd_names() {
        let tmp = playbook_lab();
        let app = app!(tmp.path());
        for body in [
            json!({"playbook": "../escape"}),
            json!({"playbook": "playbooks/we\"ird"}),
            json!({"playbook": "playbooks/fine", "play": "we\"ird"}),
        ] {
            let resp = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/api/labs/lab/playbooks/scaffold")
                    .set_json(body.clone())
                    .to_request(),
            )
            .await;
            assert_eq!(resp.status(), 400, "{body}");
        }
        assert!(!tmp.path().join("playbooks/fine").exists());
    }
}
