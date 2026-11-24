//! Samba `smb.conf` generation for the bundled-`smbd` share backend
//! (PRD §7.5, "Server implementation" strategy 2).
//!
//! The daemon serves each declared `share {}` over SMB. Rather than embed an
//! SMB2 server (no mature Rust server library exists — clients only), this
//! backend drives a stock Samba `smbd`:
//!
//! - runs **unprivileged** (no root): a localhost high port (>1024), every
//!   Samba state/lock/cache directory relocated off the system paths — the
//!   persistent ones under the lab's `.vmlab/smb`, the ones smbd binds unix
//!   sockets in under vmlab's runtime directory (see [`SmbConfig::run_dir`]) —
//!   and `force user = <invoking unix user>` so the daemon's `smbd`
//!   reads host files with the daemon's own identity rather than needing to
//!   switch uid;
//! - the switch then proxies the segment gateway's port 445 to this high port
//!   (that proxy is the network layer's job, not this module's).
//!
//! One `smbd` serves one machine ([`super::LabSmb`]). Auth baseline is NTLMv2 +
//! mandatory SMB signing. `smb1 = true` relaxes **that share**: it alone is
//! reachable over the NT1 dialect, and only an NT1 session logs on with
//! NTLMv1/LM, for XP/2003-era guests that never send NTLMv2 (PRD §7.5). One
//! `smbd` has one protocol floor, so the dialect is on offer to the whole
//! machine as soon as one of its shares asks for it; what keeps the
//! relaxation to the share is in [`SmbConfig::render_conf`] — every other
//! share turns an NT1 session away, and the NTLMv1/LM acceptance sits in a
//! file `smbd` reads for NT1 sessions only ([`SmbConfig::nt1_conf_path`]).
//! Signing is the one setting that cannot follow the session: `smbd` fixes
//! it before a dialect is chosen, so the server of a machine with an `smb1`
//! share offers signing to every session and demands it of none.
//!
//! No SMB1 client is offered the CIFS Unix extensions: with them a guest
//! creates a symlink to any host path inside a writable share
//! (docs/reviews/smb.md F4).

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use rand::RngExt;

/// One machine's SMB credential. vmlab generates these automatically and
/// plumbs them into the guest mount. Persisted per machine under
/// `.vmlab/smb/machines/<machine>/creds` (0600) so remembered guest mappings
/// survive daemon restarts, and so a human can map the share inside an
/// interactive guest session (PRD §7.5 access model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmbCredentials {
    pub username: String,
    pub password: String,
}

impl SmbCredentials {
    /// Generate the credential for one machine.
    ///
    /// ## Unprivileged-passdb constraint (important)
    ///
    /// Samba's `tdbsam` passdb requires every SMB account to map to a *real
    /// Unix account* (`pdbedit`/`smbpasswd` reject names with no `/etc/passwd`
    /// entry). Creating arbitrary per-VM Unix accounts (`vmlab-<vm>`) needs
    /// root, which this backend deliberately avoids (PRD §7.5 strategy 2 runs
    /// `smbd` unprivileged). So the SMB **username is the invoking Unix user**
    /// (the same identity `force user` exports the host tree as) for every
    /// machine, and one passdb holds one account.
    ///
    /// PRD §7.5 "a share is mappable only with its owning VM's credential" is
    /// therefore met with **one `smbd` per machine** ([`super::LabSmb`]): each
    /// has a passdb of its own, holding this account under a strong random
    /// password no other machine is given, and exports that machine's shares
    /// alone.
    pub fn generate() -> SmbCredentials {
        SmbCredentials {
            username: current_unix_user(),
            password: random_password(),
        }
    }

