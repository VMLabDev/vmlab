//! config-weave package management over a declared playbook folder: the
//! Files tab's Add/Remove/Update-all package actions, the search picker,
//! and repository management, all shelling out to the host-side
//! `config-weave pkg` CLI (`--dir <playbook>`).
//!
//! Sandbox contract matches the playbook run endpoints: only folders that
//! appear as `playbook "…"` blocks in `vmlab.wcl` (validated through
//! `playbooks::playbook_dir`) are operated on. The CLI has no JSON output,
//! so its human tables are parsed (`parse_search` / `parse_repo_list`) —
//! header-anchored to skip preamble lines like the stdlib seeding notice.

use std::path::{Path, PathBuf};
use std::time::Duration;

use actix_web::{HttpResponse, web};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Mutex;

use vmlab::weave_bin::{GuestOs, weave_binary};

use super::api::fail;
use super::playbooks::playbook_dir;
use super::state::AppState;

/// Generous ceiling: `pkg add`/`search`/`repo add` shallow-clone or fetch
/// the registered git repos over the network.
const PKG_TIMEOUT: Duration = Duration::from_secs(120);

/// Every mutating invocation rewrites `pkgs/repo.wcl` (and `search`/`add`
/// may seed the stdlib repo into it), so CLI runs are serialized.
static PKG_LOCK: Mutex<()> = Mutex::const_new(());

/// The default package repository, mirroring config-weave's own
/// `stdlib_default()` — seeded when the registry is empty so the repos
/// modal starts useful.
const STDLIB_NAME: &str = "stdlib";
const STDLIB_URL: &str = "https://github.com/Configweave/config-weave-pkgs.git";
const STDLIB_SUBDIR: &str = "pkgs";

/// config-weave's `store::valid_name`: ASCII alnum + `- _ .`, no leading
/// dot. Checked here too so a hostile package/repo name never reaches argv.
fn valid_pkg_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn bad(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::BadRequest().json(json!({"error": msg.into()}))
}

fn internal(msg: impl Into<String>) -> HttpResponse {
    HttpResponse::InternalServerError().json(json!({"error": msg.into()}))
}

// ---- CLI execution ----------------------------------------------------------

struct WeaveOutput {
    stdout: String,
    stderr: String,
    ok: bool,
}

impl WeaveOutput {
    /// The error to surface for a failed run: stderr, else stdout, else a
    /// generic line (config-weave puts diagnostics on stderr, exit 2).
    fn error_message(&self) -> String {
        let err = self.stderr.trim();
        if !err.is_empty() {
            return err.to_string();
        }
        let out = self.stdout.trim();
        if !out.is_empty() {
            return out.to_string();
        }
        "config-weave pkg failed".to_string()
    }
}

fn weave_bin(state: &AppState) -> Result<PathBuf, String> {
    weave_binary(&state.weave_bin_dir, GuestOs::Linux, "x86_64").map_err(|e| format!("{e:#}"))
}

async fn run_pkg(bin: &Path, dir: &Path, args: &[&str]) -> Result<WeaveOutput, String> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.arg("pkg")
        .arg("--dir")
        .arg(dir)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let child = cmd
        .spawn()
        .map_err(|e| format!("failed to run {}: {e}", bin.display()))?;
    let out = tokio::time::timeout(PKG_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            format!(
                "config-weave pkg {} timed out after {}s",
                args.join(" "),
                PKG_TIMEOUT.as_secs()
            )
        })?
        .map_err(|e| e.to_string())?;
    Ok(WeaveOutput {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        ok: out.status.success(),
    })
}

/// Resolve lab root + declared playbook folder, or answer with the error.
async fn resolve_dir(
    state: &AppState,
    lab: &str,
    playbook: String,
) -> Result<PathBuf, HttpResponse> {
    let root = state.lab_root(lab).await.map_err(fail)?;
    match web::block(move || playbook_dir(&root, &playbook)).await {
        Ok(Ok(dir)) => Ok(dir),
        Ok(Err(e)) => Err(e.respond()),
        Err(e) => Err(internal(e.to_string())),
    }
}

// ---- output parsers ---------------------------------------------------------

