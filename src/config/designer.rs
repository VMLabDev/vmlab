//! The visual designer's inspector forms, projected from the schema
//! (ADR-0005).
//!
//! The console used to carry a hand-copied descriptor table per block —
//! field name, help text, control kind, option list — with a header asking
//! the next contributor to keep it in sync. This module replaces it: each
//! form names a block and the fields it shows, and everything else (the help
//! prose, the control, the option list, whether a value is required, the
//! bounds, the default) comes from [`super::projection`].
//!
//! What stays here is only what the schema cannot say: which fields a tab
//! groups, the human label, a placeholder, and the handful of controls that
//! are bound to live lab state rather than to a type (a segment picker, a
//! machine picker, the event list). Those are the *overrides*, and they are
//! meant to stay few — [`FORMS`] is the whole set, reviewable in one screen.
//!
//! [`render_typescript`] renders the result into
//! `web-ui/src/editor/schema.gen.ts`, which is committed like the other
//! generated artefacts; `console_artefact_is_current` fails when it drifts.

use std::fmt::Write as _;

use serde::Serialize;

use super::projection::{Field, FieldType, SchemaProjection};

/// The control a form renders a field with. Mirrors the console's
/// `FieldType`; the names are the strings the console switches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Control {
    /// Free text.
    Text,
    /// Number input.
    Int,
    /// Tri-state default / on / off, for a `bool?` that inherits when unset.
    Bool3,
    /// Plain on/off toggle.
    Flag,
    /// Byte size, entered as `8GiB`.
    Bytes,
    /// Picker over a closed option list.
    Enum,
    /// Multi-line text, one entry per line.
    Lines,
    /// Picker over the lab's segment names.
    SegRef,
    /// Multi-select over the lab's segment names.
    SegRefs,
    /// Picker over the lab's VM names.
    VmRef,
    /// Multi-select over the lab's machine names.
    VmRefs,
    /// Picker over the lifecycle event names.
    Event,
}

impl Control {
    fn as_str(self) -> &'static str {
        match self {
            Control::Text => "text",
            Control::Int => "int",
            Control::Bool3 => "bool3",
            Control::Flag => "flag",
            Control::Bytes => "bytes",
            Control::Enum => "enum",
            Control::Lines => "lines",
            Control::SegRef => "segref",
            Control::SegRefs => "segrefs",
            Control::VmRef => "vmref",
            Control::VmRefs => "vmrefs",
            Control::Event => "event",
        }
    }

    /// The control a field's declared type implies, before any override.
    fn of(field: &Field) -> Option<Control> {
        Some(match field.ty {
            FieldType::Text => Control::Text,
            FieldType::Int | FieldType::Duration => Control::Int,
            FieldType::Bool => Control::Flag,
            FieldType::ByteSize => Control::Bytes,
            FieldType::TextList => Control::Lines,
            FieldType::Enum | FieldType::Symbol => Control::Enum,
            FieldType::Block | FieldType::Unknown => return None,
        })
    }
}

/// One field in a form, ready for the console to render.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FormField {
    /// The schema field name — the key in the console's draft model.
    pub key: String,
    pub label: String,
    /// The field's `@doc` text, verbatim. The designer's help and the
    /// rendered reference are the same string.
    pub doc: String,
    pub control: Control,
    pub options: Vec<String>,
    pub placeholder: Option<String>,
    /// The control offers no "(default)" choice: the schema either requires
    /// a value or supplies one.
    pub required: bool,
    pub min: Option<i64>,
    pub max: Option<i64>,
}

/// One exported descriptor table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Form {
    /// The `const` the console imports, e.g. `VM_OVERRIDES`.
    pub export: &'static str,
    /// The schema block the fields belong to.
    pub block: &'static str,
    /// The key this table sits under when several share an export (the
    /// per-method auth tables).
    pub variant: Option<&'static str>,
    /// Exported as a bare `FieldDesc` rather than an array.
    pub single: bool,
    pub fields: Vec<FormField>,
}

/// A field's presentation, for the parts the schema cannot carry.
struct FieldSpec {
    field: &'static str,
    label: Option<&'static str>,
    control: Option<Control>,
    placeholder: Option<&'static str>,
}

const fn f(field: &'static str) -> FieldSpec {
    FieldSpec {
        field,
        label: None,
        control: None,
        placeholder: None,
    }
}