    /// Load the persisted credential of the machine whose SMB state
    /// directory is `smb_dir`, or mint and persist one. Read and write
    /// failures are returned so the server only starts with a saved credential.
    ///
    /// Stability matters: guests remember mappings (`/persistent:yes`,
    /// fstab, credential manager). A password that rotated on every
    /// daemon start turned each remembered mapping into "user name or
    /// password is incorrect" on the next `up`. The file lives under
    /// `.vmlab/smb/` so `destroy` wipes it together with the clones.
    pub fn load_or_create(smb_dir: &Path) -> io::Result<SmbCredentials> {
        fs::create_dir_all(smb_dir)?;
        // Serialize the read and replacement across callers. Lock the
        // directory since persisting the credential replaces its inode.
        let dir = File::open(smb_dir)?;
        dir.lock()?;
        let path = smb_dir.join("creds");
        let saved = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => match fs::read_to_string(&path) {
                Ok(saved) => Some(saved),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => None,
                Err(error) => return Err(error),
            },
            Ok(metadata) if metadata.is_symlink() => None,
            Ok(_) => return Err(io::Error::other("expected a regular SMB credential file")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        if let Some(s) = saved
            && let Some((u, p)) = s.trim().split_once(':')
            && u == current_unix_user()
            && !p.is_empty()
        {
            return Ok(SmbCredentials {
                username: u.to_string(),
                password: p.to_string(),
            });
        }
        let c = Self::generate();
        // Write the private file completely before replacing the old entry.
        // A rename replaces a symlink itself without writing to its target.
        let mut file = tempfile::NamedTempFile::new_in(smb_dir)?;
        write!(file, "{}:{}", c.username, c.password)?;
        file.persist(path).map_err(|error| error.error)?;
        Ok(c)
    }
}

/// A 24-character URL-safe random password. `smbpasswd`/`pdbedit` accept it on
/// stdin so there are no shell-quoting concerns.
fn random_password() -> String {
    // Avoid characters that complicate `net use ... <pass>` quoting on the
    // Windows side and `-U user%pass` on smbclient: stick to alnum.
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..24)
        .map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char)
        .collect()
}

/// One Samba share section — one per VM `share {}` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShareDef {
    /// The SMB share name (`\\<gateway>\<name>`).
    pub name: String,
    /// Absolute host directory to export.
    pub host_path: PathBuf,
    pub readonly: bool,
    /// Whether this share requires the SMB1/NT1 dialect (XP-era guests).
    pub smb1: bool,
    /// The single account permitted to map this share: the one account of
    /// its owning machine's `smbd` (PRD §7.5: "a share is mappable only with
    /// its owning VM's credential"). Becomes `valid users = <allowed_user>`.
    pub allowed_user: String,
}

/// A complete `smb.conf` description for one `smbd` instance: the one that
/// serves one machine's shares.
#[derive(Debug, Clone)]
pub struct SmbConfig {
    /// Localhost high port `smbd` listens on (`smb ports =`).
    pub listen_port: u16,
    /// This server's directory under the lab's `.vmlab/smb`; holds the conf,
    /// passdb, log and the state/cache directories so `smbd` needs no root.
    pub lab_dir: PathBuf,
    /// The directories `smbd` binds unix sockets in — `private dir`
    /// (`msg.sock/<pid>`), `ncalrpc dir` (`np/<pipe>`) — with the volatile
    /// lock and pid directories, under vmlab's runtime directory
    /// ([`crate::paths::smb_runtime_dir`]). A socket path is capped at 108
    /// bytes, so these must not grow with the lab's own path.
    pub run_dir: PathBuf,
    pub shares: Vec<ShareDef>,
    /// True if any share opted into smb1 — lowers the protocol floor and
    /// the signing demand of the whole instance (a single `smbd` has one of
    /// each). Every share that did not opt in then refuses an NT1 session,
    /// and NTLMv1/LM is accepted from NT1 sessions only.
    pub any_smb1: bool,
}

impl SmbConfig {
    pub fn conf_path(&self) -> PathBuf {
        self.lab_dir.join("smb.conf")
    }
    pub fn passdb_path(&self) -> PathBuf {
        self.lab_dir.join("passdb.tdb")
    }
    pub fn log_path(&self) -> PathBuf {
        self.lab_dir.join("smbd.log")
    }
    pub fn pid_path(&self) -> PathBuf {
        self.run_dir.join("smbd.pid")
    }
    pub fn ncalrpc_dir(&self) -> PathBuf {
        self.run_dir.join("ncalrpc")
    }
    /// The settings `smbd` adds for a session that negotiated NT1 and for no
    /// other: `smb.conf` includes `smb.%R.conf`, and `%R` is the dialect of
    /// the session. Written beside `smb.conf` when a share asks for `smb1`.
    pub fn nt1_conf_path(&self) -> PathBuf {
        self.lab_dir.join(format!("smb.{NT1}.conf"))
    }

    /// The text of [`Self::nt1_conf_path`]: what an XP/2003-era client needs
    /// to log on, since it sends NTLMv1 or LM and never NTLMv2. `None` when
    /// no share asks for `smb1`.
    pub fn render_nt1_conf(&self) -> Option<String> {
        self.any_smb1.then(|| {
            [
                "    ntlm auth = ntlmv1-permitted\n",
                "    lanman auth = yes\n",
                "    client lanman auth = yes\n",
            ]
            .concat()
        })
    }

