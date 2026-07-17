//! Surgical edit operations over a lab's `vmlab.wcl` source, used by the web
//! visual editor. Each operation addresses a block by the byte span the
//! extracted model reports (`model::Span`); the AST is mutated via
//! `wcl_lang::edit` and re-printed with `wcl_lang::format::to_source`, so
//! hand-written comments and blank lines survive (other layout is
//! canonicalised by the printer).
//!
//! Span addresses are only meaningful against the exact source bytes the
//! model was extracted from — the caller enforces that (the web handler
//! hashes the file and rejects stale revisions with 409).

use serde::Deserialize;
use serde_json::Value;
use wcl_lang::ast::{self, Expr, Item};
use wcl_lang::{NumberLit, edit, format, parse_for_edit};

use super::model::Span;

/// One surgical operation. Spans are `[start, end]` byte offsets into the
/// base document.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// Set (or insert) a `name = value` field on the block at `block`.
    SetField {
        block: Span,
        name: String,
        value: Value,
    },
    /// Remove a field from the block at `block` (no-op if absent, so
    /// clearing an already-unset optional is not an error).
    RemoveField { block: Span, name: String },
    /// Set an inline-label slot (e.g. a vm/segment name). Renaming the
    /// `lab` block itself is rejected — the lab name is the daemon/URL
    /// identity.
    SetLabel {
        block: Span,
        slot: usize,
        value: String,
    },
    /// Add a new block. Placement: after the sibling at `after` when given,
    /// else appended inside `parent`, else appended at top level.
    AddBlock {
        #[serde(default)]
        parent: Option<Span>,
        #[serde(default)]
        after: Option<Span>,
        block: BlockSpec,
    },
    /// Remove the block at `block`.
    RemoveBlock { block: Span },
    /// Swap the block at `block` with its adjacent block sibling.
    MoveBlock { block: Span, down: bool },
}

/// A new block to create: kind, inline labels, fields (in order), and nested
/// child blocks. Recursive so a whole `vm` with its `nic`s arrives as one op
/// (new blocks have no span yet, so they can't be addressed within the same
/// batch).
#[derive(Debug, Deserialize)]
pub struct BlockSpec {
    pub kind: String,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub fields: Vec<FieldSpec>,
    #[serde(default)]
    pub children: Vec<BlockSpec>,
}

/// An ordered field for [`BlockSpec`] (a JSON array of `{name, value}` pairs
/// keeps declaration order, unlike a JSON object).
#[derive(Debug, Deserialize)]
pub struct FieldSpec {
    pub name: String,
    pub value: Value,
}

/// An operation that could not be applied. `index` is the position in the
/// batch (absent for a source parse failure).
#[derive(Debug)]
pub struct OpError {
    pub index: Option<usize>,
    pub message: String,
}

impl std::fmt::Display for OpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.index {
            Some(i) => write!(f, "op {i}: {}", self.message),
            None => write!(f, "{}", self.message),
        }
    }
}

impl std::error::Error for OpError {}

/// Apply `ops` to `source` and return the re-printed document. Nothing is
/// validated here beyond the ops themselves — the caller runs the full
/// config validation on the result before writing it anywhere.
pub fn apply_ops(source: &str, ops: &[Op]) -> Result<String, OpError> {
    let mut src = parse_for_edit(source, crate::paths::LAB_FILE).map_err(|e| OpError {
        index: None,
        message: format!("parse error: {e}"),
    })?;
    for (i, op) in ops.iter().enumerate() {
        apply(&mut src, op).map_err(|message| OpError {
            index: Some(i),
            message,
        })?;
    }
    Ok(format::to_source(&src))
}

