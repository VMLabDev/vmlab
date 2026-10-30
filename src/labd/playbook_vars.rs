//! A playbook's variables on their way to config-weave, and the secrets among
//! them on their way back out (#183).
//!
//! The variables never ride the guest command line, where the guest's process
//! list shows them and every error that quotes the command repeats them: they
//! travel in a `--var-file` ([`var_file_source`]) written into a private guest
//! directory for the run and removed after it. A `secret = true` value
//! travels in a second file, handed over with `--secret-var-file`, so that
//! config-weave scrubs it from everything it writes itself — its `--log-file`
//! included — wherever the binary takes that flag ([`SecretVarFile`]). vmlab
//! keeps it out of everything the run emits regardless, by one [`Redactor`]
//! which the run engine puts between the run and every line, event, report
//! and error that leaves it.

use serde_json::Value;

use crate::config::model::PlaybookVar;

/// What a secret value is replaced with.
pub const REDACTED: &str = "***";

/// `vars` as `playbook.list` shows them: a secret's value is [`REDACTED`],
/// and `secret` says which ones are.
pub fn listed(vars: &[PlaybookVar]) -> Value {
    vars.iter()
        .map(|v| {
            serde_json::json!({
                "name": v.name,
                "value": if v.secret { REDACTED } else { v.value.as_str() },
                "secret": v.secret,
            })
        })
        .collect()
}

/// The first config-weave release that takes `--secret-var-file`
/// (config-weave#13), as (major, minor, patch). config-weave ships every
/// release as a pre-release (`0.12.2-alpha`), so the pre-release tag is not
/// compared.
pub const SECRET_VAR_FILE_SINCE: (u64, u64, u64) = (0, 12, 2);

/// Whether the config-weave binary a run uses takes `--secret-var-file`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SecretVarFile {
    /// Secret variables go in their own file, which config-weave scrubs
    /// from all of its output.
    Supported,
    /// An older binary: every variable shares the one `--var-file`, and
    /// only vmlab's [`Redactor`] stands between a secret and the output.
    /// `found` is what the binary said its version was, when it said.
    Unsupported { found: Option<String> },
}

/// The answer from `<bin> version`, the probe every run already makes
/// before it uses a binary. `None` when the output names no version vmlab
/// can read — a custom build — and only `--help` can tell
/// ([`secret_var_file_by_help`]).
pub fn secret_var_file_by_version(version_out: &str) -> Option<SecretVarFile> {
    let found = version_out
        .split_whitespace()
        .skip_while(|w| *w != "config-weave")
        .nth(1)?;
    let core = found.split(['-', '+']).next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let version = (parts.next()??, parts.next()??, parts.next()??);
    if parts.next().is_some() {
        return None;
    }
    Some(if version >= SECRET_VAR_FILE_SINCE {
        SecretVarFile::Supported
    } else {
        SecretVarFile::Unsupported {
            found: Some(found.to_string()),
        }
    })
}

/// The answer from `<bin> --help` (`None` when it could not be run): the
/// flag is there or it is not.
pub fn secret_var_file_by_help(help_out: Option<&str>) -> SecretVarFile {
    if help_out.is_some_and(|h| h.contains("--secret-var-file")) {
        SecretVarFile::Supported
    } else {
        SecretVarFile::Unsupported { found: None }
    }
}

/// The one line a run prints when it has secret variables and a binary too
/// old to take them apart, naming the release that would. `None` otherwise.
pub fn secret_var_file_notice(vars: &[PlaybookVar], support: &SecretVarFile) -> Option<String> {
    let SecretVarFile::Unsupported { found } = support else {
        return None;
    };
    if !vars.iter().any(|v| v.secret) {
        return None;
    }
    let (major, minor, patch) = SECRET_VAR_FILE_SINCE;
    let found = match found {
        Some(v) => format!("config-weave {v}"),
        None => "this config-weave".to_string(),
    };
    Some(format!(
        "{found} has no --secret-var-file, so its own logs (--log-file) do not scrub secret \
         variables; config-weave {major}.{minor}.{patch} or later does (vmlab still redacts \
         everything the run sends it)"
    ))
}