    /// Render the `smb.conf`. Every directive is annotated below; the key
    /// theme is "run as a normal user, keep all state under `lab_dir`" — and
    /// every socket under the short `run_dir`.
    ///
    /// Refuses a share whose name is not a plain share name
    /// ([`share_name_problem`]) or whose host path could change the Samba
    /// configuration or be expanded ([`share_host_problem`]). Refuses a share
    /// whose directory holds this file or lies beside it
    /// ([`share_state_problem`]): its guest would write the configuration
    /// outright. Validation refuses all three first; this keeps a config
    /// built some other way from doing it.
    pub fn render_conf(&self) -> Result<String, ShareRefused> {
        for s in &self.shares {
            if let Some(reason) = share_name_problem(&s.name) {
                return Err(ShareRefused {
                    name: s.name.clone(),
                    reason,
                });
            }
            if let Some(reason) = share_host_problem(&s.host_path, true) {
                return Err(ShareRefused {
                    name: s.name.clone(),
                    reason,
                });
            }
            // This file, the passdb and smbd's sockets: a share over any of
            // them hands the guest the server (docs/reviews/smb.md F1).
            for (state, what) in [
                (&self.lab_dir, "vmlab's SMB state directory"),
                (&self.run_dir, "vmlab's SMB runtime directory"),
            ] {
                if let Some(reason) = share_state_problem(&s.host_path, state, what) {
                    return Err(ShareRefused {
                        name: s.name.clone(),
                        reason,
                    });
                }
            }
        }
        let dir = self.lab_dir.display();
        let run = self.run_dir.display();

        // Unix account the running daemon process belongs to. `force user`
        // makes smbd access the exported tree as this account, so the
        // unprivileged smbd reads host files with the daemon's own identity
        // (no uid switching, no root).
        let unix_user = current_unix_user();

        // Protocol floor. NT1 unlocks the SMB1 server for XP-era guests; SMB2_02
        // is the modern floor (covers Windows 7/2008R2 upward).
        let min_proto = if self.any_smb1 { "NT1" } else { "SMB2_02" };

        // Signing: mandatory is the PRD baseline. SMB1/NTLMv1 clients cannot
        // satisfy mandatory signing reliably, so relax to `auto` when smb1 is
        // in play (still negotiates signing where the client supports it).
        // smbd fixes this before a dialect is negotiated, so unlike the NTLM
        // level below it cannot be kept to NT1 sessions.
        let signing = if self.any_smb1 { "auto" } else { "mandatory" };

        let mut out = String::new();
        out.push_str("[global]\n");
        // Listen on a high, unprivileged port; the switch proxies 445 -> here.
        out.push_str(&format!("    smb ports = {}\n", self.listen_port));
        // Bind to loopback only — the only reachable path is the switch proxy.
        out.push_str("    bind interfaces only = yes\n");
        out.push_str("    interfaces = 127.0.0.1\n");
        // --- Unprivileged state relocation --------------------------------
        // Persistent state beside the lab. Sockets — msg.sock/<pid> in the
        // private dir, np/<pipe> under ncalrpc — and the volatile lock and
        // pid directories under the bounded run_dir. The passdb is named
        // explicitly below, so it stays with the lab despite private dir.
        out.push_str(&format!("    private dir = {run}\n"));
        out.push_str(&format!("    state directory = {dir}\n"));
        out.push_str(&format!("    cache directory = {dir}\n"));
        out.push_str(&format!("    lock directory = {run}\n"));
        out.push_str(&format!("    pid directory = {run}\n"));
        out.push_str(&format!(
            "    ncalrpc dir = {}\n",
            self.ncalrpc_dir().display()
        ));
        out.push_str(&format!(
            "    passdb backend = tdbsam:{}\n",
            self.passdb_path().display()
        ));
        // --- Auth model -----------------------------------------------------
        // User-level security; reject any fallthrough to guest access so a
        // share is *only* reachable with its owning VM's credential.
        out.push_str("    security = user\n");
        out.push_str("    map to guest = never\n");
        out.push_str("    restrict anonymous = 2\n");
        // No RPC pipe at all. An unprivileged smbd runs every session with
        // the uid it started with, which Samba's RPC servers take for root:
        // any guest could set the password of the account over SAMR, or
        // remove it (docs/reviews/smb.md F5). smbd serves a pipe only through
        // samba-dcerpcd, which this keeps it from starting; mounting a share
        // needs none. With it goes the share list (`net view`), which no
        // guest mount reads. Samba before 4.16 has no such switch and
        // ignores the line.
        out.push_str("    rpc start on demand helpers = no\n");
        out.push_str(&format!("    server min protocol = {min_proto}\n"));
        out.push_str(&format!("    server signing = {signing}\n"));
        out.push_str("    ntlm auth = ntlmv2-only\n");
        // Samba's SMB1 server offers the CIFS Unix extensions by default,
        // and with them a client creates a symlink with any target in a
        // writable share: a file on the host that every host program reading
        // the directory follows (docs/reviews/smb.md F4). No guest vmlab
        // mounts for asks for them.
        out.push_str("    smb1 unix extensions = no\n");
        if self.any_smb1 {
            // XP/2003 send NTLMv1/LM only. smbd reads its configuration
            // again once a session has negotiated its dialect, with `%R` as
            // that dialect, so the acceptance in `smb.NT1.conf` holds for
            // NT1 sessions and no other; the file a newer dialect names does
            // not exist, which smbd passes over. After `ntlm auth` above:
            // the later line wins.
            out.push_str(&format!("    include = {dir}/smb.%R.conf\n"));
        }
        // Quieter logging straight to our log file; no syslog, no rotation.
        out.push_str(&format!("    log file = {dir}/smbd.log\n"));
        out.push_str("    log level = 1\n");
        out.push_str("    disable spoolss = yes\n");
        out.push_str("    load printers = no\n");
        out.push_str("    printing = bsd\n");
        out.push_str("    printcap name = /dev/null\n");
        // No NetBIOS name service needed; we are reached by IP via the proxy.
        out.push_str("    disable netbios = yes\n");
        out.push_str("    smb2 leases = no\n");
        // Guests reach us as \\<gateway-ip>\..., a name smbd doesn't own.
        // With msdfs on, DFS-flagged tree connects (Explorer sends them)
        // die in parse_dfs_path_strict ("Hostname ... is not ours") — the
        // guest sees error 67.
        out.push_str("    host msdfs = no\n");
        out.push('\n');

        // --- Per-share sections --------------------------------------------
        for s in &self.shares {
            out.push_str(&format!("[{}]\n", s.name));
            out.push_str(&format!("    path = {}\n", s.host_path.display()));
            out.push_str(&format!(
                "    read only = {}\n",
                if s.readonly { "yes" } else { "no" }
            ));
            // Scope the share to exactly its owning VM's account.
            out.push_str(&format!("    valid users = {}\n", s.allowed_user));
            out.push_str("    browseable = yes\n");
            out.push_str("    guest ok = no\n");
            // Access the host tree as the daemon's own unix user.
            out.push_str(&format!("    force user = {unix_user}\n"));
            // Samba maps a file's unix mode into the NT ACL it reports, so a
            // file without an `x` bit (any `.exe` downloaded or copied on the
            // host) carries no FILE_EXECUTE, and the guest's CreateProcess
            // fails with "Access is denied" while reads and copies work.
            // Granting execute on open changes nothing a guest can write; it
            // could copy the file off the share and run it anyway.
            out.push_str("    acl allow execute always = yes\n");
            if self.any_smb1 && !s.smb1 {
                // The floor above is NT1 for the whole server. A share that
                // did not ask for `smb1` refuses the tree connect of any
                // session that is not SMB2 or newer: smbd runs this with
                // `%R` as the session's dialect, and closes the connection
                // when it fails.
                out.push_str(&format!("    preexec = {MODERN_ONLY}\n"));
                out.push_str("    preexec close = yes\n");
            }
            out.push('\n');
        }

        Ok(out)
    }
}