impl FieldSpec {
    const fn label(mut self, label: &'static str) -> Self {
        self.label = Some(label);
        self
    }

    const fn control(mut self, control: Control) -> Self {
        self.control = Some(control);
        self
    }

    /// A placeholder — an example value shown in an empty input.
    const fn hint(mut self, placeholder: &'static str) -> Self {
        self.placeholder = Some(placeholder);
        self
    }
}

struct FormSpec {
    export: &'static str,
    block: &'static str,
    variant: Option<&'static str>,
    single: bool,
    fields: &'static [FieldSpec],
}

const fn form(export: &'static str, block: &'static str, fields: &'static [FieldSpec]) -> FormSpec {
    FormSpec {
        export,
        block,
        variant: None,
        single: false,
        fields,
    }
}

impl FormSpec {
    const fn variant(mut self, variant: &'static str) -> Self {
        self.variant = Some(variant);
        self
    }

    /// Exported as one `FieldDesc`, not a table.
    const fn single(mut self) -> Self {
        self.single = true;
        self
    }
}

/// Every inspector form, and the only hand-written thing about them.
///
/// A field named here must exist in `schema.wcl`, and every schema field must
/// either appear here, be a nested-block slot, or be listed in [`UNFORMED`] —
/// `designer_covers_the_schema` enforces both directions.
static FORMS: &[FormSpec] = &[
    // --- vm -----------------------------------------------------------------
    // The General tab's template picker (arch/profile fold into it) and the
    // depends-on list are dedicated components, as are the Hardware tab's
    // cpu/memory sliders.
    form("VM_HARDWARE", "vm", &[f("nested").label("Nested virt")]),
    // Everything normally supplied by the template/profile, plus the escape
    // hatches — the trailing Overrides tab.
    form(
        "VM_OVERRIDES",
        "vm",
        &[
            f("firmware"),
            f("tpm").label("TPM 2.0").control(Control::Bool3),
            f("secure_boot").control(Control::Bool3),
            f("display").hint("e.g. virtio-vga"),
            f("disk").label("Primary disk").hint("e.g. 64GiB"),
            f("floppy"),
            f("qemu_args").label("QEMU args"),
        ],
    ),
    // --- machine children ---------------------------------------------------
    // NAT attachment (and port isolation) are wired on the canvas, not as
    // per-NIC form fields — the form covers segment, address, MAC.
    form(
        "NIC_FIELDS",
        "nic",
        &[
            f("segment").control(Control::SegRef),
            f("ip").label("Static IP").hint("10.0.0.10"),
            f("mac").label("MAC").hint("52:54:00:ab:cd:ef"),
        ],
    ),
    form(
        "DISK_FIELDS",
        "disk",
        &[
            f("name"),
            f("size").hint("e.g. 10GiB"),
            f("from").label("From folder"),
        ],
    ),
    form(
        "SHARE_FIELDS",
        "share",
        &[
            f("host").label("Host path"),
            f("guest").label("Guest path"),
            f("readonly").label("Read-only"),
            f("smb1").label("SMB1"),
            f("name").label("Share name"),
            f("transport"),
        ],
    ),
    form(
        "GPU_FIELDS",
        "gpu",
        &[
            f("mode"),
            f("address").label("PCI address").hint("0000:01:00.0"),
        ],
    ),
    form(
        "WEB_FIELDS",
        "web",
        &[
            f("port").label("Guest port"),
            f("path").label("Initial path").hint("/"),
        ],
    ),
    // The auth `method` selector drives which credential fields show.
    form("WEB_AUTH_METHOD", "auth", &[f("method")]).single(),
    form("WEB_AUTH_FIELDS", "auth", &[f("username"), f("password")]).variant("basic"),
    form("WEB_AUTH_FIELDS", "auth", &[f("token")]).variant("bearer"),
    form(
        "WEB_AUTH_FIELDS",
        "auth",
        &[
            f("header").label("Header name"),
            f("value").label("Header value"),
        ],
    )
    .variant("header"),
    form(
        "WEB_AUTH_FIELDS",
        "auth",
        &[f("username"), f("password"), f("domain")],
    )
    .variant("ntlm"),
    form(
        "WEB_AUTH_FIELDS",
        "auth",
        &[
            f("username"),
            f("password"),
            f("login_path").label("Login path").hint("/login"),
            f("login_method").label("Login method"),
            f("login_body")
                .label("Login body")
                .hint("user={user}&password={pass}"),
            f("login_content_type").label("Content type"),
            f("fail_redirect").label("Fail redirect"),
        ],
    )
    .variant("form"),
    // --- container children -------------------------------------------------
    // The image picker and the micro-VM cpu/memory sliders are dedicated
    // components; the rest of the runtime knobs are a plain form.
    form(
        "CONTAINER_RUNTIME",
        "container",
        &[
            f("restart").label("Restart policy"),
            f("workdir").label("Working directory").hint("/srv/app"),
            f("user").hint("1000:1000"),
            f("entrypoint"),
            f("command"),
        ],
    ),
    form(
        "ENV_FIELDS",
        "env",
        &[f("name").hint("APP_ENV"), f("value")],
    ),
    form(
        "VOLUME_FIELDS",
        "volume",
        &[
            f("host").label("Host path").hint("data/www"),
            f("name").label("Volume name").hint("dbdata"),
            f("target").hint("/var/lib/data"),
            f("read_only").label("Read-only"),
        ],
    ),
    form(
        "PORT_FIELDS",
        "port",
        &[
            f("host").label("Host port"),
            f("container").label("Container port"),
            f("proto").label("Protocol"),
        ],
    ),
    // The console edits the probe timings in whole seconds, so the labels
    // carry the unit the schema's `10s` literals imply.
    form(
        "HEALTHCHECK_FIELDS",
        "healthcheck",
        &[
            f("command"),
            f("interval").label("Interval (s)"),
            f("timeout").label("Timeout (s)"),
            f("retries"),
            f("start_period").label("Start period (s)"),
        ],
    ),
    // --- segment ------------------------------------------------------------
    // `mtu` renders as a dedicated slider in the segment inspector.
    form(
        "SEGMENT_GENERAL",
        "segment",
        &[f("subnet").hint("10.50.0.0/24")],
    ),
    form("SEGMENT_SERVICES", "segment", &[f("dhcp").label("DHCP")]),
    form("RECORD_FIELDS", "record", &[f("name"), f("ip").label("IP")]),
    form(
        "SINKHOLE_FIELDS",
        "sinkhole",
        &[f("pattern").hint("*.telemetry.example.com"), f("mode")],
    ),
    // --- lab children -------------------------------------------------------
    form(
        "HANDLER_FIELDS",
        "on",
        &[
            f("event").control(Control::Event),
            f("run").label("Handler script").hint("scripts/on-crash.ws"),
            f("targets")
                .label("Target machines")
                .control(Control::VmRefs),
        ],
    ),
];

