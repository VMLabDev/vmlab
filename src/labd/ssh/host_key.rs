//! vmlab's own SSH host keys (PRD §19.3).
//!
//! The guest holds no host key at all — there is no sshd in it — so the
//! facade needs one of its own. It is per (lab, machine), it lives in
//! vmlab's state directory rather than anywhere the developer's own SSH
//! setup can see, and it is written beside a `known_hosts` vmlab also owns.
//! Two consequences follow, and both are the point:
//!
//! - `~/.ssh/known_hosts` is never touched, so vmlab cannot leave anything
//!   behind in a file it does not own.
//! - The key outlives `destroy`. Destroying and recreating `dev01` presents
//!   the same key, so a rebuilt machine never trips a host-key warning
//!   (§19.7) — the real trust boundary is reaching the lab socket, not the
//!   key.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use russh::keys::{Algorithm, PrivateKey};

use crate::paths;

/// `~/.local/state/vmlab/ssh` — the keys and the `known_hosts` beside them.
pub fn ssh_state_dir() -> PathBuf {
    paths::state_dir().join("ssh")
}

/// The `known_hosts` the generated SSH block points clients at (§19.7).
pub fn known_hosts_path() -> PathBuf {
    ssh_state_dir().join("known_hosts")
}

/// Where one machine's key lives: `<state>/ssh/<lab>/<machine>`.
fn key_path(lab: &str, machine: &str) -> PathBuf {
    ssh_state_dir().join(lab).join(machine)
}

/// The alias the generated SSH block gives this machine (§19.7). Also the
/// pattern its `known_hosts` entry is keyed on, since the stanza sets no
/// `HostName` and the alias is therefore what the client verifies against.
pub fn alias(lab: &str, machine: &str) -> String {
    format!("vmlab-{lab}-{machine}")
}

/// This machine's host key, minted on first use and reused forever after.
///
/// Also (re)writes the `known_hosts` entry, so the file is correct after a
/// state directory is restored without one, and so `patterns` picking up a
/// newly declared login label does not need a separate pass.
pub fn load_or_mint(lab: &str, machine: &str, labels: &[String]) -> Result<PrivateKey> {
    let path = key_path(lab, machine);
    let key = match std::fs::read_to_string(&path) {
        Ok(pem) => PrivateKey::from_openssh(&pem)
            .with_context(|| format!("reading the host key {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => mint(&path)?,
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    record_known_host(&known_hosts_path(), &patterns(lab, machine, labels), &key)?;
    Ok(key)
}

/// Ed25519, because every client in the set supports it, it is the smallest
/// thing to write down, and vmlab owns both ends so there is nothing to
/// negotiate with.
fn mint(path: &Path) -> Result<PrivateKey> {
    let key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)
        .context("generating an SSH host key")?;
    if let Some(parent) = path.parent() {
        paths::ensure_private_dir(parent)?;
    }
    let pem = key
        .to_openssh(russh::keys::ssh_key::LineEnding::LF)
        .context("encoding the SSH host key")?;
    write_private(path, pem.as_bytes())?;
    Ok(key)
}

/// Write 0600 from the start: a host key must never exist world-readable,
/// not even for the moment between `write` and a `set_permissions`.
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;

    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(bytes)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Every alias this key answers to, as one `known_hosts` host pattern list:
/// the machine's alias plus one per declared login label, which is the shape
/// §19.7's block generates.
fn patterns(lab: &str, machine: &str, labels: &[String]) -> String {
    let base = alias(lab, machine);
    let mut all = vec![base.clone()];
    all.extend(labels.iter().map(|label| format!("{base}-{label}")));
    all.join(",")
}

/// Put `patterns key` in `known_hosts`, replacing any line already keyed on
/// exactly those patterns.
///
/// Rewriting rather than appending is what keeps the file from growing a
/// stale duplicate every time a machine's declared logins change — and the
/// key itself does not change, so there is never a second key for one
/// machine to disagree with.
fn record_known_host(path: &Path, patterns: &str, key: &PrivateKey) -> Result<()> {
    let public = key
        .public_key()
        .to_openssh()
        .context("encoding the host public key")?;
    let line = format!("{patterns} {public}");

    if let Some(parent) = path.parent() {
        paths::ensure_private_dir(parent)?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<&str> = existing
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| l.split_whitespace().next() != Some(patterns))
        .collect();
    lines.push(&line);
    lines.sort_unstable();
    let mut body = lines.join("\n");
    body.push('\n');
    if body == existing {
        return Ok(());
    }
    write_private(path, body.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole contract of a host key: minted once, then the *same* key
    /// forever — which is what makes a destroyed and recreated machine
    /// present the identity its `known_hosts` entry already records.
    #[test]
    fn a_key_is_minted_once_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev01");
        let first = mint(&path).unwrap();
        let again = PrivateKey::from_openssh(std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(first.public_key(), again.public_key());
    }

    #[cfg(unix)]
    #[test]
    fn a_minted_key_is_never_readable_by_anyone_else() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dev01");
        mint(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "host key mode {mode:o}");
    }

    /// One line per machine, keyed on every alias §19.7 generates for it,
    /// and rewritten in place rather than appended — a machine whose logins
    /// change must not leave a second entry behind.
    #[test]
    fn known_hosts_carries_one_line_per_machine() {
        let dir = tempfile::tempdir().unwrap();
        let hosts = dir.path().join("known_hosts");
        let key = mint(&dir.path().join("dev01")).unwrap();

        let one = patterns("probe", "dev01", &["dev".into(), "admin".into()]);
        record_known_host(&hosts, &one, &key).unwrap();
        record_known_host(&hosts, &one, &key).unwrap();
        let body = std::fs::read_to_string(&hosts).unwrap();
        assert_eq!(body.lines().count(), 1, "{body}");
        assert!(
            body.starts_with("vmlab-probe-dev01,vmlab-probe-dev01-dev,vmlab-probe-dev01-admin "),
            "{body}"
        );
        assert!(body.contains("ssh-ed25519 "), "{body}");

        // A second machine appends rather than replacing.
        let other = mint(&dir.path().join("web01")).unwrap();
        record_known_host(&hosts, &patterns("probe", "web01", &[]), &other).unwrap();
        let body = std::fs::read_to_string(&hosts).unwrap();
        assert_eq!(body.lines().count(), 2, "{body}");
    }
}