#[derive(Debug, PartialEq, serde::Serialize)]
struct SearchHit {
    repo: String,
    package: String,
    description: String,
    installed: bool,
    installed_from: Option<String>,
}

/// Parse `pkg search` output: preamble lines (seeding notice, .gitignore
/// note) precede a `REPO  PACKAGE  DESCRIPTION` table; the installed
/// marker is folded into the description cell. Repo and package names
/// never contain whitespace (config-weave validates them), so token
/// splitting is safe; description whitespace runs collapse (cosmetic).
fn parse_search(stdout: &str) -> Vec<SearchHit> {
    let mut hits = Vec::new();
    let mut in_table = false;
    for line in stdout.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if !in_table {
            in_table = toks.first() == Some(&"REPO") && toks.get(1) == Some(&"PACKAGE");
            continue;
        }
        if toks.len() < 2 {
            continue;
        }
        let mut description = toks[2..].join(" ");
        let mut installed = false;
        let mut installed_from = None;
        if let Some(rest) = description.strip_suffix("[installed]") {
            installed = true;
            description = rest.trim_end().to_string();
        } else if description.ends_with(']')
            && let Some(idx) = description.rfind("[installed from ")
        {
            installed = true;
            installed_from = Some(
                description[idx + "[installed from ".len()..description.len() - 1].to_string(),
            );
            description = description[..idx].trim_end().to_string();
        }
        hits.push(SearchHit {
            repo: toks[0].to_string(),
            package: toks[1].to_string(),
            description,
            installed,
            installed_from,
        });
    }
    hits
}

#[derive(Debug, PartialEq, serde::Serialize)]
struct RepoRow {
    name: String,
    url: String,
    branch: Option<String>,
    subdir: Option<String>,
    cache: String,
}

/// Parse `pkg repo list` output: `NAME  URL  BRANCH  SUBDIR  CACHE` with
/// `-` for unset cells; CACHE may be the two-word `not synced`. `None` =
/// the "no repositories registered" empty state.
fn parse_repo_list(stdout: &str) -> Option<Vec<RepoRow>> {
    if stdout
        .lines()
        .any(|l| l.trim_start().starts_with("no repositories registered"))
    {
        return None;
    }
    let none_if_dash = |s: &str| (s != "-").then(|| s.to_string());
    let mut rows = Vec::new();
    let mut in_table = false;
    for line in stdout.lines() {
        let toks: Vec<&str> = line.split_whitespace().collect();
        if !in_table {
            in_table = toks.first() == Some(&"NAME") && toks.get(1) == Some(&"URL");
            continue;
        }
        if toks.len() < 5 {
            continue;
        }
        rows.push(RepoRow {
            name: toks[0].to_string(),
            url: toks[1].to_string(),
            branch: none_if_dash(toks[2]),
            subdir: none_if_dash(toks[3]),
            cache: toks[4..].join(" "),
        });
    }
    Some(rows)
}

// ---- endpoints --------------------------------------------------------------

#[derive(Deserialize)]
pub struct PkgActionBody {
    playbook: String,
    action: String,
    package: Option<String>,
}