/// A run's variables, split between the two files config-weave is handed:
/// the secret ones get their own where the binary takes it, and share the
/// ordinary one where it does not. Declaration order is kept in each.
#[derive(Debug, Default)]
pub struct VarFiles<'a> {
    /// Into `--var-file`.
    pub plain: Vec<&'a PlaybookVar>,
    /// Into `--secret-var-file`.
    pub secret: Vec<&'a PlaybookVar>,
}

pub fn split_vars<'a>(vars: &'a [PlaybookVar], support: &SecretVarFile) -> VarFiles<'a> {
    let (secret, plain) = vars
        .iter()
        .partition(|v| v.secret && *support == SecretVarFile::Supported);
    VarFiles { plain, secret }
}

/// A var file for `vars`: one top-level `name = value` field each, in
/// declaration order. The same source serves `--var-file` and
/// `--secret-var-file`, which config-weave loads alike.
///
/// config-weave reads a `--var` value as a WCL expression where it can and
/// as a string otherwise, but a var file only as an expression — so a value
/// that does not evaluate on its own goes in as a quoted string, which is
/// what `--var` would have made of it.
pub fn var_file_source<'a>(vars: impl IntoIterator<Item = &'a PlaybookVar>) -> String {
    let mut out = String::from("// Written by vmlab for one config-weave run, then removed.\n");
    for var in vars {
        out.push_str(&var.name);
        out.push_str(" = ");
        match evaluate(&var.value) {
            Some(_) => out.push_str(&var.value),
            None => out.push_str(&crate::config::wcl_str(&var.value)),
        }
        out.push('\n');
    }
    out
}

/// `raw` as config-weave's `--var` rule reads it: the value of the WCL
/// expression it spells, when it is one that evaluates standalone.
fn evaluate(raw: &str) -> Option<wcl_lang::Value> {
    let expr = wcl_lang::parse_expr(raw, "<var>").ok()?;
    wcl_lang::Document::open("", "<var>")
        .ok()?
        .eval_expr(&expr)
        .ok()
}

/// The command that makes a Windows guest directory private before the var
/// file lands in it: inheritance off, and only SYSTEM (`S-1-5-18`, which the
/// agent runs as) and the Administrators group (`S-1-5-32-544`) granted
/// anything, by SID so a localised account name cannot break it. A Linux
/// guest gets the same from the directory's `0700` mode.
pub fn windows_private_dir_argv(dir: &str) -> Vec<String> {
    [
        "icacls",
        &dir.replace('/', "\\"),
        "/inheritance:r",
        "/grant:r",
        "*S-1-5-18:(OI)(CI)F",
        "*S-1-5-32-544:(OI)(CI)F",
    ]
    .map(str::to_string)
    .to_vec()
}

/// Replaces every `secret = true` variable's value with [`REDACTED`] in what
/// a playbook run emits.
///
/// A value is caught in both its spellings: as written in the lab file, and
/// as config-weave prints it once evaluated — `"hunter2"`, quotes and all,
/// reaches the guest as `hunter2`.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    /// Longest first, so a secret that contains another is replaced whole.
    secrets: Vec<String>,
}

impl Redactor {
    pub fn for_vars(vars: &[PlaybookVar]) -> Redactor {
        let mut secrets: Vec<String> = Vec::new();
        for var in vars.iter().filter(|v| v.secret) {
            secrets.push(var.value.clone());
            if let Some(
                wcl_lang::Value::Utf8(s)
                | wcl_lang::Value::Ascii(s)
                | wcl_lang::Value::Identifier(s)
                | wcl_lang::Value::Symbol(s),
            ) = evaluate(&var.value)
            {
                secrets.push(s);
            }
        }
        // An empty secret would match everywhere and hide nothing.
        secrets.retain(|s| !s.is_empty());
        secrets.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
        secrets.dedup();
        Redactor { secrets }
    }

