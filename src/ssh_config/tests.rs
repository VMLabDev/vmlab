//! The writer's tests, all against a temporary home directory — the reason
//! every path on [`Managed`] is a field (§19.7). A component whose failure
//! mode is "ate someone's ssh config" must be provable without one.

use std::path::{Path, PathBuf};

use super::*;
use crate::config::load_lab_source;

/// A lab file on disk, and the loaded lab beside it — pruning reads the root,
/// so a test lab has to actually exist somewhere.
struct TestLab {
    root: PathBuf,
    lab: crate::config::model::Lab,
}

fn lab_at(root: &Path, name: &str, body: &str) -> TestLab {
    std::fs::create_dir_all(root).unwrap();
    let source = format!("import <vmlab.wcl>\nlab \"{name}\" {{\n{body}\n}}\n");
    std::fs::write(root.join(crate::paths::LAB_FILE), &source).unwrap();
    let file = load_lab_source(&source, "<test>", root).expect("the lab parses");
    TestLab {
        root: root.to_path_buf(),
        lab: file.lab,
    }
}

impl TestLab {
    fn block(&self) -> LabBlock {
        LabBlock::of(&self.lab, &self.root)
    }
}

const ONE_VM: &str = r#"vm "dev01" { template = "x86_64/win" }"#;

/// A managed writer over a temp home, with the mux/lock paths inside it too
/// — nothing in a test may touch the developer's own runtime directory.
fn managed(home: &Path) -> Managed {
    let mut m = Managed::for_home(home);
    m.known_hosts = home.join("state/known_hosts");
    m.control_dir = home.join("run/ssh");
    m.lock = home.join("run/ssh/config.lock");
    m
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

/// The whole write, from nothing: the file is created, the block is in it,
/// and the file is private. `~/.ssh` did not exist either — an editor's first
/// run on a fresh machine is exactly this case.
#[test]
fn a_first_refresh_creates_the_file_and_the_directory() {
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);

    assert_eq!(m.refresh(&lab.block()).unwrap(), Outcome::Wrote);

    let body = read(&m.path);
    assert!(body.starts_with(block::BEGIN), "{body}");
    assert!(body.contains("Host vmlab-probe-dev01"), "{body}");
    assert!(body.trim_end().ends_with(block::END), "{body}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&m.path), 0o600, "the config must be private");
        assert_eq!(
            mode(&home.path().join(".ssh")),
            0o700,
            "~/.ssh must be 0700"
        );
    }
}

/// Written only on a real difference — the property a dotfiles-tracked
/// config depends on. Asserted on the file's own mtime rather than on its
/// bytes, because "same bytes, rewritten" is exactly the diff that would
/// show up in `git status`.
#[test]
fn an_unchanged_block_is_not_rewritten() {
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);

    assert_eq!(m.refresh(&lab.block()).unwrap(), Outcome::Wrote);
    let before = std::fs::metadata(&m.path).unwrap().modified().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(10));

    assert_eq!(m.refresh(&lab.block()).unwrap(), Outcome::Unchanged);
    let after = std::fs::metadata(&m.path).unwrap().modified().unwrap();
    assert_eq!(before, after, "an unchanged block must not touch the file");
}

/// The developer's own config survives, in their order, below the block —
/// and their `Host *` keeps applying to vmlab connections, which is half the
/// reason the block shares their file at all.
#[test]
fn the_developers_own_config_is_kept_and_the_block_is_hoisted() {
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    let theirs = "Host git\n  HostName github.com\n\nHost *\n  ServerAliveInterval 30\n";
    std::fs::write(&m.path, theirs).unwrap();
    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);

    m.refresh(&lab.block()).unwrap();

    let body = read(&m.path);
    assert!(body.starts_with(block::BEGIN), "{body}");
    let after = body.split(block::END).nth(1).unwrap();
    assert_eq!(after.trim_start_matches('\n'), theirs, "{body}");
}

/// A block that has been moved down the file is re-hoisted on the next
/// write, and the developer's lines keep their own relative order.
#[test]
fn a_displaced_block_is_hoisted_back_without_moving_their_lines() {
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);
    m.refresh(&lab.block()).unwrap();

    // The developer moved it: their stanza now sits above vmlab's region.
    let body = read(&m.path);
    let (block_text, rest) = body.split_at(body.find(block::END).unwrap() + block::END.len());
    std::fs::write(
        &m.path,
        format!("Host a\n  User me\nHost b\n{block_text}{rest}"),
    )
    .unwrap();

    let other = lab_at(&home.path().join("labs/other"), "other", ONE_VM);
    m.refresh(&other.block()).unwrap();

    let body = read(&m.path);
    assert!(body.starts_with(block::BEGIN), "{body}");
    let at = |needle: &str| body.find(needle).expect(needle);
    assert!(at(block::END) < at("Host a"), "{body}");
    assert!(at("Host a") < at("Host b"), "{body}");
}

