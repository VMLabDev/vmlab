//! What the protocol can do, in one place (ADR-0007).
//!
//! Because the vocabulary is enumerable, the protocol's reference
//! documentation, its coverage report, and the console's request types can all
//! be generated from it rather than restated by hand. The generated files are
//! checked in; `just proto-generate` rewrites them and `cargo test` fails when
//! they are stale, so a command added to the vocabulary cannot quietly reach
//! only half the surfaces.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{ArgSpec, CommandSpec, ErrorCode, LabRequest, SupRequest, WireRequest};

/// The generated protocol reference and coverage report.
pub const MARKDOWN_PATH: &str = "docs/protocol.md";
/// The generated console-side protocol types.
pub const TYPESCRIPT_PATH: &str = "web-ui/src/protocol.ts";

const GENERATED_BANNER: &str = "generated from `src/proto/vocab.rs` — run `just proto-generate`";

/// A caller of the protocol: a directory tree whose sources construct
/// requests. Which surface reaches which command is the coverage report.
pub struct Surface {
    /// How the report names it.
    pub name: &'static str,
    /// Repo-relative paths to scan, each with the vocabulary that tree
    /// *serves* rather than calls — a daemon's own dispatch names every one of
    /// its commands, and that is a handler, not a caller.
    pub roots: &'static [(&'static str, Option<&'static str>)],
    pub blurb: &'static str,
}

/// Every surface the report accounts for. The console is not scanned
/// separately: it reaches the daemon only through the REST layer, so what
/// `web` calls is exactly what a console user can reach.
pub const SURFACES: &[Surface] = &[
    Surface {
        name: "cli",
        roots: &[("src/cli", None), ("src/template/cli.rs", None)],
        blurb: "the `vmlab` verb surface",
    },
    Surface {
        name: "web",
        roots: &[("src/web", None)],
        blurb: "the REST/WebSocket API, and so the console",
    },
    Surface {
        name: "daemon",
        roots: &[
            ("src/labd", Some("LabRequest")),
            ("src/supervisor", Some("SupRequest")),
        ],
        blurb: "one daemon calling another",
    },
];

/// The REST endpoints that project a slice of the vocabulary onto a path
/// segment, as `(segment, wire command)`.
///
/// The console's action unions are generated from these, so it no longer
/// declares its own. Every command named here is checked against the
/// vocabulary, which is what stops the two drifting apart.
pub const LAB_ACTIONS: &[(&str, &str)] = &[
    ("up", "up"),
    ("down", "down"),
    ("destroy", "destroy"),
    ("pull", "pull"),
];

/// As [`LAB_ACTIONS`], for `POST /api/labs/{lab}/machines/{machine}/{action}`.
pub const MACHINE_ACTIONS: &[(&str, &str)] = &[
    ("start", "machine.start"),
    ("stop", "machine.stop"),
    ("restart", "machine.restart"),
    ("destroy", "machine.destroy"),
];

/// Which surfaces call each command, keyed by `Vocabulary::Variant`.
pub type Usage = BTreeMap<String, BTreeSet<&'static str>>;

/// Scan the repo for constructed requests.
///
/// A surface constructs `LabRequest::MachineStart { .. }`, so the variant name
/// is the search key. This is a text scan rather than a compiler query, which
/// is why the vocabulary uses one distinctive spelling per command.
pub fn command_usage(repo: &Path) -> Usage {
    let mut usage: Usage = BTreeMap::new();
    for spec in LabRequest::COMMANDS.iter().chain(SupRequest::COMMANDS) {
        usage.entry(spec.variant.to_string()).or_default();
    }
    for surface in SURFACES {
        for (root, serves) in surface.roots {
            let mut text = String::new();
            collect_rust_sources(&repo.join(root), &mut text);
            for (tier, specs) in [
                ("LabRequest", LabRequest::COMMANDS),
                ("SupRequest", SupRequest::COMMANDS),
            ] {
                if *serves == Some(tier) {
                    continue;
                }
                for spec in specs {
                    if text.contains(&format!("{tier}::{}", spec.variant)) {
                        usage
                            .entry(spec.variant.to_string())
                            .or_default()
                            .insert(surface.name);
                    }
                }
            }
        }
    }
    usage
}