/// `POST /api/labs/{lab}/playbooks/pkg` — run `pkg add <p>` / `pkg remove
/// <p>` / `pkg update` (always all — a per-package update is never issued)
/// against a declared playbook folder. 200 `{ok, output}` on success; a
/// CLI failure is the caller's problem (bad package name, network) → 422
/// with the stderr message.
pub async fn pkg_action(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<PkgActionBody>,
) -> HttpResponse {
    let body = body.into_inner();
    let mut args: Vec<&str> = Vec::new();
    match (body.action.as_str(), body.package.as_deref()) {
        ("add" | "remove", Some(p)) if valid_pkg_name(p) => {
            args.push(if body.action == "add" {
                "add"
            } else {
                "remove"
            });
            args.push(p);
        }
        ("add" | "remove", _) => {
            return bad(format!("pkg {} requires a valid package name", body.action));
        }
        ("update", None) => args.push("update"),
        ("update", Some(_)) => return bad("pkg update takes no package — it updates all"),
        _ => return bad(format!("unknown pkg action `{}`", body.action)),
    }
    let dir = match resolve_dir(&state, &lab, body.playbook).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let bin = match weave_bin(&state) {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    let _guard = PKG_LOCK.lock().await;
    match run_pkg(&bin, &dir, &args).await {
        Ok(out) if out.ok => HttpResponse::Ok().json(json!({"ok": true, "output": out.stdout})),
        Ok(out) => HttpResponse::UnprocessableEntity()
            .json(json!({"error": out.error_message(), "output": out.stdout})),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct PkgSearchBody {
    playbook: String,
    term: String,
}

/// `POST /api/labs/{lab}/playbooks/pkg/search` — `pkg search <term>`,
/// parsed into hits. Syncs (and on first use seeds) the registered repos,
/// hence POST + the lock despite being a query.
pub async fn pkg_search(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<PkgSearchBody>,
) -> HttpResponse {
    let body = body.into_inner();
    if body.term.trim().is_empty() {
        return bad("search term must not be empty");
    }
    let dir = match resolve_dir(&state, &lab, body.playbook).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let bin = match weave_bin(&state) {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    let _guard = PKG_LOCK.lock().await;
    match run_pkg(&bin, &dir, &["search", &body.term]).await {
        Ok(out) if out.ok => HttpResponse::Ok().json(parse_search(&out.stdout)),
        Ok(out) => HttpResponse::UnprocessableEntity()
            .json(json!({"error": out.error_message(), "output": out.stdout})),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct ReposQuery {
    playbook: String,
}

/// `GET /api/labs/{lab}/playbooks/repos?playbook=…` — `pkg repo list`,
/// seeding the stdlib repo first when the registry is empty (so a fresh
/// playbook starts with the default source). A failed seed *sync* still
/// registers the entry (config-weave behavior) — surfaced as `warning`.
pub async fn repos_list(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    query: web::Query<ReposQuery>,
) -> HttpResponse {
    let dir = match resolve_dir(&state, &lab, query.into_inner().playbook).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let bin = match weave_bin(&state) {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    let _guard = PKG_LOCK.lock().await;
    let first = match run_pkg(&bin, &dir, &["repo", "list"]).await {
        Ok(out) if out.ok => out,
        Ok(out) => {
            return HttpResponse::UnprocessableEntity()
                .json(json!({"error": out.error_message(), "output": out.stdout}));
        }
        Err(e) => return internal(e),
    };
    let mut seeded = false;
    let mut warning: Option<String> = None;
    let mut repos = parse_repo_list(&first.stdout);
    if repos.as_ref().is_none_or(|r| r.is_empty()) {
        let seed = [
            "repo",
            "add",
            STDLIB_NAME,
            STDLIB_URL,
            "--subdir",
            STDLIB_SUBDIR,
        ];
        match run_pkg(&bin, &dir, &seed).await {
            Ok(out) => {
                seeded = true;
                if !out.ok {
                    warning = Some(out.error_message());
                }
            }
            Err(e) => return internal(e),
        }
        repos = match run_pkg(&bin, &dir, &["repo", "list"]).await {
            Ok(out) if out.ok => parse_repo_list(&out.stdout),
            Ok(out) => {
                return HttpResponse::UnprocessableEntity()
                    .json(json!({"error": out.error_message(), "output": out.stdout}));
            }
            Err(e) => return internal(e),
        };
    }
    HttpResponse::Ok().json(json!({
        "repos": repos.unwrap_or_default(),
        "seeded": seeded,
        "warning": warning,
    }))
}

#[derive(Deserialize)]
pub struct RepoEditBody {
    playbook: String,
    action: String,
    name: String,
    url: Option<String>,
    branch: Option<String>,
    subdir: Option<String>,
}

/// `POST /api/labs/{lab}/playbooks/repos` — `pkg repo add|remove`. `add`
/// syncs immediately, so a bad URL fails fast here (422 with stderr —
/// note config-weave keeps the entry registered in that case).
pub async fn repos_edit(
    state: web::Data<AppState>,
    lab: web::Path<String>,
    body: web::Json<RepoEditBody>,
) -> HttpResponse {
    let body = body.into_inner();
    if !valid_pkg_name(&body.name) {
        return bad("repo name must be alphanumeric with - _ . and no leading dot");
    }
    let mut args: Vec<&str> = vec!["repo"];
    match body.action.as_str() {
        "add" => {
            let Some(url) = body.url.as_deref().filter(|u| !u.trim().is_empty()) else {
                return bad("repo add requires a url");
            };
            args.extend(["add", &body.name, url]);
            if let Some(b) = body.branch.as_deref().filter(|b| !b.trim().is_empty()) {
                args.extend(["--branch", b]);
            }
            if let Some(s) = body.subdir.as_deref().filter(|s| !s.trim().is_empty()) {
                args.extend(["--subdir", s]);
            }
        }
        "remove" => args.extend(["remove", &body.name]),
        other => return bad(format!("unknown repo action `{other}`")),
    }
    let dir = match resolve_dir(&state, &lab, body.playbook.clone()).await {
        Ok(d) => d,
        Err(resp) => return resp,
    };
    let bin = match weave_bin(&state) {
        Ok(b) => b,
        Err(e) => return internal(e),
    };
    let _guard = PKG_LOCK.lock().await;
    match run_pkg(&bin, &dir, &args).await {
        Ok(out) if out.ok => HttpResponse::Ok().json(json!({"ok": true, "output": out.stdout})),
        Ok(out) => HttpResponse::UnprocessableEntity()
            .json(json!({"error": out.error_message(), "output": out.stdout})),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::super::state::AuthConfig;
    use super::*;
    use actix_web::{App, test};
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt;

    /// A tempdir lab with one declared playbook folder (present, with a
    /// playbook.wcl) — the same shape playbooks.rs tests use.
    fn playbook_lab() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("vmlab.wcl"),
            r#"import <vmlab.wcl>
lab "lab" {
  vm "web01" { template = "x86_64/t" }
  playbook "playbooks/base" { play = "base" vms = ["web01"] }
}
"#,
        )
        .unwrap();
        std::fs::create_dir_all(tmp.path().join("playbooks/base")).unwrap();
        std::fs::write(tmp.path().join("playbooks/base/playbook.wcl"), "x").unwrap();
        tmp
    }

    /// A stub `config-weave-linux-x86_64` in its own tempdir: logs argv to
    /// `argv.log` and runs the given shell body with `$d` = the stub dir
    /// and the leading `pkg --dir <dir>` already shifted away.
    fn stub_bin(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config-weave-linux-x86_64");
        let script = format!(
            "#!/bin/sh\nd=\"$(dirname \"$0\")\"\necho \"$@\" >> \"$d/argv.log\"\nshift 3\n{body}\n"
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        dir
    }

    fn state_for(root: &std::path::Path, stub: &std::path::Path) -> web::Data<AppState> {
        let mut state = AppState::new(
            AuthConfig {
                enabled: false,
                user: String::new(),
                password_hash: String::new(),
            },
            Some(("lab".into(), root.to_path_buf())),
            false,
        );
        state.weave_bin_dir = stub.to_path_buf();
        web::Data::new(state)
    }

    macro_rules! app {
        ($root:expr, $stub:expr) => {
            test::init_service(
                App::new()
                    .app_data(state_for($root, $stub))
                    .route("/api/labs/{lab}/playbooks/pkg", web::post().to(pkg_action))
                    .route(
                        "/api/labs/{lab}/playbooks/pkg/search",
                        web::post().to(pkg_search),
                    )
                    .route("/api/labs/{lab}/playbooks/repos", web::get().to(repos_list))
                    .route(
                        "/api/labs/{lab}/playbooks/repos",
                        web::post().to(repos_edit),
                    ),
            )
            .await
        };
    }

    fn argv_log(stub: &tempfile::TempDir) -> String {
        std::fs::read_to_string(stub.path().join("argv.log")).unwrap_or_default()
    }

    #[actix_web::test]
    async fn pkg_add_runs_cli_and_reports_output() {
        let lab = playbook_lab();
        let stub = stub_bin("echo \"done $*\"");
        let app = app!(lab.path(), stub.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/pkg")
                .set_json(
                    json!({"playbook": "playbooks/base", "action": "add", "package": "redis"}),
                )
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["ok"], true);
        assert!(body["output"].as_str().unwrap().contains("done add redis"));
        let log = argv_log(&stub);
        assert!(log.contains("pkg --dir"), "{log}");
        assert!(log.contains("add redis"), "{log}");
        assert!(log.contains("playbooks/base"), "{log}");
    }

    #[actix_web::test]
    async fn pkg_rejects_undeclared_traversal_and_bad_input() {
        let lab = playbook_lab();
        std::fs::create_dir_all(lab.path().join("other")).unwrap();
        let stub = stub_bin("echo ok");
        let app = app!(lab.path(), stub.path());

        let cases = [
            (
                json!({"playbook": "other", "action": "add", "package": "x"}),
                403,
            ),
            (
                json!({"playbook": "../other", "action": "add", "package": "x"}),
                400,
            ),
            (
                json!({"playbook": "/etc", "action": "add", "package": "x"}),
                400,
            ),
            (
                json!({"playbook": "playbooks/base", "action": "add", "package": "../evil"}),
                400,
            ),
            (
                json!({"playbook": "playbooks/base", "action": "add", "package": ".hidden"}),
                400,
            ),
            (json!({"playbook": "playbooks/base", "action": "add"}), 400),
            (
                json!({"playbook": "playbooks/base", "action": "update", "package": "x"}),
                400,
            ),
            (
                json!({"playbook": "playbooks/base", "action": "install", "package": "x"}),
                400,
            ),
        ];
        for (body, status) in cases {
            let resp = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/api/labs/lab/playbooks/pkg")
                    .set_json(body.clone())
                    .to_request(),
            )
            .await;
            assert_eq!(resp.status(), status, "{body}");
        }
        // Nothing above may have reached the CLI.
        assert_eq!(argv_log(&stub), "");
    }

    #[actix_web::test]
    async fn pkg_error_exit_maps_to_422_with_stderr() {
        let lab = playbook_lab();
        let stub = stub_bin("echo \"partial output\"\necho \"no package 'nope'\" >&2\nexit 2");
        let app = app!(lab.path(), stub.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/pkg")
                .set_json(
                    json!({"playbook": "playbooks/base", "action": "remove", "package": "nope"}),
                )
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 422);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["error"], "no package 'nope'");
        assert!(body["output"].as_str().unwrap().contains("partial output"));
    }

    #[actix_web::test]
    async fn search_runs_cli_and_parses() {
        let lab = playbook_lab();
        let stub = stub_bin("cat \"$d/search.out\"");
        std::fs::write(
            stub.path().join("search.out"),
            "seeded package repo 'stdlib' (https://github.com/Configweave/config-weave-pkgs.git)\n\
             note: add '.repo-cache/' to /tmp/x/.gitignore\n\
             REPO    PACKAGE    DESCRIPTION\n\
             stdlib  redis      In-memory data store  [installed]\n\
             stdlib  nginx      Web server and reverse proxy\n\
             extra   redis-ha   HA redis setup  [installed from stdlib]\n",
        )
        .unwrap();
        let app = app!(lab.path(), stub.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/pkg/search")
                .set_json(json!({"playbook": "playbooks/base", "term": "redis"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let hits: Value = test::read_body_json(resp).await;
        let hits = hits.as_array().unwrap();
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[0]["package"], "redis");
        assert_eq!(hits[0]["description"], "In-memory data store");
        assert_eq!(hits[0]["installed"], true);
        assert_eq!(hits[0]["installed_from"], Value::Null);
        assert_eq!(hits[1]["installed"], false);
        assert_eq!(hits[1]["description"], "Web server and reverse proxy");
        assert_eq!(hits[2]["installed"], true);
        assert_eq!(hits[2]["installed_from"], "stdlib");
        assert!(argv_log(&stub).contains("search redis"));
    }

    #[actix_web::test]
    async fn repos_seeds_stdlib_when_empty() {
        let lab = playbook_lab();
        // Stateful stub: empty registry until a `repo add` runs.
        let stub = stub_bin(
            "case \"$1 $2\" in\n\
             \"repo list\") if [ -f \"$d/seeded\" ]; then cat \"$d/list.out\"; \
             else echo \"no repositories registered (pkgs/repo.wcl missing) — 'pkg add <package>' seeds the stdlib\"; fi ;;\n\
             \"repo add\") touch \"$d/seeded\"; echo \"registered repo '$3' -> $4\" ;;\n\
             esac",
        );
        std::fs::write(
            stub.path().join("list.out"),
            "NAME    URL                                                   BRANCH  SUBDIR  CACHE\n\
             stdlib  https://github.com/Configweave/config-weave-pkgs.git  -       pkgs    not synced\n",
        )
        .unwrap();
        let app = app!(lab.path(), stub.path());
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/repos?playbook=playbooks%2Fbase")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["seeded"], true);
        assert_eq!(body["warning"], Value::Null);
        let repos = body["repos"].as_array().unwrap();
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0]["name"], "stdlib");
        assert_eq!(repos[0]["branch"], Value::Null);
        assert_eq!(repos[0]["subdir"], "pkgs");
        assert_eq!(repos[0]["cache"], "not synced");
        let log = argv_log(&stub);
        assert!(
            log.contains(
                "repo add stdlib https://github.com/Configweave/config-weave-pkgs.git --subdir pkgs"
            ),
            "{log}"
        );

        // Second call: registry populated, no re-seed.
        let resp = test::call_service(
            &app,
            test::TestRequest::get()
                .uri("/api/labs/lab/playbooks/repos?playbook=playbooks%2Fbase")
                .to_request(),
        )
        .await;
        let body: Value = test::read_body_json(resp).await;
        assert_eq!(body["seeded"], false);
    }

    #[actix_web::test]
    async fn repo_edit_builds_argv_and_validates() {
        let lab = playbook_lab();
        let stub = stub_bin("echo ok");
        let app = app!(lab.path(), stub.path());

        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/repos")
                .set_json(json!({
                    "playbook": "playbooks/base", "action": "add", "name": "corp",
                    "url": "https://git.example.com/pkgs.git", "branch": "main", "subdir": "packages"
                }))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert!(argv_log(&stub).contains(
            "repo add corp https://git.example.com/pkgs.git --branch main --subdir packages"
        ));

        let resp = test::call_service(
            &app,
            test::TestRequest::post()
                .uri("/api/labs/lab/playbooks/repos")
                .set_json(json!({"playbook": "playbooks/base", "action": "remove", "name": "corp"}))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), 200);
        assert!(argv_log(&stub).contains("repo remove corp"));

        // add without url, bad name → 400 (no CLI run).
        for body in [
            json!({"playbook": "playbooks/base", "action": "add", "name": "x"}),
            json!({"playbook": "playbooks/base", "action": "add", "name": "../x", "url": "u"}),
        ] {
            let resp = test::call_service(
                &app,
                test::TestRequest::post()
                    .uri("/api/labs/lab/playbooks/repos")
                    .set_json(body.clone())
                    .to_request(),
            )
            .await;
            assert_eq!(resp.status(), 400, "{body}");
        }
    }

    #[actix_web::test]
    async fn parse_search_handles_empty_and_no_match() {
        assert_eq!(parse_search(""), vec![]);
        assert_eq!(parse_search("no packages matching 'zzz'\n"), vec![]);
    }

    #[actix_web::test]
    async fn parse_repo_list_empty_states() {
        assert_eq!(
            parse_repo_list(
                "no repositories registered (x missing) — 'pkg add <package>' seeds the stdlib\n"
            ),
            None
        );
        // Header with no rows: registry file present but no repos.
        assert_eq!(
            parse_repo_list("NAME  URL  BRANCH  SUBDIR  CACHE\n"),
            Some(vec![])
        );
    }

    #[actix_web::test]
    async fn parse_repo_list_rows() {
        let rows = parse_repo_list(
            "NAME    URL         BRANCH  SUBDIR  CACHE\n\
             stdlib  https://x   -       pkgs    abc1234\n\
             corp    ssh://y     main    -       dirty\n",
        )
        .unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].branch, None);
        assert_eq!(rows[0].subdir.as_deref(), Some("pkgs"));
        assert_eq!(rows[0].cache, "abc1234");
        assert_eq!(rows[1].branch.as_deref(), Some("main"));
        assert_eq!(rows[1].cache, "dirty");
    }
}
