//! **Lab status** — the typed projection every surface renders (ADR-0004).
//!
//! The lab daemon produces one [`LabStatus`] per `status` call; the CLI table,
//! the REST endpoint and the web console all consume *this* value rather than
//! reading keys out of a hand-built JSON object. Two rules keep it honest:
//!
//! - **Kind-specific fields are modelled, not mapped.** A VM's template and a
//!   container's health live in [`MachineDetail`]'s variants, so a field
//!   belonging to one kind cannot be read from the other and a rename is a
//!   compile error at every consumer — the failure that once rendered the CLI's
//!   status table as nothing and left the web layer's reload guard disarmed.
//! - **Surfaces render, they do not derive.** The step from raw power state,
//!   readiness and health to the words a user sees happens once, in
//!   [`MachineLabel::derive`], so `vmlab status` and the console cannot describe
//!   the same lab differently.
//!
//! The console's TypeScript types are generated from these declarations by
//! `just status-types-build` (ts-rs), which writes `web-ui/src/gen/status.ts`
//! — hence the `../` in every `export_to`: ts-rs resolves it against its default
//! `bindings/` directory under the crate root. `just ci::check` fails if the
//! committed file no longer matches the types here.
//!
//! The `#[ts(type = "number")]` on the 64-bit counters is the one place the
//! generated types are told something: ts-rs defaults `u64` to `bigint`, but
//! `serde_json` writes a plain JSON number and `JSON.parse` reads one back, so
//! `bigint` would describe a value the console never receives.

use std::fmt;

use serde::{Deserialize, Serialize};
use ts_rs::TS;

// ---- vocabulary -------------------------------------------------------------

/// A machine's raw power state, as the daemon tracks it.
///
/// Reported in the projection for detail views and `vmlab status --verbose`;
/// what a surface shows by default is the derived [`MachineLabel`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
#[serde(rename_all = "snake_case")]
pub enum PowerState {
    Stopped,
    Starting,
    Running,
    Stopping,
}

impl fmt::Display for PowerState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
        })
    }
}

/// Which of the two kinds a machine is.
///
/// Reported, so a caller can *say* what it is holding — an icon in a UI, the
/// word in an error, the filter behind `lab.vms()`. Never so a caller can pick
/// a code path for driving it: that difference belongs on
/// [`Machine`](crate::labd::machine::Machine), as a capability or as
/// implementation behind it (ADR-0002).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
#[serde(rename_all = "snake_case")]
pub enum MachineKind {
    Vm,
    Container,
}

impl fmt::Display for MachineKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Vm => "vm",
            Self::Container => "container",
        })
    }
}

/// How much attention a [`MachineLabel`] deserves, derived with it so a badge
/// colour is not a second opinion about what the state means.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Up and doing its job.
    Success,
    /// On its way somewhere: worth watching, not worth acting on.
    Warning,
    /// Wrong: a failing healthcheck, a non-zero exit.
    Danger,
    /// Deliberately not running.
    Neutral,
}

/// What a machine is doing, in the words every surface uses for it.
///
/// The vocabulary is deliberately richer than [`PowerState`]: "running" and
/// "booting" are the same power state, and the difference is the one users ask
/// about. Carries the exit code where it has one, so the console can style
/// `exited (1)` without re-deriving the sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LabelState {
    /// Running, ready, and not failing a healthcheck.
    Running,
    /// A VM whose guest is up but whose agent has not answered yet.
    Booting,
    /// Mid-start, or a container whose entrypoint has not signalled ready —
    /// which is genuinely not "booting".
    Starting,
    /// Running, but its healthcheck is failing.
    Unhealthy,
    /// Mid-shutdown. Not in the vocabulary table on issue #7, which enumerates
    /// the six labels the CLI and console disagreed about; a machine on the
    /// stop ladder still has a live QEMU process, and calling that "stopped"
    /// would say it is down while it is not.
    Stopping,
    /// Stopped, having exited non-zero. Containers only: a VM reports no exit
    /// status, so a crashed one reads `stopped` — giving VMs one would be a new
    /// status field, which issue #7 puts out of scope.
    Exited { code: i32 },
    /// Stopped cleanly.
    Stopped,
}

