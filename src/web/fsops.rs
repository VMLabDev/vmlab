//! Shared filesystem plumbing for the web editors' sandboxed file APIs
//! (the playbook folder editor, the lab Files tab): lexical path checks,
//! symlink-refusing walks, capped tree listings, and content revision
//! hashing for optimistic concurrency.

use std::path::{Component, Path, PathBuf};

use actix_web::HttpResponse;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub(crate) const MAX_FILE_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_TREE_ENTRIES: usize = 2000;
pub(crate) const MAX_TREE_DEPTH: usize = 16;

pub(crate) fn rev_of(content: &str) -> String {
    hex::encode(Sha256::digest(content.as_bytes()))
}

pub(crate) enum FsError {
    BadRequest(String),
    Forbidden(String),
    NotFound(String),
    Io(String),
}

impl FsError {
    pub(crate) fn respond(self) -> HttpResponse {
        match self {
            FsError::BadRequest(e) => HttpResponse::BadRequest().json(json!({"error": e})),
            FsError::Forbidden(e) => HttpResponse::Forbidden().json(json!({"error": e})),
            FsError::NotFound(e) => HttpResponse::NotFound().json(json!({"error": e})),
            FsError::Io(e) => HttpResponse::InternalServerError().json(json!({"error": e})),
        }
    }
}

/// Lexical shape check shared by all path params: relative, plain
/// components only (no `..`, no roots), non-empty.
pub(crate) fn plain_relative<'a>(requested: &'a str, what: &str) -> Result<&'a Path, String> {
    let p = Path::new(requested);
    if p.as_os_str().is_empty()
        || p.is_absolute()
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err(format!("{what} must be a plain relative path"));
    }
    Ok(p)
}

/// Recursive listing: dirs first, hidden entries skipped, symlinks skipped,
/// capped by depth and total entry count (cap errors keep the literal
/// "editor limit" substring — the tree handlers map it to 413).
pub(crate) fn walk_dir(
    dir: &Path,
    rel: &Path,
    depth: usize,
    count: &mut usize,
) -> Result<Vec<Value>, String> {
    if depth > MAX_TREE_DEPTH {
        return Err(format!(
            "tree deeper than {MAX_TREE_DEPTH} levels — exceeds the editor limit"
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
                "tree exceeds {MAX_TREE_ENTRIES} entries — exceeds the editor limit"
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

/// Walk `parent` down from the sandbox base `base`, refusing symlinked
/// segments and creating missing directories, with a final canonical
/// prefix re-check.
pub(crate) fn ensure_safe_parent(base: &Path, parent: &Path) -> Result<PathBuf, String> {
    let canonical_base = std::fs::canonicalize(base).map_err(|e| e.to_string())?;
    let relative = parent
        .strip_prefix(base)
        .map_err(|_| "path escapes the sandbox".to_string())?;
    let mut current = base.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err("path escapes the sandbox".into());
        };
        current.push(part);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("directories cannot be symbolic links".into());
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
        return Err("path escapes the sandbox".into());
    }
    Ok(canonical_parent)
}
