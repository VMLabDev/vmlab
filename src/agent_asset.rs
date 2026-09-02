//! Locate the vmlab-agent guest binaries — the in-guest terminal/exec/file
//! agent the template build bakes into every image (built per guest target
//! by `guest/build-agent.sh`, shipped as `agent/<os>-<arch>/vmlab-agent`
//! plus `VERSION`).
//!
//! Lookup order matches [`crate::guest_asset`]:
//!  1. `$VMLAB_GUEST_ASSET_DIR/agent/<os>-<arch>/` — explicit override (dev
//!     builds point it at `guest/dist/`).
//!  2. `/usr/share/vmlab/guest/agent/<os>-<arch>/` — system-wide install.
//!  3. `~/.local/share/vmlab/guest/agent/<os>-<arch>/` — the per-user data
//!     dir ([`crate::paths::data_dir`]).

use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

const VERSION_FILE: &str = "VERSION";

/// Guest OS flavour of the agent binary. The first two are the Rust
/// `vmlab-agent`; the rest are `vmlab-agent-legacy` (guest/agent-legacy,
/// `guest/build-agent-legacy.sh`), the C agent for guests with no
/// virtio-serial (PRD §7.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentOs {
    Linux,
    Windows,
    /// NT4 through XP/2003.
    WindowsNt,
    /// Windows 95/98/ME.
    Windows9x,
    Dos,
    /// TempleOS: the agent is HolyC source (`guest/agent-templeos`), typed
    /// into the guest over the screen since TempleOS reads no ISO 9660.
    TempleOs,
}

impl AgentOs {
    pub fn key(self) -> &'static str {
        match self {
            AgentOs::Linux => "linux",
            AgentOs::Windows => "windows",
            AgentOs::WindowsNt => "windows-nt",
            AgentOs::Windows9x => "windows-9x",
            AgentOs::Dos => "dos",
            AgentOs::TempleOs => "templeos",
        }
    }

    pub fn binary(self) -> &'static str {
        match self {
            AgentOs::Linux => "vmlab-agent",
            AgentOs::Windows => "vmlab-agent.exe",
            AgentOs::WindowsNt | AgentOs::Windows9x => "vmlab-agent-legacy.exe",
            AgentOs::Dos => "VMLABAGT.EXE",
            AgentOs::TempleOs => "VmlabAgt.HC",
        }
    }

    /// Whether this flavour is a legacy-tier agent (C, or HolyC) rather than
    /// the Rust `vmlab-agent`.
    pub fn is_legacy(self) -> bool {
        matches!(
            self,
            AgentOs::WindowsNt | AgentOs::Windows9x | AgentOs::Dos | AgentOs::TempleOs
        )
    }

    /// The dist directory under `agent/`. The Rust agent is built per guest
    /// arch; the legacy agent is 32-bit x86 for every guest it targets (an
    /// x64 XP runs it under WOW64), so its keys are fixed.
    fn dist_key(self, arch: &str) -> String {
        match self {
            AgentOs::Linux | AgentOs::Windows => format!("{}-{arch}", self.key()),
            AgentOs::WindowsNt => "windows-nt-x86".into(),
            AgentOs::Windows9x => "windows-9x-x86".into(),
            AgentOs::Dos => "dos-i386".into(),
            AgentOs::TempleOs => "templeos".into(),
        }
    }

    fn build_hint(self, key: &str) -> String {
        if self.is_legacy() {
            format!("guest/build-agent-legacy.sh {key}")
        } else {
            format!("guest/build-agent.sh {key}")
        }
    }
}

/// A resolved agent binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAsset {
    pub path: PathBuf,
    /// Content of the optional VERSION file (`"unknown"` when absent).
    pub version: String,
}

impl AgentAsset {
    /// The binary bytes (small — static, opt-level=z).
    pub fn read(&self) -> Result<Vec<u8>> {
        fs::read(&self.path).with_context(|| format!("reading {}", self.path.display()))
    }
}

