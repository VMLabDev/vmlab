//! The **mount steps**: the ordered commands that realise one guest's shares,
//! whichever transport carries them (PRD §7.5).
//!
//! These used to be assembled inside the lab runtime, which meant the
//! orchestrator carried Windows filesystem-driver paths and registry keys, and
//! the only way to see what a Windows guest would actually be told to run was
//! to boot one. They belong here, next to the SMB command builders and the
//! [`MountStep`](super::MountStep) type that already described them.
//!
//! Nothing here touches a guest. A [`MountPlan`] is a value: the steps, the
//! [`RetryPolicy`] the executor drives them with, and anything the guest
//! cannot be asked to mount at all.

use std::time::Duration;

use super::{MountStep, OsHint};

/// One share attached over virtiofs for the current run — a vhost-user-fs
/// device the guest mounts by tag (§7.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VirtiofsMount {
    pub tag: String,
    pub guest: String,
    /// Enforced host-side by virtiofsd's `--readonly` flag. Linux also
    /// advertises it to the guest with `mount -o ro`; Windows cannot, because
    /// `virtiofs.exe` has no read-only option (its `-o` expects `UID:GID`, not
    /// mount options).
    pub readonly: bool,
}

/// How hard the executor tries each step.
///
/// Early after boot Windows cannot run a mount yet: the agent briefly fails
/// to spawn children, then `net use` returns error 67 until the SMB client
/// service is up — observed at three to four minutes on Server 2025. The
/// window has to cover that, and being a value means a test can assert it
/// without living through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    pub attempts: u32,
    pub delay: Duration,
}

impl RetryPolicy {
    /// The policy every guest mount runs under: five minutes of patience.
    pub const MOUNT: RetryPolicy = RetryPolicy {
        attempts: 30,
        delay: Duration::from_secs(10),
    };
}

/// Everything to run in one guest to mount its shares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountPlan {
    /// In order: virtiofs first, then SMB. A share's provisions never need to
    /// know which transport carried it — by the last step both are mounted at
    /// the guest path the author declared.
    pub steps: Vec<MountStep>,
    pub retry: RetryPolicy,
    /// Shares this guest cannot mount at all, and why. Reported rather than
    /// dropped, so a share that silently never appears is impossible.
    pub unsupported: Vec<String>,
}

impl MountPlan {
    pub fn is_empty(&self) -> bool {
        self.steps.is_empty()
    }
}

/// The guest OS family, from the resolved profile name (which folds in
/// template metadata — a lab's `vm {}` block usually omits `profile`).
///
/// This is the *share mounting* classification, and `windows-legacy` answers
/// [`OsHint::WindowsXp`] because an XP-era guest mounts shares very
/// differently. It is deliberately not the same question as
/// [`crate::labd::playbook::guest_os_of`], which picks a config-weave binary
/// and for which XP-versus-modern is irrelevant.
pub fn guest_os_hint(profile: Option<&str>) -> OsHint {
    match profile {
        Some("windows-legacy") => OsHint::WindowsXp,
        Some(p) if p.starts_with("windows") => OsHint::Windows,
        _ => OsHint::Linux,
    }
}

/// The WinFsp launcher class registering `virtiofs.exe`. virtio-win ships the
/// binary but not the class, so vmlab adds it — idempotently, on every mount.
const WINFSP_CLASS: &str = r"HKLM\Software\WOW6432Node\WinFsp\Services\virtiofs";
const WINFSP_LAUNCHCTL: &str = r"C:\Program Files (x86)\WinFsp\bin\launchctl-x64.exe";
const VIRTIOFS_EXE: &str = r"C:\Program Files\Virtio-Win\VioFS\virtiofs.exe";

/// Everything `os_hint`'s guest must run to mount `virtiofs` (this run's
/// vhost-user-fs devices) and then `smb_steps` (whatever the lab's `smbd`
/// serves it — see [`LabSmb::mount_plan`](super::LabSmb::mount_plan)).
pub fn mount_plan(
    os_hint: OsHint,
    virtiofs: &[VirtiofsMount],
    smb_steps: Vec<MountStep>,
) -> MountPlan {
    let mut plan = virtiofs_plan(os_hint, virtiofs);
    plan.steps.extend(smb_steps);
    plan
}