impl LabelState {
    fn severity(&self) -> Severity {
        match self {
            Self::Running => Severity::Success,
            Self::Booting | Self::Starting | Self::Stopping => Severity::Warning,
            Self::Unhealthy | Self::Exited { .. } => Severity::Danger,
            Self::Stopped => Severity::Neutral,
        }
    }
}

impl fmt::Display for LabelState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Running => f.write_str("running"),
            Self::Booting => f.write_str("booting"),
            Self::Starting => f.write_str("starting"),
            Self::Unhealthy => f.write_str("unhealthy"),
            Self::Stopping => f.write_str("stopping"),
            Self::Exited { code } => write!(f, "exited ({code})"),
            Self::Stopped => f.write_str("stopped"),
        }
    }
}

/// A machine's derived status: the state, the sentence for it, and how alarming
/// it is.
///
/// `text` travels on the wire already rendered rather than being rebuilt per
/// surface — that is what stops `vmlab status` and the console from wording the
/// same machine differently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
pub struct MachineLabel {
    #[serde(flatten)]
    pub state: LabelState,
    pub text: String,
    pub severity: Severity,
}

impl MachineLabel {
    /// **The** derivation (ADR-0004): raw power state plus readiness plus
    /// whatever the kind knows about health and exit codes, in one place.
    ///
    /// Order matters where the inputs overlap. A machine that is running but
    /// not ready is reported as on its way up even if a healthcheck has already
    /// failed once — a probe against an entrypoint that has not finished
    /// starting says nothing yet.
    pub fn derive(state: PowerState, ready: bool, detail: &MachineDetail) -> Self {
        let state = match state {
            PowerState::Starting => LabelState::Starting,
            PowerState::Running if !ready => match detail {
                MachineDetail::Vm(_) => LabelState::Booting,
                MachineDetail::Container(_) => LabelState::Starting,
            },
            PowerState::Running => match detail.health() {
                Some(false) => LabelState::Unhealthy,
                _ => LabelState::Running,
            },
            PowerState::Stopping => LabelState::Stopping,
            PowerState::Stopped => match detail.exit_code() {
                Some(code) if code != 0 => LabelState::Exited { code },
                _ => LabelState::Stopped,
            },
        };
        Self::from(state)
    }
}

impl From<LabelState> for MachineLabel {
    fn from(state: LabelState) -> Self {
        Self {
            text: state.to_string(),
            severity: state.severity(),
            state,
        }
    }
}

impl fmt::Display for MachineLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

// ---- machines ---------------------------------------------------------------

/// One NIC's addressing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
pub struct NicStatus {
    /// `None` on a NAT-only NIC that joins no segment.
    pub segment: Option<String>,
    pub mac: Option<String>,
    pub static_ip: Option<String>,
    /// Live address reported by the agent; `None` until the guest is ready.
    pub ip: Option<String>,
}

/// A guest web page — no credentials; the browser only needs enough to build a
/// launch link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
pub struct WebPageStatus {
    pub name: String,
    pub port: u16,
    pub path: String,
}

/// What a VM has and a container does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
pub struct VmStatus {
    pub template: String,
    pub arch: Option<String>,
    pub cpus: Option<u32>,
    /// Bytes.
    #[ts(type = "number | null")]
    pub memory: Option<u64>,
    /// The vmlab-agent stamp baked into the template; `None` on vintage guests
    /// and pre-agent templates, which have no interactive terminal.
    pub agent_version: Option<String>,
}

/// What a container has and a VM does not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
pub struct ContainerStatus {
    pub image: String,
    /// The digest actually running, pinned at first start.
    pub digest: Option<String>,
    /// Latest healthcheck verdict; `None` = no check declared, or no report yet.
    pub health: Option<bool>,
    /// The last exit status, once it has one.
    pub exit_code: Option<i32>,
}