/// Find the agent binary for a guest `os` + `arch` (e.g. `x86_64`), or fail
/// with every searched path listed.
pub fn ensure_agent_asset(os: AgentOs, arch: &str) -> Result<AgentAsset> {
    find_in(&candidate_dirs(), os, arch)
}

/// Find the agent binary for a guest `os` + `arch` under explicit base
/// directories (the bootstrap-ISO staging tests inject their own).
pub(crate) fn ensure_agent_asset_in(
    dirs: &[PathBuf],
    os: AgentOs,
    arch: &str,
) -> Result<AgentAsset> {
    find_in(dirs, os, arch)
}

/// The base directories searched, in priority order (same roots as the
/// micro-VM boot asset).
pub(crate) fn candidate_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(dir) = env::var_os("VMLAB_GUEST_ASSET_DIR").filter(|d| !d.is_empty()) {
        dirs.push(PathBuf::from(dir));
    }
    dirs.push(PathBuf::from("/usr/share/vmlab/guest"));
    dirs.push(crate::paths::data_dir().join("guest"));
    dirs
}

fn find_in(dirs: &[PathBuf], os: AgentOs, arch: &str) -> Result<AgentAsset> {
    let key = os.dist_key(arch);
    let mut searched = Vec::new();
    for base in dirs {
        let dir = base.join("agent").join(&key);
        let path = dir.join(os.binary());
        if path.is_file() {
            let version = fs::read_to_string(dir.join(VERSION_FILE))
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "unknown".to_string());
            return Ok(AgentAsset { path, version });
        }
        searched.push(dir);
    }
    let searched = searched
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    bail!(
        "no {} binary for {key}; searched: {searched}. Build one with `{}` and install \
         it into one of those directories (or point VMLAB_GUEST_ASSET_DIR at guest/dist).",
        if os.is_legacy() {
            "vmlab-agent-legacy"
        } else {
            "vmlab-agent"
        },
        os.build_hint(&key)
    )
}

