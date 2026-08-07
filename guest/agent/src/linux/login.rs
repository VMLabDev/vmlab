//! A Linux session is a real login, not a `setuid` (PRD §19.2).
//!
//! The cheap implementation gives the right uid and nothing else — no
//! `XDG_RUNTIME_DIR`, no PAM, no keyring — which surfaces as rootless podman
//! failing while everything `$HOME`-relative works, i.e. as a bug nobody can
//! place. So the session must be indistinguishable from having logged in:
//! `HOME`, `USER`, `LOGNAME`, `SHELL` and supplementary groups from the
//! passwd entry, cwd at `HOME`, a login shell, and a runtime directory —
//! **realised through the guest's own login machinery so PAM actually runs**,
//! falling back to a plain `setuid` where that machinery does not exist. That
//! is the same standard `LoadUserProfileW` sets on Windows.
//!
//! **The secret is not used, and that is not an oversight.** The agent is
//! root, and root needs no credential to become an account — which is exactly
//! why §19.2 can say the container floor "costs nothing". Verifying it would
//! mean linking libpam and authenticating against a guest whose accounts vmlab
//! never created, to defend a boundary §1.2 says vmlab is not. What §19.2's
//! loudness rule does bind here is the *account*: one that is not in the
//! guest's passwd fails by name rather than quietly running as root.
//!
//! **Two mechanisms, and which one ran is visible.** `su -l` opens a PAM
//! session, which is what registers the login with logind (hence
//! `XDG_RUNTIME_DIR`), applies `pam_limits`, and unlocks a keyring. A guest
//! with no PAM — a BusyBox container, a stripped appliance — gets the
//! `setuid` route, where the agent assembles by hand everything PAM would
//! have done. Both are named in the agent's log and in the terminal's banner,
//! because "rootless podman does not work here" is answerable only if a
//! developer can see which of the two they got.

use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use vmlab_agent_proto::Logon;

use crate::logon::{Held, LogonCache, LogonKey};
use crate::spawn::{Adopted, Adopter, Identity};

/// The `PATH` a login gets, from `login.defs`' own two answers: `ENV_SUPATH`
/// for uid 0 and `ENV_PATH` for everyone else. Taken from there rather than
/// invented because a login shell that cannot find `ip` — or that *can* find
/// it when a real login could not — is a difference a developer will hit and
/// not be able to explain.
///
/// `SUPATH` doubles as what the agent's own root sessions get, so root's
/// `PATH` is one string rather than two that have to be kept equal.
pub const SUPATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
const USER_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// A resolved guest account — everything a login needs from the guest's own
/// account database, and nothing vmlab invents.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    /// The group list `initgroups` would build: the primary gid plus every
    /// group naming the account. Missing these is the failure §19.2 calls out
    /// by name — `docker`, `kvm` and `dialout` memberships are how a dev box
    /// is usually made usable, and a session without them fails at the one
    /// thing the developer attached to do.
    pub groups: Vec<u32>,
    pub home: String,
    pub shell: String,
}

impl Account {
    /// The ids a spawn drops to.
    fn credentials(&self) -> Credentials {
        Credentials {
            uid: self.uid,
            gid: self.gid,
            groups: self.groups.clone(),
        }
    }

    /// The account's own login shell, invoked as one (`-l`), which is what
    /// reads `/etc/profile` and the user's own dotfiles.
    pub fn login_shell(&self) -> Vec<String> {
        vec![self.shell.clone(), "-l".to_string()]
    }
}

/// Which login machinery realised a session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mechanism {
    /// The guest's own `su`, which opens a PAM session around the process it
    /// starts — logind registration, `pam_limits`, `pam_env`, the keyring.
    Pam { su: String },
    /// No PAM in this guest: the agent assembles the login itself.
    Setuid,
}

impl Mechanism {
    /// How this mechanism reads in a log line and in the terminal banner.
    pub fn describe(&self) -> String {
        match self {
            Mechanism::Pam { su } => format!("a PAM login session ({su} -l)"),
            Mechanism::Setuid => "setuid (this guest has no PAM)".to_string(),
        }
    }
}

/// A live login: who, how, and the runtime directory it was given.
pub struct Session {
    pub account: Account,
    pub mechanism: Mechanism,
    /// Whether a `login {}` asked for this, as against the container floor
    /// the agent falls to on its own. The two are the same session in every
    /// mechanical respect; they differ only in what a banner should claim,
    /// since a floor session is not something the lab author declared.
    pub declared: bool,
    /// `XDG_RUNTIME_DIR`, as seen *inside* the guest. Present whenever the
    /// agent could make one; on the PAM route `pam_systemd` sets the variable
    /// itself, so this is what the `setuid` route exports and what both
    /// routes guarantee exists.
    pub runtime_dir: Option<String>,
}