/// The kind-specific half of a machine's status: a tagged union over machine
/// kind, and the reason there is no overflow map here.
///
/// The tag is `kind`, so a surface narrows on the same field it would use to
/// pick an icon. Capabilities — a display, a clipboard, an event log — are
/// probed and reported through `machine.capabilities`; they never add a variant
/// (**CONTEXT.md**).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum MachineDetail {
    Vm(VmStatus),
    Container(ContainerStatus),
}

impl MachineDetail {
    pub fn kind(&self) -> MachineKind {
        match self {
            Self::Vm(_) => MachineKind::Vm,
            Self::Container(_) => MachineKind::Container,
        }
    }

    /// The registry artefact this machine runs: a VM's template, a container's
    /// image. The one thing both kinds have that lives in different fields, so
    /// a surface that wants to print it does not match on the kind to find it.
    pub fn artefact(&self) -> &str {
        match self {
            Self::Vm(vm) => &vm.template,
            Self::Container(c) => &c.image,
        }
    }

    /// The healthcheck verdict, for kinds that have one.
    fn health(&self) -> Option<bool> {
        match self {
            Self::Vm(_) => None,
            Self::Container(c) => c.health,
        }
    }

    /// The last exit status, for kinds that report one.
    fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Vm(_) => None,
            Self::Container(c) => c.exit_code,
        }
    }
}

/// A machine's `@dev` declaration, resolved (PRD §19.1).
///
/// Widening the machine projection rather than standing up a second status
/// verb is the whole point of ADR-0004: `vmlab status`, the REST endpoint and
/// the console all learn which machine is the dev machine from the value they
/// already read, with no second code path to keep in step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
pub struct DevStatus {
    /// This is the lab's **default** dev machine — declared `default = true`,
    /// or the only machine carrying `@dev` (§19.1). At most one machine in a
    /// lab reports `true`; `vmlab validate` rejects a second.
    pub default: bool,
    /// Host directory the workspace syncs from, as declared — relative to the
    /// lab root (§19.6). `None` = this dev machine declares no workspace.
    pub workspace: Option<String>,
    /// Guest path the workspace lands at, resolved `@dev` > profile > floor.
    /// Always answered: a dev machine has a workspace path even where nothing
    /// declared one.
    pub workspace_guest: String,
}

/// One machine's line in `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
pub struct MachineStatus {
    pub name: String,
    /// The derived label — what a surface shows.
    pub label: MachineLabel,
    /// The raw power state behind `label`, for detail views and `--verbose`.
    pub state: PowerState,
    pub ready: bool,
    /// First live address, across all NICs.
    pub ip: Option<String>,
    pub nics: Vec<NicStatus>,
    pub web: Vec<WebPageStatus>,
    /// Whether this machine's template or image is already local. False while a
    /// registry download is still pending — lab-level knowledge, filled in by
    /// the lab runtime rather than by the machine itself.
    pub cached: bool,
    /// The `@dev` declaration this machine carries, resolved (§19.1). `None`
    /// on an ordinary machine, which is most of them — zero dev machines is
    /// normal. Lab-level, like `cached`: whether this is *the* dev machine
    /// depends on what the other machines declare.
    pub dev: Option<DevStatus>,
    /// This machine's agent can serve an attach right now (§19.4):
    /// [`crate::attach::attachable`] over the features it advertised at
    /// handshake, and so `false` for a machine that is down or whose agent
    /// has not answered. The projection carries it so the console and the
    /// `dev` verbs do not each re-derive it, and `vmlab dev attach` has one
    /// thing to wait for.
    pub attachable: bool,
    /// A vmlab verb deliberately changed this machine's guest content in
    /// place, so the template's sealed metadata no longer describes what is
    /// running (§19.4). Today only `vmlab machine repair-agent` sets it, and
    /// nothing sets it by itself.
    ///
    /// Lab-level, like `cached`: divergence is recorded in the lab's own
    /// state beside the machine's MACs and snapshot records, and is forgotten
    /// with the artefacts a `destroy` removes.
    pub agent_diverged: bool,
    #[serde(flatten)]
    pub detail: MachineDetail,
}

impl MachineStatus {
    pub fn kind(&self) -> MachineKind {
        self.detail.kind()
    }