/// Samba's name for the SMB1 dialect, as `server min protocol` takes it and
/// as `%R` gives it.
const NT1: &str = "NT1";

/// The `preexec` of a share that did not ask for `smb1`, on a server where
/// another did: succeed for a session on SMB2 or newer (`%R` is `SMB2_02`,
/// `SMB2_10`, `SMB3_00`, `SMB3_02` or `SMB3_11`), fail for anything else.
/// The shell is named: smbd does not take a line with `;` in it as one shell
/// command (tried on Samba 4.25.1, where the bare `case` exits 2).
const MODERN_ONLY: &str = r#"/bin/sh -c "case %R in SMB[23]_*) exit 0;; esac; exit 1""#;

/// A share [`SmbConfig::render_conf`] will not write, and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("share \"{name}\": {reason}")]
pub struct ShareRefused {
    pub name: String,
    pub reason: String,
}

/// Section names Samba gives a meaning of its own: a share by one of these
/// names changes the server rather than adding a share. Compared without case,
/// as Samba compares section names.
const RESERVED_SHARE_NAMES: [&str; 5] = ["global", "homes", "printers", "print$", "ipc$"];

/// The longest share name Windows clients accept.
const MAX_SHARE_NAME: usize = 80;

/// Why `name` cannot be a share name, or `None` when it can. A share name is
/// written into `smb.conf` as a section header (`[name]`), so it is held to
/// letters, digits, `_`, `-` and `.`, at most 80 of them, and none of the
/// section names Samba reserves (PRD §7.5).
pub fn share_name_problem(name: &str) -> Option<String> {
    if name.is_empty() {
        return Some("a share name cannot be empty".to_string());
    }
    if name.len() > MAX_SHARE_NAME {
        return Some(format!(
            "a share name is at most {MAX_SHARE_NAME} characters, this one is {}",
            name.len()
        ));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return Some(
            "a share name may hold only letters, digits, `_`, `-` and `.` — it becomes a section \
             header in smb.conf"
                .to_string(),
        );
    }
    let lower = name.to_ascii_lowercase();
    if RESERVED_SHARE_NAMES.contains(&lower.as_str()) {
        return Some(format!(
            "\"{name}\" is a section name Samba reserves (global, homes, printers, print$, IPC$)"
        ));
    }
    None
}

