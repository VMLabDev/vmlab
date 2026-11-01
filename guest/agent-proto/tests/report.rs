//! Generate the wire inventory from the Rust syntax tree, including serde defaults.

use std::fmt::Write;
use std::path::Path;

use syn::{Attribute, Expr, Fields, GenericArgument, Item, Lit, Meta, PathArguments, Type};

const START: &str = "<!-- agent-proto tables begin -->";
const END: &str = "<!-- agent-proto tables end -->";

fn serde_value(attrs: &[Attribute], key: &str) -> Option<String> {
    let mut value = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("serde")) {
        attr.parse_nested_meta(|meta| {
            let name = meta.path.get_ident().unwrap().to_string();
            let v = if meta.input.peek(syn::Token![=]) {
                meta.value()?.parse::<syn::LitStr>()?.value()
            } else {
                String::new()
            };
            if name == key {
                value = Some(v);
            }
            Ok(())
        })
        .expect("supported serde attribute");
    }
    value
}

fn docs(attrs: &[Attribute]) -> String {
    attrs
        .iter()
        .filter_map(|a| match &a.meta {
            Meta::NameValue(m) if m.path.is_ident("doc") => match &m.value {
                Expr::Lit(e) => match &e.lit {
                    Lit::Str(s) => Some(s.value().trim().replace('|', "\\|")),
                    _ => None,
                },
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn ty(t: &Type) -> String {
    match t {
        Type::Path(p) => {
            let segment = p.path.segments.last().unwrap();
            let name = segment.ident.to_string();
            if let PathArguments::AngleBracketed(args) = &segment.arguments {
                let inner = args
                    .args
                    .iter()
                    .filter_map(|a| match a {
                        GenericArgument::Type(t) => Some(ty(t)),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                return match name.as_str() {
                    "Option" => format!("{inner} or null"),
                    "Vec" => format!("array<{inner}>"),
                    _ => panic!("undocumented generic {name}"),
                };
            }
            match name.as_str() {
                "String" => "string".into(),
                "bool" => "boolean".into(),
                "u16" => "uint16".into(),
                "u32" => "uint32".into(),
                "u64" => "uint64".into(),
                "i32" => "int32".into(),
                "i64" => "int64".into(),
                "f32" => "float32".into(),
                _ => name,
            }
        }
        Type::Tuple(t) => format!(
            "[{}]",
            t.elems.iter().map(ty).collect::<Vec<_>>().join(", ")
        ),
        _ => panic!("undocumented wire type"),
    }
}

fn snake(name: &str) -> String {
    let mut result = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.extend(c.to_lowercase());
    }
    result
}

fn fields(out: &mut String, fields: &Fields) {
    if fields.is_empty() {
        out.push_str("No additional fields.\n\n");
        return;
    }
    out.push_str("| Field | JSON type | When absent | Meaning |\n|---|---|---|---|\n");
    for f in fields {
        let name = serde_value(&f.attrs, "rename")
            .unwrap_or_else(|| f.ident.as_ref().expect("named wire field").to_string());
        let t = ty(&f.ty);
        let flattened = serde_value(&f.attrs, "flatten").is_some();
        let default = serde_value(&f.attrs, "default");
        assert!(
            default.as_ref().is_none_or(String::is_empty),
            "generator must document custom default functions"
        );
        let absent = if flattened {
            "flattened into parent".to_string()
        } else if t.ends_with(" or null") {
            "null".into()
        } else if default.is_some() {
            match t.as_str() {
                "boolean" => "false".into(),
                s if s.starts_with("array<") => "[]".into(),
                "OpenFlags" => "all flags false".into(),
                _ => panic!("undocumented default for {name}"),
            }
        } else {
            "required".into()
        };
        writeln!(out, "| `{name}` | `{t}` | {absent} | {} |", docs(&f.attrs)).unwrap();
    }
    out.push('\n');
}

fn inventory(root: &Path) -> String {
    let mut out = format!(
        "{START}\n\n## Generated wire vocabulary\n\n\
        Generated from every serializable public type in `guest/agent-proto/src`\n\
        except the guest-internal `ContainerConfig`. `just agent-proto-generate`\n\
        refreshes this section. Integer widths describe JSON number ranges,\n\
        not binary encodings. Arrays of pairs encode environment entries.\n\n"
    );
    use vmlab_agent_proto as wire;
    out.push_str("### Protocol constants\n\n| Constant | Value |\n|---|---|\n");
    for (name, value) in [
        ("Protocol version", wire::PROTO_VERSION.to_string()),
        ("Port name", wire::PORT_NAME.to_string()),
        ("Maximum outer payload bytes", wire::MAX_PAYLOAD.to_string()),
        (
            "Initial channel credit bytes",
            wire::INITIAL_WINDOW.to_string(),
        ),
        (
            "Replenishment threshold bytes",
            wire::WINDOW_REPLENISH.to_string(),
        ),
        (
            "Maximum fileops metadata bytes",
            wire::fileops::MAX_META.to_string(),
        ),
        (
            "Maximum fileops raw data bytes",
            wire::fileops::MAX_DATA.to_string(),
        ),
        (
            "Directory entries per reply",
            wire::fileops::READDIR_CHUNK.to_string(),
        ),
        (
            "Watch dirty-set capacity",
            wire::watch::DIRTY_SET_CAP.to_string(),
        ),
        (
            "Maximum watch record bytes",
            wire::watch::MAX_RECORD.to_string(),
        ),
        (
            "Maximum app record bytes",
            wire::app::MAX_RECORD.to_string(),
        ),
        (
            "Parked app connection grace seconds",
            wire::app::PENDING_GRACE_SECS.to_string(),
        ),
        ("Guest RDP port", wire::app::RDP_PORT.to_string()),
    ] {
        writeln!(out, "| {name} | `{value}` |").unwrap();
    }
    out.push('\n');
    for file in ["lib.rs", "fileops.rs", "watch.rs", "app.rs"] {
        let source = std::fs::read_to_string(root.join("src").join(file)).unwrap();
        let ast = syn::parse_file(&source).expect("valid protocol source");
        for item in ast.items {
            if let Item::Mod(module) = &item
                && module.ident == "features"
            {
                out.push_str("### Feature strings\n\n| Feature | Source meaning |\n|---|---|\n");
                for item in &module.content.as_ref().expect("inline features module").1 {
                    if let Item::Const(constant) = item {
                        let Expr::Lit(value) = constant.expr.as_ref() else {
                            panic!("feature must be a string literal")
                        };
                        let Lit::Str(value) = &value.lit else {
                            panic!("feature must be a string literal")
                        };
                        writeln!(out, "| `{}` | {} |", value.value(), docs(&constant.attrs))
                            .unwrap();
                    }
                }
                out.push('\n');
            }
            let (name, attrs) = match &item {
                Item::Enum(e) => (&e.ident, &e.attrs),
                Item::Struct(s) if s.ident != "ContainerConfig" => (&s.ident, &s.attrs),
                _ => continue,
            };
            let serializable =
                attrs.iter().any(|a| {
                    a.path().is_ident("derive") && a.parse_args_with(
                    syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated
                ).unwrap().iter().any(|p| p.is_ident("Serialize"))
                });
            if !serializable {
                continue;
            }
            let module = file.strip_suffix(".rs").unwrap();
            writeln!(out, "### {module}.{name}\n").unwrap();
            match item {
                Item::Enum(e) => {
                    assert_eq!(
                        serde_value(&e.attrs, "rename_all").as_deref(),
                        Some("snake_case"),
                        "generator must be updated for a new wire casing rule"
                    );
                    let tag = serde_value(&e.attrs, "tag");
                    for v in e.variants {
                        let spelling = serde_value(&v.attrs, "rename")
                            .unwrap_or_else(|| snake(&v.ident.to_string()));
                        if let Some(tag) = &tag {
                            writeln!(out, "#### `{tag}: {spelling}`\n").unwrap();
                        } else {
                            writeln!(out, "#### `{spelling}`\n").unwrap();
                        }
                        fields(&mut out, &v.fields);
                    }
                }
                Item::Struct(s) => fields(&mut out, &s.fields),
                _ => unreachable!(),
            }
        }
    }
    writeln!(out, "{END}").unwrap();
    out
}

#[test]
fn generated_tables_are_current() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let path = root.join("../../docs/agent-protocol.md");
    let old = std::fs::read_to_string(&path).expect("agent protocol prose exists");
    let start = old.find(START).expect("start marker");
    let end = old.find(END).expect("end marker") + END.len();
    let generated = inventory(root);
    let want = format!("{}{}{}", &old[..start], generated.trim_end(), &old[end..]);
    if std::env::var_os("VMLAB_WRITE_AGENT_PROTOCOL").is_some() {
        std::fs::write(path, want).unwrap();
    } else {
        assert_eq!(
            old, want,
            "agent wire tables are stale: just agent-proto-generate"
        );
    }
}

#[test]
fn defaults_and_flattening_follow_serde() {
    let ast: syn::ItemStruct = syn::parse_quote! {
        struct Example {
            #[serde(default)] enabled: bool,
            optional: Option<u64>,
            #[serde(flatten)] op: Op,
            #[serde(rename = "wire_name")] original: String,
        }
    };
    let mut out = String::new();
    fields(&mut out, &ast.fields);
    assert!(out.contains("`enabled` | `boolean` | false"));
    assert!(out.contains("`optional` | `uint64 or null` | null"));
    assert!(out.contains("flattened into parent"));
    assert!(out.contains("`wire_name` | `string` | required"));
}