impl Session {
    /// The environment a login gets, for the route that has to build it. On
    /// the PAM route this is PAM's job and vmlab must not second-guess it.
    pub fn env(&self) -> Vec<(String, String)> {
        let a = &self.account;
        let mut env = vec![
            ("HOME".to_string(), a.home.clone()),
            ("USER".to_string(), a.name.clone()),
            ("LOGNAME".to_string(), a.name.clone()),
            ("SHELL".to_string(), a.shell.clone()),
            (
                "PATH".to_string(),
                if a.uid == 0 { SUPATH } else { USER_PATH }.to_string(),
            ),
            ("LANG".to_string(), "C.UTF-8".to_string()),
        ];
        if let Some(dir) = &self.runtime_dir {
            env.push(("XDG_RUNTIME_DIR".to_string(), dir.clone()));
        }
        env
    }
}

/// The ids a process is dropped to, applied post-fork.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Credentials {
    pub uid: u32,
    pub gid: u32,
    pub groups: Vec<u32>,
}

impl Credentials {
    /// Drop to these ids. Async-signal-safe: raw syscalls on a group list
    /// allocated before the fork, so this is callable from a forked child and
    /// from `pre_exec`.
    ///
    /// Order is load-bearing — the group calls need the privilege `setuid`
    /// gives up, so a `setuid` that ran first would leave the session holding
    /// root's groups.
    ///
    /// # Safety
    /// Must be called between fork and exec, or in a `pre_exec` hook.
    pub unsafe fn apply(&self) -> bool {
        // Already this account: its groups are already ours, because the
        // process was started as it (a container inheriting cinit's user).
        // Nothing to become, and the three calls below would only fail.
        // SAFETY: plain id queries.
        if unsafe { libc::geteuid() } == self.uid && unsafe { libc::getegid() } == self.gid {
            return true;
        }
        // SAFETY: a group list that outlives the call, then two id changes.
        unsafe {
            if libc::setgroups(self.groups.len() as _, self.groups.as_ptr()) != 0 {
                return false;
            }
            if libc::setgid(self.gid) != 0 {
                return false;
            }
            libc::setuid(self.uid) == 0
        }
    }
}

// ---- resolving the account ------------------------------------------------

/// One `/etc/passwd` row.
struct PasswdEntry {
    name: String,
    uid: u32,
    gid: u32,
    home: String,
    shell: String,
}

fn parse_passwd(content: &str) -> Vec<PasswdEntry> {
    content
        .lines()
        .filter_map(|line| {
            // name:passwd:uid:gid:gecos:home:shell
            let f: Vec<&str> = line.split(':').collect();
            if f.len() < 7 {
                return None;
            }
            Some(PasswdEntry {
                name: f[0].to_string(),
                uid: f[2].parse().ok()?,
                gid: f[3].parse().ok()?,
                home: f[5].to_string(),
                shell: f[6].to_string(),
            })
        })
        .collect()
}

/// Every gid in `group` whose member list names `user`.
fn supplementary_groups(content: &str, user: &str) -> Vec<u32> {
    content
        .lines()
        .filter_map(|line| {
            // name:passwd:gid:members
            let f: Vec<&str> = line.split(':').collect();
            if f.len() < 4 {
                return None;
            }
            f.get(3)?
                .split(',')
                .any(|m| m.trim() == user)
                .then(|| f[2].parse().ok())?
        })
        .collect()
}

/// Look up a group by name → gid.
fn find_group(content: &str, name: &str) -> Option<u32> {
    content.lines().find_map(|line| {
        // name:passwd:gid:members
        let f: Vec<&str> = line.split(':').collect();
        (f.len() >= 3 && f[0] == name).then(|| f[2].parse().ok())?
    })
}

