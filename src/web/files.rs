//! Lab Files endpoints: a sandboxed file API over the whole lab directory
//! (the folder holding `vmlab.wcl`) — the web Files tab's backend.
//!
//! Sandbox contract: every path is a plain relative path with no hidden
//! (`.`-prefixed) segments — lab state under `.vmlab/` is invisible and
//! immutable here, matching the tree listing — and symlinks are never
//! traversed. `vmlab.wcl` itself is readable but cannot be written,
//! renamed, or deleted through this API: config writes go through the
//! validating `/config` endpoint so a broken lab file never lands silently.

use std::io::Write;
use std::path::{Component, Path, PathBuf};

use actix_web::{HttpResponse, web};
use serde::Deserialize;
use serde_json::json;

use super::api::fail;
use super::fsops::{FsError, MAX_FILE_BYTES, ensure_safe_parent, plain_relative, rev_of, walk_dir};
use super::state::AppState;

fn bad(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({"error": msg.into()}))
}

/// Validate a request path: plain relative and no hidden segments.
fn lab_rel<'a>(requested: &'a str, what: &str) -> Result<&'a Path, String> {
    let p = plain_relative(requested, what)?;
    let hidden = p.components().any(|c| match c {
        Component::Normal(part) => part.to_string_lossy().starts_with('.'),
        _ => true,
    });
    if hidden {
        return Err(format!(
            "{what} cannot contain hidden (dot-prefixed) segments"
        ));
    }
    Ok(p)
}

fn is_lab_file(rel: &Path) -> bool {
    rel == Path::new(vmlab::paths::LAB_FILE)
}

/// Resolve `rel` under `root`, walking segment by segment: every segment
/// must exist and must not be a symlink. No canonicalization — nothing is
/// ever resolved through a link.
fn resolve_existing(root: &Path, rel: &Path) -> Result<PathBuf, FsError> {
    let mut current = root.to_path_buf();
    for component in rel.components() {
        let Component::Normal(part) = component else {
            return Err(FsError::BadRequest(
                "path must be a plain relative path".into(),
            ));
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(FsError::BadRequest(
                    "path cannot traverse symbolic links".into(),
                ));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(FsError::NotFound(format!("{}: not found", rel.display())));
            }
            Err(e) => return Err(FsError::Io(e.to_string())),
        }
    }
    Ok(current)
}

// ---- tree -------------------------------------------------------------------

