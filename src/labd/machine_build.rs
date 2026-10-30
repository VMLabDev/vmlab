//! Building one machine from its declaration, and the set of machines a lab
//! runtime holds.
//!
//! A lab daemon builds every machine when it starts and rebuilds one when a
//! reload finds its block changed (PRD §7.1), so the construction lives here,
//! once, for both: template or image binding (offline — an uncached artefact
//! becomes a deferred pull), hardware resolution (ADR-0008), stable MACs,
//! media and share hosts.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};

use super::container::{ContainerDirs, ContainerInstance, resolve_volume_hosts};
use super::freeze_clone::FreezePlan;
use super::machine::Machine;
use super::network::nic_segment_name;
use super::pull_ledger::PullJob;
use super::share_plan;
use super::state::{LabState, generate_mac};
use super::vm::{VmDirs, VmInstance};
use crate::config::LabFile;
use crate::config::hypervisor::HypervisorKind;
use crate::config::model::{Container, MachineCfg, Nic, TemplateRef, Vm};
use crate::profiles::ProfileSet;
use crate::template::TemplateStore;
use crate::template::store::Purpose;

/// What building a machine needs that is the lab's rather than the machine's.
pub(super) struct BuildContext<'a> {
    /// The lab's effective name (an instance's, when it runs as one).
    pub lab: &'a str,
    pub root: &'a Path,
    pub lab_local: &'a Path,
    pub profiles: &'a ProfileSet,
    pub store: &'a TemplateStore,
    pub home: Option<&'a Path>,
    /// Each assembled segment's effective MTU, which its NICs inherit.
    pub mtus: &'a HashMap<String, u16>,
    /// The main checkout's freeze, in a worktree lab of a frozen family: a
    /// machine about to be cloned from it binds the frozen MACs and pins.
    pub freeze: Option<&'a FreezePlan>,
    /// The host config: the default hypervisor per machine kind and the
    /// vmlab-hv binary (§22).
    pub host: &'a crate::config::host::HostConfig,
}

impl BuildContext<'_> {
    fn nic_mtus(&self, nics: &[Nic]) -> Vec<u16> {
        nics.iter()
            .map(|nic| {
                self.mtus
                    .get(nic_segment_name(nic))
                    .copied()
                    .unwrap_or(crate::labd::network::STANDARD_MTU)
            })
            .collect()
    }
}

/// A VM's declaration as its machine runs it: the lab file's folder paths
/// resolved against the lab root. The daemon's working directory is not the
/// lab's, so a relative `disk { from }` left alone names a folder nobody
/// meant — validation and template builds already read it from the root.
pub(super) fn rooted_vm_config(vm_cfg: &Vm, root: &Path) -> Vm {
    let mut cfg = vm_cfg.clone();
    if let Some(vars) = &cfg.secure_boot_vars {
        cfg.secure_boot_vars = Some(root.join(vars));
    }
    for d in &mut cfg.extra_disks {
        if let Some(from) = &d.from {
            d.from = Some(root.join(from));
        }
    }
    cfg
}