fn apply(src: &mut ast::Source, op: &Op) -> Result<(), String> {
    match op {
        Op::SetField { block, name, value } => {
            let b = block_at(src, *block)?;
            edit::set_or_insert_field(b, name, json_to_expr(value)?);
            Ok(())
        }
        Op::RemoveField { block, name } => {
            let b = block_at(src, *block)?;
            b.items
                .retain(|it| !matches!(it, Item::Field(f) if f.name == *name));
            Ok(())
        }
        Op::SetLabel { block, slot, value } => {
            let b = block_at(src, *block)?;
            if b.kind == "lab" {
                return Err("renaming the lab is not supported".into());
            }
            if !edit::set_label(b, *slot, edit::string_literal_expr(value)) {
                return Err(format!("label slot {slot} is out of range"));
            }
            Ok(())
        }
        Op::AddBlock {
            parent,
            after,
            block,
        } => {
            let new = build_spec(block)?;
            if let Some(after) = after {
                if !edit::insert_block_after_span(&mut src.items, ast_span(*after), new) {
                    return Err(format!("no block at span {after:?} to insert after"));
                }
                return Ok(());
            }
            match parent {
                Some(span) => {
                    let p = block_at(src, *span)?;
                    p.items.push(Item::Block(new));
                }
                None => edit::append_top_level_block(src, new),
            }
            Ok(())
        }
        Op::RemoveBlock { block } => {
            if !edit::remove_block_by_span(&mut src.items, ast_span(*block)) {
                return Err(format!("no block at span {block:?}"));
            }
            Ok(())
        }
        Op::MoveBlock { block, down } => {
            if !edit::move_block_by_span(&mut src.items, ast_span(*block), *down) {
                return Err(format!(
                    "cannot move block at span {block:?} (not found, or already at the edge)"
                ));
            }
            Ok(())
        }
    }
}

fn ast_span(s: Span) -> ast::Span {
    ast::Span::new(s.0, s.1)
}

fn block_at(src: &mut ast::Source, span: Span) -> Result<&mut ast::Block, String> {
    edit::find_block_by_span(&mut src.items, ast_span(span))
        .ok_or_else(|| format!("no block at span {span:?} — the document changed underneath"))
}

fn build_spec(spec: &BlockSpec) -> Result<ast::Block, String> {
    let labels = spec
        .labels
        .iter()
        .map(|l| edit::string_literal_expr(l))
        .collect();
    let fields = spec
        .fields
        .iter()
        .map(|f| Ok((f.name.clone(), json_to_expr(&f.value)?)))
        .collect::<Result<Vec<_>, String>>()?;
    let mut block = edit::build_block(&spec.kind, &[], labels, fields);
    for child in &spec.children {
        block.items.push(Item::Block(build_spec(child)?));
    }
    Ok(block)
}