/// Why `host` cannot be a share's host path in `smb.conf`, or `None`. A line
/// break would end the `path =` line. Samba expands percent variables.
pub fn share_host_problem(host: &Path, may_use_smb: bool) -> Option<String> {
    let text = host.to_string_lossy();
    if text.contains(['\n', '\r']) {
        Some(format!("the host path {text:?} holds a line break"))
    } else if may_use_smb && text.contains('%') {
        Some(format!(
            "the host path {text:?} contains %, which Samba uses for variable expansion"
        ))
    } else {
        None
    }
}

/// Why the directory `host` cannot be shared given that `state` is one of
/// vmlab's own directories (`what` names it), or `None` when it can.
///
/// A share is confined to its declared directory (PRD §2), and that holds
/// only while the directory and vmlab's state are apart. A share that holds
/// the lab's `.vmlab`, or lies inside it, shows the guest `smb/smb.conf` —
/// which `smbd` reads again by itself when it changes, so a guest that can
/// write it adds a share on any directory of the host user — and every
/// machine's disks, TPM state and firmware variables, which a read-only
/// share still hands over.
///
/// Both paths are compared as the kernel resolves them, so a symlink or a
/// `..` does not get a share past this.
pub fn share_state_problem(
    host: &std::path::Path,
    state: &std::path::Path,
    what: &str,
) -> Option<String> {
    let real_host = real_path(host);
    let real_state = real_path(state);
    if real_state.starts_with(&real_host) {
        Some(format!(
            "the host path {} holds {what} ({}) — a guest could rewrite vmlab's own files \
             there; share a directory that does not contain it",
            host.display(),
            state.display()
        ))
    } else if real_host.starts_with(&real_state) {
        Some(format!(
            "the host path {} is inside {what} ({}) — a guest could rewrite vmlab's own \
             files there; share a directory outside it",
            host.display(),
            state.display()
        ))
    } else {
        None
    }
}

/// `path` as the kernel would resolve it. The part that exists is
/// canonicalized — a lab that has never run has no `.vmlab` yet — and the
/// rest is appended as written.
fn real_path(path: &std::path::Path) -> PathBuf {
    let path = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut rest = Vec::new();
    let mut existing = path.as_path();
    loop {
        if let Ok(real) = existing.canonicalize() {
            return rest.iter().rev().fold(real, |p, name| p.join(name));
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            // Nothing of it resolves, or it ends in `..`: as written.
            _ => return path,
        }
    }
}

