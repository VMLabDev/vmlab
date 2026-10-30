//! Which hypervisor runs a machine (PRD §22, ADR-0021), and what the second
//! one cannot run yet.
//!
//! A machine names its hypervisor with `hypervisor = "qemu" | "vmlab"`; one
//! that names none takes the host config's default for its kind, which
//! starts at QEMU for both. `vmlab` is `vmlab-hv`, vmlab's own hypervisor,
//! which grows phase by phase. [`vmlab_refusals`] is **the** list of what it
//! cannot run yet — validation reads it, and so does the lab daemon before
//! it builds a machine — so a phase that lands shrinks one function.

use serde::{Deserialize, Serialize};

use super::model::{AccelMode, Container, MachineCfg, Vm};

/// A hypervisor a machine can run on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HypervisorKind {
    /// QEMU, driven over QMP: every machine vmlab has ever run.
    #[default]
    Qemu,
    /// `vmlab-hv`, driven over its own control protocol.
    Vmlab,
}

impl HypervisorKind {
    /// The spelling of each value, once, for the extractors.
    pub const NAMES: &'static [(&'static str, Self)] =
        &[("qemu", Self::Qemu), ("vmlab", Self::Vmlab)];

    /// The schema spelling, which every surface that reports one uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qemu => "qemu",
            Self::Vmlab => "vmlab",
        }
    }
}

impl std::fmt::Display for HypervisorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The host config's default hypervisor for each machine kind (`host {
/// hypervisor { … } }`). Keyed by kind because the cutover is per phase: a
/// phase that passes its gate flips the default for its machines alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HypervisorDefaults {
    pub container: HypervisorKind,
    pub vm: HypervisorKind,
}

impl HypervisorDefaults {
    /// The hypervisor `machine` runs on: its own `hypervisor`, else this
    /// host's default for its kind.
    pub fn effective(&self, machine: MachineCfg<'_>) -> HypervisorKind {
        match machine {
            MachineCfg::Vm(vm) => vm.hypervisor.unwrap_or(self.vm),
            MachineCfg::Container(c) => c.hypervisor.unwrap_or(self.container),
        }
    }
}

/// What the refusals need to know about the host doing the running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostFacts {
    /// The host's architecture, which a container always runs.
    pub arch: String,
    /// `/dev/kvm` opens read/write.
    pub kvm: bool,
}

impl HostFacts {
    /// This host, probed.
    pub fn probe() -> Self {
        Self {
            arch: std::env::consts::ARCH.to_string(),
            kvm: crate::qemu::kvm_available(),
        }
    }
}

/// The one architecture `vmlab-hv` runs today.
const VMLAB_ARCH: &str = "x86_64";

/// Why `vmlab-hv` cannot run `machine` on `host` — one sentence per reason,
/// each naming the machine, the reason and what to do; empty when it can.
///
/// Phase 1 runs container micro-VMs on x86_64 under KVM, so a VM is refused
/// whole, and the reasons a container can meet are the two host facts. The
/// VM's own reasons are listed beside the whole-VM refusal so the one that
/// replaces it in phase 2 starts from them.
pub fn vmlab_refusals(machine: MachineCfg<'_>, host: &HostFacts) -> Vec<String> {
    match machine {
        MachineCfg::Vm(vm) => vm_refusals(vm),
        MachineCfg::Container(c) => container_refusals(c, host),
    }
}

fn vm_refusals(vm: &Vm) -> Vec<String> {
    let name = &vm.name;
    let mut why = vec![format!(
        "vm \"{name}\": hypervisor = \"vmlab\" runs container micro-VMs only so far; \
         a VM runs on QEMU (PRD §22) — remove `hypervisor`, or set it to \"qemu\""
    )];
    if !vm.qemu_args.is_empty() {
        why.push(qemu_args_refusal("vm", name));
    }
    if vm.accel == Some(AccelMode::Tcg) {
        why.push(format!(
            "vm \"{name}\": accel = \"tcg\" — vmlab-hv runs under KVM only (PRD §22)"
        ));
    }
    if let Some(arch) = crate::qemu::resolve::vm_arch(vm)
        && arch != VMLAB_ARCH
    {
        why.push(arch_refusal("vm", name, &arch));
    }
    why
}

fn container_refusals(c: &Container, host: &HostFacts) -> Vec<String> {
    let name = &c.name;
    let mut why = Vec::new();
    // A container runs the host's own architecture.
    if host.arch != VMLAB_ARCH {
        why.push(arch_refusal("container", name, &host.arch));
    } else if !host.kvm {
        why.push(format!(
            "container \"{name}\": hypervisor = \"vmlab\" needs KVM, and /dev/kvm is not usable \
             on this host — vmlab-hv has no emulation fallback yet (PRD §22); use \
             hypervisor = \"qemu\", which falls back to TCG"
        ));
    }
    why
}

