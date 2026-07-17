//! Playbook endpoints: run config-weave check/apply against a machine
//! (proxied to the lab daemon's `playbook.*` commands, progress via the
//! `playbook.op.*` events), and a sandboxed file API over the playbook
//! folders declared in `vmlab.wcl` — the web playbook editor's backend.
//!
//! Sandbox contract: the file API only ever touches folders that appear as
//! `playbook "…"` blocks in the lab file (re-derived per request — the
//! declarations are the sole authority), and only files with editable
//! extensions inside them. Playbooks declared outside the lab root work at
//! run time but are not editable here.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use actix_web::{HttpResponse, web};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;

use super::api::fail;
use super::state::AppState;

const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_TREE_ENTRIES: usize = 2000;
const MAX_TREE_DEPTH: usize = 16;
const EDITABLE_EXTS: &[&str] = &["wcl", "wscript", "ws"];

/// How long a run request waits for a fast verdict (validation errors,
/// already-running conflicts) before detaching to the event stream.
const RUN_DETACH_AFTER: Duration = Duration::from_millis(800);

fn rev_of(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

fn bad(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({"error": msg.into()}))
}

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

enum PbDirError {
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    Io(String),
}

impl PbDirError {
    fn respond(self) -> HttpResponse {
        match self {
            PbDirError::BadRequest(e) => HttpResponse::BadRequest().json(json!({"error": e})),
            PbDirError::Forbidden(e) => HttpResponse::Forbidden().json(json!({"error": e})),
            PbDirError::NotFound(e) => HttpResponse::NotFound().json(json!({"error": e})),
            PbDirError::Io(e) => HttpResponse::InternalServerError().json(json!({"error": e})),
        }
    }
}

/// Lexical shape check shared by both request params: relative, plain
/// components only (no `..`, no roots), non-empty.
fn plain_relative<'a>(requested: &'a str, what: &str) -> Result<&'a Path, String> {
    let p = Path::new(requested);
    if p.as_os_str().is_empty()
        || p.is_absolute()
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("{what} must be a plain relative path"));
    }
    Ok(p)
}