/// `GET /api/labs/{lab}/files/tree` — recursive listing of the lab root:
/// dirs first, hidden entries and symlinks skipped, capped.
pub async fn tree(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let outcome = web::block(move || {
        let mut count = 0usize;
        walk_dir(&root, Path::new(""), 0, &mut count)
    })
    .await;
    match outcome {
        Ok(Ok(entries)) => HttpResponse::Ok().json(json!({"entries": entries})),
        Ok(Err(e)) if e.contains("editor limit") => {
            HttpResponse::PayloadTooLarge().json(json!({"error": e}))
        }
        Ok(Err(e)) => HttpResponse::InternalServerError().json(json!({"error": e})),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

// ---- read -------------------------------------------------------------------

#[derive(Deserialize)]
pub struct PathQuery {
    path: String,
}

/// `GET /api/labs/{lab}/files/file?path=…` — read one file. Oversized or
/// non-UTF-8 files return metadata (`tooLarge`/`binary` + `size`) instead
/// of content so the client can render a placeholder.
pub async fn get_file(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    query: web::Query<PathQuery>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let rel = query.path.clone();
    let outcome = web::block(move || {
        let rel_path = lab_rel(&rel, "path").map_err(FsError::BadRequest)?;
        let path = resolve_existing(&root, rel_path)?;
        let meta = std::fs::metadata(&path).map_err(|e| FsError::Io(e.to_string()))?;
        if meta.is_dir() {
            return Err(FsError::BadRequest(format!("{rel} is a directory")));
        }
        if meta.len() as usize > MAX_FILE_BYTES {
            return Ok((rel, None, meta.len()));
        }
        let bytes = std::fs::read(&path).map_err(|e| FsError::Io(e.to_string()))?;
        let size = bytes.len() as u64;
        match String::from_utf8(bytes) {
            Ok(content) => Ok((rel, Some(content), size)),
            Err(_) => Ok((rel, None, size)),
        }
    })
    .await;
    match outcome {
        Ok(Ok((rel, Some(content), _))) => HttpResponse::Ok().json(json!({
            "path": rel,
            "rev": rev_of(&content),
            "content": content,
        })),
        Ok(Ok((rel, None, size))) if size as usize > MAX_FILE_BYTES => {
            HttpResponse::Ok().json(json!({"path": rel, "tooLarge": true, "size": size}))
        }
        Ok(Ok((rel, None, size))) => {
            HttpResponse::Ok().json(json!({"path": rel, "binary": true, "size": size}))
        }
        Ok(Err(e)) => e.respond(),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

// ---- write ------------------------------------------------------------------

#[derive(Deserialize)]
pub struct SaveBody {
    path: String,
    content: String,
    /// SHA-256 returned by `GET`; `None` means create without overwriting.
    base_rev: Option<String>,
}

enum FileSave {
    Saved { rev: String },
    Stale { rev: Option<String> },
    Invalid(String),
    Error(String),
}

/// `PUT /api/labs/{lab}/files/file` — revision-aware create/update, the
/// playbook editor's `save_file` contract scoped to the lab root. Parent
/// directories are created (symlink-refusing walk).
pub async fn save_file(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<SaveBody>,
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
        let rel = match lab_rel(&body.path, "path") {
            Ok(p) => p.to_path_buf(),
            Err(e) => return FileSave::Invalid(e),
        };
        if is_lab_file(&rel) {
            return FileSave::Invalid(
                "vmlab.wcl is edited through the config endpoint (it validates before writing)"
                    .into(),
            );
        }
        let target = root.join(&rel);
        let Some(parent) = target.parent() else {
            return FileSave::Invalid("path has no parent directory".into());
        };
        let canonical_parent = match ensure_safe_parent(&root, parent) {
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
        Ok(FileSave::Invalid(error)) => bad(error),
        Ok(FileSave::Error(error)) => {
            HttpResponse::InternalServerError().json(json!({"error": error}))
        }
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

// ---- mkdir ------------------------------------------------------------------

#[derive(Deserialize)]
pub struct MkdirBody {
    path: String,
}

/// `POST /api/labs/{lab}/files/mkdir` — create a directory (and missing
/// parents) inside the lab root.
pub async fn mkdir(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<MkdirBody>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let body = body.into_inner();
    let outcome = web::block(move || {
        let rel = lab_rel(&body.path, "path").map_err(FsError::BadRequest)?;
        let target = root.join(rel);
        if std::fs::symlink_metadata(&target).is_ok_and(|m| !m.is_dir()) {
            return Err(FsError::BadRequest(format!(
                "a file already exists at {}",
                body.path
            )));
        }
        // Creates every missing segment, refusing symlinks along the way.
        ensure_safe_parent(&root, &target).map_err(FsError::BadRequest)?;
        Ok(body.path)
    })
    .await;
    match outcome {
        Ok(Ok(path)) => HttpResponse::Ok().json(json!({"ok": true, "path": path})),
        Ok(Err(e)) => e.respond(),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

// ---- rename -----------------------------------------------------------------

#[derive(Deserialize)]
pub struct RenameBody {
    from: String,
    to: String,
}

/// `POST /api/labs/{lab}/files/rename` — rename/move a file or directory.
/// Never overwrites: an existing destination is a 409.
pub async fn rename(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<RenameBody>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let body = body.into_inner();
    let outcome = web::block(move || {
        let from = lab_rel(&body.from, "from")
            .map_err(FsError::BadRequest)?
            .to_path_buf();
        let to = lab_rel(&body.to, "to")
            .map_err(FsError::BadRequest)?
            .to_path_buf();
        if is_lab_file(&from) || is_lab_file(&to) {
            return Err(FsError::BadRequest(
                "vmlab.wcl cannot be renamed — the lab needs it where it is".into(),
            ));
        }
        if to.starts_with(&from) && to != from {
            return Err(FsError::BadRequest(
                "cannot move a directory inside itself".into(),
            ));
        }
        let source = resolve_existing(&root, &from)?;
        let Some(to_parent) = root.join(&to).parent().map(Path::to_path_buf) else {
            return Err(FsError::BadRequest("destination has no parent".into()));
        };
        let canonical_parent =
            ensure_safe_parent(&root, &to_parent).map_err(FsError::BadRequest)?;
        let dest = canonical_parent.join(
            to.file_name()
                .ok_or_else(|| FsError::BadRequest("destination has no file name".into()))?,
        );
        if std::fs::symlink_metadata(&dest).is_ok() {
            return Err(FsError::Forbidden(format!(
                "{} already exists — pick another name",
                body.to
            )));
        }
        std::fs::rename(&source, &dest).map_err(|e| FsError::Io(e.to_string()))?;
        Ok((body.from, body.to))
    })
    .await;
    match outcome {
        Ok(Ok((from, to))) => HttpResponse::Ok().json(json!({"ok": true, "from": from, "to": to})),
        Ok(Err(FsError::Forbidden(e))) => HttpResponse::Conflict().json(json!({"error": e})),
        Ok(Err(e)) => e.respond(),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

// ---- delete -----------------------------------------------------------------

#[derive(Deserialize)]
pub struct DeleteQuery {
    path: String,
    #[serde(default)]
    recursive: bool,
}

/// `DELETE /api/labs/{lab}/files/file?path=…&recursive=…` — delete a file
/// or directory. Non-empty directories require `recursive=true` (409
/// otherwise, so the client can confirm).
pub async fn delete(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    query: web::Query<DeleteQuery>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(root) => root,
        Err(e) => return fail(e),
    };
    let (rel, recursive) = (query.path.clone(), query.recursive);
    let outcome = web::block(move || {
        let rel_path = lab_rel(&rel, "path").map_err(FsError::BadRequest)?;
        if is_lab_file(rel_path) {
            return Err(FsError::BadRequest(
                "vmlab.wcl cannot be deleted — the lab needs it".into(),
            ));
        }
        let target = resolve_existing(&root, rel_path)?;
        let meta = std::fs::symlink_metadata(&target).map_err(|e| FsError::Io(e.to_string()))?;
        if meta.is_dir() {
            let empty = std::fs::read_dir(&target)
                .map_err(|e| FsError::Io(e.to_string()))?
                .next()
                .is_none();
            if empty {
                std::fs::remove_dir(&target).map_err(|e| FsError::Io(e.to_string()))?;
            } else if recursive {
                // remove_dir_all does not follow symlinks (they are removed,
                // not traversed).
                std::fs::remove_dir_all(&target).map_err(|e| FsError::Io(e.to_string()))?;
            } else {
                return Err(FsError::Forbidden(format!("{rel} is not empty")));
            }
        } else {
            std::fs::remove_file(&target).map_err(|e| FsError::Io(e.to_string()))?;
        }
        Ok(rel)
    })
    .await;
    match outcome {
        Ok(Ok(path)) => HttpResponse::Ok().json(json!({"ok": true, "path": path})),
        Ok(Err(FsError::Forbidden(e))) => HttpResponse::Conflict().json(json!({"error": e})),
        Ok(Err(e)) => e.respond(),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{App, test};
    use serde_json::Value;

    /// A tempdir lab: vmlab.wcl, a script, a playbook folder, a binary
    /// blob, and hidden state under `.vmlab/`.
    fn files_lab() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("vmlab.wcl"), "lab \"lab\" {}\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("scripts")).unwrap();
        std::fs::write(tmp.path().join("scripts/setup.ws"), "fn main() {}\n").unwrap();
        std::fs::create_dir_all(tmp.path().join("playbooks/base")).unwrap();
        std::fs::write(tmp.path().join("playbooks/base/playbook.wcl"), "x").unwrap();
        std::fs::write(tmp.path().join("logo.bin"), [0u8, 159, 146, 150]).unwrap();
        std::fs::create_dir_all(tmp.path().join(".vmlab")).unwrap();
        std::fs::write(tmp.path().join(".vmlab/state.json"), "{}").unwrap();
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
                    .route("/api/labs/{lab}/files/tree", web::get().to(tree))
                    .route("/api/labs/{lab}/files/file", web::get().to(get_file))
                    .route("/api/labs/{lab}/files/file", web::put().to(save_file))
                    .route("/api/labs/{lab}/files/file", web::delete().to(delete))
                    .route("/api/labs/{lab}/files/mkdir", web::post().to(mkdir))
                    .route("/api/labs/{lab}/files/rename", web::post().to(rename)),
            )
            .await
        };
    }

    #[actix_web::test]
    async fn tree_lists_lab_root_hidden_skipped() {
        let tmp = files_lab();
        let app = app!(tmp.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/files/tree")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        let names: Vec<&str> = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        // dirs first, then files; .vmlab absent.
        assert_eq!(names, vec!["playbooks", "scripts", "logo.bin", "vmlab.wcl"]);
    }

    #[actix_web::test]
    async fn read_rejects_traversal_hidden_and_dirs() {
        let tmp = files_lab();
        let app = app!(tmp.path());
        for uri in [
            "/api/labs/lab/files/file?path=..%2Fetc%2Fpasswd",
            "/api/labs/lab/files/file?path=%2Fetc%2Fpasswd",
            "/api/labs/lab/files/file?path=.vmlab%2Fstate.json",
            "/api/labs/lab/files/file?path=scripts",
        ] {
            let resp =
                test::call_service(&app, test::TestRequest::get().uri(uri).to_request()).await;
            assert_eq!(resp.status(), 400, "{uri}");
        }
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/files/file?path=nope.txt")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn read_reports_binary_and_too_large() {
        let tmp = files_lab();
        std::fs::write(tmp.path().join("big.txt"), "x".repeat(MAX_FILE_BYTES + 1)).unwrap();
        let app = app!(tmp.path());

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/files/file?path=logo.bin")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["binary"], true);
        assert_eq!(body["size"], 4);
        assert!(body.get("content").is_none());

        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/files/file?path=big.txt")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["tooLarge"], true);
    }

    #[actix_web::test]
    async fn save_create_read_stale_roundtrip() {
        let tmp = files_lab();
        let app = app!(tmp.path());

        // Create in a new subfolder (parents made on the fly).
        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/api/labs/lab/files/file")
                .set_json(json!({"path": "notes/todo.md", "content": "hi\n", "base_rev": null}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let created: Value = test::read_body_json(resp).await;
        let rev = created["rev"].as_str().unwrap().to_string();

        // Read back.
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/files/file?path=notes%2Ftodo.md")
                .to_request(),
        )
        .await;
        let doc: Value = test::read_body_json(resp).await;
        assert_eq!(doc["rev"].as_str().unwrap(), rev);
        assert_eq!(doc["content"], "hi\n");

        // Stale write → 409 with current rev; duplicate create → 409.
        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/api/labs/lab/files/file")
                .set_json(json!({"path": "notes/todo.md", "content": "x", "base_rev": "dead"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 409);
        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/api/labs/lab/files/file")
                .set_json(json!({"path": "notes/todo.md", "content": "x", "base_rev": null}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 409);
    }

    #[actix_web::test]
    async fn lab_file_is_protected() {
        let tmp = files_lab();
        let app = app!(tmp.path());

        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/api/labs/lab/files/file")
                .set_json(json!({"path": "vmlab.wcl", "content": "x", "base_rev": null}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);

        let resp = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri("/api/labs/lab/files/file?path=vmlab.wcl")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);

        for body in [
            json!({"from": "vmlab.wcl", "to": "other.wcl"}),
            json!({"from": "logo.bin", "to": "vmlab.wcl"}),
        ] {
            let resp = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/api/labs/lab/files/rename")
                    .set_json(body)
                    .to_request(),
            )
            .await;
            assert_eq!(resp.status(), 400);
        }

        // Reading it is fine.
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/files/file?path=vmlab.wcl")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
    }

    #[actix_web::test]
    async fn mkdir_and_file_in_the_way() {
        let tmp = files_lab();
        let app = app!(tmp.path());

        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/files/mkdir")
                .set_json(json!({"path": "assets/img"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert!(tmp.path().join("assets/img").is_dir());

        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/files/mkdir")
                .set_json(json!({"path": "logo.bin"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn rename_moves_and_never_overwrites() {
        let tmp = files_lab();
        let app = app!(tmp.path());

        // Move into a new folder — parents created.
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/files/rename")
                .set_json(json!({"from": "scripts/setup.ws", "to": "provision/main.ws"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert!(tmp.path().join("provision/main.ws").is_file());
        assert!(!tmp.path().join("scripts/setup.ws").exists());

        // Existing destination → 409.
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/files/rename")
                .set_json(json!({"from": "provision/main.ws", "to": "logo.bin"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 409);

        // Missing source → 404; dir into itself → 400.
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/files/rename")
                .set_json(json!({"from": "ghost.txt", "to": "real.txt"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 404);
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/files/rename")
                .set_json(json!({"from": "playbooks", "to": "playbooks/inner"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);

        // Directory rename works.
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/files/rename")
                .set_json(json!({"from": "playbooks", "to": "runbooks"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert!(tmp.path().join("runbooks/base/playbook.wcl").is_file());
    }

    #[actix_web::test]
    async fn delete_file_and_dir_semantics() {
        let tmp = files_lab();
        let app = app!(tmp.path());

        let resp = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri("/api/labs/lab/files/file?path=logo.bin")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert!(!tmp.path().join("logo.bin").exists());

        // Non-empty dir without recursive → 409, still there.
        let resp = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri("/api/labs/lab/files/file?path=playbooks")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 409);
        assert!(tmp.path().join("playbooks").is_dir());

        // With recursive → gone.
        let resp = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri("/api/labs/lab/files/file?path=playbooks&recursive=true")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert!(!tmp.path().join("playbooks").exists());

        // Empty dir deletes without the flag.
        std::fs::create_dir(tmp.path().join("empty")).unwrap();
        let resp = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri("/api/labs/lab/files/file?path=empty")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
    }

    #[actix_web::test]
    async fn symlinks_are_refused_everywhere() {
        let tmp = files_lab();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s").unwrap();
        std::os::unix::fs::symlink(outside.path(), tmp.path().join("linked")).unwrap();
        let app = app!(tmp.path());

        // Read through the link, write through it, delete it via traversal.
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/files/file?path=linked%2Fsecret.txt")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);
        let resp = test::call_service(
            &app,
            test::TestRequest::put()
                .uri("/api/labs/lab/files/file")
                .set_json(json!({"path": "linked/escape.txt", "content": "x", "base_rev": null}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);
        assert!(!outside.path().join("escape.txt").exists());
        let resp = test::call_service(
            &app,
            test::TestRequest::delete()
                .uri("/api/labs/lab/files/file?path=linked%2Fsecret.txt")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 400);
        assert!(outside.path().join("secret.txt").exists());
    }
}