/// Build one VM, and the download it is waiting on when its registry
/// template is not in the store yet. Never downloads.
pub(super) async fn build_vm(
    cx: &BuildContext<'_>,
    vm_cfg: &Vm,
    state: &mut LabState,
) -> Result<(Arc<VmInstance>, Option<PullJob>)> {
    let mut pending = None;
    // Backing disk + effective metadata: the variant the reference
    // resolves to, or the base (§6.6).
    let (backing, meta, variant, disk_size, template_vars) = match &vm_cfg.template {
        TemplateRef::Scratch => (None, None, None, vm_cfg.disk, None),
        TemplateRef::Store {
            arch,
            name: tname,
            version,
            variant,
        } => {
            let resolved = cx
                .store
                .resolve(
                    arch,
                    tname,
                    version.as_deref(),
                    variant.as_ref(),
                    Purpose::Lab,
                )
                .with_context(|| format!("vm \"{}\"", vm_cfg.name))?;
            let template_vars =
                crate::template::store::template_vars(&resolved.dir, &resolved.meta);
            (
                Some(resolved.disk_path),
                Some(resolved.meta),
                resolved.variant,
                None,
                template_vars,
            )
        }
        TemplateRef::Registry { reference, variant } => {
            // A registry reference is pulled on first `up` if absent
            // from the store, never re-pulled implicitly (PRD §6.4).
            // Build NEVER downloads: an uncached template becomes a
            // pending pull (placeholder hardware resolution below) so
            // the daemon starts instantly; `ensure_pulled` binds the
            // real parts at up/start/`pull` time, with progress.
            let arch = vm_cfg.arch.clone().ok_or_else(|| {
                anyhow!(
                    "vm \"{}\": registry template needs an explicit arch",
                    vm_cfg.name
                )
            })?;
            // A version in the store without the variant this
            // machine names is still a pull: of that variant.
            let cached = crate::oci::cached_registry_template(reference, &arch, cx.store)?
                .filter(|v| crate::oci::client::VariantWant::of(variant.as_ref()).held_by(&v.meta));
            match cached {
                Some(version) => {
                    let resolved = version
                        .select(variant.as_ref(), Purpose::Lab)
                        .with_context(|| format!("vm \"{}\"", vm_cfg.name))?;
                    let template_vars =
                        crate::template::store::template_vars(&resolved.dir, &resolved.meta);
                    (
                        Some(resolved.disk_path),
                        Some(resolved.meta),
                        resolved.variant,
                        None,
                        template_vars,
                    )
                }
                None => {
                    pending = Some(PullJob::Template {
                        reference: reference.clone(),
                        arch,
                    });
                    (None, None, None, None, None)
                }
            }
        }
    };

    let resolved = crate::qemu::resolve_vm(vm_cfg, meta.as_ref(), cx.profiles)?;
    // A VM on vmlab-hv is refused anything its delivered phases cannot run;
    // a pending pull's placeholder hardware is checked again once it binds.
    let hypervisor = hypervisor_for(cx, MachineCfg::Vm(vm_cfg), Some(&resolved))?;

    // Stable MACs: explicit > persisted > generated (PRD §9.4). A
    // machine about to be cloned from the freeze has no disk, so
    // nothing persisted here has reached a guest: the frozen
    // machine's MACs take that place, and are persisted with the
    // rest of its record when the clone is made (§21.2).
    let frozen = cx.freeze.and_then(|f| f.record(&vm_cfg.name));
    let mut macs = Vec::new();
    for (i, nic) in vm_cfg.nics.iter().enumerate() {
        let mac = nic
            .mac
            .or_else(|| match frozen {
                Some(record) => record.macs.get(i).copied(),
                None => state.machine_mut(&vm_cfg.name).macs.get(i).copied(),
            })
            .unwrap_or_else(|| generate_mac(cx.lab, &vm_cfg.name, i));
        macs.push(mac);
    }
    if frozen.is_none() {
        state.machine_mut(&vm_cfg.name).macs = macs.clone();
    }

    let dirs = VmDirs::new(cx.lab, &vm_cfg.name, cx.lab_local);
    let mut cdroms = Vec::new();
    if let Some(c) = &vm_cfg.cdrom {
        cdroms.push(cx.root.join(c));
    }
    let mut floppy = vm_cfg.floppy.as_ref().map(|f| cx.root.join(f));

    // media {} blocks: ISO/floppy images built from folders,
    // content-addressed in .vmlab/media (PRD §6.3).
    let media_dir = cx.lab_local.join("media");
    for m in &vm_cfg.media {
        let src = cx.root.join(&m.from);
        // xorriso/mtools under a blocking task: building an ISO from a
        // folder takes seconds, and this runs on the daemon's runtime.
        let (kind, label, dir) = (m.kind, m.label.clone(), media_dir.clone());
        let built = tokio::task::spawn_blocking(move || {
            crate::media::MediaCache::new(dir).ensure(kind, &src, label.as_deref())
        })
        .await
        .map_err(|e| anyhow!("media build task: {e}"))?
        .with_context(|| format!("building media for vm \"{}\"", vm_cfg.name))?;
        match m.kind {
            crate::config::model::MediaKind::Iso => cdroms.push(built),
            crate::config::model::MediaKind::Floppy => {
                if floppy.is_some() {
                    bail!(
                        "vm \"{}\": both a floppy attachment and floppy media declared — \
                         a VM has one floppy drive",
                        vm_cfg.name
                    );
                }
                floppy = Some(built);
            }
        }
    }

    let first_boot = meta.as_ref().and_then(|m| m.first_boot());
    let agent_version = meta.as_ref().and_then(|m| m.agent_version.clone());
    // Each NIC inherits its segment's effective MTU (jumbo on NAT/global
    // by default); drives `host_mtu=` on virtio NICs in the cmdline.
    let nic_mtus = cx.nic_mtus(&vm_cfg.nics);
    let share_hosts: Vec<PathBuf> = vm_cfg
        .shares
        .iter()
        .map(|s| share_plan::resolve_share_host(cx.root, cx.home, &s.host))
        .collect();
    let vm = VmInstance::new(
        cx.lab,
        rooted_vm_config(vm_cfg, cx.root),
        dirs,
        macs,
        nic_mtus,
        cdroms,
        floppy,
        share_hosts,
        crate::labd::vm::TemplateParts {
            resolved,
            backing,
            disk_size,
            first_boot,
            agent_version,
            variant,
            template_vars,
        },
    )
    .on_hypervisor(
        hypervisor,
        super::hypervisor::for_machine(hypervisor, cx.host, true),
    );
    Ok((vm, pending))
}