/// The virtiofs half on its own.
fn virtiofs_plan(os_hint: OsHint, mounts: &[VirtiofsMount]) -> MountPlan {
    let mut steps = Vec::new();
    let mut unsupported = Vec::new();
    let step = |command: &str, args: Vec<String>| MountStep {
        os_hint,
        command: command.to_string(),
        args,
    };
    match os_hint {
        // XP-era guests have no vmlab agent to run commands through and no
        // WinFsp to mount a vhost-user-fs device with. A virtiofs share on
        // one is a declaration that cannot be honoured — say so rather than
        // emitting Linux commands at a Windows guest, which is what fell out
        // of the old orchestrator's `if windows` fork.
        OsHint::WindowsXp => {
            for m in mounts {
                unsupported.push(format!(
                    "virtiofs share \"{}\": XP-era guests have neither the vmlab agent nor \
                     WinFsp — mount it over smb instead (transport = \"smb\")",
                    m.tag
                ));
            }
        }
        OsHint::Windows => {
            // Register the launcher class first, once, then start one
            // launchctl instance per tag. Without WinFsp in the template the
            // launchctl step fails and surfaces as the usual mount warning.
            if !mounts.is_empty() {
                for (value, kind, data) in [
                    ("Executable", "REG_SZ", VIRTIOFS_EXE),
                    ("CommandLine", "REG_SZ", "-t %1 -m %2"),
                    ("Security", "REG_SZ", "D:P(A;;RPWPLC;;;WD)"),
                    ("JobControl", "REG_DWORD", "1"),
                ] {
                    steps.push(step(
                        "reg",
                        vec![
                            "add".into(),
                            WINFSP_CLASS.into(),
                            "/v".into(),
                            value.into(),
                            "/t".into(),
                            kind.into(),
                            "/d".into(),
                            data.into(),
                            "/f".into(),
                        ],
                    ));
                }
            }
            for m in mounts {
                // `readonly` is deliberately absent here: Windows has no
                // guest-side equivalent. See `VirtiofsMount::readonly`.
                steps.push(step(
                    WINFSP_LAUNCHCTL,
                    vec![
                        "start".into(),
                        "virtiofs".into(),
                        format!("viofs-{}", m.tag),
                        m.tag.clone(),
                        m.guest.clone(),
                    ],
                ));
            }
        }
        OsHint::Linux => {
            for m in mounts {
                steps.push(step("mkdir", vec!["-p".into(), m.guest.clone()]));
                let mut args = vec![
                    "-t".into(),
                    "virtiofs".into(),
                    m.tag.clone(),
                    m.guest.clone(),
                ];
                if m.readonly {
                    args.push("-o".into());
                    args.push("ro".into());
                }
                steps.push(step("mount", args));
            }
        }
    }
    MountPlan {
        steps,
        retry: RetryPolicy::MOUNT,
        unsupported,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mounts() -> Vec<VirtiofsMount> {
        vec![
            VirtiofsMount {
                tag: "src".into(),
                guest: "/mnt/src".into(),
                readonly: false,
            },
            VirtiofsMount {
                tag: "docs".into(),
                guest: "/mnt/docs".into(),
                readonly: true,
            },
        ]
    }

    fn argv(step: &MountStep) -> String {
        let mut parts = vec![step.command.clone()];
        parts.extend(step.args.iter().cloned());
        parts.join(" ")
    }

    /// The classification the share layer uses, in full. `windows-legacy`
    /// answers `WindowsXp` here and `Windows` to the playbook layer — two
    /// different questions, both correctly answered.
    #[test]
    fn guest_os_hint_from_the_resolved_profile() {
        assert_eq!(guest_os_hint(Some("windows-server")), OsHint::Windows);
        assert_eq!(guest_os_hint(Some("windows-desktop")), OsHint::Windows);
        assert_eq!(guest_os_hint(Some("windows-legacy")), OsHint::WindowsXp);
        assert_eq!(guest_os_hint(Some("linux-modern")), OsHint::Linux);
        assert_eq!(guest_os_hint(None), OsHint::Linux);
    }

    /// The common path: make the mount point, then mount the tag — and a
    /// read-only share says so.
    #[test]
    fn a_linux_guest_mkdirs_then_mounts_each_tag() {
        let plan = mount_plan(OsHint::Linux, &mounts(), Vec::new());
        let lines: Vec<String> = plan.steps.iter().map(argv).collect();
        assert_eq!(
            lines,
            [
                "mkdir -p /mnt/src",
                "mount -t virtiofs src /mnt/src",
                "mkdir -p /mnt/docs",
                "mount -t virtiofs docs /mnt/docs -o ro",
            ]
        );
        assert!(plan.unsupported.is_empty());
    }

    /// The path that could previously only be checked by booting Windows:
    /// register the WinFsp launcher class once, then one launchctl instance
    /// per tag.
    #[test]
    fn a_windows_guest_registers_winfsp_then_launches_one_per_tag() {
        let plan = mount_plan(OsHint::Windows, &mounts(), Vec::new());
        let lines: Vec<String> = plan.steps.iter().map(argv).collect();
        assert_eq!(
            lines.len(),
            6,
            "four registry values, two launches: {lines:#?}"
        );
        assert_eq!(
            lines[0],
            format!(r"reg add {WINFSP_CLASS} /v Executable /t REG_SZ /d {VIRTIOFS_EXE} /f")
        );
        assert_eq!(
            lines[1],
            format!(r"reg add {WINFSP_CLASS} /v CommandLine /t REG_SZ /d -t %1 -m %2 /f")
        );
        assert_eq!(
            lines[2],
            format!(r"reg add {WINFSP_CLASS} /v Security /t REG_SZ /d D:P(A;;RPWPLC;;;WD) /f")
        );
        assert_eq!(
            lines[3],
            format!(r"reg add {WINFSP_CLASS} /v JobControl /t REG_DWORD /d 1 /f")
        );
        assert_eq!(
            lines[4],
            format!(r"{WINFSP_LAUNCHCTL} start virtiofs viofs-src src /mnt/src")
        );
        assert_eq!(
            lines[5],
            format!(r"{WINFSP_LAUNCHCTL} start virtiofs viofs-docs docs /mnt/docs")
        );
    }

    /// Host-side enforcement distinguishes the shares, but virtio-win exposes
    /// no guest-side flag with which to distinguish their mount plans.
    #[test]
    fn a_windows_readonly_mount_has_the_same_guest_plan_as_readwrite() {
        let mut mount = VirtiofsMount {
            tag: "docs".into(),
            guest: "D:".into(),
            readonly: false,
        };
        let readwrite = mount_plan(OsHint::Windows, std::slice::from_ref(&mount), Vec::new());

        mount.readonly = true;
        let readonly = mount_plan(OsHint::Windows, &[mount], Vec::new());

        assert_eq!(readonly, readwrite);
    }

    /// No virtiofs shares means no registry work either.
    #[test]
    fn a_windows_guest_with_no_virtiofs_shares_registers_nothing() {
        assert!(mount_plan(OsHint::Windows, &[], Vec::new()).is_empty());
    }

    /// The divergence between the guest-OS classifications, made deliberate:
    /// an XP-era guest has neither the agent to run commands through nor
    /// WinFsp to mount with, so a virtiofs share on one is reported as
    /// unmountable rather than issued as commands that cannot work.
    #[test]
    fn a_legacy_windows_guest_cannot_mount_virtiofs_and_says_so() {
        let plan = mount_plan(OsHint::WindowsXp, &mounts(), Vec::new());
        assert!(plan.steps.is_empty(), "{:#?}", plan.steps);
        assert_eq!(plan.unsupported.len(), 2);
        assert!(plan.unsupported[0].contains("\"src\""), "{plan:#?}");
        assert!(
            plan.unsupported[0].contains("transport = \"smb\""),
            "the message says what to do instead: {plan:#?}"
        );
    }

    /// virtiofs first, then SMB — one ordered list, so a provision waiting on
    /// a share never has to know which transport brought it.
    #[test]
    fn smb_steps_follow_the_virtiofs_ones() {
        let smb = vec![MountStep {
            os_hint: OsHint::Linux,
            command: "mount".into(),
            args: vec!["-t".into(), "cifs".into(), "//10.0.0.1/data".into()],
        }];
        let plan = mount_plan(OsHint::Linux, &mounts()[..1], smb);
        let lines: Vec<String> = plan.steps.iter().map(argv).collect();
        assert_eq!(
            lines,
            [
                "mkdir -p /mnt/src",
                "mount -t virtiofs src /mnt/src",
                "mount -t cifs //10.0.0.1/data",
            ]
        );
    }

    /// An SMB-only guest still gets a plan — and the same retry policy, so
    /// the executor has one loop rather than a special case per transport.
    #[test]
    fn the_retry_policy_rides_in_the_plan() {
        let plan = mount_plan(OsHint::Linux, &[], Vec::new());
        assert_eq!(plan.retry, RetryPolicy::MOUNT);
        assert_eq!(plan.retry.attempts, 30);
        assert_eq!(plan.retry.delay, Duration::from_secs(10));
        assert_eq!(
            plan.retry.attempts * plan.retry.delay.as_secs() as u32,
            300,
            "five minutes: Server 2025 takes three to four to bring its smb client up"
        );
    }
}