fn collect_rust_sources(path: &Path, out: &mut String) {
    if path.is_file() {
        if path.extension().is_some_and(|e| e == "rs")
            && let Ok(text) = std::fs::read_to_string(path)
        {
            out.push_str(&text);
        }
        return;
    }
    let Ok(dir) = std::fs::read_dir(path) else {
        return;
    };
    let mut entries: Vec<_> = dir.filter_map(Result::ok).map(|e| e.path()).collect();
    entries.sort();
    for entry in entries {
        collect_rust_sources(&entry, out);
    }
}

/// `stringify!` on a type puts spaces around punctuation; the report and the
/// generated TypeScript both want it back the way it was written.
fn tidy_type(ty: &str) -> String {
    ty.replace(" :: ", "::")
        .replace(" < ", "<")
        .replace(" > ", ">")
        .replace(" >", ">")
        .replace("< ", "<")
        .replace(" ,", ",")
}

fn arg_list(args: &[ArgSpec]) -> String {
    if args.is_empty() {
        return "—".to_string();
    }
    args.iter()
        .map(|a| format!("`{}: {}`", a.name, tidy_type(a.ty)))
        .collect::<Vec<_>>()
        .join(", ")
}

fn first_doc_line(spec: &CommandSpec) -> String {
    spec.doc
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// The protocol reference plus the coverage report, as Markdown.
pub fn protocol_markdown(repo: &Path) -> String {
    let usage = command_usage(repo);
    let mut out = String::new();
    out.push_str("# The vmlab wire protocol\n\n");
    out.push_str(&format!("<!-- {GENERATED_BANNER} -->\n\n"));
    out.push_str(
        "JSON lines over a unix socket: a request is a `cmd` string and an `args` object, and a\n\
         reply carries either `ok` or an `err` message with a machine-readable `code`. Callers\n\
         inside this repo construct requests through the vocabulary in `src/proto/vocab.rs`\n\
         rather than spelling the strings; the strings are what goes on the wire, and are what an\n\
         out-of-repo client writes.\n\n",
    );

    out.push_str("## Error codes\n\n");
    out.push_str("A failure says why by code. The message is prose and may be reworded freely;\n");
    out.push_str("the code is the contract.\n\n");
    out.push_str("| code | HTTP status | `vmlab` exit code |\n|---|---|---|\n");
    for code in ErrorCode::ALL {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            code.as_str(),
            http_status_name(*code),
            code.exit_code(),
        ));
    }
    out.push('\n');

    for (title, specs) in [
        ("The supervisor socket (`vmlabd`)", SupRequest::COMMANDS),
        ("A lab daemon's socket", LabRequest::COMMANDS),
    ] {
        out.push_str(&format!("## {title}\n\n"));
        out.push_str("| command | arguments | called by | what it does |\n|---|---|---|---|\n");
        for spec in specs {
            let callers = usage
                .get(spec.variant)
                .map(|s| {
                    if s.is_empty() {
                        "**nothing**".to_string()
                    } else {
                        s.iter()
                            .map(|n| format!("`{n}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    }
                })
                .unwrap_or_default();
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                spec.cmd,
                arg_list(spec.args),
                callers,
                first_doc_line(spec),
            ));
        }
        out.push('\n');
    }

    out.push_str("## Coverage\n\n");
    for surface in SURFACES {
        let roots: Vec<&str> = surface.roots.iter().map(|(root, _)| *root).collect();
        out.push_str(&format!(
            "- `{}` — {} (`{}`)\n",
            surface.name,
            surface.blurb,
            roots.join("`, `"),
        ));
    }
    out.push('\n');
    out.push_str(
        "Asymmetry is not automatically wrong — some commands only make sense from one place.\n\
         The lists below exist so that each one is a decision somebody made rather than a gap\n\
         nobody noticed. A command that carries its reason declares it in the vocabulary, beside\n\
         its doc comment; a command listed bare is one nobody has decided about yet.\n\n",
    );

    let by_caller = |want: &dyn Fn(&BTreeSet<&'static str>) -> bool| -> Vec<&CommandSpec> {
        LabRequest::COMMANDS
            .iter()
            .chain(SupRequest::COMMANDS)
            .filter(|spec| usage.get(spec.variant).is_some_and(want))
            .collect()
    };

    let orphans = by_caller(&|s| s.is_empty());
    out.push_str("### Reachable from no surface\n\n");
    push_list(&mut out, &orphans, "Every command has a caller.");

    for surface in SURFACES {
        let only = by_caller(&|s| s.len() == 1 && s.contains(surface.name));
        out.push_str(&format!("### Reachable only from `{}`\n\n", surface.name));
        push_list(
            &mut out,
            &only,
            &format!("Nothing is exclusive to `{}`.", surface.name),
        );
    }

    out.push_str("## REST action segments\n\n");
    out.push_str(
        "The REST layer projects a slice of the vocabulary onto URL path segments. The console's\n\
         action types are generated from these, so it holds no command list of its own.\n\n",
    );
    for (title, actions) in [
        ("`POST /api/labs/{lab}/{action}`", LAB_ACTIONS),
        (
            "`POST /api/labs/{lab}/machines/{machine}/{action}`",
            MACHINE_ACTIONS,
        ),
    ] {
        out.push_str(&format!("{title}\n\n| segment | command |\n|---|---|\n"));
        for (segment, cmd) in actions {
            out.push_str(&format!("| `{segment}` | `{cmd}` |\n"));
        }
        out.push('\n');
    }
    out
}