/// Build one container, and the download it is waiting on when its image is
/// not cached yet. Binds the image offline when it is (the digest pin makes a
/// previously pulled image hit); never downloads — `ensure_pulled` does, with
/// progress, and pins the digest so `up` never re-pulls implicitly (PRD §6.4
/// semantics).
pub(super) fn build_container(
    cx: &BuildContext<'_>,
    c_cfg: &Container,
    state: &mut LabState,
) -> Result<(Arc<ContainerInstance>, Option<PullJob>)> {
    // Micro-VM containers run the host architecture (v1).
    let arch = std::env::consts::ARCH;
    let cache = crate::oci::image::ImageCache::new(crate::paths::oci_cache_dir());
    let mut pending = None;
    // A container about to be cloned from the freeze binds the
    // frozen image pin and MACs, written to the lab's state only
    // when the clone is made (§21.2).
    let mut frozen = cx.freeze.and_then(|f| f.record(&c_cfg.name)).cloned();
    let c_state = match &mut frozen {
        Some(record) => record,
        None => state.machine_mut(&c_cfg.name),
    };
    if c_state.image_ref.as_deref() != Some(c_cfg.image.reference.as_str()) {
        // The `image =` line changed — drop the stale pin.
        c_state.image_digest = None;
    }
    let reference = match &c_state.image_digest {
        // `name:tag@digest` is valid and the digest wins; a
        // reference already carrying a digest equals its pin.
        Some(d) if !c_cfg.image.reference.contains('@') => {
            format!("{}@{}", c_cfg.image.reference, d)
        }
        _ => c_cfg.image.reference.clone(),
    };
    let image = crate::oci::image::cached_container_image(&reference, &cache)
        .with_context(|| format!("container \"{}\": image {}", c_cfg.name, c_cfg.image))?;
    if let Some(image) = &image {
        c_state.image_digest = Some(image.manifest_digest.clone());
        c_state.image_ref = Some(c_cfg.image.reference.clone());
    } else {
        pending = Some(PullJob::Image {
            reference,
            arch: arch.to_string(),
        });
    }

    // Stable MACs: explicit > persisted > generated — the unified
    // name namespace keeps the hash inputs collision-free.
    let mut macs = Vec::new();
    for (i, nic) in c_cfg.nics.iter().enumerate() {
        let mac = nic
            .mac
            .or_else(|| c_state.macs.get(i).copied())
            .unwrap_or_else(|| generate_mac(cx.lab, &c_cfg.name, i));
        macs.push(mac);
    }
    c_state.macs = macs.clone();

    let nic_mtus = cx.nic_mtus(&c_cfg.nics);
    let dirs = ContainerDirs::new(cx.root, cx.lab, &c_cfg.name);
    let volumes = resolve_volume_hosts(c_cfg, cx.root);
    // Same resolver as the VMs: declaration > profile (a container has no
    // template layer). §5.1 validation has already rejected a container no
    // layer sizes.
    let resolved = crate::qemu::resolve_container(c_cfg, arch, cx.profiles)?;
    let hypervisor = hypervisor_for(cx, MachineCfg::Container(c_cfg), None)?;
    let container = ContainerInstance::new(
        cx.lab,
        c_cfg.clone(),
        resolved,
        dirs,
        macs,
        nic_mtus,
        image,
        volumes,
        hypervisor,
        super::hypervisor::for_machine(hypervisor, cx.host, false),
    );
    Ok((container, pending))
}

