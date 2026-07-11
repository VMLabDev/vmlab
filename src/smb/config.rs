//! Samba `smb.conf` generation for the bundled-`smbd` share backend
//! (PRD §7.5, "Server implementation" strategy 2).
//!
//! The daemon serves each declared `share {}` over SMB. Rather than embed an
//! SMB2 server (no mature Rust server library exists — clients only), this
//! backend drives a stock Samba `smbd`:
//!
//! - runs **unprivileged** (no root): a localhost high port (>1024), every
//!   Samba state/lock/cache directory relocated under the lab's `.vmlab/smb`
//!   tree, and `force user = <invoking unix user>` so the daemon's `smbd`
//!   reads host files with the daemon's own identity rather than needing to
//!   switch uid;
//! - the switch then proxies the segment gateway's port 445 to this high port
//!   (that proxy is the network layer's job, not this module's).
//!
//! Auth baseline is NTLMv2 + mandatory SMB signing. `smb1 = true` on any share
//! relaxes the whole server to the NT1 dialect with NTLMv1/LM acceptance for
//! XP/2003-era guests that never send NTLMv2 (PRD §7.5 — irrelevant as a
//! security concern on an isolated lab segment).

use std::path::PathBuf;
use std::process::Command;

use rand::Rng;

/// Per-VM SMB credential. vmlab generates these automatically and plumbs them
/// into the guest mount. Persisted per lab under `.vmlab/smb/creds` (0600)
/// so remembered guest mappings survive daemon restarts, and so a human can
/// map the share inside an interactive guest session (PRD §7.5 access model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SmbCredentials {
    pub username: String,
    pub password: String,
}

impl SmbCredentials {
    /// Generate the credential for a lab.
    ///
    /// ## Unprivileged-passdb constraint (important)
    ///
    /// Samba's `tdbsam` passdb requires every SMB account to map to a *real
    /// Unix account* (`pdbedit`/`smbpasswd` reject names with no `/etc/passwd`
    /// entry). Creating arbitrary per-VM Unix accounts (`vmlab-<vm>`) needs
    /// root, which this backend deliberately avoids (PRD §7.5 strategy 2 runs
    /// `smbd` unprivileged). So the SMB **username is the invoking Unix user**
    /// (the same identity `force user` exports the host tree as) and there is
    /// **one credential per lab**, with a strong random password.
    ///
    /// The `vm` argument is retained for API shape and so a future privileged /
    /// embedded-server backend (strategy 1) can mint genuinely per-VM accounts
    /// without changing callers; today it does not affect the username.
    ///
    /// ⚠ Deviation from PRD §7.5 "a share is mappable only with its owning VM's
    /// credential": with one unprivileged passdb account, all of a lab's shares
    /// authenticate with the same credential. Per-VM credential scoping needs
    /// either root (to create per-VM Unix users) or the embedded SMB2 server of
    /// strategy 1. `valid users` still gates each share to this single lab
    /// account, so shares remain authenticated (never anonymous).
    pub fn generate(lab: &str, vm: &str) -> SmbCredentials {
        let _ = (lab, vm); // see doc: one credential per lab, mapped to the unix user.
        SmbCredentials {
            username: current_unix_user(),
            password: random_password(),
        }
    }

    /// Load the lab's persisted credential, or mint and persist one.
    ///
    /// Stability matters: guests remember mappings (`/persistent:yes`,
    /// fstab, credential manager). A password that rotated on every
    /// daemon start turned each remembered mapping into "user name or
    /// password is incorrect" on the next `up`. The file lives in
    /// `.vmlab/smb/` so `destroy` wipes it together with the clones.
    pub fn load_or_create(lab: &str, smb_dir: &std::path::Path) -> SmbCredentials {
        let path = smb_dir.join("creds");
        if let Ok(s) = std::fs::read_to_string(&path)
            && let Some((u, p)) = s.trim().split_once(':')
            && u == current_unix_user()
            && !p.is_empty()
        {
            return SmbCredentials {
                username: u.to_string(),
                password: p.to_string(),
            };
        }
        let c = Self::generate(lab, "");
        let _ = std::fs::create_dir_all(smb_dir);
        if std::fs::write(&path, format!("{}:{}", c.username, c.password)).is_ok() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
            }
        }
        c
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
    /// The single account permitted to map this share — its owning VM's
    /// credential (PRD §7.5: "a share is mappable only with its owning VM's
    /// credential"). Becomes `valid users = <allowed_user>`.
    pub allowed_user: String,
}