/// Best-effort current unix username for `force user`. Falls back to the uid
/// or `nobody` when the environment is unhelpful.
pub fn current_unix_user() -> String {
    if let Ok(u) = std::env::var("USER")
        && !u.is_empty()
    {
        return u;
    }
    if let Ok(u) = std::env::var("LOGNAME")
        && !u.is_empty()
    {
        return u;
    }
    // Fall back to `id -un`.
    if let Ok(out) = Command::new("id").arg("-un").output()
        && out.status.success()
    {
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    "nobody".to_string()
}

/// Verify the target `smbd` build retains SMB1/NT1 support (PRD §7.5 ⚠:
/// distros increasingly trim it). We check `smbd -b` for the `WITH_SMB1SERVER`
/// build flag, which Samba sets when the NT1 server is compiled in.
pub fn check_nt1_supported() -> bool {
    match Command::new("smbd").arg("-b").output() {
        Ok(out) if out.status.success() => {
            let txt = String::from_utf8_lossy(&out.stdout);
            txt.contains("WITH_SMB1SERVER")
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(smb1: bool) -> SmbConfig {
        SmbConfig {
            listen_port: 14450,
            lab_dir: PathBuf::from("/labroot/.vmlab/smb"),
            run_dir: PathBuf::from("/run/vmlab/smb/0123456789ab"),
            any_smb1: smb1,
            shares: vec![
                ShareDef {
                    name: "src".to_string(),
                    host_path: PathBuf::from("/home/u/proj/src"),
                    readonly: false,
                    smb1,
                    allowed_user: "labuser".to_string(),
                },
                ShareDef {
                    name: "data".to_string(),
                    host_path: PathBuf::from("/home/u/datasets"),
                    readonly: true,
                    smb1,
                    allowed_user: "labuser".to_string(),
                },
            ],
        }
    }

    /// docs/reviews/smb.md F1: no share section is written over the server's
    /// own files, whichever way round the two directories sit and however
    /// the path is spelled.
    #[test]
    fn a_share_over_the_servers_own_state_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let lab = tmp.path().join("lab");
        let smb = lab.join(".vmlab/smb");
        let run = tmp.path().join("run/smb/0123");
        std::fs::create_dir_all(&smb).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        std::fs::create_dir_all(lab.join("src")).unwrap();
        std::fs::create_dir_all(lab.join(".vmlab/volumes/data")).unwrap();
        std::os::unix::fs::symlink(".vmlab/smb", lab.join("link")).unwrap();
        let render = |host: PathBuf| {
            let mut c = cfg(false);
            c.lab_dir = smb.clone();
            c.run_dir = run.clone();
            c.shares[0].host_path = host;
            c.render_conf()
        };
        for host in [
            lab.clone(),
            tmp.path().to_path_buf(),
            lab.join("src/.."),
            lab.join(".vmlab"),
            smb.clone(),
            smb.join("not-there-yet"),
            lab.join("link"),
            tmp.path().join("run"),
            run.join("ncalrpc"),
        ] {
            let Err(refused) = render(host.clone()) else {
                panic!("{} was served", host.display());
            };
            assert_eq!(refused.name, "src");
            assert!(refused.reason.contains("vmlab's SMB"), "{refused}");
        }
        // Beside the state, and a named volume under `.vmlab`, are served.
        for host in [lab.join("src"), lab.join(".vmlab/volumes/data")] {
            if let Err(refused) = render(host) {
                panic!("{refused}");
            }
        }
        // A state directory that does not exist yet is still protected.
        assert!(share_state_problem(&lab, &lab.join("new/.vmlab"), "x").is_some());
        assert!(share_state_problem(&lab.join("src"), &lab.join("new/.vmlab"), "x").is_none());
    }

    #[test]
    fn conf_has_unprivileged_dirs() {
        let conf = cfg(false).render_conf().unwrap();
        for dir_key in [
            "private dir = /run/vmlab/smb/0123456789ab\n",
            "state directory = /labroot/.vmlab/smb",
            "cache directory = /labroot/.vmlab/smb",
            "lock directory = /run/vmlab/smb/0123456789ab\n",
            "pid directory = /run/vmlab/smb/0123456789ab\n",
            "ncalrpc dir = /run/vmlab/smb/0123456789ab/ncalrpc\n",
            "passdb backend = tdbsam:/labroot/.vmlab/smb/passdb.tdb",
        ] {
            assert!(conf.contains(dir_key), "missing `{dir_key}` in:\n{conf}");
        }
        assert!(conf.contains("smb ports = 14450"));
    }

    /// Review F12 (config): a share named `global` would add a second
    /// `[global]` section that rewrites the server's settings, and a `]` or a
    /// line break in a name or a host path writes a directive of its own. The
    /// render refuses each, so the file keeps one `[global]`.
    #[test]
    fn a_share_name_cannot_write_configuration_of_its_own() {
        for name in ["global", "GLOBAL", "homes", "IPC$", "a]b", "a\nb", ""] {
            let mut c = cfg(false);
            c.shares[0].name = name.to_string();
            let refused = c.render_conf().expect_err(name);
            assert_eq!(refused.name, name);
        }
        let mut c = cfg(false);
        c.shares[0].host_path = PathBuf::from("/home/u/x\n[global]");
        assert!(c.render_conf().is_err());

        let mut c = cfg(false);
        c.shares[0].name = "global".to_string();
        let conf = c.render_conf().unwrap_or_default();
        assert!(conf.matches("[global]\n").count() <= 1, "{conf}");

        let conf = cfg(false).render_conf().unwrap();
        assert_eq!(conf.matches("[global]\n").count(), 1);
        for ok in ["src", "Data-2", "my.share", "x_y"] {
            assert_eq!(share_name_problem(ok), None, "{ok}");
        }
    }

    #[test]
    fn conf_per_share_blocks() {
        let conf = cfg(false).render_conf().unwrap();
        assert!(conf.contains("[src]"));
        assert!(conf.contains("path = /home/u/proj/src"));
        assert!(conf.contains("[data]"));
        assert!(conf.contains("path = /home/u/datasets"));
        // readonly toggling
        assert!(conf.contains("read only = no")); // src
        assert!(conf.contains("read only = yes")); // data
        // scoping
        assert!(conf.contains("valid users = labuser"));
        assert!(conf.contains("guest ok = no"));
    }

    /// A host file is executable to the guest whatever its unix mode, on a
    /// read-only share as on a writable one, and the read-only share stays
    /// read-only.
    #[test]
    fn conf_allows_execute_on_every_share() {
        let conf = cfg(false).render_conf().unwrap();
        for name in ["[src]", "[data]"] {
            let section = conf
                .split_once(name)
                .map(|(_, rest)| rest.split("\n[").next().unwrap_or(rest))
                .unwrap_or_else(|| panic!("no {name} section in:\n{conf}"));
            assert!(
                section.contains("acl allow execute always = yes"),
                "{name} lacks execute:\n{section}"
            );
        }
        let data = conf.split_once("[data]").unwrap().1;
        assert!(data.contains("read only = yes"), "{data}");
    }

    #[test]
    fn conf_protocol_floor_toggles_on_smb1() {
        let modern = cfg(false).render_conf().unwrap();
        assert!(modern.contains("server min protocol = SMB2_02"));
        assert!(modern.contains("server signing = mandatory"));
        assert!(modern.contains("ntlm auth = ntlmv2-only"));
        assert!(!modern.contains("ntlmv1-permitted"));

        assert!(!modern.contains("include ="));
        assert!(!modern.contains("preexec"));
        assert_eq!(cfg(false).render_nt1_conf(), None);

        let legacy = cfg(true).render_conf().unwrap();
        assert!(legacy.contains("server min protocol = NT1"));
        // signing relaxed for smb1
        assert!(legacy.contains("server signing = auto"));
        // NTLMv1 and LM for NT1 sessions only, from the file smbd reads for
        // them; every share here is smb1, so none turns NT1 away.
        assert!(legacy.contains("ntlm auth = ntlmv2-only"));
        assert!(legacy.contains("include = /labroot/.vmlab/smb/smb.%R.conf\n"));
        assert!(!legacy.contains("preexec"));
        assert_eq!(
            cfg(true).nt1_conf_path(),
            PathBuf::from("/labroot/.vmlab/smb/smb.NT1.conf")
        );
        let nt1 = cfg(true).render_nt1_conf().unwrap();
        assert!(nt1.contains("ntlm auth = ntlmv1-permitted"));
        assert!(nt1.contains("lanman auth = yes"));
    }

    /// The text of one share's section in a rendered `smb.conf`.
    fn section<'a>(conf: &'a str, name: &str) -> &'a str {
        let rest = conf
            .split_once(&format!("[{name}]\n"))
            .unwrap_or_else(|| panic!("no [{name}] section in:\n{conf}"))
            .1;
        rest.split("\n[").next().unwrap_or(rest)
    }

    /// One `smb1` share and one modern share, as two machines of one lab
    /// declare them.
    fn mixed() -> SmbConfig {
        let mut c = cfg(false);
        c.any_smb1 = true;
        c.shares[0].smb1 = true;
        c
    }

    /// docs/reviews/smb.md F4, point 1: the relaxation `smb1 = true` asks for
    /// belongs to that share. The server-wide settings stay at the baseline
    /// for every session newer than SMB1, and the modern share refuses an
    /// SMB1 session. (`an_smb1_share_does_not_relax_the_share_beside_it` in
    /// `server.rs` asks a real smbd the same.)
    #[test]
    fn an_smb1_share_relaxes_only_itself() {
        let conf = mixed().render_conf().unwrap();
        let global = conf.split("\n[").next().unwrap();
        // The baseline, with the dialect on offer: smbd has one floor.
        assert!(global.contains("server min protocol = NT1\n"), "{global}");
        assert!(global.contains("ntlm auth = ntlmv2-only\n"), "{global}");
        for relaxed in ["ntlmv1-permitted", "lanman auth = yes"] {
            assert!(
                !global.contains(relaxed),
                "`{relaxed}` for every session:\n{global}"
            );
        }
        // The modern share turns an SMB1 session away, the smb1 share not.
        let modern = section(&conf, "data");
        assert!(modern.contains("preexec close = yes\n"), "{modern}");
        let gate = "preexec = /bin/sh -c \"case %R in SMB[23]_*) exit 0;; esac; exit 1\"\n";
        assert!(modern.contains(gate), "{modern}");
        assert!(!section(&conf, "src").contains("preexec"), "{conf}");
    }

    /// docs/reviews/smb.md F4, point 2: no SMB1 client is offered the CIFS
    /// Unix extensions, whatever the lab's shares ask for.
    #[test]
    fn conf_never_offers_the_smb1_unix_extensions() {
        for conf in [cfg(false), cfg(true), mixed()] {
            let conf = conf.render_conf().unwrap();
            let global = conf.split("\n[").next().unwrap();
            assert!(
                global.contains("smb1 unix extensions = no\n"),
                "the SMB1 Unix extensions are on offer:\n{global}"
            );
        }
    }

    #[test]
    fn conf_never_maps_to_guest() {
        let conf = cfg(false).render_conf().unwrap();
        assert!(conf.contains("map to guest = never"));
        assert!(conf.contains("security = user"));
    }

    /// No RPC pipe is served, with or without SMB1: over SAMR a guest sets
    /// the password of the account (docs/reviews/smb.md F5).
    #[test]
    fn conf_serves_no_rpc_pipe() {
        for smb1 in [false, true] {
            let conf = cfg(smb1).render_conf().unwrap();
            assert!(
                conf.contains("    rpc start on demand helpers = no\n"),
                "{conf}"
            );
        }
    }

    #[test]
    fn a_percent_in_a_host_path_is_not_a_samba_variable() {
        let mut config = cfg(false);
        config.shares[0].host_path = PathBuf::from("/home/u/100%U");
        let refusal = config
            .render_conf()
            .expect_err("Samba expands percent variables");
        assert_eq!(refusal.name, "src");
        assert!(refusal.reason.contains('%'));
    }

    #[test]
    fn a_credential_file_that_cannot_be_parsed_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("creds");
        std::fs::write(&path, "").unwrap();
        let first = SmbCredentials::load_or_create(dir.path()).unwrap();
        let second = SmbCredentials::load_or_create(dir.path()).unwrap();
        assert!(first == second, "credential changed after restart");
        assert!(
            std::fs::read_to_string(path).unwrap()
                == format!("{}:{}", first.username, first.password),
            "the credential was not saved"
        );
    }

    #[test]
    fn a_credential_path_that_cannot_be_replaced_fails() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("creds")).unwrap();
        assert!(SmbCredentials::load_or_create(dir.path()).is_err());
    }

    #[test]
    fn credentials_username_is_unix_user_and_password_strong() {
        // Unprivileged passdb constraint: the SMB username must be a real Unix
        // account, so it is the invoking user (same for every VM in a lab).
        let a = SmbCredentials::generate();
        let b = SmbCredentials::generate();
        assert_eq!(a.username, current_unix_user());
        assert_eq!(a.username, b.username);
        assert!(!a.username.is_empty());
        // Strong, random passwords (two draws differ with overwhelming odds).
        assert!(a.password.len() >= 20);
        assert_ne!(a.password, b.password);
    }

    #[test]
    fn check_nt1_returns_bool() {
        // Must not panic regardless of whether smbd exists.
        let _ = check_nt1_supported();
    }

    #[cfg(unix)]
    #[test]
    fn persisted_credentials_are_private_and_stable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let first = SmbCredentials::load_or_create(dir.path()).unwrap();
        assert_eq!(
            std::fs::metadata(dir.path().join("creds"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(SmbCredentials::load_or_create(dir.path()).unwrap() == first);
    }

    #[cfg(unix)]
    #[test]
    fn credential_creation_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, "do not overwrite").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join("creds")).unwrap();
        SmbCredentials::load_or_create(dir.path()).unwrap();
        assert_eq!(std::fs::read_to_string(victim).unwrap(), "do not overwrite");
    }
}
