//! The managed `~/.ssh/config` block (PRD §19.7) — vmlab's whole host-side
//! footprint for the SSH facade.
//!
//! **One artefact, no private path.** The stanzas go in the developer's own
//! `~/.ssh/config`, inside markers vmlab owns. A vmlab-owned file behind an
//! `Include` was the obvious shape and it fails on evidence: JetBrains
//! Toolbox's config importer does not follow `Include`, so the `Include`'s
//! entire value was serving third-party clients and the one client that needs
//! it cannot read it. Reaching a private file with `-F` was rejected *because*
//! it works — vmlab's own commands would keep succeeding while every editor
//! saw nothing. Sharing one path means a broken or displaced block breaks
//! `vmlab ssh` too, deliberately, so the developer meets the failure at a
//! terminal that can explain it.
//!
//! **The failure mode is "ate someone's ssh config"**, so the write discipline
//! is the feature:
//!
//! - an advisory `flock` across the whole read-modify-write, so two vmlab
//!   commands cannot interleave one;
//! - a temp file in the same directory, fsynced and renamed onto the
//!   **resolved** path, so a stow/chezmoi symlink keeps its symlink instead of
//!   being replaced by a regular file;
//! - an absent file created `0600` under a `0700` `~/.ssh`, and an existing
//!   one keeping the mode it already had;
//! - deterministic ordering and a write only on a real difference, so a
//!   dotfiles diff means something ([`block`]);
//! - a refusal, naming file and line, on markers vmlab cannot read.
//!
//! **Placement stops being a parsing problem.** OpenSSH takes the first value
//! it obtains for each keyword, so an earlier `Host *` setting `ProxyCommand`
//! silently wins. Every write therefore re-hoists vmlab's own region to the
//! top — relocating its own region and never a line the developer wrote — and
//! then asks [`ssh -G`](Managed::verify) whether the resolved proxy command is
//! vmlab's. That is OpenSSH's own resolver, the one every client shells out
//! to, and it catches displacement, an overriding `Host *`, a stale hand-paste
//! and a redirected block with one mechanism and no ssh_config grammar in
//! vmlab.
//!
//! **[`Managed`] is the seam.** Every path this writes to or reads from is a
//! field, so the whole component is testable without a real home directory —
//! which matters more here than anywhere else in vmlab. The host config's
//! `ssh_config` key sets the file for real, with one code path behind it: a
//! *location* knob, never an on/off.
//!
//! **One deviation from §19.7, stated rather than hidden.** The section says
//! `destroy` "withdraws the master with `ssh -O exit <alias>` before removing
//! the stanza … then removes the stanza". [`withdraw`] happens exactly there,
//! first and while the alias still resolves — but the stanza *stays*, because
//! the same section's stronger rule is that stanzas cover **declared**
//! machines: `destroy` removes a machine's disk, not its declaration, and the
//! very next command that loads the lab would render the stanza back. A
//! stanza therefore leaves the block when the lab does — when its root no
//! longer holds a `vmlab.wcl` — which is the pruning rule §19.7 also states.

pub mod block;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::config::host::HostConfig;
use crate::config::model::{Lab, default_login};
use crate::labd::ssh::host_key;
use crate::paths;

/// One alias vmlab publishes: a machine, optionally under one of its declared
/// logins.
///
/// The pairing is `(machine, login)` because "attach as admin" has to be a
/// *pick* in an editor's host list rather than something you must know to
/// type — it is the only way elevation is reachable from a client that
/// invokes `ssh <alias>` and nothing else (§19.7).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Alias {
    pub machine: String,
    /// The login label, or `None` for the machine's default identity.
    pub login: Option<String>,
}

impl Alias {
    /// `vmlab-<lab>-<machine>`, or `vmlab-<lab>-<machine>-<label>`.
    ///
    /// `<lab>/<machine>` is disqualified as an alias because it lands in
    /// `ControlPath` via `%n` and a slash turns the mux socket path into a
    /// nonexistent subdirectory; it survives as the *argument* form, which is
    /// what `ssh-proxy` takes.
    pub fn name(&self, lab: &str) -> String {
        let base = host_key::alias(lab, &self.machine);
        match &self.login {
            Some(label) => format!("{base}-{label}"),
            None => base,
        }
    }
}

