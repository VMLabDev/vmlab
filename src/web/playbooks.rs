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
//! run time but are not editable or manageable here.

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
                .map(|d| json!({"path": d.path, "play": d.play, "vms": d.vms}))
                .collect::<Vec<_>>(),
        ),
        Ok(Err(e)) => HttpResponse::UnprocessableEntity().json(json!({"error": e})),
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
}

/// `POST /api/labs/{lab}/playbooks/scaffold` — create the declared folder
/// with a starter `playbook.wcl`, for playbook blocks added in the designer
/// before any files exist.
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
    let outcome = web::block(move || {
        // Declared (and lexically clean) but allowed to not exist yet.
        match playbook_dir(&root, &body.playbook) {
            Ok(_) | Err(PbDirError::NotFound(_)) => {}
            Err(e) => return Err(e),
        }
        let decls = declared_playbooks(&root).map_err(PbDirError::Forbidden)?;
        let play = decls
            .iter()
            .find(|d| d.path == body.playbook)
            .map(|d| d.play.clone())
            .unwrap_or_else(|| "main".to_string());
        let dir = root.join(&body.playbook);
        std::fs::create_dir_all(&dir).map_err(|e| PbDirError::Io(e.to_string()))?;
        let target = dir.join("playbook.wcl");
        if target.exists() {
            return Ok(body.playbook);
        }
        let name = body
            .playbook
            .rsplit('/')
            .next()
            .unwrap_or(&body.playbook)
            .to_string();
        let skeleton = format!(
            "playbook \"{name}\" {{\n  description = \"Describe what this playbook converges\"\n  version = \"0.1.0\"\n\n  play \"{play}\" {{\n    description = \"A starter play\"\n  }}\n}}\n"
        );
        std::fs::write(&target, skeleton).map_err(|e| PbDirError::Io(e.to_string()))?;
        Ok(body.playbook)
    })
    .await;
    match outcome {
        Ok(Ok(playbook)) => HttpResponse::Ok().json(json!({"ok": true, "playbook": playbook})),
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
        std::fs::write(tmp.path().join("playbooks/base/playbook.wcl"), "x").unwrap();
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
        assert_eq!(body[1]["vms"].as_array().unwrap().len(), 0);
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

        // Undeclared folder cannot be scaffolded.
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/scaffold")
                .set_json(json!({"playbook": "playbooks/rogue"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 403);
    }
}