/// Schema fields the designer deliberately does not render as a form field,
/// each with the reason. Nested-block slots are not listed: they are always
/// rendered as child lists.
///
/// This is the "field a surface cannot reach" ledger. Adding a field to
/// `schema.wcl` fails `designer_covers_the_schema` until it either joins a
/// form or is entered here, so a field cannot be added and forgotten.
///
/// Three reasons appear. `LABEL` and `DEDICATED` are covered — the field is
/// editable, just not through a descriptor row. `RAW_ONLY` is the honest
/// backlog: the designer does not edit that field at all, and the author has
/// to drop to the text editor for it.
pub static UNFORMED: &[(&str, &str, &str)] = &[
    // The block's label, edited as its name in the inspector header.
    ("lab", "name", LABEL),
    ("segment", "name", LABEL),
    ("vm", "name", LABEL),
    ("container", "name", LABEL),
    ("web", "name", LABEL),
    ("provision", "script", LABEL),
    ("playbook", "path", LABEL),
    ("var", "name", LABEL),
    // Dedicated components, because the control is richer than a row.
    ("vm", "template", "the template picker"),
    ("vm", "arch", "folded into the template picker"),
    ("vm", "profile", "folded into the template picker"),
    ("vm", "cpus", "the vCPU slider"),
    ("vm", "memory", "the memory slider"),
    ("vm", "cdrom", "the storage tab's CD-ROM row"),
    (
        "vm",
        "depends_on",
        "dependency edges on the topology canvas",
    ),
    ("container", "image", "the image picker"),
    ("container", "cpus", "the micro-VM vCPU slider"),
    ("container", "memory", "the micro-VM memory slider"),
    (
        "container",
        "depends_on",
        "dependency edges on the topology canvas",
    ),
    ("segment", "mtu", "the MTU slider"),
    ("dns", "server", "the segment's DNS editor"),
    ("dns", "enabled", "the segment's DNS editor"),
    ("block", "cidr", "the segment's L3 rules table"),
    ("block", "proto", "the segment's L3 rules table"),
    ("block", "port", "the segment's L3 rules table"),
    ("redirect", "from", "the segment's redirect rules table"),
    ("redirect", "to", "the segment's redirect rules table"),
    ("redirect", "proto", "the segment's redirect rules table"),
    // Wired by drawing on the topology canvas rather than typed.
    ("nic", "nat", "cabling a NIC to the NAT bus"),
    ("nic", "gateway", "cabling, which marks the gateway NIC"),
    ("nic", "isolated", "the canvas's port-isolation toggle"),
    ("segment", "nat", "cabling the segment to the NAT bus"),
    ("segment", "global", "cabling a cross-host trunk"),
    ("connect", "host", "cabling a cross-host trunk"),
    // Not editable in the designer — the raw config editor covers them.
    ("lab", "gui", RAW_ONLY),
    ("vm", "gui", RAW_ONLY),
    ("segment", "routes_to", RAW_ONLY),
    (
        "container",
        "mode",
        "the edit-op writer has no symbol support yet",
    ),
    ("forward", "host_port", RAW_ONLY),
    ("forward", "to", RAW_ONLY),
    ("forward", "proto", RAW_ONLY),
    ("route", "dest", RAW_ONLY),
    ("route", "via", RAW_ONLY),
    ("media", "kind", RAW_ONLY),
    ("media", "from", RAW_ONLY),
    ("media", "label", RAW_ONLY),
    ("playbook", "play", "the playbook panel's play list"),
    ("var", "value", "the playbook panel's variable rows"),
    // Template definitions are managed by the templates view and the
    // `vmlab template` verbs, not by the lab designer's inspector.
    ("template", "name", TEMPLATE_VIEW),
    ("template", "arch", TEMPLATE_VIEW),
    ("template", "version", TEMPLATE_VIEW),
    ("template", "registry", TEMPLATE_VIEW),
    ("template", "profile", TEMPLATE_VIEW),
    ("template", "cpus", TEMPLATE_VIEW),
    ("template", "memory", TEMPLATE_VIEW),
    ("template", "disk", TEMPLATE_VIEW),
    ("template", "display", TEMPLATE_VIEW),
    ("template", "firmware", TEMPLATE_VIEW),
    ("template", "tpm", TEMPLATE_VIEW),
    ("template", "secure_boot", TEMPLATE_VIEW),
    ("template", "nested", TEMPLATE_VIEW),
    ("template", "gui", TEMPLATE_VIEW),
    ("template", "qemu_args", TEMPLATE_VIEW),
    ("template", "first_boot", TEMPLATE_VIEW),
    ("template", "agent", TEMPLATE_VIEW),
    ("source", "kind", TEMPLATE_VIEW),
    ("source", "path", TEMPLATE_VIEW),
    ("source", "url", TEMPLATE_VIEW),
    ("source", "sha256", TEMPLATE_VIEW),
    ("source", "from", TEMPLATE_VIEW),
];