    /// The VM-only fields, if this is a VM.
    pub fn vm(&self) -> Option<&VmStatus> {
        match &self.detail {
            MachineDetail::Vm(vm) => Some(vm),
            MachineDetail::Container(_) => None,
        }
    }

    /// The container-only fields, if this is a container.
    pub fn container(&self) -> Option<&ContainerStatus> {
        match &self.detail {
            MachineDetail::Container(c) => Some(c),
            MachineDetail::Vm(_) => None,
        }
    }
}

impl LabStatus {
    /// Every dev machine in the lab, in the order `machines` reports them.
    pub fn dev_machines(&self) -> impl Iterator<Item = (&MachineStatus, &DevStatus)> {
        self.machines
            .iter()
            .filter_map(|m| m.dev.as_ref().map(|dev| (m, dev)))
    }

    /// The lab's default dev machine (§19.1), or `None` — no dev machine, or
    /// several with none declared the default.
    pub fn default_dev(&self) -> Option<&MachineStatus> {
        self.dev_machines()
            .find(|(_, dev)| dev.default)
            .map(|m| m.0)
    }
}

// ---- network ----------------------------------------------------------------

/// A segment's switch counters.
///
/// `dropped` is the one worth surfacing: anything other than zero means the
/// fabric is shedding frames under load — the thing that makes guest transfers
/// mysteriously slow, and which the daemon has always measured but only the CLI
/// ever showed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
pub struct SegmentFrames {
    #[ts(type = "number")]
    pub forwarded: u64,
    #[ts(type = "number")]
    pub flooded: u64,
    #[ts(type = "number")]
    pub dropped: u64,
    /// Forwarded in-kernel by the fast path (already counted in `forwarded`);
    /// 0 on a pure-userspace switch.
    #[ts(type = "number")]
    pub offloaded: u64,
}

/// One virtual segment's line in `status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
pub struct SegmentStatus {
    pub name: String,
    pub subnet: String,
    pub gateway: String,
    pub nat: bool,
    pub dhcp: bool,
    pub global: bool,
    /// The cross-host trunk target (`connect { host }`), when declared.
    pub connect: Option<String>,
    /// Live trunk state, keyed by segment name in the supervisor, so the accept
    /// side (which has no local `connect`) lights up too. `None` = not a global
    /// segment, or the supervisor is unreachable.
    pub peer_connected: Option<bool>,
    pub frames: SegmentFrames,
}

// ---- downloads --------------------------------------------------------------

/// Which artefact a download is fetching.
///
/// Also the prefix of the events it emits (`template.pull.start`,
/// `container.pull.progress`), so the two cannot name the same download
/// differently — the console's hand-written copy of this had drifted to
/// `"template" | "image"` and would never have matched a container's pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
#[serde(rename_all = "snake_case")]
pub enum PullKind {
    /// A VM disk template from a registry.
    Template,
    /// A container image.
    Container,
}

impl PullKind {
    /// The name this kind goes by on the wire: the event prefix, and the JSON
    /// value `PullStatus::kind` serialises to.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Container => "container",
        }
    }

    /// What a pull's events call the machine they are about: a template belongs
    /// to a `vm`, an image to a `container`.
    pub fn subject(&self) -> &'static str {
        match self {
            Self::Template => "vm",
            Self::Container => "container",
        }
    }
}

impl fmt::Display for PullKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A registry download running right now.
///
/// Reported in `status` as well as through events, so a surface that connects
/// mid-pull still shows progress rather than a machine that looks stuck.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
pub struct PullStatus {
    pub machine: String,
    pub kind: PullKind,
    pub reference: String,
    #[ts(type = "number")]
    pub bytes_done: u64,
    /// 0 until the registry has told us how big the artefact is.
    #[ts(type = "number")]
    pub bytes_total: u64,
    pub percent: u32,
}

// ---- the lab ----------------------------------------------------------------