/// `qemu_args` is QEMU's argv; no other hypervisor has one to append to.
fn qemu_args_refusal(kind: &str, name: &str) -> String {
    format!(
        "{kind} \"{name}\": `qemu_args` is QEMU's own argv and means nothing to \
         hypervisor = \"vmlab\" (PRD §22) — drop it, or run this machine on \"qemu\""
    )
}

fn arch_refusal(kind: &str, name: &str, arch: &str) -> String {
    format!(
        "{kind} \"{name}\": hypervisor = \"vmlab\" runs x86_64 only so far, and this is \
         {arch} (PRD §22) — use hypervisor = \"qemu\""
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kvm_host() -> HostFacts {
        HostFacts {
            arch: "x86_64".into(),
            kvm: true,
        }
    }

    fn lab(src: &str) -> crate::config::LabFile {
        let src = format!("import <vmlab.wcl>\n{src}");
        crate::config::load_lab_source(&src, "t.wcl", std::path::Path::new("/nonexistent"))
            .expect("parses")
    }

    #[test]
    fn a_machine_takes_its_own_hypervisor_over_the_hosts_default() {
        let file = lab(r#"lab "l" {
  container "a" { image = "alpine" hypervisor = "vmlab" }
  container "b" { image = "alpine" }
  vm "c" { template = "x86_64/t" hypervisor = "qemu" }
  vm "d" { template = "x86_64/t" }
}"#);
        let defaults = HypervisorDefaults {
            container: HypervisorKind::Qemu,
            vm: HypervisorKind::Vmlab,
        };
        let pick = |name: &str| defaults.effective(file.lab.machine(name).unwrap());
        assert_eq!(pick("a"), HypervisorKind::Vmlab);
        assert_eq!(pick("b"), HypervisorKind::Qemu);
        assert_eq!(pick("c"), HypervisorKind::Qemu);
        assert_eq!(pick("d"), HypervisorKind::Vmlab);
        // With no host config, everything is QEMU.
        let none = HypervisorDefaults::default();
        assert_eq!(
            none.effective(file.lab.machine("b").unwrap()),
            HypervisorKind::Qemu
        );
        assert_eq!(
            none.effective(file.lab.machine("d").unwrap()),
            HypervisorKind::Qemu
        );
    }

    #[test]
    fn a_container_on_an_x86_64_kvm_host_is_refused_nothing() {
        let file = lab(r#"lab "l" { container "a" { image = "alpine" } }"#);
        assert!(vmlab_refusals(file.lab.machine("a").unwrap(), &kvm_host()).is_empty());
    }

    #[test]
    fn a_container_without_kvm_is_refused_naming_kvm() {
        let file = lab(r#"lab "l" { container "a" { image = "alpine" } }"#);
        let host = HostFacts {
            kvm: false,
            ..kvm_host()
        };
        let why = vmlab_refusals(file.lab.machine("a").unwrap(), &host);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("container \"a\""), "{}", why[0]);
        assert!(why[0].contains("/dev/kvm"), "{}", why[0]);
    }

    #[test]
    fn a_container_on_another_arch_is_refused_naming_it() {
        let file = lab(r#"lab "l" { container "a" { image = "alpine" } }"#);
        let host = HostFacts {
            arch: "aarch64".into(),
            kvm: true,
        };
        let why = vmlab_refusals(file.lab.machine("a").unwrap(), &host);
        assert_eq!(why.len(), 1, "{why:?}");
        assert!(why[0].contains("x86_64 only"), "{}", why[0]);
        assert!(why[0].contains("aarch64"), "{}", why[0]);
    }

    #[test]
    fn a_vm_is_refused_with_each_of_its_own_reasons() {
        let file = lab(r#"lab "l" {
  vm "plain" { template = "x86_64/t" }
  vm "busy" {
    template = "aarch64/t"
    accel = "tcg"
    qemu_args = ["-device", "foo"]
  }
}"#);
        let plain = vmlab_refusals(file.lab.machine("plain").unwrap(), &kvm_host());
        assert_eq!(plain.len(), 1, "{plain:?}");
        assert!(
            plain[0].contains("container micro-VMs only"),
            "{}",
            plain[0]
        );

        let busy = vmlab_refusals(file.lab.machine("busy").unwrap(), &kvm_host());
        assert_eq!(busy.len(), 4, "{busy:?}");
        assert!(busy.iter().any(|w| w.contains("`qemu_args`")), "{busy:?}");
        assert!(
            busy.iter().any(|w| w.contains("accel = \"tcg\"")),
            "{busy:?}"
        );
        assert!(busy.iter().any(|w| w.contains("aarch64")), "{busy:?}");
    }
}