/// The block accumulates across labs, and prunes by lab root: a root that no
/// longer holds a lab file has its stanzas dropped, because the block *is*
/// the record.
#[test]
fn labs_accumulate_and_a_vanished_lab_is_pruned() {
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    let gone = lab_at(&home.path().join("labs/gone"), "gone", ONE_VM);
    let probe = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);

    m.refresh(&gone.block()).unwrap();
    m.refresh(&probe.block()).unwrap();
    let body = read(&m.path);
    assert!(body.contains("Host vmlab-gone-dev01"), "{body}");
    assert!(body.contains("Host vmlab-probe-dev01"), "{body}");

    std::fs::remove_dir_all(&gone.root).unwrap();
    // Something has to change for a write to happen at all, so the surviving
    // lab gains a machine at the same time.
    let probe = lab_at(
        &probe.root,
        "probe",
        &format!("{ONE_VM}\nvm \"web01\" {{ template = \"x86_64/win\" }}"),
    );
    m.refresh(&probe.block()).unwrap();

    let body = read(&m.path);
    assert!(!body.contains("vmlab-gone-dev01"), "{body}");
    assert!(body.contains("Host vmlab-probe-web01"), "{body}");
}

/// A lab that moved directory does not leave its old stanzas behind: a lab
/// name is its host-global runtime identity (ADR-0011), so the section
/// claiming the name from another root is stale by definition.
#[test]
fn a_lab_that_moved_replaces_its_own_section() {
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    let first = lab_at(&home.path().join("labs/one"), "probe", ONE_VM);
    m.refresh(&first.block()).unwrap();

    let moved = lab_at(&home.path().join("labs/two"), "probe", ONE_VM);
    m.refresh(&moved.block()).unwrap();

    let body = read(&m.path);
    assert_eq!(
        body.matches("Host vmlab-probe-dev01\n").count(),
        1,
        "{body}"
    );
    assert!(body.contains("labs/two"), "{body}");
    assert!(!body.contains("labs/one"), "{body}");
}

/// Stanzas cover *declared* machines and one per (machine, login): the
/// editor's picker carries every identity, including the never-started
/// machine and the elevated login.
#[test]
fn every_declared_machine_and_login_gets_an_alias() {
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    let lab = lab_at(
        &home.path().join("labs/probe"),
        "probe",
        r#"vm "dev01" {
             template = "x86_64/win"
             login "dev"   { user = "PROBE\\dev"   password = "p" default = true }
             login "admin" { user = "PROBE\\admin" password = "p" }
           }
           container "buildbox" { image = "sdk:9.0" }"#,
    );
    m.refresh(&lab.block()).unwrap();

    let body = read(&m.path);
    for alias in [
        "Host vmlab-probe-dev01\n",
        "Host vmlab-probe-dev01-admin\n",
        "Host vmlab-probe-buildbox\n",
    ] {
        assert!(body.contains(alias), "{alias} missing from\n{body}");
    }
    // The default login is the bare alias — it needs no second spelling.
    assert!(!body.contains("vmlab-probe-dev01-dev"), "{body}");
}

/// Mangled markers change nothing at all — not the file, not its mtime.
#[test]
fn mangled_markers_leave_the_file_alone() {
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    let broken = format!("Host git\n{}\nHost vmlab-probe-dev01\n", block::BEGIN);
    std::fs::write(&m.path, &broken).unwrap();
    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);

    let err = m.refresh(&lab.block()).unwrap_err();
    let shown = format!("{err:#}");
    assert!(shown.contains(&m.path.display().to_string()), "{shown}");
    assert!(shown.contains(":2:"), "{shown}");
    assert_eq!(read(&m.path), broken, "the file must be untouched");
}

/// A symlinked config keeps its symlink: the rename lands on the resolved
/// path, so a stow/chezmoi dotfiles tree is updated in place rather than
/// being replaced by a regular file that stops tracking.
#[cfg(unix)]
#[test]
fn a_symlinked_config_keeps_its_symlink() {
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    let dotfiles = home.path().join("dotfiles");
    std::fs::create_dir_all(&dotfiles).unwrap();
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    let real = dotfiles.join("ssh_config");
    std::fs::write(&real, "Host git\n  HostName github.com\n").unwrap();
    std::os::unix::fs::symlink(&real, &m.path).unwrap();

    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);
    m.refresh(&lab.block()).unwrap();

    assert!(
        std::fs::symlink_metadata(&m.path).unwrap().is_symlink(),
        "the symlink must survive the write"
    );
    assert!(
        read(&real).contains("Host vmlab-probe-dev01"),
        "{}",
        read(&real)
    );
}