/// Everything `status` reports about one lab.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../web-ui/src/gen/status.ts")]
pub struct LabStatus {
    pub lab: String,
    /// VMs and containers in one collection, in declaration order. A surface
    /// that groups by kind filters this on the `kind` tag — the console does,
    /// once, in `web-ui/src/status.ts` — rather than reading a second list the
    /// daemon would have to keep consistent with the first.
    pub machines: Vec<MachineStatus>,
    pub segments: Vec<SegmentStatus>,
    /// The lab has clones, container overlays or named volumes on disk — i.e.
    /// `destroy` has something to remove.
    pub provisioned: bool,
    pub pulls: Vec<PullStatus>,
}

impl LabStatus {
    /// Whether nothing in this lab is running — the question the web layer's
    /// reload guard asks, since the daemon cannot re-adopt a live QEMU process
    /// across a restart.
    ///
    /// Every non-stopped state blocks, not just `Running`: a machine mid-boot
    /// has a process the restart would orphan just the same.
    pub fn all_stopped(&self) -> bool {
        self.machines.iter().all(|m| m.state == PowerState::Stopped)
    }
}

/// Projection values to assert against, for every test in this crate that
/// renders or reads a status — the point of the projection being a value is
/// that a surface can be exercised without a lab (ADR-0004).
///
/// The web binary keeps its own copy: it links the ordinary library, not this
/// test build, so it cannot see anything behind `cfg(test)` here.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;

    pub(crate) fn vm() -> MachineDetail {
        MachineDetail::Vm(VmStatus {
            template: "x86_64/win11".into(),
            arch: Some("x86_64".into()),
            cpus: Some(4),
            memory: Some(8 << 30),
            agent_version: Some("0.1.0".into()),
        })
    }

    pub(crate) fn container(health: Option<bool>, exit_code: Option<i32>) -> MachineDetail {
        MachineDetail::Container(ContainerStatus {
            image: "docker.io/library/nginx:latest".into(),
            digest: Some("sha256:abc".into()),
            health,
            exit_code,
        })
    }

    /// One machine, with the label its inputs derive to. `ip` is `None`; a test
    /// that needs an address sets it with struct-update syntax.
    pub(crate) fn machine(
        name: &str,
        state: PowerState,
        ready: bool,
        detail: MachineDetail,
    ) -> MachineStatus {
        MachineStatus {
            name: name.into(),
            label: MachineLabel::derive(state, ready, &detail),
            state,
            ready,
            ip: None,
            nics: Vec::new(),
            web: Vec::new(),
            cached: true,
            dev: None,
            attachable: false,
            agent_diverged: false,
            detail,
        }
    }

    /// The same machine, with an agent that can serve an attach (§19.4).
    pub(crate) fn attachable(machine: MachineStatus) -> MachineStatus {
        MachineStatus {
            attachable: true,
            ..machine
        }
    }

    /// The same machine, designated a dev machine (§19.1) — `default` says
    /// whether it is *the* one.
    pub(crate) fn dev(machine: MachineStatus, default: bool) -> MachineStatus {
        MachineStatus {
            dev: Some(DevStatus {
                default,
                workspace: Some("./src".into()),
                workspace_guest: "C:\\src".into(),
            }),
            ..machine
        }
    }

    /// A lab of `machines`, with no segments and no downloads running.
    pub(crate) fn lab(machines: Vec<MachineStatus>) -> LabStatus {
        LabStatus {
            lab: "demo".into(),
            machines,
            segments: Vec::new(),
            provisioned: true,
            pulls: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn label(state: PowerState, ready: bool, detail: &MachineDetail) -> String {
        MachineLabel::derive(state, ready, detail).text
    }

    /// A VM that is up but whose agent has not answered is booting, not
    /// running: the power state alone cannot tell those apart, and "running"
    /// for a guest still on its login screen is the report users query.
    #[test]
    fn a_running_but_not_ready_vm_is_booting() {
        assert_eq!(label(PowerState::Running, false, &vm()), "booting");
        assert_eq!(label(PowerState::Running, true, &vm()), "running");
    }

    /// A container that is up but not ready is *starting*, not booting — there
    /// is no guest OS coming up, only an entrypoint that has not signalled.
    #[test]
    fn a_running_but_not_ready_container_is_starting() {
        let c = container(None, None);
        assert_eq!(label(PowerState::Running, false, &c), "starting");
        assert_eq!(label(PowerState::Running, true, &c), "running");
    }

    /// A failing healthcheck is the whole point of declaring one, so it beats
    /// the plain "running" a ready container would otherwise get.
    #[test]
    fn a_container_failing_its_healthcheck_is_unhealthy() {
        let sick = container(Some(false), None);
        assert_eq!(label(PowerState::Running, true, &sick), "unhealthy");
        assert_eq!(
            MachineLabel::derive(PowerState::Running, true, &sick).severity,
            Severity::Danger
        );
        // A check that has not reported yet is not a failing one.
        assert_eq!(
            label(PowerState::Running, true, &container(None, None)),
            "running"
        );
    }

    /// Before the healthcheck can mean anything the entrypoint has to finish
    /// starting, so the not-ready wording wins while both are true.
    #[test]
    fn a_starting_container_is_not_called_unhealthy() {
        assert_eq!(
            label(PowerState::Running, false, &container(Some(false), None)),
            "starting"
        );
    }

    /// A crashed container has to say so and carry the code: "stopped" reads
    /// as deliberate, which is exactly the wrong impression.
    #[test]
    fn a_non_zero_exit_is_reported_with_its_code() {
        assert_eq!(
            label(PowerState::Stopped, false, &container(None, Some(1))),
            "exited (1)"
        );
        // Exit 0 is an ordinary stop, and so is a machine that never ran.
        assert_eq!(
            label(PowerState::Stopped, false, &container(None, Some(0))),
            "stopped"
        );
        assert_eq!(
            label(PowerState::Stopped, false, &container(None, None)),
            "stopped"
        );
        assert_eq!(label(PowerState::Stopped, false, &vm()), "stopped");
    }

    /// Mid-start is `starting` for both kinds — the machine exists but nothing
    /// inside it has been asked to do anything yet.
    #[test]
    fn a_machine_mid_start_is_starting() {
        assert_eq!(label(PowerState::Starting, false, &vm()), "starting");
        assert_eq!(
            label(PowerState::Starting, false, &container(None, None)),
            "starting"
        );
    }

    /// A machine on the stop ladder still has a live process; calling that
    /// "stopped" would tell a user it is down while it is not.
    #[test]
    fn a_machine_mid_stop_says_so() {
        assert_eq!(label(PowerState::Stopping, false, &vm()), "stopping");
        assert_eq!(
            MachineLabel::derive(PowerState::Stopping, false, &vm()).severity,
            Severity::Warning
        );
    }

    /// The rendered text and the badge tone travel with the state, so a surface
    /// never has to rebuild either.
    #[test]
    fn the_label_carries_its_own_rendering() {
        let running = MachineLabel::from(LabelState::Running);
        assert_eq!(running.text, "running");
        assert_eq!(running.severity, Severity::Success);
        let stopped = MachineLabel::from(LabelState::Stopped);
        assert_eq!(stopped.severity, Severity::Neutral);
    }

    /// Kind-specific fields are reachable only through the matching variant —
    /// the property the overflow map could not give us.
    #[test]
    fn a_machines_fields_belong_to_its_kind() {
        let machine = machine("web", PowerState::Running, true, vm());
        assert!(machine.container().is_none());
        assert_eq!(machine.vm().unwrap().cpus, Some(4));
        assert_eq!(machine.kind(), MachineKind::Vm);
    }

    /// Both kinds report the artefact they run, in one call — so a surface
    /// printing that column does not match on the kind to find the field.
    #[test]
    fn both_kinds_report_the_artefact_they_run() {
        assert_eq!(vm().artefact(), "x86_64/win11");
        assert_eq!(
            container(None, None).artefact(),
            "docker.io/library/nginx:latest"
        );
    }

    /// Every non-stopped state is evidence a reload would orphan a process.
    #[test]
    fn a_lab_is_only_all_stopped_when_every_machine_is() {
        assert!(lab(Vec::new()).all_stopped());
        assert!(lab(vec![machine("dc01", PowerState::Stopped, false, vm())]).all_stopped());
        for state in [
            PowerState::Running,
            PowerState::Starting,
            PowerState::Stopping,
        ] {
            let status = lab(vec![
                machine("dc01", PowerState::Stopped, false, vm()),
                machine("web", state, false, container(None, None)),
            ]);
            assert!(!status.all_stopped(), "state {state:?}");
        }
    }

    /// Which machine is the dev machine is answered by the projection every
    /// surface already reads (§19.1), so nothing re-derives it from a lab
    /// file — and a machine carrying no `@dev` simply says nothing.
    #[test]
    fn the_projection_names_the_dev_machine() {
        let status = lab(vec![
            machine("dc01", PowerState::Running, true, vm()),
            dev(machine("dev01", PowerState::Running, true, vm()), true),
            dev(
                machine(
                    "buildbox",
                    PowerState::Stopped,
                    false,
                    container(None, None),
                ),
                false,
            ),
        ]);
        assert_eq!(
            status
                .dev_machines()
                .map(|(m, _)| m.name.as_str())
                .collect::<Vec<_>>(),
            ["dev01", "buildbox"]
        );
        assert_eq!(
            status
                .dev_machines()
                .find(|(_, d)| d.default)
                .map(|(m, _)| m.name.as_str()),
            Some("dev01")
        );
        assert_eq!(
            status.machines[1].dev.as_ref().unwrap().workspace_guest,
            "C:\\src"
        );
        assert!(status.machines[0].dev.is_none());

        // No dev machine at all is the ordinary case, and answers cleanly.
        let plain = lab(vec![machine("dc01", PowerState::Running, true, vm())]);
        assert_eq!(plain.dev_machines().count(), 0);
    }

    /// `attachable` travels on the projection every surface already reads
    /// (§19.4), so the console and the `dev` verbs do not each re-derive it —
    /// and it is a per-machine answer, not a lab-wide one.
    #[test]
    fn the_projection_carries_attachable_per_machine() {
        let status = lab(vec![
            attachable(machine("dev01", PowerState::Running, true, vm())),
            machine("stale", PowerState::Running, true, vm()),
            machine("dc01", PowerState::Stopped, false, vm()),
        ]);
        assert!(status.machines[0].attachable);
        // Running, ready, perfectly good — and still not attachable.
        assert!(!status.machines[1].attachable);
        assert_eq!(status.machines[1].label.text, "running");
        assert!(!status.machines[2].attachable);

        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["machines"][0]["attachable"], true);
        assert_eq!(json["machines"][1]["attachable"], false);
    }

    /// Divergence is reported wherever machine state is (§19.4), and defaults
    /// to the ordinary answer: nothing has changed this machine in place.
    #[test]
    fn the_projection_reports_a_diverged_machine() {
        let mut diverged = machine("dev01", PowerState::Running, true, vm());
        assert!(!diverged.agent_diverged, "nothing diverges by itself");
        diverged.agent_diverged = true;
        let status = lab(vec![diverged]);
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["machines"][0]["agent_diverged"], true);
        let back: LabStatus = serde_json::from_value(json).unwrap();
        assert!(back.machines[0].agent_diverged);
    }

    /// The wire shape: kind-specific fields sit alongside the common ones under
    /// a `kind` tag, and the whole thing survives the round trip a surface makes
    /// when it reads the daemon's reply.
    #[test]
    fn the_projection_round_trips_through_json() {
        let status = lab(vec![
            machine("dc01", PowerState::Running, false, vm()),
            machine("web", PowerState::Stopped, false, container(None, Some(3))),
        ]);
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json["machines"][0]["kind"], "vm");
        assert_eq!(json["machines"][0]["template"], "x86_64/win11");
        assert_eq!(json["machines"][0]["label"]["state"], "booting");
        assert_eq!(json["machines"][1]["label"]["text"], "exited (3)");
        assert_eq!(json["machines"][1]["label"]["code"], 3);
        assert!(json["machines"][1].get("template").is_none());

        let back: LabStatus = serde_json::from_value(json).unwrap();
        assert_eq!(back, status);
    }
}
