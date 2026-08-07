//! The Schema projection (ADR-0005): one reflected description of
//! `schema.wcl` — every block, field, type, optionality, default, doc string,
//! enum option list, nesting and cardinality.
//!
//! `schema.wcl` is the single source of truth for the shape of `vmlab.wcl`.
//! Every surface that needs that shape reads this projection instead of
//! restating it: the console's pickers (`/api/catalog/meta`), the designer's
//! inspector forms (see [`super::designer`], which renders the console's form
//! tables from here), and the rendered schema reference (which reflects the
//! same file through WCL's `type_table` in the wskill).
//!
//! Reflection is the same machinery the wskill's reference uses — WCL's
//! declaration views — reached from Rust rather than from WCL source. Nothing
//! here is hand-maintained: adding a field to `schema.wcl` adds it here.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use serde::Serialize;
use wcl_lang::{BuiltinType, DeclName, Document, TypeRef, Value};

/// How a field's value is written, which is what a form control is chosen
/// from. `Enum` and `Symbol` both carry a closed option list; they differ in
/// how the value is spelled in WCL (`"tcp"` vs `:workload`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    /// `utf8`
    Text,
    /// `i64`
    Int,
    /// `bool`
    Bool,
    /// `std.ByteSize`
    ByteSize,
    /// `std.Duration`
    Duration,
    /// `list<utf8>`
    TextList,
    /// `utf8` narrowed by `@options([…])`
    Enum,
    /// A `symbol_set`-typed field; values are written `:like_this`
    Symbol,
    /// A `@child` / `@children` slot — a nested block, not a scalar
    Block,
    /// A shape the projection does not classify (there are none today; the
    /// variant keeps an unrecognised type from being silently mistyped)
    Unknown,
}

/// A nested-block slot: which kind it holds, and how many.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChildSlot {
    /// The nested block's kind, e.g. `"nic"`.
    pub kind: String,
    /// `@children` (a list) rather than `@child` (at most one).
    pub repeated: bool,
    /// `@children(min = N)`.
    pub min: Option<u64>,
    /// `@children(max = N)`.
    pub max: Option<u64>,
}

/// A `@one_of([…])` rule: at least one of the named fields must be set, and
/// unless the rule says otherwise, no more than one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequiredGroup {
    /// The field names the rule spans.
    pub fields: Vec<String>,
    /// Setting more than one is an error. `volume`'s host/name is exclusive;
    /// `disk`'s size/from is not — a disk may carry both.
    pub exclusive: bool,
}

/// One field of one block, as the schema declares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Field {
    /// The field name as written in `vmlab.wcl`.
    pub name: String,
    /// The field's `@doc("…")` text — the single source for help prose.
    pub doc: String,
    pub ty: FieldType,
    /// `false` when the schema requires the field (`name: utf8`).
    pub optional: bool,
    /// The schema default, rendered as WCL source (`"tcp"`, `10s`, `256MiB`)
    /// when the field declares one. Absent is not the same as `false`/`""`:
    /// a field with no default simply stays unset.
    pub default: Option<String>,
    /// The same default as a number in the field's base unit — seconds for a
    /// `Duration`, bytes for a `ByteSize`, the value itself for an integer —
    /// for the surfaces that need to compute with it rather than show it.
    pub default_number: Option<i64>,
    /// The closed set of accepted values, for `Enum` and `Symbol` fields.
    pub options: Vec<String>,
    /// Inclusive `@range(min[, max])` lower bound. On a `Duration` field the
    /// bound is whole seconds — the unit a form edits in.
    pub min: Option<i64>,
    /// Inclusive `@range(min, max)` upper bound.
    pub max: Option<i64>,
    /// `@inline(N)`: the field is written as the block's Nth label rather
    /// than as `name = value`.
    pub label_slot: Option<u64>,
    /// Set when the field is a nested-block slot (`ty == FieldType::Block`).
    pub child: Option<ChildSlot>,
}