/// The hypervisor `machine` runs on (§22), refusing — in validation's words,
/// from the one list — a machine vmlab-hv cannot run yet. Validation said so
/// first; this is the daemon not taking its word for it, since a host config
/// edited since can move a machine onto vmlab-hv without touching the lab
/// file.
fn hypervisor_for(
    cx: &BuildContext<'_>,
    machine: MachineCfg<'_>,
    hardware: Option<&crate::qemu::ResolvedVm>,
) -> Result<HypervisorKind> {
    let kind = cx.host.hypervisor.effective(machine);
    if kind == HypervisorKind::Vmlab {
        let why = crate::config::hypervisor::vmlab_refusals(
            machine,
            &crate::config::hypervisor::HostFacts::probe(),
            hardware,
        );
        if !why.is_empty() {
            bail!("{}", why.join("\n"));
        }
    }
    Ok(kind)
}

/// Build one machine of either kind from its declaration.
pub(super) async fn build(
    cx: &BuildContext<'_>,
    cfg: MachineCfg<'_>,
    state: &mut LabState,
) -> Result<(Built, Option<PullJob>)> {
    Ok(match cfg {
        MachineCfg::Vm(vm) => {
            let (vm, pull) = build_vm(cx, vm, state).await?;
            (Built::Vm(vm), pull)
        }
        MachineCfg::Container(c) => {
            let (c, pull) = build_container(cx, c, state)?;
            (Built::Container(c), pull)
        }
    })
}

/// One machine just built, as the concrete type a deferred pull binds into.
pub(super) enum Built {
    Vm(Arc<VmInstance>),
    Container(Arc<ContainerInstance>),
    /// A test double, which no pull ever binds into.
    #[cfg(test)]
    Double(Arc<dyn Machine>),
}

impl Built {
    /// The machine, whichever kind it is.
    pub fn machine(&self) -> Arc<dyn Machine> {
        match self {
            Built::Vm(v) => v.clone(),
            Built::Container(c) => c.clone(),
            #[cfg(test)]
            Built::Double(m) => m.clone(),
        }
    }

    /// Its NICs' hardware addresses.
    pub fn macs(&self) -> Vec<crate::config::model::MacAddr> {
        self.machine().macs().to_vec()
    }
}

/// Every machine a lab runtime holds, under one name namespace, and the
/// declaration each was built from.
///
/// The concrete maps stay beside the one of `dyn Machine` for the handful of
/// places that legitimately want a concrete type (binding a deferred pull's
/// template or image). A reload replaces an entry whole; whoever still holds
/// the old `Arc` holds a machine that is stopped and no longer in the lab.
#[derive(Default)]
pub(super) struct MachineSet {
    vms: BTreeMap<String, Arc<VmInstance>>,
    containers: BTreeMap<String, Arc<ContainerInstance>>,
    all: BTreeMap<String, Arc<dyn Machine>>,
    /// The lab file each machine was built from — the daemon's current one,
    /// or an older one for a machine a reload left pending (PRD §7.1).
    built: BTreeMap<String, Arc<LabFile>>,
}

impl MachineSet {
    /// Hold `machine`, built from `from`, replacing whatever held its name.
    pub fn insert(&mut self, machine: Built, from: Arc<LabFile>) {
        let m = machine.machine();
        let name = m.name().to_string();
        self.remove(&name);
        match machine {
            Built::Vm(v) => {
                self.vms.insert(name.clone(), v);
            }
            Built::Container(c) => {
                self.containers.insert(name.clone(), c);
            }
            #[cfg(test)]
            Built::Double(_) => {}
        }
        self.all.insert(name.clone(), m);
        self.built.insert(name, from);
    }

    pub fn remove(&mut self, name: &str) {
        self.vms.remove(name);
        self.containers.remove(name);
        self.all.remove(name);
        self.built.remove(name);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Machine>> {
        self.all.get(name).cloned()
    }

    /// Every machine, in name order.
    pub fn all(&self) -> Vec<Arc<dyn Machine>> {
        self.all.values().cloned().collect()
    }

    pub fn vm(&self, name: &str) -> Option<Arc<VmInstance>> {
        self.vms.get(name).cloned()
    }

    /// Every VM, by name.
    pub fn vms(&self) -> BTreeMap<String, Arc<VmInstance>> {
        self.vms.clone()
    }

    pub fn container(&self, name: &str) -> Option<Arc<ContainerInstance>> {
        self.containers.get(name).cloned()
    }

    /// Hold `name` as built from `from`: the file changed, but not its block.
    pub fn rebase(&mut self, name: &str, from: Arc<LabFile>) {
        if let Some(built) = self.built.get_mut(name) {
            *built = from;
        }
    }

    /// The lab file `name` was built from.
    pub fn built_from(&self, name: &str) -> Option<Arc<LabFile>> {
        self.built.get(name).cloned()
    }
}