/// One command per line, each with its reason when it has one.
fn push_list(out: &mut String, specs: &[&CommandSpec], empty: &str) {
    if specs.is_empty() {
        out.push_str(empty);
        out.push_str("\n\n");
        return;
    }
    for spec in specs {
        match spec.one_way {
            Some(one_way) => out.push_str(&format!("- `{}` — {}\n", spec.cmd, one_way.why)),
            None => out.push_str(&format!("- `{}`\n", spec.cmd)),
        }
    }
    out.push('\n');
}

/// `404` as `404 Not Found`, for the reference table. The number itself comes
/// from [`ErrorCode::http_status`] — the reason phrase is presentation.
fn http_status_name(code: ErrorCode) -> String {
    let status = code.http_status();
    let phrase = match status {
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        _ => "",
    };
    format!("{status} {phrase}").trim_end().to_string()
}

/// The console's protocol types, generated from the vocabulary.
///
/// The console speaks REST, not the wire, so what it needs from the protocol
/// is the error codes it branches on and the action segments it posts to —
/// both of which used to be hand-written string unions in `api.ts`.
pub fn protocol_typescript() -> String {
    let mut out = String::new();
    out.push_str(&format!("// {GENERATED_BANNER}\n"));
    out.push_str("//\n// Edit the vocabulary, not this file.\n\n");

    out.push_str(
        "/** Why a request failed. The daemon sends this alongside the message, and the REST\n\
         \u{20}*  layer maps it to the HTTP status — so branch on the code, never on the prose. */\n",
    );
    out.push_str("export type ErrorCode =\n");
    for code in ErrorCode::ALL {
        out.push_str(&format!("  | \"{}\"\n", code.as_str()));
    }
    out.push_str(";\n\n");

    out.push_str("/** An error body from any `/api` endpoint. */\n");
    out.push_str("export interface ApiError {\n  error: string;\n  code?: ErrorCode;\n}\n\n");

    for (name, actions, doc) in [
        (
            "LabAction",
            LAB_ACTIONS,
            "Lab-wide actions: `POST /api/labs/{lab}/{action}`.",
        ),
        (
            "MachineAction",
            MACHINE_ACTIONS,
            "Per-machine actions: `POST /api/labs/{lab}/machines/{machine}/{action}`.",
        ),
    ] {
        out.push_str(&format!("/** {doc} */\nexport type {name} =\n"));
        for (segment, cmd) in actions {
            out.push_str(&format!("  /** `{cmd}` */\n  | \"{segment}\"\n"));
        }
        out.push_str(";\n\n");
    }

    for (name, specs) in [
        ("LabCommand", LabRequest::COMMANDS),
        ("SupervisorCommand", SupRequest::COMMANDS),
    ] {
        out.push_str(&format!(
            "/** Every command the {} socket serves. */\nexport type {name} =\n",
            if name == "LabCommand" {
                "lab daemon's"
            } else {
                "supervisor"
            }
        ));
        for spec in specs {
            out.push_str(&format!("  | \"{}\"\n", spec.cmd));
        }
        out.push_str(";\n\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    /// The generated files are checked in so a reader (and a reviewer) sees
    /// the protocol without running anything, and so a command that reaches
    /// only one surface shows up in review as a diff.
    ///
    /// `just proto-generate` sets `VMLAB_WRITE_PROTOCOL_DOCS` and rewrites
    /// them; without it this only checks.
    #[test]
    fn generated_artefacts_are_current() {
        let write = std::env::var_os("VMLAB_WRITE_PROTOCOL_DOCS").is_some();
        for (path, want) in [
            (MARKDOWN_PATH, protocol_markdown(repo())),
            (TYPESCRIPT_PATH, protocol_typescript()),
        ] {
            let full = repo().join(path);
            if write {
                std::fs::write(&full, &want).expect("writing the generated file");
                continue;
            }
            let on_disk = std::fs::read_to_string(&full).unwrap_or_default();
            assert_eq!(on_disk, want, "{path} is stale — run `just proto-generate`");
        }
    }

    /// The REST action tables name real commands. Rename one in the
    /// vocabulary and this fails, rather than the console silently posting to
    /// an endpoint that no longer maps anywhere.
    #[test]
    fn rest_action_tables_name_real_commands() {
        for (segment, cmd) in LAB_ACTIONS.iter().chain(MACHINE_ACTIONS) {
            assert!(
                LabRequest::spec(cmd).is_some(),
                "action `{segment}` maps to `{cmd}`, which is not in the lab vocabulary"
            );
        }
    }

    /// A reason names the surface it claims the command is reachable from, so
    /// a renamed or removed surface cannot leave a reason pointing at nothing.
    #[test]
    fn a_one_way_annotation_names_a_real_surface() {
        for spec in LabRequest::COMMANDS.iter().chain(SupRequest::COMMANDS) {
            let Some(one_way) = spec.one_way else {
                continue;
            };
            assert!(
                SURFACES.iter().any(|s| s.name == one_way.surface),
                "`{}` is annotated for surface `{}`, which does not exist",
                spec.cmd,
                one_way.surface,
            );
        }
    }

    /// An annotation asserts an asymmetry, so the asymmetry has to still be
    /// there. Give an annotated command a second caller and this fails, rather
    /// than leaving a reason behind that explains something no longer true.
    #[test]
    fn an_annotated_command_is_still_one_way() {
        let usage = command_usage(repo());
        for spec in LabRequest::COMMANDS.iter().chain(SupRequest::COMMANDS) {
            let Some(one_way) = spec.one_way else {
                continue;
            };
            let callers = &usage[spec.variant];
            assert!(
                callers.len() == 1 && callers.contains(one_way.surface),
                "`{}` says it is reachable only from `{}`, but is called by {:?}",
                spec.cmd,
                one_way.surface,
                callers,
            );
        }
    }

    /// The report is only worth reading if it reflects real callers, so check
    /// the scan against a command each surface is known to make.
    #[test]
    fn the_usage_scan_finds_real_callers() {
        let usage = command_usage(repo());
        assert!(usage["Up"].contains("cli"), "the CLI brings labs up");
        assert!(usage["MachineStart"].contains("web"), "so does the console");
        assert!(
            usage["GlobalAttach"].contains("daemon"),
            "a lab daemon attaches its own global segments"
        );
        // Nothing should be missing from the map: every command has an entry,
        // even when the set behind it is empty.
        for spec in LabRequest::COMMANDS.iter().chain(SupRequest::COMMANDS) {
            assert!(usage.contains_key(spec.variant), "{}", spec.cmd);
        }
    }
}