/// Resolve an account against passwd/group *content*, so the whole of it is
/// testable with no guest.
///
/// The selector is the same `name[:group]` / `uid[:gid]` spelling cinit takes
/// (`ContainerConfig::user`) and a `login {}` writes, because the container
/// floor arrives in that form: `USER node:node` and `--user 1000:1000` are
/// both ordinary. A bare uid is accepted the way the OCI runtimes accept one,
/// because a container's `USER` is often numeric and absent from passwd; a
/// *name* that is absent is an error, since there is nothing left to guess.
pub fn resolve_account(selector: &str, passwd: &str, group: &str) -> Result<Account, String> {
    let (user, wanted_group) = match selector.split_once(':') {
        Some((_, "")) => return Err(format!("bad account `{selector}`: empty group")),
        Some((user, g)) => (user, Some(g)),
        None => (selector, None),
    };
    if user.is_empty() {
        return Err("empty account name".to_string());
    }
    let entries = parse_passwd(passwd);
    let entry = match user.parse::<u32>() {
        Ok(uid) => entries.iter().find(|e| e.uid == uid),
        Err(_) => Some(entries.iter().find(|e| e.name == user).ok_or_else(|| {
            // §19.2: loud, and naming the account. The host adds the machine.
            format!("no account `{user}` in this guest's /etc/passwd")
        })?),
    };
    // A numeric id absent from passwd: gid 0, no home of its own, and the
    // shell the fallback finds — the same answer `docker run --user 4242`
    // gives, and the only one available.
    let mut account = match entry {
        None => {
            let uid = user.parse::<u32>().expect("numeric selector");
            Account {
                name: user.to_string(),
                uid,
                gid: 0,
                groups: vec![0],
                home: "/".to_string(),
                shell: String::new(),
            }
        }
        Some(entry) => {
            let mut groups = vec![entry.gid];
            for gid in supplementary_groups(group, &entry.name) {
                if !groups.contains(&gid) {
                    groups.push(gid);
                }
            }
            Account {
                name: entry.name.clone(),
                uid: entry.uid,
                gid: entry.gid,
                groups,
                home: if entry.home.is_empty() {
                    "/".to_string()
                } else {
                    entry.home.clone()
                },
                shell: entry.shell.clone(),
            }
        }
    };
    // An explicit group replaces the passwd entry's primary, and with it the
    // group the session's files are created in.
    if let Some(wanted) = wanted_group {
        let gid = match wanted.parse::<u32>() {
            Ok(gid) => gid,
            Err(_) => find_group(group, wanted)
                .ok_or_else(|| format!("no group `{wanted}` in this guest's /etc/group"))?,
        };
        account.groups.retain(|g| *g != account.gid);
        account.gid = gid;
        if !account.groups.contains(&gid) {
            account.groups.insert(0, gid);
        }
    }
    Ok(account)
}

/// Fill in a shell for an account whose passwd entry names none (or names one
/// the guest does not have), so a terminal still lands somewhere.
pub fn usable_shell(root: &Path, shell: &str) -> String {
    let exists = |s: &str| !s.is_empty() && root.join(s.trim_start_matches('/')).exists();
    if exists(shell) {
        return shell.to_string();
    }
    for candidate in ["/bin/bash", "/usr/bin/bash", "/bin/sh"] {
        if exists(candidate) {
            return candidate.to_string();
        }
    }
    "/bin/sh".to_string()
}

// ---- choosing the machinery ------------------------------------------------

/// Whether this guest has login machinery that runs PAM, and where.
///
/// The probe is *PAM config plus a `su` that would read it*: BusyBox ships a
/// `/bin/su` that never links PAM, so a `su` on its own would pick the
/// mechanism that silently does none of what this module exists for. A guest
/// with `/etc/pam.d` has a `su` that is the real one.
pub fn choose_mechanism(root: &Path) -> Mechanism {
    let has = |p: &str| root.join(p.trim_start_matches('/')).exists();
    if !(has("/etc/pam.d/su-l") || has("/etc/pam.d/su")) {
        return Mechanism::Setuid;
    }
    for su in ["/bin/su", "/usr/bin/su"] {
        if has(su) {
            return Mechanism::Pam { su: su.to_string() };
        }
    }
    Mechanism::Setuid
}

/// Create the session's `XDG_RUNTIME_DIR` and hand back its guest-side path.
///
/// Rootless container tooling — the case §19.2 names — puts its state here
/// and fails in ways that read as a broken installation when it is absent.
/// logind owns this directory on a guest that has one and will mount its own
/// tmpfs over ours; doing it anyway is what covers the guests that have PAM
/// but no logind, and the `setuid` route, which has neither.
fn ensure_runtime_dir(root: &Path, account: &Account) -> Option<String> {
    let inside = format!("/run/user/{}", account.uid);
    let path = root.join(inside.trim_start_matches('/'));
    std::fs::create_dir_all(&path).ok()?;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
    let dir = std::fs::File::open(&path).ok()?;
    // Root-owned it would be useless to the session it exists for.
    nix::unistd::fchown(
        dir.as_fd(),
        Some(nix::unistd::Uid::from_raw(account.uid)),
        Some(nix::unistd::Gid::from_raw(account.gid)),
    )
    .ok()?;
    Some(inside)
}

// ---- the shell script the PAM route runs ----------------------------------

/// Quote one argument for the shell `su -c` hands its script to.
///
/// Single quotes, with the one escape POSIX allows inside them. A dev box is
/// full of paths with spaces and of commands with `$` and `"` in them, and a
/// script that resplits them would run something the caller never asked for.
pub fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

/// The script `su -l <user> -c` runs: the caller's environment, its working
/// directory, and then its argv *exec'd*, so the shell su started is replaced
/// rather than left waiting — one process, and the exit code is the command's
/// own.
///
/// `su -l` resets the environment on purpose (that is the login), so the
/// host's overrides are re-applied here rather than passed to `su`, which
/// would drop them.
pub fn login_script(env: &[(String, String)], cwd: Option<&str>, argv: &[String]) -> String {
    let mut script = String::new();
    for (key, value) in env {
        script.push_str(&format!("export {key}={}; ", shell_quote(value)));
    }
    if let Some(cwd) = cwd {
        script.push_str(&format!("cd {} || exit 1; ", shell_quote(cwd)));
    }
    script.push_str("exec");
    for arg in argv {
        script.push(' ');
        script.push_str(&shell_quote(arg));
    }
    script
}