    /// `text` with every secret in it replaced, wherever it sits — inside a
    /// longer word, a path or a quoted command line.
    pub fn scrub(&self, text: &str) -> String {
        if self.secrets.is_empty() {
            return text.to_string();
        }
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        'scan: while !rest.is_empty() {
            for secret in &self.secrets {
                if let Some(after) = rest.strip_prefix(secret.as_str()) {
                    out.push_str(REDACTED);
                    rest = after;
                    continue 'scan;
                }
            }
            let c = rest.chars().next().expect("rest is not empty");
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
        out
    }

    /// [`Self::scrub`] over every string in a JSON value — config-weave's
    /// step events and run report, whose messages and diffs may quote a
    /// variable — keys included.
    pub fn scrub_json(&self, value: Value) -> Value {
        if self.secrets.is_empty() {
            return value;
        }
        match value {
            Value::String(s) => Value::String(self.scrub(&s)),
            Value::Array(items) => {
                Value::Array(items.into_iter().map(|v| self.scrub_json(v)).collect())
            }
            Value::Object(map) => Value::Object(
                map.into_iter()
                    .map(|(k, v)| (self.scrub(&k), self.scrub_json(v)))
                    .collect(),
            ),
            other => other,
        }
    }

    /// An error with no secret anywhere in its chain comes back untouched,
    /// keeping its type (a conflict stays a conflict); one that has a secret
    /// is replaced by its scrubbed rendering.
    pub fn scrub_error(&self, err: anyhow::Error) -> anyhow::Error {
        let text = format!("{err:#}");
        let scrubbed = self.scrub(&text);
        if scrubbed == text {
            err
        } else {
            anyhow::anyhow!(scrubbed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(name: &str, value: &str, secret: bool) -> PlaybookVar {
        PlaybookVar {
            name: name.into(),
            value: value.into(),
            secret,
            span: (0, 0),
        }
    }

    /// What config-weave's `--var` makes of a raw value — the rule the var
    /// file has to reproduce (`engine::vars::parse_value` there).
    fn as_var_flag(raw: &str) -> wcl_lang::Value {
        evaluate(raw).unwrap_or_else(|| wcl_lang::Value::Utf8(raw.to_string()))
    }

    /// The var file, read back the way config-weave's `load_var_file` reads
    /// it: each top-level field evaluated standalone.
    fn read_back(source: &str) -> Vec<(String, wcl_lang::Value)> {
        let parsed = wcl_lang::parse_for_edit(source, "<var-file>")
            .unwrap_or_else(|e| panic!("var file must parse: {e:?}\n{source}"));
        let scratch = wcl_lang::Document::open("", "<var-file>").unwrap();
        parsed
            .items
            .iter()
            .filter_map(|item| match item {
                wcl_lang::ast::Item::Field(f) => {
                    Some((f.name.clone(), scratch.eval_expr(&f.expr).unwrap()))
                }
                _ => None,
            })
            .collect()
    }

    /// Every value means in the file what it meant as `--var name=value`:
    /// an expression stays one, and anything else — a bare word, a quote, a
    /// backslash, a line break — arrives as the same string.
    #[test]
    fn the_var_file_means_what_the_flags_meant() {
        let vars = vec![
            var("tz", "UTC", false),
            var("count", "3", false),
            var("enabled", "true", false),
            var("quoted", r#""already a string""#, false),
            var("list", "[1, 2]", false),
            var("path", r"C:\weave\x", false),
            var("tricky", r#"pa"ss\word"#, true),
            var("lines", "one\ntwo\r\nthree\tfour", true),
            var("comment", "a // not a comment", false),
            var("unbalanced", "[1, 2", false),
        ];
        let source = var_file_source(&vars);
        let back = read_back(&source);
        assert_eq!(back.len(), vars.len(), "{source}");
        for (v, (name, value)) in vars.iter().zip(&back) {
            assert_eq!(&v.name, name);
            assert_eq!(value, &as_var_flag(&v.value), "{}: {source}", v.name);
        }
        assert_eq!(back[1].1, as_var_flag("3"));
        assert!(matches!(&back[0].1, wcl_lang::Value::Utf8(s) if s == "UTC"));
    }

    /// The bug's own shape: a secret inside a longer string — a quoted
    /// command line, a word it is glued to — is replaced where it sits, and
    /// nothing else is touched.
    #[test]
    fn a_secret_inside_a_longer_string_is_redacted() {
        let r = Redactor::for_vars(&[
            var("tz", "UTC", false),
            var("api_token", "tok-S3cr3t", true),
        ]);
        assert_eq!(
            r.scrub("`config-weave apply --var api_token=tok-S3cr3t --var tz=UTC` timed out"),
            "`config-weave apply --var api_token=*** --var tz=UTC` timed out"
        );
        assert_eq!(r.scrub("xtok-S3cr3ty tok-S3cr3t"), "x***y ***");
        assert_eq!(r.scrub("nothing secret here"), "nothing secret here");
    }

    /// A quoted WCL string reaches the guest unquoted, and is caught there
    /// too; the longer of two overlapping secrets wins.
    #[test]
    fn both_spellings_and_overlaps_are_redacted() {
        let r = Redactor::for_vars(&[
            var("pw", r#""hunter2""#, true),
            var("short", "hunt", true),
            var("empty", "", true),
        ]);
        assert_eq!(r.scrub(r#"value = "hunter2""#), "value = ***");
        assert_eq!(r.scrub("logged in with hunter2"), "logged in with ***");
        assert_eq!(r.scrub("hunting"), "***ing");
        // The empty secret matches nothing rather than everything.
        assert_eq!(r.scrub("plain"), "plain");
        // Multi-byte text around a secret survives intact.
        assert_eq!(r.scrub("é hunter2 ✓"), "é *** ✓");
    }

    #[test]
    fn json_strings_and_keys_are_scrubbed_and_the_rest_kept() {
        let r = Redactor::for_vars(&[var("pw", "s3cr3t", true)]);
        let ev = serde_json::json!({
            "message": "failed with s3cr3t",
            "steps": [{"diff": "-old\n+s3cr3t"}, 3, true, null],
            "s3cr3t": "key",
        });
        assert_eq!(
            r.scrub_json(ev),
            serde_json::json!({
                "message": "failed with ***",
                "steps": [{"diff": "-old\n+***"}, 3, true, null],
                "***": "key",
            })
        );
    }

    /// An error carrying a secret in any link of its chain is replaced; one
    /// without keeps its type for the code the surface reports.
    #[test]
    fn errors_are_scrubbed_through_their_whole_chain() {
        let r = Redactor::for_vars(&[var("pw", "s3cr3t", true)]);
        let leaky = anyhow::anyhow!("exec of --var pw=s3cr3t failed").context("playbook run");
        let out = r.scrub_error(leaky);
        assert_eq!(
            format!("{out:#}"),
            "playbook run: exec of --var pw=*** failed"
        );

        let conflict = anyhow::Error::new(crate::proto::CommandError::conflict("busy"));
        let kept = r.scrub_error(conflict);
        assert!(kept.downcast_ref::<crate::proto::CommandError>().is_some());
    }

    /// Variables nobody marked secret are not redacted: a `tz = "UTC"`
    /// would otherwise turn every mention of UTC into `***`.
    #[test]
    fn only_marked_variables_are_secrets() {
        let r = Redactor::for_vars(&[var("tz", "UTC", false)]);
        assert_eq!(r.scrub("tz=UTC"), "tz=UTC");
        let j = serde_json::json!({"a": "UTC"});
        assert_eq!(r.scrub_json(j.clone()), j);
    }

    #[test]
    fn a_listing_never_shows_a_secret_value() {
        let listed = listed(&[var("tz", "UTC", false), var("pw", "s3cr3t", true)]);
        assert_eq!(
            listed,
            serde_json::json!([
                {"name": "tz", "value": "UTC", "secret": false},
                {"name": "pw", "value": "***", "secret": true},
            ])
        );
    }

    /// Read from the `version` probe every run already makes: 0.12.2 and
    /// later — pre-release tag or not — take the flag, older ones do not,
    /// and output that names no version leaves it to `--help`.
    #[test]
    fn support_is_read_from_the_version() {
        use SecretVarFile::*;
        let v = secret_var_file_by_version;
        assert_eq!(v("config-weave 0.12.2-alpha\n"), Some(Supported));
        assert_eq!(v("config-weave 0.12.2"), Some(Supported));
        assert_eq!(v("config-weave 0.13.0-alpha"), Some(Supported));
        assert_eq!(v("config-weave 1.0.0+build.7"), Some(Supported));
        assert_eq!(
            v("config-weave 0.11.0-alpha\n"),
            Some(Unsupported {
                found: Some("0.11.0-alpha".into())
            })
        );
        assert_eq!(
            v("config-weave 0.12.1"),
            Some(Unsupported {
                found: Some("0.12.1".into())
            })
        );
        assert_eq!(v("my-weave build abc123"), None);
        assert_eq!(v("config-weave dev"), None);
        assert_eq!(v("config-weave 0.12"), None);
        assert_eq!(v("config-weave 0.12.2.1"), None);
        assert_eq!(v(""), None);
    }

    #[test]
    fn support_is_read_from_the_help_when_the_version_says_nothing() {
        let help = "      --var-file <PATH>  Merge\n      --secret-var-file <PATH>  Like";
        assert_eq!(
            secret_var_file_by_help(Some(help)),
            SecretVarFile::Supported
        );
        assert_eq!(
            secret_var_file_by_help(Some("      --var-file <PATH>  Merge")),
            SecretVarFile::Unsupported { found: None }
        );
        assert_eq!(
            secret_var_file_by_help(None),
            SecretVarFile::Unsupported { found: None }
        );
    }

    /// Secret variables get the second file only from a binary that takes
    /// it; no name lands in both, which config-weave refuses.
    #[test]
    fn secrets_split_into_their_own_file_where_supported() {
        let vars = vec![
            var("tz", "UTC", false),
            var("pw", "s3cr3t", true),
            var("domain", "corp", false),
            var("token", "tok", true),
        ];
        let names = |v: &[&PlaybookVar]| v.iter().map(|v| v.name.clone()).collect::<Vec<_>>();
        let split = split_vars(&vars, &SecretVarFile::Supported);
        assert_eq!(names(&split.plain), ["tz", "domain"]);
        assert_eq!(names(&split.secret), ["pw", "token"]);
        let old = SecretVarFile::Unsupported { found: None };
        let split = split_vars(&vars, &old);
        assert_eq!(names(&split.plain), ["tz", "pw", "domain", "token"]);
        assert!(split.secret.is_empty());
        let source = var_file_source(split_vars(&vars, &SecretVarFile::Supported).secret);
        assert_eq!(
            read_back(&source)
                .into_iter()
                .map(|(n, _)| n)
                .collect::<Vec<_>>(),
            ["pw", "token"]
        );
    }

    /// One line, only for an old binary with secrets to hand it, naming
    /// what it found and the release that would scrub them.
    #[test]
    fn the_fallback_notice_names_the_version_needed() {
        let secret = [var("pw", "s3cr3t", true)];
        let plain = [var("tz", "UTC", false)];
        let old = SecretVarFile::Unsupported {
            found: Some("0.11.0-alpha".into()),
        };
        let note = secret_var_file_notice(&secret, &old).unwrap();
        assert!(
            note.starts_with("config-weave 0.11.0-alpha has no --secret-var-file"),
            "{note}"
        );
        assert!(note.contains("config-weave 0.12.2 or later"), "{note}");
        assert!(!note.contains('\n'));
        let unknown = SecretVarFile::Unsupported { found: None };
        assert!(
            secret_var_file_notice(&secret, &unknown)
                .unwrap()
                .starts_with("this config-weave has no")
        );
        assert_eq!(secret_var_file_notice(&plain, &old), None);
        assert_eq!(
            secret_var_file_notice(&secret, &SecretVarFile::Supported),
            None
        );
    }

    #[test]
    fn the_windows_acl_keeps_only_system_and_administrators() {
        assert_eq!(
            windows_private_dir_argv("C:/weave/vars"),
            vec![
                "icacls",
                r"C:\weave\vars",
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-18:(OI)(CI)F",
                "*S-1-5-32-544:(OI)(CI)F",
            ]
        );
    }
}