/// The text a provision types at the TempleOS shell to land the agent:
/// every line of `source` as an `A("…")` statement into a buffer, then one
/// `FileWrite` to `~/VmlabAgt.HC`, the include, and the spawn. Comment-only
/// and blank lines are dropped because every character costs a keystroke.
/// TempleOS reads no ISO 9660 and has no network, so the screen is the one
/// way in, and this is the shape that survives a shell that compiles each
/// line as it lands (PRD §7.4).
pub fn templeos_typescript(source: &str) -> String {
    let mut out = String::new();
    out.push_str("U8 *g_src=MAlloc(200000);I64 g_n=0;\n");
    out.push_str("U0 A(U8 *s){I64 l=StrLen(s);MemCpy(g_src+g_n,s,l);g_n+=l;g_src[g_n++]='\\n';}\n");
    for line in source.lines() {
        let line = line.trim_end();
        let t = line.trim_start();
        if t.is_empty() || t.starts_with("//") {
            continue;
        }
        out.push_str("A(\"");
        for c in line.chars() {
            match c {
                '\\' => out.push_str("\\\\"),
                '"' => out.push_str("\\\""),
                c => out.push(c),
            }
        }
        out.push_str("\");\n");
    }
    out.push_str("FileWrite(\"~/VmlabAgt.HC\",g_src,g_n);Free(g_src);\n");
    out.push_str("#include \"~/VmlabAgt\"\n");
    out.push_str("VmlabAgentInstall;\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The typescript quotes what HolyC would misread and drops what costs
    /// keystrokes for nothing.
    #[test]
    fn templeos_typescript_escapes_and_prunes() {
        let src = "// header\n\nU0 F(){ \"say \\\"hi\\\"\\n\"; }\n  // trailing\n";
        let ts = templeos_typescript(src);
        let lines: Vec<&str> = ts.lines().collect();
        assert_eq!(
            lines[2],
            "A(\"U0 F(){ \\\"say \\\\\\\"hi\\\\\\\"\\\\n\\\"; }\");"
        );
        assert_eq!(lines.len(), 6, "{ts}");
        assert!(ts.ends_with("#include \"~/VmlabAgt\"\nVmlabAgentInstall;\n"));
    }

    fn write_asset(dir: &std::path::Path, key: &str, binary: &str, version: Option<&str>) {
        let d = dir.join("agent").join(key);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(binary), b"binary").unwrap();
        if let Some(v) = version {
            fs::write(d.join(VERSION_FILE), format!("{v}\n")).unwrap();
        }
    }

    #[test]
    fn finds_linux_and_windows_flavours() {
        let tmp = tempfile::tempdir().unwrap();
        write_asset(tmp.path(), "linux-x86_64", "vmlab-agent", Some("agent=abc"));
        write_asset(tmp.path(), "windows-x86_64", "vmlab-agent.exe", None);
        let dirs = vec![tmp.path().to_path_buf()];

        let linux = find_in(&dirs, AgentOs::Linux, "x86_64").unwrap();
        assert!(linux.path.ends_with("agent/linux-x86_64/vmlab-agent"));
        assert_eq!(linux.version, "agent=abc");
        assert_eq!(linux.read().unwrap(), b"binary");

        let win = find_in(&dirs, AgentOs::Windows, "x86_64").unwrap();
        assert!(win.path.ends_with("agent/windows-x86_64/vmlab-agent.exe"));
        assert_eq!(win.version, "unknown");
    }

    /// The legacy flavours have fixed 32-bit keys whatever the guest arch.
    #[test]
    fn finds_legacy_flavours_at_their_fixed_keys() {
        let tmp = tempfile::tempdir().unwrap();
        write_asset(
            tmp.path(),
            "windows-nt-x86",
            "vmlab-agent-legacy.exe",
            Some("agent-legacy=1"),
        );
        write_asset(tmp.path(), "windows-9x-x86", "vmlab-agent-legacy.exe", None);
        write_asset(tmp.path(), "dos-i386", "VMLABAGT.EXE", None);
        let dirs = vec![tmp.path().to_path_buf()];
        for arch in ["x86", "x86_64"] {
            let nt = find_in(&dirs, AgentOs::WindowsNt, arch).unwrap();
            assert!(
                nt.path
                    .ends_with("agent/windows-nt-x86/vmlab-agent-legacy.exe")
            );
            assert_eq!(nt.version, "agent-legacy=1");
            assert!(find_in(&dirs, AgentOs::Windows9x, arch).is_ok());
            assert!(
                find_in(&dirs, AgentOs::Dos, arch)
                    .unwrap()
                    .path
                    .ends_with("agent/dos-i386/VMLABAGT.EXE")
            );
        }
        let err = find_in(
            &[tempfile::tempdir().unwrap().path().to_path_buf()],
            AgentOs::Dos,
            "x86",
        )
        .unwrap_err();
        assert!(
            format!("{err}").contains("build-agent-legacy.sh dos-i386"),
            "{err}"
        );
    }

    #[test]
    fn priority_order_wins() {
        let tmp = tempfile::tempdir().unwrap();
        let hi = tmp.path().join("hi");
        let lo = tmp.path().join("lo");
        write_asset(&hi, "linux-aarch64", "vmlab-agent", Some("hi"));
        write_asset(&lo, "linux-aarch64", "vmlab-agent", Some("lo"));
        let got = find_in(&[hi, lo], AgentOs::Linux, "aarch64").unwrap();
        assert_eq!(got.version, "hi");
    }

    #[test]
    fn missing_asset_error_lists_paths_and_hint() {
        let tmp = tempfile::tempdir().unwrap();
        let err = find_in(&[tmp.path().to_path_buf()], AgentOs::Windows, "x86_64").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("windows-x86_64"), "{msg}");
        assert!(msg.contains("build-agent.sh windows-x86_64"), "{msg}");
        assert!(
            msg.contains(&tmp.path().join("agent").display().to_string()),
            "{msg}"
        );
    }
}