/// Validate the `playbook` param against the declared set and resolve the
/// existing folder to its canonical path (prefix-checked under the lab
/// root). `NotFound` = declared but the folder doesn't exist yet — the
/// editor offers scaffolding for that case.
fn playbook_dir(root: &Path, playbook: &str) -> Result<PathBuf, PbDirError> {
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

fn editable(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EDITABLE_EXTS.contains(&e))
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

// ---- file API -----------------------------------------------------------------

#[derive(Deserialize)]
pub struct TreeQuery {
    playbook: String,
}

/// `GET /api/labs/{lab}/playbooks/tree?playbook=…` — recursive listing of a
/// declared playbook folder: dirs first, hidden entries skipped, capped.
pub async fn tree(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    query: web::Query<TreeQuery>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let playbook = query.playbook.clone();
    let outcome = web::block(move || {
        let dir = playbook_dir(&root, &playbook)?;
        let mut count = 0usize;
        let entries =
            walk_dir(&dir, Path::new(""), 0, &mut count).map_err(PbDirError::BadRequest)?;
        Ok::<_, PbDirError>((playbook, entries))
    })
    .await;
    match outcome {
        Ok(Ok((playbook, entries))) => {
            HttpResponse::Ok().json(json!({"playbook": playbook, "entries": entries}))
        }
        Ok(Err(PbDirError::BadRequest(e))) if e.contains("editor limit") => {
            HttpResponse::PayloadTooLarge().json(json!({"error": e}))
        }
        Ok(Err(e)) => e.respond(),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

fn walk_dir(dir: &Path, rel: &Path, depth: usize, count: &mut usize) -> Result<Vec<Value>, String> {
    if depth > MAX_TREE_DEPTH {
        return Err(format!(
            "playbook tree deeper than {MAX_TREE_DEPTH} levels — exceeds the editor limit"
        ));
    }
    let mut names: Vec<(String, bool)> = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        // Symlinks are not followed: a link to a big/hostile tree must not
        // widen the listing (writes reject them independently).
        let meta = entry.metadata().map_err(|e| e.to_string())?;
        let is_symlink = std::fs::symlink_metadata(entry.path())
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(true);
        if is_symlink {
            continue;
        }
        names.push((name, meta.is_dir()));
    }
    names.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut out = Vec::new();
    for (name, is_dir) in names {
        *count += 1;
        if *count > MAX_TREE_ENTRIES {
            return Err(format!(
                "playbook tree exceeds {MAX_TREE_ENTRIES} entries — exceeds the editor limit"
            ));
        }
        let rel_path = if rel.as_os_str().is_empty() {
            PathBuf::from(&name)
        } else {
            rel.join(&name)
        };
        let rel_str = rel_path.to_string_lossy().replace('\\', "/");
        if is_dir {
            let children = walk_dir(&dir.join(&name), &rel_path, depth + 1, count)?;
            out.push(json!({"name": name, "path": rel_str, "dir": true, "children": children}));
        } else {
            let size = std::fs::metadata(dir.join(&name)).map(|m| m.len()).ok();
            out.push(json!({"name": name, "path": rel_str, "dir": false, "size": size}));
        }
    }
    Ok(out)
}

#[derive(Deserialize)]
pub struct FileQuery {
    playbook: String,
    path: String,
}

/// `GET /api/labs/{lab}/playbooks/file?playbook=…&path=…` — read one file.
pub async fn get_file(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    query: web::Query<FileQuery>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let (playbook, rel) = (query.playbook.clone(), query.path.clone());
    let outcome = web::block(move || {
        let dir = playbook_dir(&root, &playbook)?;
        let rel_path = plain_relative(&rel, "path").map_err(PbDirError::BadRequest)?;
        if !editable(rel_path) {
            return Err(PbDirError::BadRequest(format!(
                "only {} files are editable",
                EDITABLE_EXTS.join("/")
            )));
        }
        let path = match std::fs::canonicalize(dir.join(rel_path)) {
            Ok(p) if p.starts_with(&dir) => p,
            Ok(_) => {
                return Err(PbDirError::BadRequest(
                    "path escapes the playbook folder".into(),
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(PbDirError::NotFound(format!("{rel}: not found")));
            }
            Err(e) => return Err(PbDirError::Io(e.to_string())),
        };
        match std::fs::read_to_string(&path) {
            Ok(content) if content.len() <= MAX_FILE_BYTES => Ok((rel, content)),
            Ok(_) => Err(PbDirError::BadRequest(
                "file exceeds the 1 MiB editor limit".into(),
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                Err(PbDirError::NotFound(format!("{rel}: not found")))
            }
            Err(e) => Err(PbDirError::Io(e.to_string())),
        }
    })
    .await;
    match outcome {
        Ok(Ok((rel, content))) => HttpResponse::Ok().json(json!({
            "path": rel,
            "rev": rev_of(&content),
            "content": content,
        })),
        Ok(Err(e)) => e.respond(),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

#[derive(Deserialize)]
pub struct FileBody {
    playbook: String,
    path: String,
    content: String,
    /// SHA-256 returned by `GET`; `None` means create without overwriting.
    base_rev: Option<String>,
}

enum FileSave {
    Saved { rev: String },
    Stale { rev: Option<String> },
    Dir(PbDirError),
    Invalid(String),
    Error(String),
}

/// `PUT /api/labs/{lab}/playbooks/file` — revision-aware create/update,
/// the `save_script` contract scoped to a declared playbook folder. Parent
/// directories are created (symlink-refusing walk), so new files in new
/// package subfolders need no separate mkdir.
pub async fn save_file(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<FileBody>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let body = body.into_inner();
    if body.content.len() > MAX_FILE_BYTES {
        return HttpResponse::PayloadTooLarge()
            .json(json!({"error": "file exceeds the 1 MiB editor limit"}));
    }
    let outcome = web::block(move || {
        let dir = match playbook_dir(&root, &body.playbook) {
            Ok(d) => d,
            Err(e) => return FileSave::Dir(e),
        };
        let rel = match plain_relative(&body.path, "path") {
            Ok(p) => p.to_path_buf(),
            Err(e) => return FileSave::Invalid(e),
        };
        if !editable(&rel) {
            return FileSave::Invalid(format!(
                "only {} files are editable",
                EDITABLE_EXTS.join("/")
            ));
        }
        let target = dir.join(&rel);
        let Some(parent) = target.parent() else {
            return FileSave::Invalid("path has no parent directory".into());
        };
        let canonical_parent = match ensure_safe_parent(&dir, parent) {
            Ok(p) => p,
            Err(e) => return FileSave::Invalid(e),
        };
        if std::fs::symlink_metadata(&target)
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
        {
            return FileSave::Invalid("path cannot be a symbolic link".into());
        }
        let safe_path = canonical_parent.join(target.file_name().expect("validated filename"));
        let existing = match std::fs::read_to_string(&safe_path) {
            Ok(source) => Some(source),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return FileSave::Error(e.to_string()),
        };
        let current_rev = existing.as_deref().map(rev_of);
        if current_rev != body.base_rev {
            return FileSave::Stale { rev: current_rev };
        }
        let creating = body.base_rev.is_none();
        let mut temp = match tempfile::NamedTempFile::new_in(&canonical_parent) {
            Ok(file) => file,
            Err(e) => return FileSave::Error(e.to_string()),
        };
        if let Err(e) = temp
            .write_all(body.content.as_bytes())
            .and_then(|_| temp.flush())
        {
            return FileSave::Error(e.to_string());
        }
        let persisted = if creating {
            temp.persist_noclobber(&safe_path)
        } else {
            temp.persist(&safe_path)
        };
        if let Err(e) = persisted {
            if creating && e.error.kind() == std::io::ErrorKind::AlreadyExists {
                return FileSave::Stale {
                    rev: std::fs::read_to_string(&safe_path)
                        .ok()
                        .as_deref()
                        .map(rev_of),
                };
            }
            return FileSave::Error(e.error.to_string());
        }
        FileSave::Saved {
            rev: rev_of(&body.content),
        }
    })
    .await;
    match outcome {
        Ok(FileSave::Saved { rev }) => HttpResponse::Ok().json(json!({"ok": true, "rev": rev})),
        Ok(FileSave::Stale { rev }) => {
            HttpResponse::Conflict().json(json!({"error": "file changed on disk", "rev": rev}))
        }
        Ok(FileSave::Dir(e)) => e.respond(),
        Ok(FileSave::Invalid(error)) => bad(error),
        Ok(FileSave::Error(error)) => {
            HttpResponse::InternalServerError().json(json!({"error": error}))
        }
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

/// `POST /api/labs/{lab}/playbooks/scaffold` — create the declared folder
/// with a starter `playbook.wcl`, for playbook blocks added in the designer
/// before any files exist.
pub async fn scaffold(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<TreeQuery>,
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

/// Walk `parent` down from the sandbox base `dir`, refusing symlinked
/// segments and creating missing directories ([`super::api`]'s script
/// variant, generalised to an arbitrary base).
fn ensure_safe_parent(base: &Path, parent: &Path) -> Result<PathBuf, String> {
    let canonical_base = std::fs::canonicalize(base).map_err(|e| e.to_string())?;
    let relative = parent
        .strip_prefix(base)
        .map_err(|_| "path escapes the playbook folder".to_string())?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err("path escapes the playbook folder".into());
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("playbook directories cannot be symbolic links".into());
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
    if !canonical_parent.starts_with(canonical_base) {
        return Err("path escapes the playbook folder".into());
    }
    Ok(canonical_parent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, test};

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
                    .route("/api/labs/{lab}/playbooks/tree", web::get().to(tree))
                    .route("/api/labs/{lab}/playbooks/file", web::get().to(get_file))
                    .route("/api/labs/{lab}/playbooks/file", web::put().to(save_file))
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
    async fn tree_rejects_undeclared_and_traversal() {
        let tmp = playbook_lab();
        std::fs::create_dir_all(tmp.path().join("other")).unwrap();
        let app = app!(tmp.path());

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/tree?playbook=other")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 403);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/tree?playbook=..%2Fother")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/tree?playbook=%2Fetc")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn tree_lists_nested_dirs_first_hidden_skipped() {
        let tmp = playbook_lab();
        let app = app!(tmp.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/tree?playbook=playbooks%2Fbase")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        let entries = body["entries"].as_array().unwrap();
        // dirs first: pkgs before playbook.wcl; .hidden skipped.
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["name"], "pkgs");
        assert_eq!(entries[0]["dir"], true);
        let pkg = &entries[0]["children"][0];
        assert_eq!(pkg["name"], "example");
        assert_eq!(pkg["children"][0]["path"], "pkgs/example/package.wcl");
        assert_eq!(entries[1]["name"], "playbook.wcl");

        // Declared but missing folder → 404 (scaffolding cue).
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/tree?playbook=playbooks%2Fghost")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn file_create_read_stale_and_extension_gate() {
        let tmp = playbook_lab();
        let app = app!(tmp.path());

        // Create (base_rev: null) in a new subfolder.
        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/api/labs/lab/playbooks/file")
                .set_json(json!({
                    "playbook": "playbooks/base",
                    "path": "pkgs/redis/resources/svc.wscript",
                    "content": "export fn check() {}\n",
                    "base_rev": null,
                }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let created: Value = test::read_body_json(resp).await;
        let rev = created["rev"].as_str().unwrap().to_string();

        // Read it back.
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/file?playbook=playbooks%2Fbase&path=pkgs%2Fredis%2Fresources%2Fsvc.wscript")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let doc: Value = test::read_body_json(resp).await;
        assert_eq!(doc["rev"].as_str().unwrap(), rev);

        // Stale write (wrong base_rev) → 409 with the current rev.
        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/api/labs/lab/playbooks/file")
                .set_json(json!({
                    "playbook": "playbooks/base",
                    "path": "pkgs/redis/resources/svc.wscript",
                    "content": "changed",
                    "base_rev": "deadbeef",
                }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 409);
        let conflict: Value = test::read_body_json(resp).await;
        assert_eq!(conflict["rev"].as_str().unwrap(), rev);

        // Duplicate create → 409 (noclobber).
        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/api/labs/lab/playbooks/file")
                .set_json(json!({
                    "playbook": "playbooks/base",
                    "path": "pkgs/redis/resources/svc.wscript",
                    "content": "again",
                    "base_rev": null,
                }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 409);

        // Non-editable extension → 400.
        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/api/labs/lab/playbooks/file")
                .set_json(json!({
                    "playbook": "playbooks/base",
                    "path": "run.sh",
                    "content": "#!/bin/sh",
                    "base_rev": null,
                }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn save_refuses_symlinked_dirs() {
        let tmp = playbook_lab();
        // A symlinked dir inside the playbook pointing outside the lab.
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("playbooks/base/linked"))
            .unwrap();
        let app = app!(tmp.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/api/labs/lab/playbooks/file")
                .set_json(json!({
                    "playbook": "playbooks/base",
                    "path": "linked/escape.wcl",
                    "content": "x",
                    "base_rev": null,
                }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);
        assert!(!outside.path().join("escape.wcl").exists());
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