/// One decorator the schema declares (`@decorator("name")`), as WCL validates
/// it: where it may be written, how often, and the arguments it takes.
///
/// A decorator declares its arguments as a block declares its fields: same
/// types, same `@doc`, `@default` and `@options`. Each argument projects to
/// the same [`Field`], so a surface renders it with the machinery it has.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Decorator {
    /// The spelling in a WCL file, without the `@`, e.g. `"one_of"`.
    pub name: String,
    /// The WCL type declaring it, e.g. `"RequiredGroup"`.
    pub type_name: String,
    /// The type's doc comment (the `//` lines above it).
    pub doc: String,
    /// The `@applies_to(on = […])` positions, e.g. `["block"]`. Empty when the
    /// declaration names none — the decorator is then legal in every position.
    pub positions: Vec<String>,
    /// The `@applies_to(kinds = […])` block kinds. Empty means every kind.
    pub kinds: Vec<String>,
    /// `@decorator(…, repeatable = true)`: may be written more than once on
    /// one node. Otherwise it may appear at most once.
    pub repeatable: bool,
    /// The declared arguments, in declaration order.
    pub args: Vec<Field>,
}

impl Decorator {
    pub fn arg(&self, name: &str) -> Option<&Field> {
        self.args.iter().find(|a| a.name == name)
    }
}

/// One block kind, as the schema declares it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Block {
    /// The keyword written in `vmlab.wcl`, e.g. `"vm"`.
    pub kind: String,
    /// The WCL type name backing the block, e.g. `"Vm"`.
    pub type_name: String,
    /// The type's doc comment (the `//` lines above it).
    pub doc: String,
    pub fields: Vec<Field>,
    /// `@one_of([…])` rules over this block's fields.
    pub required_groups: Vec<RequiredGroup>,
    /// The names of the decorators an author may write on a block of this
    /// kind, in schema declaration order. WCL decides applicability from each
    /// declaration's `@applies_to`; this only reflects its answer.
    pub decorators: Vec<String>,
}

impl Block {
    pub fn field(&self, name: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.name == name)
    }

    /// The block kinds that may nest inside this one, in declaration order.
    pub fn child_kinds(&self) -> impl Iterator<Item = &str> {
        self.fields
            .iter()
            .filter_map(|f| f.child.as_ref())
            .map(|c| c.kind.as_str())
    }
}

/// Every block the schema declares, plus the kinds a document may hold at the
/// top level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SchemaProjection {
    /// Block kinds accepted at the document root (`lab`, `template`).
    pub roots: Vec<String>,
    /// Every block, in schema declaration order.
    pub blocks: Vec<Block>,
    /// Every decorator the schema declares, in declaration order. WCL's own
    /// built-in decorators (`@doc`, `@block`, `@children`, …) are not here:
    /// they belong to the language, not to this schema.
    pub decorators: Vec<Decorator>,
}