/// `su -l <account>`, optionally running one script rather than a shell.
pub fn su_argv(su: &str, account: &str, script: Option<String>) -> Vec<String> {
    let mut argv = vec![su.to_string(), "-l".to_string(), account.to_string()];
    if let Some(script) = script {
        argv.push("-c".to_string());
        argv.push(script);
    }
    argv
}

// ---- the cache -------------------------------------------------------------

/// The agent's logins — one per agent, which is one per machine, which is why
/// §19.2's "never survives the machine stopping" needs no code.
///
/// What the cache holds on Linux is the *resolution*: the account, the
/// machinery, the runtime directory. The PAM session itself belongs to each
/// `su`, which opens and closes one around the process it starts, so nothing
/// is released when an entry is dropped — there is no hive to unmount. It is
/// still the same cache as Windows' because it answers the same question:
/// every channel resolving one account resolves it once, and identically.
pub struct Logins {
    cache: LogonCache<Session>,
    /// The filesystem the accounts live in: `/` on a VM, the container's
    /// rootfs inside a container micro-VM.
    root: PathBuf,
    /// Whether this guest could have login machinery at all. A container
    /// micro-VM has no init and no logind, so there is nothing for `su` to
    /// open a session *with* — the honest answer there is the `setuid` route
    /// rather than a PAM stack whose session modules all fail.
    pam: bool,
}

impl Logins {
    pub fn for_vm() -> Logins {
        Logins {
            cache: LogonCache::new(),
            root: PathBuf::from("/"),
            pam: true,
        }
    }

    pub fn for_container(rootfs: &str) -> Logins {
        Logins {
            cache: LogonCache::new(),
            root: PathBuf::from(rootfs),
            pam: false,
        }
    }

    /// The live session for `identity`, resolved if the cache has none.
    /// `Identity::Agent` has no session at all — that is §19.2's floor.
    pub fn resolve(&self, identity: &Identity) -> std::io::Result<Option<Arc<Held<Session>>>> {
        let Identity::Declared(logon) = identity else {
            return Ok(None);
        };
        let held = self.cache.get_or_mint(
            LogonKey::new(&logon.user, &logon.secret),
            Instant::now(),
            || self.mint(logon),
        )?;
        Ok(Some(held))
    }

    fn mint(&self, logon: &Logon) -> std::io::Result<Session> {
        let session = self.log_in(&logon.user, true).map_err(|e| {
            // §19.2: failure is loud and names the account. Falling back to
            // the agent identity would leave commands mysteriously running as
            // root and writing root-owned files into the developer's tree.
            std::io::Error::other(format!("cannot log on as `{}`: {e}", logon.user))
        })?;
        announce(&session);
        Ok(session)
    }

    /// The container floor (§19.2): with no `login {}` declared, a session
    /// lands as the user cinit already resolved — the declared `user`, else
    /// the image's `USER`, else root. Devcontainers' own default, and it
    /// costs nothing because Linux needs no credential to become that user.
    ///
    /// Best-effort on purpose: cinit resolved the same string against the
    /// same passwd to start the workload, so a failure here means the rootfs
    /// changed underneath us — and landing as root is a better answer than
    /// refusing every attach to a running container.
    pub fn floor(&self, selector: &str) -> Option<Session> {
        match self.log_in(selector, false) {
            Ok(session) => {
                announce(&session);
                Some(session)
            }
            Err(e) => {
                eprintln!(
                    "vmlab-agent: container user `{selector}` is unusable ({e}); sessions will run as root"
                );
                None
            }
        }
    }

    /// Resolve one account into a session against this guest. Separate from
    /// [`Logins::mint`] so a test can drive it against a tempdir root.
    ///
    /// A guest with no passwd file at all is not an error — a distroless
    /// image often has none, and a numeric selector resolves against it
    /// perfectly well, which is the case cinit already tolerates.
    fn log_in(&self, selector: &str, declared: bool) -> Result<Session, String> {
        let read = |file: &str| std::fs::read_to_string(self.root.join(file)).unwrap_or_default();
        let mut account = resolve_account(selector, &read("etc/passwd"), &read("etc/group"))?;
        account.shell = usable_shell(&self.root, &account.shell);
        let mechanism = if self.pam {
            choose_mechanism(&self.root)
        } else {
            Mechanism::Setuid
        };
        let runtime_dir = ensure_runtime_dir(&self.root, &account);
        Ok(Session {
            account,
            mechanism,
            declared,
            runtime_dir,
        })
    }