/// Everything one lab contributes to the block.
///
/// Built from the `vmlab.wcl` the CLI already has in hand, so working inside
/// a lab directory is enough to register it — and it covers **declared**
/// machines, not running ones. An alias means "this machine exists in this
/// lab", not "it is attachable right now"; listing only running machines
/// would empty the editor's picker at exactly the moment you want it.
#[derive(Debug, Clone)]
pub struct LabBlock {
    pub lab: String,
    /// The lab's canonical root — the key everything prunes by.
    pub root: PathBuf,
    pub aliases: Vec<Alias>,
    /// `(machine, label)` for every login that could not have an alias
    /// ([`alias_safe`]). Carried rather than dropped so `vmlab ssh-config`
    /// can say so: an identity missing from an editor's picker with no
    /// explanation anywhere is the failure this avoids.
    pub unaliasable: Vec<(String, String)>,
}

impl LabBlock {
    /// Every alias a lab file declares, in the block's own order.
    pub fn of(lab: &Lab, root: &Path) -> Self {
        let mut aliases: Vec<Alias> = Vec::new();
        let mut unaliasable: Vec<(String, String)> = Vec::new();
        for machine in lab.machines() {
            let logins = machine.logins();
            let default = default_login(logins).map(|l| l.label.as_str());
            aliases.push(Alias {
                machine: machine.name().to_string(),
                login: None,
            });
            for login in logins.iter().filter(|l| Some(l.label.as_str()) != default) {
                match alias_safe(&login.label) {
                    true => aliases.push(Alias {
                        machine: machine.name().to_string(),
                        login: Some(login.label.clone()),
                    }),
                    false => unaliasable.push((machine.name().to_string(), login.label.clone())),
                }
            }
        }
        Self {
            lab: lab.name.clone(),
            root: canonical(root),
            aliases,
            unaliasable,
        }
    }

    /// This machine's aliases — the bare one and each labelled one.
    pub fn for_machine<'a>(&'a self, machine: &'a str) -> impl Iterator<Item = &'a Alias> {
        self.aliases.iter().filter(move |a| a.machine == machine)
    }

    /// The same, named — for `ssh-config --print`, the editor snippet, and
    /// the withdrawal `destroy` performs.
    pub fn aliases_for(&self, machine: &str) -> Vec<String> {
        self.for_machine(machine)
            .map(|a| a.name(&self.lab))
            .collect()
    }

    /// Every alias in the block, named.
    pub fn alias_names(&self) -> Vec<String> {
        self.aliases.iter().map(|a| a.name(&self.lab)).collect()
    }
}

/// Whether a login label can be part of an alias at all.
///
/// A label is a §19.2 selector and nothing constrains its spelling, but an
/// alias is one whitespace-separated token in a file OpenSSH parses. A label
/// that cannot be one gets no alias of its own rather than a stanza that
/// silently corrupts the file around it; the machine's own alias still
/// attaches, and `ssh -l "<label>"` still selects the identity.
fn alias_safe(label: &str) -> bool {
    !label.is_empty()
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// What a refresh did. `Unchanged` is the common case and writes nothing —
/// the whole point of rendering and comparing rather than rewriting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Unchanged,
    Wrote,
}

/// The file vmlab manages its block in, and everything the stanzas name.
///
/// Every path is a field rather than a call to [`crate::paths`], which is
/// what makes the writer testable against a temporary home
/// ([`Managed::for_home`]).
#[derive(Debug, Clone)]
pub struct Managed {
    /// The config file vmlab writes its block into.
    pub path: PathBuf,
    /// The home directory `ssh -G` resolves under. Equal to the real one
    /// outside tests; the check has to see the same file the client will.
    pub home: PathBuf,
    /// The `vmlab` binary the `ProxyCommand` names — an absolute path
    /// wherever one can be found, because an editor spawns the proxy with an
    /// environment vmlab has no say in and a `PATH` that may not carry it.
    pub exe: PathBuf,
    /// vmlab's own `known_hosts` (§19.3).
    pub known_hosts: PathBuf,
    /// Directory the `ControlPath` mux sockets live in.
    pub control_dir: PathBuf,
    /// The advisory lock every read-modify-write takes. vmlab's own, not a
    /// file dropped in `~/.ssh`.
    pub lock: PathBuf,
}