impl SchemaProjection {
    /// The projection of the embedded `schema.wcl`, reflected once per
    /// process.
    pub fn get() -> &'static SchemaProjection {
        static PROJECTION: OnceLock<SchemaProjection> = OnceLock::new();
        PROJECTION.get_or_init(|| {
            Self::reflect(super::SCHEMA_WCL, "vmlab.wcl")
                .expect("the embedded vmlab.wcl schema must reflect")
        })
    }

    /// Reflect a WCL schema source into a projection. Fails only when the
    /// source does not parse.
    pub fn reflect(source: &str, name: &str) -> Result<SchemaProjection, String> {
        let doc = Document::open(source, name).map_err(|e| e.to_string())?;
        let symbol_sets: BTreeMap<String, Vec<String>> = doc
            .symbol_sets()
            .map(|set| {
                (
                    set.name_segments().join("."),
                    set.symbols().map(|s| s.name().to_string()).collect(),
                )
            })
            .collect();

        // A decorator this schema declares, paired with the type declaring
        // it. WCL's built-ins come through the same iterator; they are
        // synthesised, so they span no source text, and an imported
        // declaration belongs to another schema.
        let declared: Vec<(String, wcl_lang::TypeDecl<'_>)> = doc
            .declared_decorators()
            .filter(|(_, decl)| !decl.is_imported() && !decl.span().is_empty())
            .collect();

        let decorators: Vec<Decorator> = declared
            .iter()
            .map(|(name, decl)| decorator(name, decl, &symbol_sets))
            .collect();

        let blocks: Vec<Block> = doc
            .type_decls()
            .filter(|t| !t.is_imported())
            .filter_map(|decl| {
                let kind = decorator_str(decl.decorators(), "block")?;
                Some(Block {
                    decorators: declared
                        .iter()
                        .filter(|(_, decl)| decl.decorator_applies_to("block", Some(&kind)))
                        .map(|(name, _)| name.clone())
                        .collect(),
                    kind,
                    type_name: decl.name_segments().join("."),
                    doc: decl.doc_comment().unwrap_or_default(),
                    required_groups: decl
                        .decorators()
                        .filter(|d| d.name() == "one_of")
                        .filter_map(|d| {
                            Some(RequiredGroup {
                                fields: string_list(d.positional().ok()?.first()?)?,
                                // Exclusive unless the rule opts out.
                                exclusive: !matches!(
                                    d.named_arg("exclusive"),
                                    Some(Ok(Value::Bool(false)))
                                ),
                            })
                        })
                        .collect(),
                    fields: decl.fields().map(|f| field(&f, &symbol_sets)).collect(),
                })
            })
            .collect();

        let roots = doc
            .doc_schema()
            .map(|root| {
                root.fields()
                    .filter_map(|f| f.child_block_kind().or_else(|| f.children_block_kind()))
                    .collect()
            })
            .unwrap_or_default();

        Ok(SchemaProjection {
            roots,
            blocks,
            decorators,
        })
    }

    pub fn block(&self, kind: &str) -> Option<&Block> {
        self.blocks.iter().find(|b| b.kind == kind)
    }

    pub fn decorator(&self, name: &str) -> Option<&Decorator> {
        self.decorators.iter().find(|d| d.name == name)
    }

    /// The decorators an author may write on a block of `kind`, in schema
    /// declaration order. Empty for a kind the schema does not declare.
    pub fn block_decorators(&self, kind: &str) -> Vec<&Decorator> {
        self.block(kind)
            .map(|block| {
                block
                    .decorators
                    .iter()
                    .filter_map(|name| self.decorator(name))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The option list of one field, e.g. `("gpu", "mode")`. Empty when the
    /// field is not a closed enum or does not exist.
    pub fn options(&self, kind: &str, field: &str) -> &[String] {
        self.block(kind)
            .and_then(|b| b.field(field))
            .map(|f| f.options.as_slice())
            .unwrap_or(&[])
    }

    /// One field's default as a number in its base unit — seconds for a
    /// duration, bytes for a byte size. `None` when the field declares no
    /// default, does not exist, or its default is not numeric.
    pub fn default_number(&self, kind: &str, field: &str) -> Option<i64> {
        self.block(kind)?.field(field)?.default_number
    }
}

/// Project one declared decorator. `name` is the spelling `@decorator("…")`
/// gives it, which is not the declaring type's name.
fn decorator(
    name: &str,
    decl: &wcl_lang::TypeDecl<'_>,
    symbol_sets: &BTreeMap<String, Vec<String>>,
) -> Decorator {
    let applies_to = decl.decorators().find(|d| d.name() == "applies_to");
    let applicability = |arg: &str| {
        applies_to
            .as_ref()
            .and_then(|d| d.resolved_arg_value(arg).or_else(|| d.named_arg(arg)))
            .and_then(Result::ok)
            .and_then(|value| name_list(&value))
            .unwrap_or_default()
    };

    Decorator {
        name: name.to_string(),
        type_name: decl.name_segments().join("."),
        doc: decl.doc_comment().unwrap_or_default(),
        positions: applicability("on"),
        kinds: applicability("kinds"),
        repeatable: matches!(
            decl.decorators()
                .find(|d| d.name() == "decorator")
                .and_then(|d| d.named_arg("repeatable")),
            Some(Ok(Value::Bool(true)))
        ),
        args: decl.fields().map(|f| field(&f, symbol_sets)).collect(),
    }
}

/// Project one declared field.
fn field(f: &wcl_lang::TypeField<'_>, symbol_sets: &BTreeMap<String, Vec<String>>) -> Field {
    let decorators: Vec<_> = f.decorators().collect();
    let options = decorator_positional(&decorators, "options")
        .and_then(|args| string_list(args.first()?))
        .unwrap_or_default();

    let child = f
        .child_block_kind()
        .map(|kind| ChildSlot {
            kind,
            repeated: false,
            min: None,
            max: None,
        })
        .or_else(|| {
            f.children_block_kind().map(|kind| ChildSlot {
                kind,
                repeated: true,
                min: f.children_min(),
                max: f.children_max(),
            })
        });

    let range = decorator_positional(&decorators, "range");
    let default_value = f.default_value();
    let symbol_options = symbol_set_options(f.type_ref(), symbol_sets);
    let ty = classify(
        f.type_ref(),
        child.is_some(),
        !options.is_empty(),
        &symbol_options,
    );

    Field {
        name: f.name().to_string(),
        doc: decorator_positional(&decorators, "doc")
            .and_then(|args| as_string(args.first()?))
            .unwrap_or_default(),
        ty,
        optional: f.optional(),
        // `Value`'s Display is WCL source form — `"tcp"`, `:idle`, `10s` —
        // which is exactly what a form should show as "the value you get if
        // you leave this blank".
        default: default_value.as_ref().map(|v| v.to_string()),
        default_number: default_value.as_ref().and_then(base_units),
        options: if options.is_empty() {
            symbol_options.unwrap_or_default()
        } else {
            options
        },
        min: range.as_ref().and_then(|args| as_i64(args.first()?)),
        max: range.as_ref().and_then(|args| as_i64(args.get(1)?)),
        label_slot: f.inline_slot(),
        child,
    }
}

fn classify(
    ty: &TypeRef,
    is_block: bool,
    has_options: bool,
    symbol_options: &Option<Vec<String>>,
) -> FieldType {
    if is_block {
        return FieldType::Block;
    }
    if symbol_options.is_some() {
        return FieldType::Symbol;
    }
    match ty {
        TypeRef::Builtin(BuiltinType::Utf8) if has_options => FieldType::Enum,
        TypeRef::Builtin(BuiltinType::Utf8) => FieldType::Text,
        TypeRef::Builtin(BuiltinType::Bool) => FieldType::Bool,
        TypeRef::Builtin(
            BuiltinType::I8
            | BuiltinType::I16
            | BuiltinType::I32
            | BuiltinType::I64
            | BuiltinType::U8
            | BuiltinType::U16
            | BuiltinType::U32
            | BuiltinType::U64,
        ) => FieldType::Int,
        TypeRef::List(inner) => match inner.as_ref() {
            TypeRef::Builtin(BuiltinType::Utf8) => FieldType::TextList,
            _ => FieldType::Unknown,
        },
        TypeRef::Named { path, .. } => match path.join(".").as_str() {
            "std.ByteSize" => FieldType::ByteSize,
            "std.Duration" => FieldType::Duration,
            _ => FieldType::Unknown,
        },
        _ => FieldType::Unknown,
    }
}

/// The symbols of the `symbol_set` a field is typed with, or `None` when the
/// field is not symbol-typed.
fn symbol_set_options(
    ty: &TypeRef,
    symbol_sets: &BTreeMap<String, Vec<String>>,
) -> Option<Vec<String>> {
    match ty {
        TypeRef::Named { path, .. } => symbol_sets.get(&path.join(".")).cloned(),
        _ => None,
    }
}

fn decorator_positional<'a>(
    decorators: &[wcl_lang::Decorator<'a>],
    name: &str,
) -> Option<Vec<Value>> {
    decorators
        .iter()
        .find(|d| d.name() == name)?
        .positional()
        .ok()
}

fn decorator_str<'a>(
    mut decorators: impl Iterator<Item = wcl_lang::Decorator<'a>>,
    name: &str,
) -> Option<String> {
    let args = decorators.find(|d| d.name() == name)?.positional().ok()?;
    as_string(args.first()?)
}

fn as_string(value: &Value) -> Option<String> {
    match value {
        Value::Utf8(s) | Value::Ascii(s) => Some(s.clone()),
        _ => None,
    }
}

/// A scalar default as a number in its base unit. A literal with a unit
/// suffix (`10s`, `256MiB`) reaches a decorator argument unresolved — the
/// coercion that would fold in the unit's factor is type-directed, and a
/// decorator argument has no declared type — so the factor is applied here.
/// The factors are the SI/IEC conventions `std.Duration` and `std.ByteSize`
/// are defined in terms of.
fn base_units(value: &Value) -> Option<i64> {
    match value {
        Value::PendingUnit { magnitude, unit } => {
            let factor = match unit.as_str() {
                "s" => 1,
                "m" => 60,
                "h" => 3600,
                "d" => 86_400,
                "B" => 1,
                "KiB" => 1 << 10,
                "MiB" => 1 << 20,
                "GiB" => 1 << 30,
                "TiB" => 1i64 << 40,
                "KB" => 1_000,
                "MB" => 1_000_000,
                "GB" => 1_000_000_000,
                _ => return None,
            };
            as_i64(magnitude)?.checked_mul(factor)
        }
        other => as_i64(other),
    }
}

fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::I8(n) => Some(*n as i64),
        Value::I16(n) => Some(*n as i64),
        Value::I32(n) => Some(*n as i64),
        Value::I64(n) => Some(*n),
        Value::U8(n) => Some(*n as i64),
        Value::U16(n) => Some(*n as i64),
        Value::U32(n) => Some(*n as i64),
        _ => None,
    }
}