    /// Drop resolutions nothing holds and nothing has taken lately.
    pub fn sweep(&self) {
        self.cache.sweep(Instant::now());
    }
}

/// Say which mechanism realised a session, once, where it was realised.
///
/// The agent's stderr is the guest's journal (or the micro-VM's console), so
/// this is the durable half of §19.2's "which one ran is observable" — the
/// terminal banner is the half a developer sees without going looking.
fn announce(session: &Session) {
    eprintln!(
        "vmlab-agent: logged `{}` in (uid {}, groups {:?}) via {}{}",
        session.account.name,
        session.account.uid,
        session.account.groups,
        session.mechanism.describe(),
        match &session.runtime_dir {
            Some(dir) => format!(", XDG_RUNTIME_DIR {dir}"),
            None => ", with no runtime directory".to_string(),
        }
    );
}

/// Start the background sweep that drops idle logins.
pub fn start_sweeper(logins: Arc<Logins>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(crate::logon::SWEEP_INTERVAL);
            logins.sweep();
        }
    });
}

// ---- what the seam does with a session ------------------------------------

/// Give the session's terminal to the session.
///
/// A PTY slave the agent opened is root-owned, and an already-open fd works
/// regardless — which is exactly why missing this is invisible until it is
/// not. `tmux`, `sudo` and anything else that reopens `/dev/tty` needs the
/// device itself to belong to the user, the way `login` leaves it. 0620 is
/// what `login` sets: the user writes, their tty group reads.
pub fn own_the_terminal(slave: impl AsFd, account: &Account) {
    let _ = nix::unistd::fchown(
        slave.as_fd(),
        Some(nix::unistd::Uid::from_raw(account.uid)),
        Some(nix::unistd::Gid::from_raw(account.gid)),
    );
    let _ = nix::sys::stat::fchmod(
        slave.as_fd(),
        nix::sys::stat::Mode::from_bits_truncate(0o620),
    );
}

// ---- reading as the session ------------------------------------------------

/// The thread is reading as a session; dropping this puts it back.
///
/// `setfsuid`/`setfsgid` are Linux's per-thread filesystem identity and the
/// exact analogue of the impersonation the Windows adapter does — a `tail`
/// that ran as root would succeed on files the session itself could not open,
/// which is the same class of "indistinguishable from a login" this module is
/// about. Supplementary groups stay root's: they are not per-thread, so a
/// file reachable only through one is the one read this cannot mirror.
///
/// Raw syscalls because musl does not expose `setfsuid`/`setfsgid` at all,
/// and the agent is built against it.
struct AsSession {
    previous_uid: u32,
    previous_gid: u32,
    /// Keeps the resolution alive for as long as the thread is wearing it.
    /// Absent for the container floor, which the cache never held.
    _held: Option<Arc<Held<Session>>>,
}

impl Adopted for AsSession {}

impl Drop for AsSession {
    fn drop(&mut self) {
        set_fs_ids(self.previous_uid, self.previous_gid);
    }
}

/// Set this thread's filesystem uid/gid, returning the previous pair. Neither
/// call reports failure — both answer with the *previous* value — so the
/// caller reads back to find out whether it took.
fn set_fs_ids(uid: u32, gid: u32) -> (u32, u32) {
    // SAFETY: two argument-free-by-value Linux syscalls that only ever affect
    // the calling thread.
    unsafe {
        let previous_gid = libc::syscall(libc::SYS_setfsgid, gid) as u32;
        let previous_uid = libc::syscall(libc::SYS_setfsuid, uid) as u32;
        (previous_uid, previous_gid)
    }
}

/// Lend a session's identity to whichever thread calls the adopter.
///
/// The account is passed rather than read off `held` because the container
/// floor is a session the cache never minted and so has nothing to hold.
pub fn adopter_as(account: Account, held: Option<Arc<Held<Session>>>) -> Adopter {
    Box::new(move || {
        let (previous_uid, previous_gid) = set_fs_ids(account.uid, account.gid);
        Ok(Box::new(AsSession {
            previous_uid,
            previous_gid,
            _held: held.clone(),
        }) as Box<dyn Adopted>)
    })
}