/// JSON value → WCL expression. Objects must be `{num, unit}` and become
/// unit literals (`memory = 8GiB`) so byte sizes stay readable in the file.
fn json_to_expr(v: &Value) -> Result<Expr, String> {
    match v {
        Value::String(s) => Ok(edit::string_literal_expr(s)),
        Value::Bool(b) => Ok(Expr::Bool(*b)),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(Expr::I64(i))
            } else if let Some(f) = n.as_f64() {
                Ok(Expr::F64(f))
            } else {
                Err(format!("unrepresentable number `{n}`"))
            }
        }
        Value::Array(items) => Ok(Expr::ListLit {
            elements: items
                .iter()
                .map(json_to_expr)
                .collect::<Result<Vec<_>, _>>()?,
            elem_trivia: Vec::new(),
            trailing_trivia: Vec::new(),
            span: ast::Span::new(0, 0),
        }),
        Value::Object(map) => {
            let num = map.get("num").and_then(Value::as_i64);
            let unit = map.get("unit").and_then(Value::as_str);
            match (num, unit) {
                (Some(n), Some(u)) if !u.is_empty() => Ok(Expr::UnitLiteral {
                    value: NumberLit::I64(n),
                    unit: u.to_string(),
                    span: ast::Span::new(0, 0),
                }),
                _ => Err("object values must be {num: <int>, unit: <suffix>}".into()),
            }
        }
        Value::Null => Err("null is not a value — use remove_field to unset".into()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;
    use crate::config::load_lab_source;

    const SRC: &str = r#"import <vmlab.wcl>

// topology for the demo lab
lab "demo" {

  segment "corp" {
    subnet = "10.50.0.0/24" // corp subnet
  }

  vm "dc01" {
    template = "x86_64/windows-server-2025"
    cpus     = 4
    memory   = 8GiB
    nic { segment = "corp" ip = "10.50.0.10" }
  }

  vm "client01" {
    template   = "x86_64/windows-11"
    depends_on = ["dc01"]
    nic { segment = "corp" }
  }
}
"#;

    /// Parse SRC and return (lab, reloaded model) so tests can take real spans.
    fn model() -> crate::config::model::Lab {
        load_lab_source(SRC, "<test>", Path::new("/tmp"))
            .unwrap()
            .lab
    }

    /// Ops must produce output that still extracts cleanly.
    fn reload(out: &str) -> crate::config::model::Lab {
        load_lab_source(out, "<test>", Path::new("/tmp"))
            .unwrap()
            .lab
    }

    fn op(v: Value) -> Op {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn set_field_updates_and_inserts() {
        let lab = model();
        let vm = lab.vms[0].span;
        let out = apply_ops(
            SRC,
            &[
                op(json!({"op": "set_field", "block": vm, "name": "cpus", "value": 8})),
                op(json!({"op": "set_field", "block": vm, "name": "gui", "value": true})),
                op(json!({"op": "set_field", "block": vm, "name": "memory",
                          "value": {"num": 16, "unit": "GiB"}})),
            ],
        )
        .unwrap();
        assert!(out.contains("cpus = 8"), "{out}");
        assert!(out.contains("gui = true"), "{out}");
        assert!(out.contains("memory = 16GiB"), "{out}");
        let re = reload(&out);
        assert_eq!(re.vms[0].cpus, Some(8));
        assert_eq!(re.vms[0].memory, Some(16 << 30));
        // Comments survive the round trip: the standalone one before the lab
        // block and the trailing one on the subnet field (the printer
        // normalises the delimiter to `#`).
        assert!(out.contains("# topology for the demo lab"), "{out}");
        assert!(out.contains("# corp subnet"), "{out}");
    }

    #[test]
    fn set_field_accepts_lists() {
        let lab = model();
        let vm = lab.vms[1].span;
        let out = apply_ops(
            SRC,
            &[op(json!({"op": "set_field", "block": vm,
                        "name": "depends_on", "value": ["dc01", "client01"]}))],
        )
        .unwrap();
        // client01 can't depend on itself — but that's validation's job, not
        // the op layer's; the text must simply round-trip.
        assert!(
            out.contains(r#"depends_on = ["dc01", "client01"]"#),
            "{out}"
        );
    }

    #[test]
    fn remove_field_unsets_and_tolerates_absent() {
        let lab = model();
        let vm = lab.vms[0].span;
        let out = apply_ops(
            SRC,
            &[
                op(json!({"op": "remove_field", "block": vm, "name": "cpus"})),
                op(json!({"op": "remove_field", "block": vm, "name": "not_there"})),
            ],
        )
        .unwrap();
        let re = reload(&out);
        assert_eq!(re.vms[0].cpus, None);
    }

    #[test]
    fn set_label_renames_but_not_the_lab() {
        let lab = model();
        let vm = lab.vms[0].span;
        let out = apply_ops(
            SRC,
            &[op(
                json!({"op": "set_label", "block": vm, "slot": 0, "value": "dc02"}),
            )],
        )
        .unwrap();
        assert_eq!(reload(&out).vms[0].name, "dc02");

        let err = apply_ops(
            SRC,
            &[op(
                json!({"op": "set_label", "block": lab.span, "slot": 0, "value": "other"}),
            )],
        )
        .unwrap_err();
        assert!(err.message.contains("renaming the lab"), "{err}");
    }

    #[test]
    fn add_block_nested_spec() {
        let lab = model();
        let out = apply_ops(
            SRC,
            &[op(json!({"op": "add_block", "parent": lab.span, "block": {
                "kind": "vm",
                "labels": ["web01"],
                "fields": [
                    {"name": "template", "value": "x86_64/linux-modern"},
                    {"name": "memory", "value": {"num": 2, "unit": "GiB"}},
                ],
                "children": [
                    {"kind": "nic", "fields": [{"name": "segment", "value": "corp"}]},
                    {"kind": "disk", "labels": ["data"],
                     "fields": [{"name": "size", "value": {"num": 10, "unit": "GiB"}}]},
                ],
            }}))],
        )
        .unwrap();
        let re = reload(&out);
        let web = re.vms.iter().find(|v| v.name == "web01").unwrap();
        assert_eq!(web.memory, Some(2 << 30));
        assert_eq!(web.nics.len(), 1);
        assert_eq!(web.nics[0].segment.as_deref(), Some("corp"));
        assert_eq!(web.extra_disks.len(), 1);
        assert_eq!(web.extra_disks[0].name, "data");
    }

    #[test]
    fn add_block_playbook_round_trips() {
        let lab = model();
        let out = apply_ops(
            SRC,
            &[op(json!({"op": "add_block", "parent": lab.span, "block": {
                "kind": "playbook",
                "labels": ["playbooks/base"],
                "fields": [
                    {"name": "play", "value": "base"},
                    {"name": "vms", "value": ["dc01"]},
                ],
            }}))],
        )
        .unwrap();
        let re = reload(&out);
        assert_eq!(re.playbooks.len(), 1);
        assert_eq!(re.playbooks[0].path.display().to_string(), "playbooks/base");
        assert_eq!(re.playbooks[0].play, "base");
        assert_eq!(re.playbooks[0].vms, vec!["dc01".to_string()]);
    }

    #[test]
    fn add_block_container_with_nested_children() {
        let lab = model();
        let out = apply_ops(
            SRC,
            &[op(json!({"op": "add_block", "parent": lab.span, "block": {
                "kind": "container",
                "labels": ["web"],
                "fields": [
                    {"name": "image", "value": "nginx:1.27"},
                    {"name": "memory", "value": {"num": 256, "unit": "MiB"}},
                    {"name": "restart", "value": "always"},
                ],
                "children": [
                    {"kind": "nic", "fields": [{"name": "segment", "value": "corp"}]},
                    {"kind": "env", "fields": [
                        {"name": "name", "value": "MODE"},
                        {"name": "value", "value": "prod"},
                    ]},
                    {"kind": "port", "fields": [
                        {"name": "host", "value": 18080},
                        {"name": "container", "value": 80},
                    ]},
                ],
            }}))],
        )
        .unwrap();
        let re = reload(&out);
        let web = re.containers.iter().find(|c| c.name == "web").unwrap();
        assert_eq!(web.image.reference, "nginx:1.27");
        assert_eq!(web.memory, Some(256 << 20));
        assert_eq!(web.restart, crate::config::model::RestartPolicy::Always);
        assert_eq!(web.nics.len(), 1);
        assert_eq!(web.env.len(), 1);
        assert_eq!(web.ports[0].host_port, 18080);
    }

    #[test]
    fn add_block_after_sibling_orders() {
        let lab = model();
        let first_vm = lab.vms[0].span;
        let out = apply_ops(
            SRC,
            &[op(json!({"op": "add_block", "after": first_vm, "block": {
                "kind": "vm", "labels": ["mid"],
                "fields": [{"name": "template", "value": "x86_64/linux-modern"}],
            }}))],
        )
        .unwrap();
        let re = reload(&out);
        let names: Vec<&str> = re.vms.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["dc01", "mid", "client01"]);
    }

    #[test]
    fn remove_block_drops_a_vm_and_a_nic() {
        let lab = model();
        let out = apply_ops(
            SRC,
            &[
                op(json!({"op": "remove_block", "block": lab.vms[1].span})),
                op(json!({"op": "remove_block", "block": lab.vms[0].nics[0].span})),
            ],
        )
        .unwrap();
        let re = reload(&out);
        assert_eq!(re.vms.len(), 1);
        assert!(re.vms[0].nics.is_empty());
    }

    #[test]
    fn move_block_reorders_siblings() {
        let lab = model();
        let out = apply_ops(
            SRC,
            &[op(
                json!({"op": "move_block", "block": lab.vms[1].span, "down": false}),
            )],
        )
        .unwrap();
        let re = reload(&out);
        let names: Vec<&str> = re.vms.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, ["client01", "dc01"]);
    }

    #[test]
    fn stale_span_is_an_error_with_index() {
        let err = apply_ops(
            SRC,
            &[op(
                json!({"op": "set_field", "block": [1, 2], "name": "cpus", "value": 1}),
            )],
        )
        .unwrap_err();
        assert_eq!(err.index, Some(0));
        assert!(err.message.contains("no block at span"), "{err}");
    }

    #[test]
    fn edit_after_remove_of_same_block_fails() {
        let lab = model();
        let vm = lab.vms[0].span;
        let err = apply_ops(
            SRC,
            &[
                op(json!({"op": "remove_block", "block": vm})),
                op(json!({"op": "set_field", "block": vm, "name": "cpus", "value": 2})),
            ],
        )
        .unwrap_err();
        assert_eq!(err.index, Some(1));
    }

    #[test]
    fn null_values_are_rejected() {
        let lab = model();
        let err = apply_ops(
            SRC,
            &[op(
                json!({"op": "set_field", "block": lab.vms[0].span, "name": "cpus", "value": null}),
            )],
        )
        .unwrap_err();
        assert!(err.message.contains("remove_field"), "{err}");
    }
}
