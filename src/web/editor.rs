//! Structured lab-model endpoints for the visual editor: read the parsed
//! lab as JSON (block spans double as edit addresses) and apply surgical
//! edit operations to `vmlab.wcl`.
//!
//! Staleness contract: spans are byte offsets into an exact file revision,
//! so every edit carries the `rev` (SHA-256 of the file bytes) it was
//! computed against; a mismatch is a 409 and the client re-fetches. The
//! success response returns the fresh model + rev, so the client re-syncs
//! addresses in one round trip.

use actix_web::{HttpResponse, web};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use vmlab::cli::validate::{SourceIssue, validate_source};
use vmlab::config::{self, dto::LabModelDto, edit_ops};

use super::state::AppState;

fn rev_of(source: &str) -> String {
    hex::encode(Sha256::digest(source.as_bytes()))
}

fn issue_json(i: &SourceIssue) -> Value {
    json!({"message": i.message, "line": i.line})
}

/// `{lab, templates}` for a parsed lab file.
fn model_json(file: &config::LabFile) -> Value {
    serde_json::to_value(LabModelDto::from(file)).unwrap_or(Value::Null)
}

enum ModelOutcome {
    Ok {
        path: String,
        rev: String,
        model: Value,
    },
    /// The file exists but doesn't parse/extract — the editor falls back to
    /// the raw config page.
    Issues(Vec<Value>),
    Missing(String),
    Io(String),
}

/// `GET /api/labs/{lab}/model` — the full parsed lab document as JSON.
pub async fn get_model(state: web::Data<AppState>, lab: web::Path<String>) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(r) => r,
        Err(e) => return super::api::fail(e),
    };

    let outcome = web::block(move || {
        let path = root.join(vmlab::paths::LAB_FILE);
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return ModelOutcome::Missing(format!("{}: not found", path.display()));
            }
            Err(e) => return ModelOutcome::Io(e.to_string()),
        };
        let rev = rev_of(&source);
        match config::load_lab_source(&source, &path.display().to_string(), &root) {
            Ok(file) => ModelOutcome::Ok {
                path: path.display().to_string(),
                rev,
                model: model_json(&file),
            },
            Err(errs) => ModelOutcome::Issues(
                errs.issues
                    .iter()
                    .map(|i| issue_json(&SourceIssue::from_issue(&source, i)))
                    .collect(),
            ),
        }
    })
    .await;

    match outcome {
        Ok(ModelOutcome::Ok { path, rev, model }) => HttpResponse::Ok().json(json!({
            "path": path,
            "rev": rev,
            "lab": model["lab"],
            "templates": model["templates"],
        })),
        Ok(ModelOutcome::Issues(issues)) => {
            HttpResponse::UnprocessableEntity().json(json!({"issues": issues}))
        }
        Ok(ModelOutcome::Missing(e)) => HttpResponse::NotFound().json(json!({"error": e})),
        Ok(ModelOutcome::Io(e)) => HttpResponse::InternalServerError().json(json!({"error": e})),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}

#[derive(Deserialize)]
pub struct EditBody {
    /// SHA-256 of the file bytes the ops' spans were computed against.
    base_rev: String,
    /// Validate the edited document without writing it.
    #[serde(default)]
    validate_only: bool,
    ops: Vec<edit_ops::Op>,
}

enum EditOutcome {
    Saved {
        rev: String,
        model: Value,
        source: String,
    },
    Valid {
        source: String,
    },
    Stale {
        rev: String,
    },
    OpFail(String),
    Invalid {
        issues: Vec<Value>,
        source: String,
    },
    Missing(String),
    Io(String),
}

/// `POST /api/labs/{lab}/model/edit` `{base_rev, validate_only?, ops}` —
/// apply a batch of surgical ops, validate the result, and (unless
/// `validate_only`) write it. Mirrors `save_config`'s guarantee: a running
/// daemon never inherits a broken config, because validation failures leave
/// the on-disk file untouched.
pub async fn edit_model(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<EditBody>,
) -> HttpResponse {
    let root = match state.lab_root(&lab).await {
        Ok(r) => r,
        Err(e) => return super::api::fail(e),
    };
    let body = body.into_inner();

    let outcome = web::block(move || {
        let path = root.join(vmlab::paths::LAB_FILE);
        let source = match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return EditOutcome::Missing(format!("{}: not found", path.display()));
            }
            Err(e) => return EditOutcome::Io(e.to_string()),
        };
        let rev = rev_of(&source);
        if rev != body.base_rev {
            return EditOutcome::Stale { rev };
        }
        let new_source = match edit_ops::apply_ops(&source, &body.ops) {
            Ok(s) => s,
            Err(e) => return EditOutcome::OpFail(e.to_string()),
        };
        if let Err(issues) = validate_source(&new_source, &root) {
            return EditOutcome::Invalid {
                issues: issues.iter().map(issue_json).collect(),
                source: new_source,
            };
        }
        if body.validate_only {
            return EditOutcome::Valid { source: new_source };
        }
        if let Err(e) = std::fs::write(&path, &new_source) {
            return EditOutcome::Io(e.to_string());
        }
        // Validation succeeded, so this reload can't fail in practice; the
        // fallback just degrades to "re-fetch the model".
        match config::load_lab_source(&new_source, &path.display().to_string(), &root) {
            Ok(file) => EditOutcome::Saved {
                rev: rev_of(&new_source),
                model: model_json(&file),
                source: new_source,
            },
            Err(_) => EditOutcome::Io("saved, but re-reading the model failed".into()),
        }
    })
    .await;

    match outcome {
        Ok(EditOutcome::Saved { rev, model, source }) => HttpResponse::Ok().json(json!({
            "ok": true,
            "rev": rev,
            "lab": model["lab"],
            "templates": model["templates"],
            "source": source,
        })),
        Ok(EditOutcome::Valid { source }) => {
            HttpResponse::Ok().json(json!({"ok": true, "source": source}))
        }
        Ok(EditOutcome::Stale { rev }) => HttpResponse::Conflict().json(json!({
            "error": "config changed on disk — reload the editor",
            "rev": rev,
        })),
        Ok(EditOutcome::OpFail(e)) => HttpResponse::BadRequest().json(json!({"error": e})),
        Ok(EditOutcome::Invalid { issues, source }) => {
            HttpResponse::UnprocessableEntity().json(json!({"issues": issues, "source": source}))
        }
        Ok(EditOutcome::Missing(e)) => HttpResponse::NotFound().json(json!({"error": e})),
        Ok(EditOutcome::Io(e)) => HttpResponse::InternalServerError().json(json!({"error": e})),
        Err(e) => HttpResponse::InternalServerError().json(json!({"error": e.to_string()})),
    }
}