/// An existing config keeps the mode its owner gave it. vmlab is a guest in
/// this file; tightening it to 0600 behind their back is not vmlab's call.
#[cfg(unix)]
#[test]
fn an_existing_config_keeps_its_mode() {
    use std::os::unix::fs::PermissionsExt as _;
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    std::fs::write(&m.path, "Host git\n").unwrap();
    std::fs::set_permissions(&m.path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);
    m.refresh(&lab.block()).unwrap();

    let mode = std::fs::metadata(&m.path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o644);
}

/// OpenSSH's own resolver agrees the block applies — the check that catches
/// displacement, an overriding `Host *` and a stale hand-paste alike.
#[test]
fn the_real_ssh_resolves_vmlabs_proxy_command() {
    if !have_ssh() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);
    // The refresh verifies for itself; a failure here *is* the assertion.
    m.refresh(&lab.block()).unwrap();

    let out = std::process::Command::new("ssh")
        .arg("-F")
        .arg(home.path().join(".ssh/config"))
        .args(["-G", "vmlab-probe-dev01"])
        .env("HOME", home.path())
        .output()
        .unwrap();
    let resolved = String::from_utf8_lossy(&out.stdout);
    assert!(resolved.contains("ssh-proxy probe/dev01"), "{resolved}");
    assert!(resolved.contains("controlmaster auto"), "{resolved}");
}

/// A `Host *` hand-pasted above the block does not survive the next write:
/// hoisting moves vmlab's own region back to the top, where OpenSSH's
/// first-value-wins rule puts it ahead again — and `ssh -G` confirms it
/// rather than the comment claiming it.
#[test]
fn a_host_star_pasted_above_the_block_stops_beating_it() {
    if !have_ssh() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);
    m.refresh(&lab.block()).unwrap();

    let body = read(&m.path);
    std::fs::write(
        &m.path,
        format!("Host *\n  ProxyCommand /bin/false\n{body}"),
    )
    .unwrap();
    let resolved = ssh_g(home.path(), "vmlab-probe-dev01");
    assert!(resolved.contains("proxycommand /bin/false"), "{resolved}");

    // Any refresh at all repairs it: a displaced block renders differently
    // from what is on disk, so there is always a real difference to write.
    assert_eq!(m.refresh(&lab.block()).unwrap(), Outcome::Wrote);
    let resolved = ssh_g(home.path(), "vmlab-probe-dev01");
    assert!(resolved.contains("ssh-proxy probe/dev01"), "{resolved}");
}

/// The redirect the host config's override allows is checked like everything
/// else, and fails **honestly** when `ssh` cannot reach it: a location knob
/// that pretended to work would be the on/off switch §19.7 rejected.
#[test]
fn a_block_ssh_never_reads_says_so() {
    if !have_ssh() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let mut m = managed(home.path());
    m.path = home.path().join("elsewhere/ssh.conf");
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    std::fs::write(home.path().join(".ssh/config"), "Host git\n").unwrap();
    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);

    let err = m.refresh(&lab.block()).unwrap_err();
    let shown = format!("{err:#}");
    assert!(shown.contains("elsewhere/ssh.conf"), "{shown}");
    assert!(shown.contains("`ssh -G vmlab-probe-dev01`"), "{shown}");
    // The block itself was still written: the file is right, the routing is
    // not, and the developer is told which.
    assert!(read(&m.path).contains("Host vmlab-probe-dev01"), "{shown}");

    // An `Include` the developer wrote is how a redirect earns its keep —
    // and then the same check passes.
    std::fs::write(
        home.path().join(".ssh/config"),
        format!("Include {}\nHost git\n", m.path.display()),
    )
    .unwrap();
    let lab = lab_at(
        &lab.root,
        "probe",
        &format!("{ONE_VM}\nvm \"web01\" {{ template = \"x86_64/win\" }}"),
    );
    assert_eq!(m.refresh(&lab.block()).unwrap(), Outcome::Wrote);
}

/// A block that has been losing all along keeps saying so, with nothing to
/// write. `refresh` alone would go quiet the moment the block stopped
/// changing, which is why `vmlab ssh` — the command whose alias is
/// load-bearing — asks [`Managed::verify`] for itself.
#[test]
fn an_unchanged_block_can_still_be_asked_whether_it_applies() {
    if !have_ssh() {
        return;
    }
    let home = tempfile::tempdir().unwrap();
    let mut m = managed(home.path());
    m.path = home.path().join("elsewhere/ssh.conf");
    std::fs::create_dir_all(home.path().join(".ssh")).unwrap();
    std::fs::write(home.path().join(".ssh/config"), "Host git\n").unwrap();
    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);
    let block = lab.block();

    // The first write says so. The second has nothing to write…
    assert!(m.refresh(&block).is_err());
    assert_eq!(m.refresh(&block).unwrap(), Outcome::Unchanged);
    // …and the question is still answerable, with the same answer.
    let err = m.verify(&block.lab, block.aliases.first()).unwrap_err();
    assert!(format!("{err:#}").contains("elsewhere/ssh.conf"), "{err:#}");
}