/// The credentials a spawn drops to for `session`, or `None` where the login
/// machinery does the dropping itself.
pub fn credentials_for(session: &Session) -> Option<Credentials> {
    match session.mechanism {
        // `su` drops privileges as part of opening the session; doing it
        // first would leave it unable to.
        Mechanism::Pam { .. } => None,
        Mechanism::Setuid => Some(session.account.credentials()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PASSWD: &str = "root:x:0:0:root:/root:/bin/sh\n\
                          dev:x:1000:1000:Dev User:/home/dev:/bin/bash\n\
                          nohome:x:1001:1001:::/bin/false\n\
                          malformed-line\n";
    const GROUP: &str = "root:x:0:\n\
                         dev:x:1000:\n\
                         docker:x:990:dev,other\n\
                         kvm:x:36:dev\n\
                         wheel:x:10:other\n";

    /// §19.2 names supplementary groups explicitly: a session missing its
    /// `docker`/`kvm` memberships fails at the one thing a dev box exists
    /// for, and does it in a way that looks like a broken install.
    #[test]
    fn an_account_resolves_with_its_home_shell_and_supplementary_groups() {
        let dev = resolve_account("dev", PASSWD, GROUP).unwrap();
        assert_eq!(dev.uid, 1000);
        assert_eq!(dev.gid, 1000);
        assert_eq!(dev.home, "/home/dev");
        assert_eq!(dev.shell, "/bin/bash");
        assert_eq!(
            dev.groups,
            vec![1000, 990, 36],
            "the primary gid first, then every group naming the account"
        );
        assert_eq!(dev.login_shell(), vec!["/bin/bash", "-l"]);
    }

    /// A container's `USER` is often a bare uid that passwd never mentions —
    /// the same case `docker run --user 4242` has to answer.
    #[test]
    fn a_numeric_id_resolves_with_or_without_a_passwd_entry() {
        let known = resolve_account("1000", PASSWD, GROUP).unwrap();
        assert_eq!(known.name, "dev");
        assert_eq!(known.home, "/home/dev");

        let stranger = resolve_account("4242", PASSWD, GROUP).unwrap();
        assert_eq!(stranger.uid, 4242);
        assert_eq!(stranger.gid, 0);
        assert_eq!(stranger.home, "/");
    }

    /// The floor arrives in cinit's own `name[:group]` / `uid[:gid]`
    /// spelling — `USER node:node` and `--user 1000:1000` are both ordinary.
    /// A resolver that could not read them would land the session as root
    /// while the workload ran as somebody else: the acceptance criterion
    /// inverted, and silently.
    #[test]
    fn an_account_may_name_its_group_the_way_a_container_spec_does() {
        let by_name = resolve_account("dev:docker", PASSWD, GROUP).unwrap();
        assert_eq!(by_name.uid, 1000);
        assert_eq!(by_name.gid, 990);
        assert_eq!(
            by_name.groups,
            vec![990, 36],
            "an explicit group replaces the passwd primary, as `initgroups` \
             with an explicit gid would"
        );

        assert_eq!(resolve_account("1000:33", PASSWD, GROUP).unwrap().gid, 33);

        // Two bare numbers need no account database at all, which is the
        // distroless image `cinit` already tolerates having none for.
        let distroless = resolve_account("65532:65532", "", "").unwrap();
        assert_eq!((distroless.uid, distroless.gid), (65532, 65532));
        assert_eq!(distroless.groups, vec![65532]);

        assert!(resolve_account("dev:ghosts", PASSWD, GROUP).is_err());
        assert!(resolve_account("dev:", PASSWD, GROUP).is_err());
    }

    /// §19.2: a declared account that does not exist fails by name — never a
    /// silent fall back to root, which would leave the developer's tree
    /// root-owned and the failure invisible.
    #[test]
    fn an_account_the_guest_does_not_have_fails_by_name() {
        let err = resolve_account("ghost", PASSWD, GROUP).unwrap_err();
        assert!(err.contains("`ghost`"), "{err}");
        assert!(resolve_account("", PASSWD, GROUP).is_err());
    }

    /// A passwd entry with no home is still a login; `/` is where it lands
    /// rather than an empty cwd the spawn would fail on.
    #[test]
    fn a_home_less_entry_still_lands_somewhere() {
        let account = resolve_account("nohome", PASSWD, GROUP).unwrap();
        assert_eq!(account.home, "/");
    }

    fn touch(root: &Path, path: &str) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, "").unwrap();
    }

    /// The probe is PAM config *plus* a `su`: BusyBox ships a `/bin/su` that
    /// links no PAM, so a `su` alone would pick the mechanism that silently
    /// does none of what §19.2 asks for.
    #[test]
    fn pam_is_chosen_only_where_the_guest_has_both_halves() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert_eq!(choose_mechanism(root), Mechanism::Setuid);

        // BusyBox: a `su` with nothing behind it.
        touch(root, "bin/su");
        assert_eq!(choose_mechanism(root), Mechanism::Setuid);

        touch(root, "etc/pam.d/su-l");
        assert_eq!(
            choose_mechanism(root),
            Mechanism::Pam {
                su: "/bin/su".into()
            }
        );
    }

    /// PAM config with no `su` at all is the fallback, not a spawn of a
    /// binary that is not there.
    #[test]
    fn pam_config_without_su_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "etc/pam.d/su");
        assert_eq!(choose_mechanism(dir.path()), Mechanism::Setuid);
    }

    fn session(mechanism: Mechanism) -> Session {
        Session {
            account: resolve_account("dev", PASSWD, GROUP).unwrap(),
            mechanism,
            declared: true,
            runtime_dir: Some("/run/user/1000".to_string()),
        }
    }

    /// §19.2's list, in full: a session that has `HOME` but not `USER`, or a
    /// uid but no `XDG_RUNTIME_DIR`, is the failure this module exists to
    /// prevent.
    #[test]
    fn the_setuid_route_builds_the_whole_login_environment() {
        let env = session(Mechanism::Setuid).env();
        let value = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(value("HOME"), Some("/home/dev"));
        assert_eq!(value("USER"), Some("dev"));
        assert_eq!(value("LOGNAME"), Some("dev"));
        assert_eq!(value("SHELL"), Some("/bin/bash"));
        assert_eq!(value("XDG_RUNTIME_DIR"), Some("/run/user/1000"));
        assert_eq!(
            value("PATH"),
            Some(USER_PATH),
            "a non-root login gets login.defs' ENV_PATH, not root's"
        );
    }

    #[test]
    fn root_gets_login_defs_supath() {
        let mut s = session(Mechanism::Setuid);
        s.account = resolve_account("root", PASSWD, GROUP).unwrap();
        let env = s.env();
        assert!(env.contains(&("PATH".into(), SUPATH.into())));
    }

    /// The PAM route drops privileges inside `su`; doing it first would leave
    /// `su` unable to open a session at all.
    #[test]
    fn only_the_setuid_route_drops_privileges_itself() {
        assert!(credentials_for(&session(Mechanism::Setuid)).is_some());
        assert!(
            credentials_for(&session(Mechanism::Pam {
                su: "/bin/su".into()
            }))
            .is_none()
        );
    }

    /// A dev box is full of paths with spaces and commands with `$` in them,
    /// and `su -c` hands its argument to a shell — so what the caller asked
    /// for has to survive being re-parsed by one.
    #[test]
    fn the_login_script_survives_the_shell_that_runs_it() {
        let script = login_script(
            &[("K".into(), "a b".into())],
            Some("/src/my project"),
            &[
                "git".into(),
                "commit".into(),
                "-m".into(),
                "it's $HOME".into(),
            ],
        );
        assert_eq!(
            script,
            "export K='a b'; cd '/src/my project' || exit 1; \
             exec 'git' 'commit' '-m' 'it'\\''s $HOME'"
        );
    }

    /// `exec` rather than a plain call: the shell `su` started is replaced, so
    /// the exit code the host sees is the command's own.
    #[test]
    fn the_login_script_execs_and_su_is_told_to_log_in() {
        assert_eq!(login_script(&[], None, &["whoami".into()]), "exec 'whoami'");
        assert_eq!(
            su_argv("/bin/su", "dev", None),
            vec!["/bin/su", "-l", "dev"],
            "no script means the account's own login shell"
        );
        assert_eq!(
            su_argv("/bin/su", "dev", Some("exec 'id'".into())),
            vec!["/bin/su", "-l", "dev", "-c", "exec 'id'"]
        );
    }

    /// A passwd entry naming a shell the guest does not have would otherwise
    /// fail the spawn with a bare ENOENT.
    #[test]
    fn a_missing_shell_falls_back_to_one_the_guest_has() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "bin/sh");
        assert_eq!(usable_shell(dir.path(), "/bin/zsh"), "/bin/sh");
        touch(dir.path(), "bin/bash");
        assert_eq!(usable_shell(dir.path(), "/bin/bash"), "/bin/bash");
        assert_eq!(usable_shell(dir.path(), ""), "/bin/bash");
    }

    /// Rootless container tooling puts its state in the runtime directory and
    /// fails as a broken install without one — so it exists before the
    /// session does, and belongs to the session.
    #[test]
    fn a_session_gets_a_runtime_directory_it_owns() {
        let dir = tempfile::tempdir().unwrap();
        let account = Account {
            name: "dev".into(),
            // The build host is not root, so this can only chown to itself.
            uid: nix::unistd::Uid::effective().as_raw(),
            gid: nix::unistd::Gid::effective().as_raw(),
            groups: vec![],
            home: "/home/dev".into(),
            shell: "/bin/sh".into(),
        };
        let inside = ensure_runtime_dir(dir.path(), &account).unwrap();
        assert_eq!(inside, format!("/run/user/{}", account.uid));
        let made = dir.path().join(inside.trim_start_matches('/'));
        assert!(made.is_dir());
        assert_eq!(
            made.metadata().unwrap().permissions().mode() & 0o777,
            0o700,
            "a runtime directory anyone can read is not one"
        );
    }

    /// The whole resolution, against a guest laid out in a tempdir: passwd,
    /// group, the PAM probe and the runtime directory in one answer.
    #[test]
    fn a_guest_with_no_pam_resolves_to_the_setuid_route() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let me = nix::unistd::Uid::effective().as_raw();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(
            root.join("etc/passwd"),
            format!("dev:x:{me}:{me}:Dev:/home/dev:/bin/sh\n"),
        )
        .unwrap();
        std::fs::write(root.join("etc/group"), "docker:x:990:dev\n").unwrap();
        touch(root, "bin/sh");

        let logins = Logins {
            cache: LogonCache::new(),
            root: root.to_path_buf(),
            pam: true,
        };
        let session = logins.log_in("dev", true).unwrap();
        assert_eq!(session.mechanism, Mechanism::Setuid);
        assert_eq!(session.account.groups, vec![me, 990]);
        assert_eq!(session.runtime_dir, Some(format!("/run/user/{me}")));

        // And an account the guest does not have never becomes a session.
        assert!(logins.log_in("ghost", true).is_err());
    }

    /// A container micro-VM has no init and no logind, so `su` would open a
    /// session against nothing. The honest answer there is the `setuid`
    /// route, whatever the image ships.
    #[test]
    fn a_container_never_takes_the_pam_route() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let me = nix::unistd::Uid::effective().as_raw();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(
            root.join("etc/passwd"),
            format!("app:x:{me}:{me}::/app:/bin/sh\n"),
        )
        .unwrap();
        touch(root, "bin/su");
        touch(root, "etc/pam.d/su-l");
        touch(root, "bin/sh");

        let logins = Logins::for_container(root.to_str().unwrap());
        assert_eq!(
            logins.log_in("app", false).unwrap().mechanism,
            Mechanism::Setuid
        );
    }

    /// A distroless image has no account database at all, and cinit starts
    /// its workload anyway. The floor has to reach the same answer, or the
    /// session lands as root beside a workload that is not.
    #[test]
    fn a_guest_with_no_passwd_still_resolves_a_numeric_floor() {
        let dir = tempfile::tempdir().unwrap();
        let logins = Logins::for_container(dir.path().to_str().unwrap());
        let session = logins.floor("65532:65532").unwrap();
        assert_eq!(session.account.uid, 65532);
        assert_eq!(session.account.gid, 65532);
        assert!(!session.declared);
        // A *name* still has nothing to resolve against, and says so.
        assert!(logins.log_in("nonroot", false).is_err());
    }

    /// The cache is the portable one: two channels naming one account resolve
    /// it once, so the file transfer's session and the shell's are the same
    /// session (§19.2).
    #[test]
    fn two_channels_naming_one_account_share_one_resolution() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let me = nix::unistd::Uid::effective().as_raw();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(
            root.join("etc/passwd"),
            format!("dev:x:{me}:{me}::/home/dev:/bin/sh\n"),
        )
        .unwrap();
        let logins = Logins::for_container(root.to_str().unwrap());
        let logon = Logon {
            user: "dev".into(),
            secret: String::new(),
            elevated: false,
        };
        let a = logins
            .resolve(&Identity::Declared(logon.clone()))
            .unwrap()
            .unwrap();
        let b = logins.resolve(&Identity::Declared(logon)).unwrap().unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        // §19.2's floor resolves to no session at all.
        assert!(logins.resolve(&Identity::Agent).unwrap().is_none());
    }

    /// §19.2's exception, at the seam that now delivers it: a file a push or
    /// the syncer produced must be indistinguishable from one the
    /// developer's own shell wrote, parents included.
    ///
    /// `fileops` creates through the adopter rather than through a handle the
    /// seam hands back (#84), so what makes the file the session's is
    /// `setfsuid`/`setfsgid` — the fs identity governs the owner of anything
    /// the thread creates, not just what it may open. The build host is not
    /// root, so the account can only be this uid; what the test pins is that
    /// creation happens *under the guard*.
    #[test]
    fn a_created_file_and_the_directories_it_needed_belong_to_the_session() {
        use std::io::Write;
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().unwrap();
        let account = Account {
            name: "dev".into(),
            uid: nix::unistd::Uid::effective().as_raw(),
            gid: nix::unistd::Gid::effective().as_raw(),
            groups: vec![],
            home: "/home/dev".into(),
            shell: "/bin/sh".into(),
        };

        let adopter = adopter_as(account.clone(), None);
        let adopted = adopter().unwrap();
        // Exactly what a `fileops` worker does: `mkdir` per level, then the
        // open that creates.
        std::fs::create_dir(dir.path().join("deep")).unwrap();
        std::fs::create_dir(dir.path().join("deep/tree")).unwrap();
        let path = dir.path().join("deep/tree/app.conf");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(b"hello").unwrap();
        drop(file);
        drop(adopted);

        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        assert_eq!(path.metadata().unwrap().uid(), account.uid);
        assert_eq!(
            dir.path().join("deep").metadata().unwrap().uid(),
            account.uid,
            "a root-owned parent is the same failure one level up"
        );
    }
}