/// The usable length of a `ControlPath`, in bytes.
///
/// **90, not 108**: `muxserver_listen` binds a temporary `"<path>.<16 random
/// chars>"` *before* the `sun_path` length check, so 108 − 1 − 17 = 90. vmlab
/// does not refuse at generation when a home directory pushes past it — a lab
/// would be valid on one machine and invalid on another — it keeps to the
/// rule that makes the question not arise:
///
/// > Anything vmlab puts in a Unix socket path is bounded by construction,
/// > never by a name it does not control.
///
/// `%C` is OpenSSH's own token, 40 hex characters, and the runtime directory
/// is where every other vmlab control socket already lives.
pub const CONTROL_PATH_BUDGET: usize = 90;

impl Managed {
    /// The real one: the host config's `ssh_config` override, else
    /// `~/.ssh/config`.
    pub fn from_env(cfg: &HostConfig) -> Result<Self> {
        let home = paths::home();
        let mut managed = Self::for_home(&home);
        if let Some(path) = &cfg.ssh_config {
            managed.path = expand_home(path, &home);
        }
        Ok(managed)
    }

    /// The same writer against one directory — the test seam, and the shape
    /// every field of [`Managed`] exists for.
    pub fn for_home(home: &Path) -> Self {
        Self {
            path: home.join(".ssh/config"),
            home: home.to_path_buf(),
            // `vmlab` on `PATH` only where this build cannot locate itself,
            // which no installed binary hits.
            exe: paths::vmlab_exe().unwrap_or_else(|_| PathBuf::from("vmlab")),
            known_hosts: host_key::known_hosts_path(),
            control_dir: paths::ssh_runtime_dir(),
            lock: paths::ssh_runtime_dir().join("config.lock"),
        }
    }

    /// Whether the block lives where OpenSSH reads it without being told.
    fn at_default_location(&self) -> bool {
        self.path == self.home.join(".ssh/config")
    }

    /// Render this lab's stanzas into the block, prune what no longer exists,
    /// re-hoist, and write — but only on a real difference.
    ///
    /// Failure is the caller's to weigh: ambient refreshes warn, and
    /// `vmlab ssh` fails hard, because there the alias is load-bearing
    /// (§19.7's ladder).
    pub fn refresh(&self, block: &LabBlock) -> Result<Outcome> {
        let _lock = self.lock()?;

        let existing = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", self.path.display())),
        };
        let split = block::split(&existing, &self.path)?;

        let mut sections = block::sections(&split.inside);
        // This lab's own section is replaced, by root *and* by name: a lab
        // name is its host-global runtime identity (ADR-0011), so a section
        // claiming this name from another root is stale by definition.
        sections.retain(|s| s.root != block.root && s.lab != block.lab);
        // Pruned by lab root: the block *is* the record, so a root that no
        // longer holds a lab file has its stanzas dropped and there is no
        // bookkeeping file to disagree with.
        sections.retain(|s| s.root.join(paths::LAB_FILE).is_file());
        sections.push(block::Section::of(block, self));