fn string_list(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::List(items) => items.iter().map(as_string).collect(),
        _ => None,
    }
}

/// A list of names, however they are spelled. `@applies_to` takes its
/// positions as symbols (`:block`) and its kinds as strings (`"vm"`), and WCL
/// accepts an identifier for either, so all four spellings read as the name.
fn name_list(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::List(items) => items
            .iter()
            .map(|item| match item {
                Value::Symbol(name) | Value::Identifier(name) => Some(name.clone()),
                other => as_string(other),
            })
            .collect(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projection() -> &'static SchemaProjection {
        SchemaProjection::get()
    }

    #[test]
    fn roots_are_the_document_slots() {
        assert_eq!(projection().roots, ["lab", "template"]);
    }

    #[test]
    fn every_block_kind_is_projected() {
        // Reflection, not a list: the assertion is that the count matches the
        // `@block` declarations in schema.wcl, and that a spot-check of kinds
        // across the file is present.
        let kinds: Vec<&str> = projection()
            .blocks
            .iter()
            .map(|b| b.kind.as_str())
            .collect();
        assert_eq!(
            kinds.len(),
            super::super::SCHEMA_WCL.matches("@block(\"").count()
        );
        for kind in [
            "lab",
            "segment",
            "vm",
            "container",
            "volume",
            "auth",
            "source",
        ] {
            assert!(kinds.contains(&kind), "{kind} missing from {kinds:?}");
        }
    }

    #[test]
    fn field_types_come_from_the_declared_type() {
        let vm = projection().block("vm").expect("vm block");
        assert_eq!(vm.field("template").unwrap().ty, FieldType::Text);
        assert_eq!(vm.field("cpus").unwrap().ty, FieldType::Int);
        assert_eq!(vm.field("nested").unwrap().ty, FieldType::Bool);
        assert_eq!(vm.field("memory").unwrap().ty, FieldType::ByteSize);
        assert_eq!(vm.field("qemu_args").unwrap().ty, FieldType::TextList);
        assert_eq!(vm.field("firmware").unwrap().ty, FieldType::Enum);
        assert_eq!(vm.field("nics").unwrap().ty, FieldType::Block);

        let healthcheck = projection()
            .block("healthcheck")
            .expect("healthcheck block");
        assert_eq!(
            healthcheck.field("interval").unwrap().ty,
            FieldType::Duration
        );

        let container = projection().block("container").expect("container block");
        assert_eq!(container.field("mode").unwrap().ty, FieldType::Symbol);

        // Nothing in the schema should fall through to Unknown — that would
        // mean a surface is being handed a field it cannot render.
        for block in &projection().blocks {
            for field in &block.fields {
                assert_ne!(
                    field.ty,
                    FieldType::Unknown,
                    "{}.{} is unclassified",
                    block.kind,
                    field.name
                );
            }
        }
    }

    #[test]
    fn optionality_comes_from_the_schema() {
        let vm = projection().block("vm").expect("vm block");
        assert!(!vm.field("template").unwrap().optional);
        assert!(vm.field("cpus").unwrap().optional);
        // A `@children` slot is a list, never a value the author must supply.
        assert!(!vm.field("nics").unwrap().optional);
        assert!(vm.field("nics").unwrap().child.is_some());
    }

    #[test]
    fn doc_strings_come_from_the_schema() {
        let field = projection()
            .block("volume")
            .and_then(|b| b.field("read_only"))
            .expect("volume.read_only");
        assert_eq!(field.doc, "Mount read-only (default false)");
        // Every field carries help text; a missing @doc is a schema bug.
        for block in &projection().blocks {
            for field in &block.fields {
                assert!(
                    !field.doc.is_empty(),
                    "{}.{} has no @doc",
                    block.kind,
                    field.name
                );
            }
        }
    }

    #[test]
    fn enum_options_come_from_the_schema() {
        assert_eq!(
            projection().options("gpu", "mode"),
            ["passthrough", "virgl", "vulkan"]
        );
        assert_eq!(projection().options("media", "kind"), ["iso", "floppy"]);
        assert_eq!(
            projection().options("block", "proto"),
            ["tcp", "udp", "icmp"]
        );
        assert_eq!(projection().options("redirect", "proto"), ["tcp", "udp"]);
        // A symbol_set-typed field needs no annotation; its set is its type.
        assert_eq!(
            projection().options("container", "mode"),
            ["workload", "idle"]
        );
        assert_eq!(
            projection().options("auth", "method"),
            ["basic", "bearer", "header", "ntlm", "form"]
        );
        // Every closed field carries a non-empty set, and only closed fields do.
        for block in &projection().blocks {
            for field in &block.fields {
                let closed = matches!(field.ty, FieldType::Enum | FieldType::Symbol);
                assert_eq!(
                    closed,
                    !field.options.is_empty(),
                    "{}.{}: ty {:?} vs options {:?}",
                    block.kind,
                    field.name,
                    field.ty,
                    field.options
                );
            }
        }
    }

    #[test]
    fn block_nesting_and_cardinality_come_from_the_schema() {
        let vm = projection().block("vm").expect("vm block");
        let nics = vm.field("nics").unwrap().child.clone().expect("nic slot");
        assert_eq!(nics.kind, "nic");
        assert!(nics.repeated);
        let gpu = vm.field("gpu").unwrap().child.clone().expect("gpu slot");
        assert_eq!(gpu.kind, "gpu");
        assert!(!gpu.repeated, "@child holds at most one");
        assert_eq!(
            vm.child_kinds().collect::<Vec<_>>(),
            [
                "gpu",
                "nic",
                "disk",
                "share",
                "media",
                "web",
                "login",
                "provision",
                "playbook"
            ]
        );
    }

    #[test]
    fn label_slots_come_from_the_schema() {
        let vm = projection().block("vm").expect("vm block");
        assert_eq!(vm.field("name").unwrap().label_slot, Some(0));
        assert_eq!(vm.field("template").unwrap().label_slot, None);
    }

    #[test]
    fn unit_defaults_resolve_to_base_units() {
        let healthcheck = projection()
            .block("healthcheck")
            .expect("healthcheck block");
        let interval = healthcheck.field("interval").unwrap();
        assert_eq!(interval.default.as_deref(), Some("10s"));
        assert_eq!(interval.default_number, Some(10));
        assert_eq!(
            projection().default_number("healthcheck", "timeout"),
            Some(5)
        );
        // A micro-VM's size comes from its profile, not from the schema
        // (ADR-0008), so there is no default to report.
        assert_eq!(projection().default_number("container", "memory"), None);
        assert_eq!(projection().default_number("vm", "cpus"), None);
        assert_eq!(projection().default_number("vm", "no_such_field"), None);
    }

    #[test]
    fn ranges_come_from_the_schema() {
        let forward = projection().block("forward").expect("forward block");
        let host_port = forward.field("host_port").unwrap();
        assert_eq!((host_port.min, host_port.max), (Some(1), Some(65535)));
        let to = forward.field("to").unwrap();
        assert_eq!((to.min, to.max), (None, None));
        // `@range(min)` is a floor with no ceiling.
        let retries = projection()
            .block("healthcheck")
            .and_then(|b| b.field("retries"))
            .unwrap();
        assert_eq!((retries.min, retries.max), (Some(1), None));
    }

    #[test]
    fn required_group_rules_come_from_the_schema() {
        // `volume` takes a host bind or a named volume, never both.
        let volume = projection().block("volume").expect("volume block");
        assert_eq!(volume.required_groups.len(), 1);
        assert_eq!(volume.required_groups[0].fields, ["host", "name"]);
        assert!(volume.required_groups[0].exclusive);
        // A `disk` needs a size or a source folder, and may carry both —
        // `check_disk_block` reads "size and/or from".
        let disk = projection().block("disk").expect("disk block");
        assert_eq!(disk.required_groups[0].fields, ["size", "from"]);
        assert!(!disk.required_groups[0].exclusive);
        // Every named field in a rule must exist on the block it constrains.
        for block in &projection().blocks {
            for group in &block.required_groups {
                for name in &group.fields {
                    assert!(
                        block.field(name).is_some(),
                        "@one_of on `{}` names unknown field `{name}`",
                        block.kind
                    );
                }
            }
        }
    }

    #[test]
    fn defaults_are_reflected_in_wcl_source_form() {
        let source = r#"
@document type Doc { @children("thing") things: list<Thing> }
@block("thing")
type Thing {
  @doc("A defaulted string") @default("hi") greeting: utf8?
  @doc("A defaulted int") @default(7) count: i64?
  @doc("No default") other: utf8?
}
"#;
        let projected = SchemaProjection::reflect(source, "test.wcl").expect("reflect");
        let thing = projected.block("thing").expect("thing block");
        assert_eq!(
            thing.field("greeting").unwrap().default.as_deref(),
            Some("\"hi\"")
        );
        assert_eq!(thing.field("count").unwrap().default.as_deref(), Some("7"));
        assert_eq!(thing.field("count").unwrap().default_number, Some(7));
        assert_eq!(thing.field("other").unwrap().default, None);
    }

    /// Every decorator the schema declares is projected the way its blocks
    /// are: name, doc, applicability, cardinality and typed arguments.
    #[test]
    fn decorator_declarations_come_from_the_schema() {
        let names: Vec<&str> = projection()
            .decorators
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(names, ["options", "range", "one_of"]);

        let options = projection().decorator("options").expect("@options");
        assert_eq!(options.type_name, "FieldOptions");
        assert!(options.doc.starts_with("The exact set of values"));
        assert_eq!(options.positions, ["type_field"]);
        assert!(options.kinds.is_empty(), "no kinds narrows nothing");
        assert!(!options.repeatable);
        let values = options.arg("values").expect("the values argument");
        assert_eq!(values.ty, FieldType::TextList);
        assert!(!values.optional);
        assert_eq!(values.label_slot, Some(0));
        assert_eq!(
            values.doc,
            "Every value the validator accepts, in the order a picker should offer them"
        );

        // An optional argument reads as optional, so a surface knows it may
        // be left out.
        let range = projection().decorator("range").expect("@range");
        assert_eq!(range.arg("min").unwrap().ty, FieldType::Int);
        assert!(!range.arg("min").unwrap().optional);
        assert!(range.arg("max").unwrap().optional);

        // `@one_of` is the one a block may carry more than once.
        let one_of = projection().decorator("one_of").expect("@one_of");
        assert_eq!(one_of.positions, ["type"]);
        assert!(one_of.repeatable);
        assert_eq!(one_of.arg("exclusive").unwrap().ty, FieldType::Bool);
        assert!(one_of.arg("exclusive").unwrap().optional);
    }

    /// WCL's own decorators belong to the language. Projecting them would
    /// offer every surface annotations no lab file has any business carrying.
    #[test]
    fn built_in_decorators_are_not_projected() {
        for name in ["doc", "block", "children", "inline", "default", "decorator"] {
            assert!(
                projection().decorator(name).is_none(),
                "@{name} is WCL's, not the schema's"
            );
        }
    }

    /// The schema's three decorators annotate the schema file itself, so no
    /// block offers any of them — the list fills in when one declares
    /// `@applies_to(on = [:block])`.
    #[test]
    fn a_block_carries_only_the_decorators_declared_for_its_kind() {
        for block in &projection().blocks {
            assert!(
                block.decorators.is_empty(),
                "{} offers {:?}",
                block.kind,
                block.decorators
            );
        }

        let source = r#"
@decorator("options") @applies_to(on = [:type_field])
type FieldOptions { @inline(0) values: list<utf8> }
@document type Doc {
  @children("gizmo") gizmos: list<Gizmo>
  @children("widget") widgets: list<Widget>
}
// Marks a gizmo for the dev machine.
@decorator("dev", repeatable = true)
@applies_to(on = [:block], kinds = ["gizmo"])
type Dev {
  @doc("Who to hand the machine to") @inline(0) owner: utf8
  @doc("How to reach it") @options(["ssh", "rdp"]) @default("ssh") over: utf8?
}
// Legal anywhere, because it says nothing about where.
@decorator("tag") type Tag { @inline(0) name: utf8 }
@block("gizmo") type Gizmo { @doc("A field") flavour: utf8? }
@block("widget") type Widget { @doc("A field") size: i64? }
"#;
        let projected = SchemaProjection::reflect(source, "test.wcl").expect("reflect");
        assert_eq!(projected.block("gizmo").unwrap().decorators, ["dev", "tag"]);
        assert_eq!(projected.block("widget").unwrap().decorators, ["tag"]);

        let dev = projected.block_decorators("gizmo")[0];
        assert_eq!(dev.name, "dev");
        assert_eq!(dev.doc, "Marks a gizmo for the dev machine.");
        assert_eq!(dev.kinds, ["gizmo"]);
        assert!(dev.repeatable);
        // An argument carries everything a block field carries.
        let over = dev.arg("over").expect("the over argument");
        assert_eq!(over.ty, FieldType::Enum);
        assert_eq!(over.options, ["ssh", "rdp"]);
        assert_eq!(over.default.as_deref(), Some("\"ssh\""));
        assert_eq!(over.doc, "How to reach it");
        assert!(over.optional);
        assert!(!dev.arg("owner").unwrap().optional);
    }

    #[test]
    fn a_new_schema_field_appears_without_touching_the_projection() {
        let source = r#"
@decorator("options") type FieldOptions { @inline(0) values: list<utf8> }
@document type Doc { @children("gizmo") gizmos: list<Gizmo> }
// A gizmo.
@block("gizmo")
type Gizmo {
  @doc("The new field") @options(["a", "b"]) flavour: utf8?
}
"#;
        let projected = SchemaProjection::reflect(source, "test.wcl").expect("reflect");
        let gizmo = projected.block("gizmo").expect("gizmo block");
        assert_eq!(gizmo.doc, "A gizmo.");
        let flavour = gizmo.field("flavour").expect("flavour");
        assert_eq!(flavour.ty, FieldType::Enum);
        assert_eq!(flavour.options, ["a", "b"]);
        assert_eq!(flavour.doc, "The new field");
    }
}