fn ssh_g(home: &Path, alias: &str) -> String {
    let out = std::process::Command::new("ssh")
        .arg("-F")
        .arg(home.join(".ssh/config"))
        .args(["-G", alias])
        .env("HOME", home)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The mux socket is bounded by construction: a fixed prefix under the
/// runtime directory plus OpenSSH's own 40-character `%C`, inside the real
/// 90-byte budget on any ordinary home directory and uid.
#[test]
fn the_control_path_stays_inside_the_real_budget() {
    let m = Managed::for_home(Path::new("/home/a-developer-with-a-long-name"));
    let rendered = m.control_dir.join("%C").display().to_string();
    let bound = rendered.replace("%C", &"0".repeat(40));
    assert!(
        bound.len() <= CONTROL_PATH_BUDGET,
        "{bound} is {} bytes, over the {CONTROL_PATH_BUDGET}-byte budget",
        bound.len()
    );
}

/// `--print` hands over the same stanza the block carries, plus the
/// client-side settings a Windows dev machine needs (§19.8).
#[test]
fn print_emits_the_stanza_and_the_editor_snippet() {
    let home = tempfile::tempdir().unwrap();
    let m = managed(home.path());
    let lab = lab_at(&home.path().join("labs/probe"), "probe", ONE_VM);
    let block = lab.block();

    let stanza = m.print(&block, "dev01").unwrap();
    assert!(stanza.starts_with("Host vmlab-probe-dev01\n"), "{stanza}");
    assert!(stanza.contains("ssh-proxy probe/dev01"), "{stanza}");
    assert!(m.print(&block, "nope").is_err());

    let snippet = editor_snippet(&block.aliases_for("dev01"), true);
    assert!(
        snippet.contains("\"remote.SSH.localServerDownload\": \"always\""),
        "{snippet}"
    );
    assert!(
        snippet.contains("\"vmlab-probe-dev01\": \"windows\""),
        "{snippet}"
    );
    // A Linux dev machine needs no platform hint at all.
    assert!(!editor_snippet(&block.aliases_for("dev01"), false).contains("remotePlatform"));
}

/// A login label that cannot be one ssh_config token gets no alias of its
/// own — the machine still attaches, and the file stays parseable.
#[test]
fn a_label_that_cannot_be_an_alias_gets_none() {
    let home = tempfile::tempdir().unwrap();
    let lab = lab_at(
        &home.path().join("labs/probe"),
        "probe",
        r#"vm "dev01" {
             template = "x86_64/win"
             login "dev"      { user = "PROBE\\dev" password = "p" default = true }
             login "two words" { user = "PROBE\\x"  password = "p" }
           }"#,
    );
    let block = lab.block();
    assert_eq!(block.alias_names(), vec!["vmlab-probe-dev01".to_string()]);
    // …but it is carried, not swallowed: `vmlab ssh-config` names the login
    // and how to still reach it.
    assert_eq!(
        block.unaliasable,
        vec![("dev01".to_string(), "two words".to_string())]
    );
}

/// The host config's override is a location knob with one code path behind
/// it: the same writer, the same block, a different file.
#[test]
fn the_host_config_override_names_the_file() {
    let home = tempfile::tempdir().unwrap();
    let cfg = crate::config::host::HostConfig {
        ssh_config: Some(home.path().join("elsewhere/ssh.conf")),
        ..Default::default()
    };
    // `from_env` reads the process's own HOME for everything else, so only
    // the override is asserted here; the writer itself is exercised above.
    let m = Managed::from_env(&cfg).unwrap();
    assert_eq!(m.path, home.path().join("elsewhere/ssh.conf"));
    // The `ProxyCommand` names the running binary by path wherever one can
    // be found, because an editor spawns the proxy with an environment vmlab
    // has no say in; `vmlab` on `PATH` is the last resort (a test binary is
    // the case that reaches it).
    assert!(
        m.exe.is_absolute() || m.exe == Path::new("vmlab"),
        "{}",
        m.exe.display()
    );
}

fn have_ssh() -> bool {
    match std::process::Command::new("ssh").arg("-V").output() {
        Ok(_) => true,
        Err(_) => {
            eprintln!("no ssh(1) on this host — skipping the resolver check");
            false
        }
    }
}