        let body = hoist(&block::render(&sections), &split.outside);
        if body == existing {
            return Ok(Outcome::Unchanged);
        }
        self.write(&body)?;
        // The mux socket's directory has to exist before a client binds one;
        // it is created here rather than by the proxy, which runs under an
        // editor and has nothing to say if it fails.
        paths::ensure_private_dir(&self.control_dir)?;
        self.verify(&block.lab, block.aliases.first())?;
        Ok(Outcome::Wrote)
    }

    /// One machine's stanzas, for `ssh-config --print` — the same text the
    /// block carries, for a client that will not read the file.
    pub fn print(&self, block: &LabBlock, machine: &str) -> Result<String> {
        let mine = LabBlock {
            aliases: block.for_machine(machine).cloned().collect(),
            ..block.clone()
        };
        if mine.aliases.is_empty() {
            bail!("lab \"{}\" declares no machine \"{machine}\"", block.lab);
        }
        Ok(block::Section::of(&mine, self).body.join("\n"))
    }

    /// Take the advisory lock the whole read-modify-write runs under.
    fn lock(&self) -> Result<nix::fcntl::Flock<std::fs::File>> {
        if let Some(parent) = self.lock.parent() {
            paths::ensure_private_dir(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock)
            .with_context(|| format!("opening the lock {}", self.lock.display()))?;
        nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
            .map_err(|(_, e)| anyhow!("locking {}: {e}", self.lock.display()))
    }

    /// Write `body` the only way a file vmlab does not own may be written:
    /// same-directory temp file, fsynced, renamed onto the **resolved** path.
    fn write(&self, body: &str) -> Result<()> {
        use std::io::Write as _;

        // Resolved, so a `~/.ssh/config` that is a stow/chezmoi symlink into a
        // dotfiles repo keeps its symlink: renaming onto the link path would
        // replace the link itself with a regular file.
        let target = self
            .path
            .canonicalize()
            .unwrap_or_else(|_| self.path.clone());
        let dir = target
            .parent()
            .ok_or_else(|| anyhow!("{} has no parent directory", target.display()))?;
        if !dir.is_dir() {
            // An absent `~/.ssh` is created private — the mode OpenSSH itself
            // insists on for the directory holding its keys.
            paths::ensure_private_dir(dir)?;
        }

        let mode = std::fs::metadata(&target)
            .ok()
            .map(|m| {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    m.permissions().mode() & 0o777
                }
                #[cfg(not(unix))]
                {
                    let _ = m;
                    0o600
                }
            })
            .unwrap_or(0o600);

        let mut tmp = tempfile::Builder::new()
            .prefix(".vmlab-ssh-config-")
            .tempfile_in(dir)
            .with_context(|| format!("creating a temp file in {}", dir.display()))?;
        tmp.write_all(body.as_bytes())
            .and_then(|()| tmp.as_file().sync_all())
            .with_context(|| format!("writing {}", target.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(mode))
                .with_context(|| format!("setting the mode on {}", target.display()))?;
        }
        tmp.persist(&target)
            .map_err(|e| anyhow!("renaming onto {}: {}", target.display(), e.error))?;
        // The rename is only durable once the directory entry is.
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
        Ok(())
    }

    /// Ask OpenSSH's own resolver whether the block actually applies.
    ///
    /// `ssh -G` is the resolver every client shells out to, so one mechanism
    /// catches displacement, an overriding `Host *`, a stale hand-paste and a
    /// block redirected to a file `ssh` never reads. A host with no `ssh` at
    /// all cannot be checked and is not failed: there is no client there to
    /// be wrong for.
    ///
    /// It asks about **the user's own config file, by name** — `~/.ssh/config`
    /// — rather than letting `ssh` find it, for two reasons that point the
    /// same way. OpenSSH expands the default `~` from `getpwuid`, not from
    /// `$HOME`, so a test could not otherwise ask about anything but the
    /// developer's real config; and the answer is the same one either way,
    /// because `-F` only drops `/etc/ssh/ssh_config`, which is read *last* and
    /// therefore can never beat a value the user's file already supplied.
    /// A block redirected elsewhere by the host config is checked here too,
    /// and passes only if the user's config actually reaches it — through an
    /// `Include` they wrote — which is the honest answer.
    /// `alias` is the one being asked about — the block's first on a write,
    /// and the one about to be attached to at `vmlab ssh`, which asks for
    /// itself even when nothing was written: an unchanged block that has been
    /// losing all along would otherwise never say so, and `vmlab ssh` is
    /// exactly the terminal §19.7 wants the failure to surface at.
    pub fn verify(&self, lab: &str, alias: Option<&Alias>) -> Result<()> {
        let Some(alias) = alias else {
            return Ok(());
        };
        let name = alias.name(lab);
        let out = match std::process::Command::new("ssh")
            .arg("-F")
            .arg(self.home.join(".ssh/config"))
            .arg("-G")
            .arg(&name)
            .env("HOME", &self.home)
            .output()
        {
            Ok(out) => out,
            Err(e) => {
                tracing::debug!(error = %e, "no ssh(1) to verify the managed block with");
                return Ok(());
            }
        };
        let resolved = String::from_utf8_lossy(&out.stdout)
            .lines()
            .find_map(|l| l.strip_prefix("proxycommand ").map(str::to_string));

        let wanted = format!("ssh-proxy {lab}/{}", alias.machine);
        if resolved.as_deref().is_some_and(|r| r.contains(&wanted)) {
            return Ok(());
        }

        let shown = resolved.unwrap_or_else(|| "(none)".to_string());
        let mut why = format!(
            "vmlab's block is in {} but `ssh -G {name}` resolves a different ProxyCommand:\n  \
             {shown}\n",
            self.path.display()
        );
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        match block::culprit(&text, "proxycommand") {
            Some((pattern, keyword, line)) => why.push_str(&format!(
                "`{keyword}` under `{pattern}` at {}:{line} is read first, and OpenSSH keeps the \
                 first value it obtains for a keyword.",
                self.path.display()
            )),
            None => why.push_str(
                "nothing above vmlab's block sets it, so the winner is outside this file — an \
                 `Include`, /etc/ssh/ssh_config, or an option on the command line.",
            ),
        }
        if !self.at_default_location() {
            why.push_str(&format!(
                "\nThe host config points vmlab's block at {}, which is not the {} `ssh` reads by \
                 itself.",
                self.path.display(),
                self.home.join(".ssh/config").display()
            ));
        }
        bail!(why)
    }
}

