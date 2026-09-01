//! What the protocol can do, in one place (ADR-0007).
//!
//! Because the vocabulary is enumerable, the protocol's reference
//! documentation and its coverage report can both be generated from it rather
//! than restated by hand. The generated file is checked in; `just
//! proto-generate` rewrites it and `cargo test` fails when it is stale, so a
//! command added to the vocabulary cannot quietly reach only half the
//! surfaces.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{ArgSpec, CommandSpec, ErrorCode, LabRequest, OneWay, SupRequest, WireRequest};

/// The generated protocol reference and coverage report.
pub const MARKDOWN_PATH: &str = "docs/protocol.md";

const GENERATED_BANNER: &str = "generated from `src/proto/vocab.rs` — run `just proto-generate`";

/// Where an open gap's tracking issue lives, so the report can link it.
const ISSUES_URL: &str = "https://github.com/VMLabDev/vmlab/issues";

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

/// Every surface the report accounts for.
pub const SURFACES: &[Surface] = &[
    Surface {
        name: "cli",
        roots: &[("src/cli", None), ("src/template/cli.rs", None)],
        blurb: "the `vmlab` verb surface",
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

/// `stringify!` on a type puts spaces around punctuation; the report wants it
/// back the way it was written.
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
         nobody noticed. Every command reachable from a single surface says which it is, beside\n\
         its declaration in the vocabulary, and the build fails while one says neither — so the\n\
         open gaps below are a worklist rather than a list somebody has to re-derive.\n\n",
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
    if orphans.is_empty() {
        out.push_str("Every command has a caller.\n\n");
    } else {
        // No annotation applies: a command nothing calls has no asymmetry to
        // explain, only a caller to find.
        for spec in &orphans {
            out.push_str(&bullet(spec, ""));
        }
        out.push('\n');
    }

    for surface in SURFACES {
        let only = by_caller(&|s| s.len() == 1 && s.contains(surface.name));
        out.push_str(&format!("### Reachable only from `{}`\n\n", surface.name));
        if only.is_empty() {
            out.push_str(&format!("Nothing is exclusive to `{}`.\n\n", surface.name));
            continue;
        }
        push_one_way_lists(&mut out, &only);
    }
    out
}

/// One command as a list item, with whatever the list says about it after the
/// name.
fn bullet(spec: &CommandSpec, tail: &str) -> String {
    format!("- `{}`{tail}\n", spec.cmd)
}

/// One surface's one-way commands, as the lists they divide into.
///
/// Deliberate asymmetries and open gaps are rendered apart because telling
/// them apart is what the report is for: one list is settled and the other is
/// work. Reading them as one list is what made this exercise happen three
/// times.
fn push_one_way_lists(out: &mut String, specs: &[&CommandSpec]) {
    let (mut deliberate, mut gaps, mut bare) = (String::new(), String::new(), String::new());
    for spec in specs {
        match spec.one_way {
            Some(OneWay::Deliberate { why, .. }) => {
                deliberate.push_str(&bullet(spec, &format!(" — {why}")));
            }
            Some(OneWay::Gap { issue, .. }) => {
                gaps.push_str(&bullet(
                    spec,
                    &format!(" — tracked in [#{issue}]({ISSUES_URL}/{issue})"),
                ));
            }
            None => bare.push_str(&bullet(spec, "")),
        }
    }
    for (lead, list) in [
        (
            "Deliberate, with the reason recorded beside the declaration:",
            &deliberate,
        ),
        (
            "Open gaps — nobody wrote the other half, and each is tracked:",
            &gaps,
        ),
        // Empty in any tree that passes `every_one_way_command_records_why`.
        // Rendered rather than dropped so that generating the report before
        // running the test shows the same worklist the test is about to name.
        ("Neither, which the build rejects:", &bare),
    ] {
        if list.is_empty() {
            continue;
        }
        out.push_str(&format!("{lead}\n\n{list}\n"));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::OneWay;

    fn repo() -> &'static Path {
        Path::new(env!("CARGO_MANIFEST_DIR"))
    }

    /// The generated file is checked in so a reader (and a reviewer) sees
    /// the protocol without running anything, and so a command that reaches
    /// only one surface shows up in review as a diff.
    ///
    /// `just proto-generate` sets `VMLAB_WRITE_PROTOCOL_DOCS` and rewrites
    /// it; without it this only checks.
    #[test]
    fn generated_artefacts_are_current() {
        let write = std::env::var_os("VMLAB_WRITE_PROTOCOL_DOCS").is_some();
        let (path, want) = (MARKDOWN_PATH, protocol_markdown(repo()));
        let full = repo().join(path);
        if write {
            std::fs::write(&full, &want).expect("writing the generated file");
            return;
        }
        let on_disk = std::fs::read_to_string(&full).unwrap_or_default();
        assert_eq!(on_disk, want, "{path} is stale — run `just proto-generate`");
    }

    /// Every command that claims to be one-way, with the claim.
    fn annotated() -> impl Iterator<Item = (&'static CommandSpec, OneWay)> {
        LabRequest::COMMANDS
            .iter()
            .chain(SupRequest::COMMANDS)
            .filter_map(|spec| spec.one_way.map(|one_way| (spec, one_way)))
    }

    /// An annotation of either kind names the surface it claims the command is
    /// reachable from, so a renamed or removed surface cannot leave one
    /// pointing at nothing.
    #[test]
    fn a_one_way_annotation_names_a_real_surface() {
        for (spec, one_way) in annotated() {
            assert!(
                SURFACES.iter().any(|s| s.name == one_way.surface()),
                "`{}` is annotated for surface `{}`, which does not exist",
                spec.cmd,
                one_way.surface(),
            );
        }
    }

    /// An annotation of either kind asserts an asymmetry, so the asymmetry has
    /// to still be there. Give an annotated command a second caller and this
    /// fails, rather than leaving behind a reason that explains something no
    /// longer true, or a gap the report advertises as open after it closed.
    #[test]
    fn an_annotated_command_is_still_one_way() {
        let usage = command_usage(repo());
        for (spec, one_way) in annotated() {
            let callers = &usage[spec.variant];
            assert!(
                callers.len() == 1 && callers.contains(one_way.surface()),
                "`{}` says it is reachable only from `{}`, but is called by {:?}",
                spec.cmd,
                one_way.surface(),
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