/// A complete `smb.conf` description for one lab's `smbd` instance.
#[derive(Debug, Clone)]
pub struct SmbConfig {
    /// Localhost high port `smbd` listens on (`smb ports =`).
    pub listen_port: u16,
    /// The lab's `.vmlab/smb` directory; holds the conf, passdb, and every
    /// Samba state/lock/cache/pid directory so `smbd` needs no root.
    pub lab_dir: PathBuf,
    pub shares: Vec<ShareDef>,
    /// True if any share opted into smb1 — toggles the global protocol floor,
    /// signing, and NTLM auth relaxation for the whole instance (a single
    /// `smbd` can only have one protocol floor).
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
        self.lab_dir.join("smbd.pid")
    }

    /// Render the `smb.conf`. Every directive is annotated below; the key
    /// theme is "run as a normal user, keep all state under `lab_dir`".
    pub fn render_conf(&self) -> String {
        let dir = self.lab_dir.display();

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
        let signing = if self.any_smb1 { "auto" } else { "mandatory" };

        let mut out = String::new();
        out.push_str("[global]\n");
        // Listen on a high, unprivileged port; the switch proxies 445 -> here.
        out.push_str(&format!("    smb ports = {}\n", self.listen_port));
        // Bind to loopback only — the only reachable path is the switch proxy.
        out.push_str("    bind interfaces only = yes\n");
        out.push_str("    interfaces = 127.0.0.1\n");
        // --- Unprivileged state relocation (everything under lab_dir) -------
        out.push_str(&format!("    private dir = {dir}\n"));
        out.push_str(&format!("    state directory = {dir}\n"));
        out.push_str(&format!("    cache directory = {dir}\n"));
        out.push_str(&format!("    lock directory = {dir}\n"));
        out.push_str(&format!("    pid directory = {dir}\n"));
        out.push_str(&format!("    ncalrpc dir = {dir}/ncalrpc\n"));
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
        out.push_str(&format!("    server min protocol = {min_proto}\n"));
        out.push_str(&format!("    server signing = {signing}\n"));
        if self.any_smb1 {
            // XP/2003 send NTLMv1/LM only; permit it for this instance.
            out.push_str("    ntlm auth = ntlmv1-permitted\n");
            out.push_str("    lanman auth = yes\n");
            out.push_str("    client lanman auth = yes\n");
        } else {
            out.push_str("    ntlm auth = ntlmv2-only\n");
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
            out.push('\n');
        }

        out
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
            any_smb1: smb1,
            shares: vec![
                ShareDef {
                    name: "src".to_string(),
                    host_path: PathBuf::from("/home/u/proj/src"),
                    readonly: false,
                    smb1,
                    allowed_user: "vmlab-web".to_string(),
                },
                ShareDef {
                    name: "data".to_string(),
                    host_path: PathBuf::from("/home/u/datasets"),
                    readonly: true,
                    smb1,
                    allowed_user: "vmlab-web".to_string(),
                },
            ],
        }
    }

    #[test]
    fn conf_has_unprivileged_dirs() {
        let conf = cfg(false).render_conf();
        for dir_key in [
            "private dir = /labroot/.vmlab/smb",
            "state directory = /labroot/.vmlab/smb",
            "cache directory = /labroot/.vmlab/smb",
            "lock directory = /labroot/.vmlab/smb",
            "pid directory = /labroot/.vmlab/smb",
            "passdb backend = tdbsam:/labroot/.vmlab/smb/passdb.tdb",
        ] {
            assert!(conf.contains(dir_key), "missing `{dir_key}` in:\n{conf}");
        }
        assert!(conf.contains("smb ports = 14450"));
    }

    #[test]
    fn conf_per_share_blocks() {
        let conf = cfg(false).render_conf();
        assert!(conf.contains("[src]"));
        assert!(conf.contains("path = /home/u/proj/src"));
        assert!(conf.contains("[data]"));
        assert!(conf.contains("path = /home/u/datasets"));
        // readonly toggling
        assert!(conf.contains("read only = no")); // src
        assert!(conf.contains("read only = yes")); // data
        // scoping
        assert!(conf.contains("valid users = vmlab-web"));
        assert!(conf.contains("guest ok = no"));
    }

    #[test]
    fn conf_protocol_floor_toggles_on_smb1() {
        let modern = cfg(false).render_conf();
        assert!(modern.contains("server min protocol = SMB2_02"));
        assert!(modern.contains("server signing = mandatory"));
        assert!(modern.contains("ntlm auth = ntlmv2-only"));
        assert!(!modern.contains("ntlmv1-permitted"));

        let legacy = cfg(true).render_conf();
        assert!(legacy.contains("server min protocol = NT1"));
        // signing relaxed for smb1
        assert!(legacy.contains("server signing = auto"));
        assert!(legacy.contains("ntlm auth = ntlmv1-permitted"));
        assert!(legacy.contains("lanman auth = yes"));
    }

    #[test]
    fn conf_never_maps_to_guest() {
        let conf = cfg(false).render_conf();
        assert!(conf.contains("map to guest = never"));
        assert!(conf.contains("security = user"));
    }

    #[test]
    fn credentials_username_is_unix_user_and_password_strong() {
        // Unprivileged passdb constraint: the SMB username must be a real Unix
        // account, so it is the invoking user (same for every VM in a lab).
        let a = SmbCredentials::generate("lab", "web");
        let b = SmbCredentials::generate("lab", "db");
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
}