/// Put vmlab's region first and everything the developer wrote after it, in
/// the order they wrote it.
///
/// Hoisting is what makes `ProxyCommand` and `ControlPath` land: OpenSSH
/// takes the first value it obtains for a keyword. Only vmlab's own region
/// moves — a developer's `Host *` stays exactly where it was relative to
/// every other line of theirs.
fn hoist(rendered: &str, outside: &[String]) -> String {
    let mut out = String::from(rendered);
    out.push('\n');
    let rest: Vec<&String> = outside.iter().skip_while(|l| l.trim().is_empty()).collect();
    if !rest.is_empty() {
        out.push('\n');
        for line in rest {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// A path the host config wrote, with a leading `~` meaning this home.
fn expand_home(path: &Path, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => home.join(rest),
        Err(_) => path.to_path_buf(),
    }
}

/// A lab root as the block records it. Canonical where the directory exists,
/// so two spellings of one lab cannot both hold sections.
fn canonical(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

/// Refresh the block for a lab the caller has just loaded (§19.7: *any
/// command that successfully loads a lab*), handing back the writer and what
/// it did — the one place the host config, the writer and the block are
/// assembled, so every caller reads the same answer.
pub fn refresh_lab(lab: &Lab, root: &Path) -> Result<(Managed, LabBlock, Outcome)> {
    let cfg = HostConfig::load_default().context("reading the host config")?;
    let managed = Managed::from_env(&cfg)?;
    let block = LabBlock::of(lab, root);
    let outcome = managed.refresh(&block)?;
    Ok((managed, block, outcome))
}

/// The ambient refresh: every command that loads a lab, minus the ones whose
/// own job it is.
///
/// A failure **warns** here (§19.7's ladder) — the command the developer ran
/// was about something else, and `vmlab ssh`, where the alias is
/// load-bearing, calls [`refresh_lab`] itself and fails hard on the same
/// error.
pub fn refresh_or_warn(lab: &Lab, root: &Path) {
    if let Err(e) = refresh_lab(lab, root) {
        eprintln!("vmlab: warning: the managed SSH block was not updated: {e:#}");
    }
}

/// Withdraw a machine's multiplexer before its stanza stops meaning anything
/// (§19.7).
///
/// `ssh -O exit` is the tool's own way to kill a mux, and it needs the alias
/// to still resolve — which is why `destroy` does this *first*, while the
/// block still carries the stanza. Nothing is reported: there is usually no
/// master to withdraw, and a machine being destroyed does not care.
pub fn withdraw(aliases: &[String]) {
    for alias in aliases {
        let _ = std::process::Command::new("ssh")
            .args(["-O", "exit", alias])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
}

/// The editor settings snippet §19.8 describes, for a client that will not
/// read `~/.ssh/config`.
///
/// Both keys are **client-side**, which is the whole reason vmlab hands them
/// over rather than configuring anything: a dev machine cannot make itself
/// offline-capable unilaterally. `localServerDownload: always` makes VS Code
/// download the server host-side and push it over `scp`, and
/// `remotePlatform` is the documented workaround for its Windows
/// host-detection bug.
pub fn editor_snippet(aliases: &[String], windows: bool) -> String {
    let mut out = String::from(
        "// VS Code settings.json — the client-side half (PRD §19.8):\n\
         {\n  \"remote.SSH.localServerDownload\": \"always\"",
    );
    if windows {
        let entries: Vec<String> = aliases
            .iter()
            .map(|a| format!("\n    \"{a}\": \"windows\""))
            .collect();
        out.push_str(&format!(
            ",\n  \"remote.SSH.remotePlatform\": {{{}\n  }}",
            entries.join(",")
        ));
    }
    out.push_str("\n}\n");
    out
}