const LABEL: &str = "the block's label, edited as its name";
const RAW_ONLY: &str = "no designer control yet — raw config only";
const TEMPLATE_VIEW: &str = "template definitions are not edited by the lab designer";

/// Every form, with each field resolved against the schema.
pub fn forms() -> Vec<Form> {
    build(SchemaProjection::get()).expect("the designer's forms must resolve against the schema")
}

/// Resolve every form against a projection, reporting each field a form names
/// that the schema does not define.
pub fn build(schema: &SchemaProjection) -> Result<Vec<Form>, Vec<String>> {
    let mut out = Vec::with_capacity(FORMS.len());
    let mut errors = Vec::new();
    for spec in FORMS {
        let Some(block) = schema.block(spec.block) else {
            errors.push(format!(
                "form `{}` names block `{}`, which the schema does not define",
                spec.export, spec.block
            ));
            continue;
        };
        let mut fields = Vec::with_capacity(spec.fields.len());
        for field_spec in spec.fields {
            let Some(field) = block.field(field_spec.field) else {
                errors.push(format!(
                    "form `{}` names `{}.{}`, which the schema does not define",
                    spec.export, spec.block, field_spec.field
                ));
                continue;
            };
            let Some(control) = field_spec.control.or_else(|| Control::of(field)) else {
                errors.push(format!(
                    "form `{}` names `{}.{}`, which is a nested block, not a form field",
                    spec.export, spec.block, field_spec.field
                ));
                continue;
            };
            fields.push(FormField {
                key: field.name.clone(),
                label: field_spec
                    .label
                    .map(str::to_string)
                    .unwrap_or_else(|| sentence_case(&field.name)),
                doc: field.doc.clone(),
                control,
                options: field.options.clone(),
                placeholder: field_spec.placeholder.map(str::to_string),
                // A picker only offers "(default)" where leaving the field
                // blank genuinely leaves it unset — so a field the schema
                // requires, or one whose default the picker would otherwise
                // let the author unselect, is marked required.
                required: !field.optional || (control == Control::Enum && field.default.is_some()),
                min: field.min,
                max: field.max,
            });
        }
        out.push(Form {
            export: spec.export,
            block: spec.block,
            variant: spec.variant,
            single: spec.single,
            fields,
        });
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

/// `secure_boot` → `Secure boot`.
fn sentence_case(name: &str) -> String {
    let mut out = name.replace('_', " ");
    if let Some(first) = out.get_mut(0..1) {
        first.make_ascii_uppercase();
    }
    out
}

// --- the generated console artefact -----------------------------------------

/// The committed console artefact, rendered from the projection.
///
/// Regenerate with `just schema-gen`; `console_artefact_is_current` fails when
/// the committed copy no longer matches.
pub const CONSOLE_ARTEFACT: &str = "web-ui/src/editor/schema.gen.ts";

/// Render the descriptor tables the console imports.
pub fn render_typescript(schema: &SchemaProjection, forms: &[Form]) -> String {
    let mut out = String::new();
    out.push_str(
        "// GENERATED — do not edit. Run `just schema-gen` after changing\n\
         // src/config/schema.wcl or src/config/designer.rs.\n\
         //\n\
         // The Schema projection (ADR-0005) as the console consumes it: the\n\
         // inspector's field descriptors, with help text, option lists, bounds\n\
         // and defaults reflected from src/config/schema.wcl. Nothing here is\n\
         // hand-maintained, so nothing here can drift from the schema.\n\n",
    );
    out.push_str(
        "export type FieldType =\n\
         \x20 | \"text\" // utf8 → Input\n\
         \x20 | \"int\" // i64 / duration in seconds → Input[number]\n\
         \x20 | \"bool3\" // bool? with an inherited default → default/on/off ToggleGroup\n\
         \x20 | \"flag\" // plain bool → Toggle\n\
         \x20 | \"bytes\" // std.ByteSize → ByteSizeInput\n\
         \x20 | \"enum\" // closed option list → Select\n\
         \x20 | \"lines\" // list<utf8> → Textarea, one per line\n\
         \x20 | \"segref\" // one segment name\n\
         \x20 | \"segrefs\" // several segment names\n\
         \x20 | \"vmref\" // one VM name\n\
         \x20 | \"vmrefs\" // several VM names\n\
         \x20 | \"event\"; // lifecycle event picker\n\n",
    );
    out.push_str(
        "export interface FieldDesc {\n\
         \x20 key: string;\n\
         \x20 label: string;\n\
         \x20 /** The schema field's `@doc` text, verbatim. */\n\
         \x20 doc: string;\n\
         \x20 type: FieldType;\n\
         \x20 options?: string[];\n\
         \x20 placeholder?: string;\n\
         \x20 /** The schema requires a value or supplies one, so a picker\n\
         \x20  *  offers no \"(default)\" choice. */\n\
         \x20 required?: boolean;\n\
         \x20 min?: number;\n\
         \x20 max?: number;\n\
         \x20 /** The schema default, in WCL source form, when it declares one. */\n\
         \x20 default?: string;\n\
         }\n\n\
         export interface Section {\n\
         \x20 title: string;\n\
         \x20 fields: FieldDesc[];\n\
         }\n\n",
    );

    // Enum option lists, keyed by `block.field` — the console's pickers read
    // these instead of re-typing the sets.
    out.push_str(
        "/** Every closed option list in the schema, keyed `block.field`. */\n\
         export const SCHEMA_OPTIONS: Record<string, string[]> = {\n",
    );
    for block in &schema.blocks {
        for field in &block.fields {
            if field.options.is_empty() {
                continue;
            }
            let _ = writeln!(
                out,
                "  \"{}.{}\": [{}],",
                block.kind,
                field.name,
                field
                    .options
                    .iter()
                    .map(|o| format!("{o:?}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
    }
    out.push_str("};\n\n");

    // Scalar defaults in base units — seconds for a duration, bytes for a
    // byte size — so the console computes with the schema's numbers.
    out.push_str(
        "/** Schema defaults in base units (seconds / bytes / count), keyed `block.field`. */\n\
         export const SCHEMA_DEFAULTS: Record<string, number> = {\n",
    );
    for block in &schema.blocks {
        for field in &block.fields {
            let Some(number) = field.default_number else {
                continue;
            };
            let _ = writeln!(out, "  \"{}.{}\": {number},", block.kind, field.name);
        }
    }
    out.push_str("};\n\n");

    // "Exactly one of" rules, so the console's edit operations consume the
    // schema's rule instead of restating it.
    out.push_str(
        "/** `@one_of` rules: exactly one field of each group must be set. */\n\
         export const ONE_OF: Record<string, string[][]> = {\n",
    );
    for block in &schema.blocks {
        if block.one_of.is_empty() {
            continue;
        }
        let groups = block
            .one_of
            .iter()
            .map(|group| {
                format!(
                    "[{}]",
                    group
                        .iter()
                        .map(|n| format!("{n:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "  {:?}: [{groups}],", block.kind);
    }
    out.push_str("};\n\n");

    // The descriptor tables.
    let mut variants: Vec<&Form> = Vec::new();
    for form in forms {
        if form.variant.is_some() {
            variants.push(form);
            continue;
        }
        let block = schema.block(form.block);
        if form.single {
            let _ = write!(
                out,
                "/** `{}.{}` — the selector the other tables key off. */\nexport const {}: FieldDesc = ",
                form.block,
                form.fields.first().map(|f| f.key.as_str()).unwrap_or(""),
                form.export
            );
            out.push_str(&render_field(&form.fields[0], block));
            out.push_str(";\n\n");
        } else {
            let _ = writeln!(
                out,
                "/** `{}` block fields. */\nexport const {}: FieldDesc[] = [",
                form.block, form.export
            );
            for field in &form.fields {
                let _ = writeln!(out, "  {},", render_field(field, block));
            }
            out.push_str("];\n\n");
        }
    }

    // Grouped tables (per auth method), emitted as one record per export.
    let mut seen: Vec<&str> = Vec::new();
    for form in &variants {
        if seen.contains(&form.export) {
            continue;
        }
        seen.push(form.export);
        let _ = writeln!(
            out,
            "/** `{}` block fields, grouped by `{}`. */\nexport const {}: Record<string, FieldDesc[]> = {{",
            form.block,
            variants
                .iter()
                .find(|f| f.export == form.export)
                .map(|_| "method")
                .unwrap_or("variant"),
            form.export
        );
        for grouped in variants.iter().filter(|f| f.export == form.export) {
            let block = schema.block(grouped.block);
            let _ = writeln!(out, "  {}: [", grouped.variant.unwrap_or_default());
            for field in &grouped.fields {
                let _ = writeln!(out, "    {},", render_field(field, block));
            }
            out.push_str("  ],\n");
        }
        out.push_str("};\n\n");
    }

    out
}

fn render_field(field: &FormField, block: Option<&super::projection::Block>) -> String {
    let mut parts = vec![
        format!("key: {:?}", field.key),
        format!("label: {:?}", field.label),
        format!("doc: {:?}", field.doc),
        format!("type: {:?}", field.control.as_str()),
    ];
    if !field.options.is_empty() {
        parts.push(format!(
            "options: [{}]",
            field
                .options
                .iter()
                .map(|o| format!("{o:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if let Some(placeholder) = &field.placeholder {
        parts.push(format!("placeholder: {placeholder:?}"));
    }
    if field.required {
        parts.push("required: true".to_string());
    }
    if let Some(min) = field.min {
        parts.push(format!("min: {min}"));
    }
    if let Some(max) = field.max {
        parts.push(format!("max: {max}"));
    }
    if let Some(default) = block
        .and_then(|b| b.field(&field.key))
        .and_then(|f| f.default.as_deref())
    {
        parts.push(format!("default: {default:?}"));
    }
    format!("{{ {} }}", parts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    /// Story 15: a form cannot name a field the schema does not define.
    #[test]
    fn forms_resolve_against_the_schema() {
        if let Err(errors) = build(SchemaProjection::get()) {
            panic!("{}", errors.join("\n"));
        }
    }

    /// Story 16: a schema field cannot be added and forgotten. Every field is
    /// either in a form, a nested-block slot, or an explicit exception.
    #[test]
    fn designer_covers_the_schema() {
        let schema = SchemaProjection::get();
        let forms = forms();
        let mut missing = Vec::new();
        for block in &schema.blocks {
            for field in &block.fields {
                if field.child.is_some() {
                    continue; // rendered as a child block list
                }
                let in_form = forms.iter().any(|form| {
                    form.block == block.kind && form.fields.iter().any(|f| f.key == field.name)
                });
                let excused = UNFORMED
                    .iter()
                    .any(|(kind, name, _)| *kind == block.kind && *name == field.name);
                if !in_form && !excused {
                    missing.push(format!("{}.{}", block.kind, field.name));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "schema fields the designer cannot reach — put each in a form or in UNFORMED with a \
             reason:\n  {}",
            missing.join("\n  ")
        );
    }

    /// An exception must name a real field, so removing a schema field forces
    /// removing its exception.
    #[test]
    fn exceptions_name_real_fields() {
        let schema = SchemaProjection::get();
        for (kind, name, reason) in UNFORMED {
            let block = schema
                .block(kind)
                .unwrap_or_else(|| panic!("UNFORMED names unknown block `{kind}`"));
            assert!(
                block.field(name).is_some(),
                "UNFORMED names unknown field `{kind}.{name}`"
            );
            assert!(!reason.is_empty(), "`{kind}.{name}` needs a reason");
        }
    }

    /// Help prose has one source: the schema's `@doc`.
    #[test]
    fn help_text_is_the_schema_doc() {
        let schema = SchemaProjection::get();
        for form in forms() {
            let block = schema.block(form.block).expect("form block");
            for field in &form.fields {
                assert_eq!(
                    field.doc,
                    block.field(&field.key).expect("field").doc,
                    "{}.{} help text diverged from the schema",
                    form.block,
                    field.key
                );
            }
        }
    }

    /// Option lists come from the schema, so a picker cannot offer a value the
    /// validator rejects.
    #[test]
    fn pickers_offer_exactly_the_schema_options() {
        let schema = SchemaProjection::get();
        for form in forms() {
            for field in &form.fields {
                if field.control != Control::Enum {
                    continue;
                }
                assert_eq!(
                    field.options,
                    schema.options(form.block, &field.key),
                    "{}.{}",
                    form.block,
                    field.key
                );
                assert!(
                    !field.options.is_empty(),
                    "{}.{} renders a picker with no options",
                    form.block,
                    field.key
                );
            }
        }
    }

    #[test]
    fn an_override_for_an_unknown_field_is_an_error() {
        let schema = SchemaProjection::reflect(
            r#"
@document type Doc { @children("gizmo") gizmos: list<Gizmo> }
@block("gizmo") type Gizmo { @doc("Real") real: utf8? }
"#,
            "test.wcl",
        )
        .expect("reflect");
        // `build` walks the real FORMS table, so drive the same check with a
        // schema that has none of its blocks: every form must be reported.
        let errors = build(&schema).expect_err("a schema without these blocks must fail");
        assert!(
            errors.iter().all(|e| e.contains("does not define")),
            "{errors:?}"
        );
    }

    #[test]
    fn labels_default_to_the_field_name() {
        assert_eq!(sentence_case("secure_boot"), "Secure boot");
        assert_eq!(sentence_case("subnet"), "Subnet");
    }

    /// The committed console artefact is what the projection renders today.
    /// Set `VMLAB_BLESS=1` (or run `just schema-gen`) to rewrite it.
    #[test]
    fn console_artefact_is_current() {
        let rendered = render_typescript(SchemaProjection::get(), &forms());
        let path = repo_root().join(CONSOLE_ARTEFACT);
        if std::env::var_os("VMLAB_BLESS").is_some() {
            std::fs::write(&path, &rendered).expect("write the console artefact");
            return;
        }
        let committed = std::fs::read_to_string(&path).unwrap_or_default();
        assert_eq!(
            committed, rendered,
            "{CONSOLE_ARTEFACT} is stale — run `just schema-gen`"
        );
    }
}
