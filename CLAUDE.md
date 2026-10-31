# CLAUDE.md

Project context for Claude Code.

## Project Purpose

**vmlab** is a VM lab management tool written in Rust. This is a fresh
rewrite; the product requirements live in `docs/vmlab-prd.md` — read that first;
it is the source of truth for design and scope.

Many earlier attempts are archived under
github.com/wiltaylor/.graveyard-private — notably `vmlab_qemu` (QMP/QGA
driver crate), `vmlab_oci` (OCI registry client for VM disk images), and
`vmlab_floppy` (pure-Rust FAT for floppy images), all buried 2026-06-12.
Consult them for prior art only — the PRD overrides anything they did.

## Status

PRD implemented (M1–M6). **The browser console (`vmlab-web`, `web-ui/`, the
`web {}` block) and the official runtime container image (§14) were removed
before the first release** — the CLI is the only front end, and a native
install beside QEMU the supported path; OCI *template* distribution and
`container {}` machines stay. **§19 (Dev machines) is partly built** — the
workspace syncer's later halves are spec only; implementation is tracked by
#78–#98. The **workspace syncer syncs both ways** (#93, #94): the ledger, the
layered ignores, the size guard, the two watchers' per-path debounce and
temp-then-rename applies on either side, as the machine's default login.
Guest changes arrive by draining the agent's dirty set into the ledger, under
a host-computed **prune list** the guest is handed rather than asked about;
the **stat-walk** runs on a watch discontinuity and nowhere else — first sync,
ledger loss, overflow, a dropped channel — and an overflow warns, forces the
walk and **blocks both directions until it completes**. So are the **Windows
preconditions** (#95): the NTFS case-sensitivity flag on every directory the
syncer creates including the workspace root, the collision refused by name
where the flag cannot be set, the symlink attempted and warned about, the
guest's `core.autocrlf` turned off, and the two degradations an
`elevated = false` login brings, said up front. So is **the conflict halt**
(#96): a conflict stops the whole workspace, both directions, on **one**
machine, naming every path in the batch — scan-then-halt, because a host-side
`git pull` collides in batches — writing no conflict copy, keeping the watch
running, and leaving both copies exactly where they were. A **marker file** at
the guest's workspace root is the only signal that side of ADR-0013's invariant
can get; resolution is host-side necessarily, through `vmlab dev sync
status/flush/diff/resolve`, with `--host`/`--guest`/`--all` and a free third
route — make the two sides identical by hand — that needs no verb. With it the
**guest→host bulk-delete guard** (a proportion with a floor; host→guest deletes
stay unguarded), the **volume warning** that names a subtree and never halts,
and the `.git` **lock deferral** that clears itself. So is **snapshot
bracketing** (#97): vmlab performs a
restore, so it brackets it — capture **pre-flight flushes and refuses with no
escape flag** where the guest holds work the canonical copy has never seen, and
restore takes the syncer off the workspace, rewinds, and puts it back owing a
**re-seed**: a host-only, **digest-based** reconcile that emits no guest→host
action at all, completes **before the watch reopens**, and replaces the
stat-walk rather than following one. The note that a re-seed is owed, and the
halt a restore refuses on, both ride the **ledger**, because a restore does not
need a running machine. Restore's refusal has the one escape flag in §19.6
(`--discard-guest-changes`); capture's has none. Every surface that takes or
restores one says **snapshots are not a workspace backup**. The `@dev`
declaration (#80) and both
agent vocabularies (§19.5) are built: `watch` (#86) and
`fileops` (#84) — the handle-based, offset-addressed, pipelined file RPC
session that **replaced** the whole-file push/pull pair outright, carrying
`vmlab cp`, wscript push/pull and tree pushes. So is
machine-level `login {}` (#81) on both guest families: the **Windows logon it
mints** (#82) — the wire's per-open `logon`,
`LogonUser`/`LoadUserProfileW`/linked-token minting, the (account, secret,
machine) cache and `exec`/`shell`'s `--user`/`--password` — and the **Linux
session** (#83): `su -l` where the guest has PAM, `setuid` where it does not,
plus the container floor. `vmlab machine repair-agent` pushes the host's shipped
agent into a running machine and marks it **diverged**; `up` and `vm start` run
the same push on a stale agent by default (`agent_update = false` opts out),
and it is meaningless on a container, which it says. **The worked example runs end to
end** (§19.8): `examples/dev-container`, a Linux container micro-VM with a
synced workspace and a `login "dev"` whose home is provisioned **before that
user has ever logged on**, through the **wscript rung** of §19.2's ladder —
`m.as_login(label)` / `m.as_account(user, password)`, a second handle onto the
same machine whose every call (exec, `copy_to`, terminal) lands under that
identity — and through `provision {}`, never `playbook {}`, which has no rung
on the ladder at all. **The SSH editor-attach surface was removed before the
first release**: the host-side SSH facade (ADR-0012, superseded), `vmlab ssh`,
`ssh-config` and `ssh-proxy`, the managed `~/.ssh/config` block, `dev attach`
and `dev use`, `attachable`, and the agent's `tunnel` vocabulary. A dev
machine is reached with `vmlab shell` and `vmlab exec`.
**A second hypervisor is selectable** (§22, ADR-0021): `hypervisor = "qemu" |
"vmlab"` on a machine, else the host config's default per kind (QEMU), puts it
on QEMU or on vmlab-hv (github.com/VMLabDev/vmlab-hypervisor, protocol crate
pinned by git rev). Phase 1 runs container micro-VMs; phases 2a to 2f a
modern VM on q35, SeaBIOS or OVMF with secure boot, a TPM and Hyper-V, Linux
or Windows (`vmlab_hv/describe.rs` places every device where QEMU's q35 would;
a guest reset, which ends vmlab-hv, boots the VM again as one run), its screen, input and recording over vmlab-hv's
native frame protocol (`src/hvdisplay.rs`, `vmlab_hv/display.rs`) with RFB on
`vnc.sock` for viewers; a `gpu {}` (virgl and Venus on the render node the
host GPU pick resolves, a host GPU's `qemu_*` keys warned about and ignored;
passthrough as a vfio device vmlab-hv takes only pre-bound), virtio-fs shares
it serves in process (no virtiofsd; the share plan asks each machine what
serves its virtio-fs), `usb = true` (attach/detach as `device_add` and
`device_del`, waiting on `device_deleted`), and **guest audio** — `audio` and
`microphone` on a VM (§11, hv-audio D7), an HDA on the host node
`vmlab.<lab>.<machine>`, refused on QEMU. Everything else — macOS, i440fx,
a pinned TSC (no host-facts probe in vmlab-hv yet), `qemu_args`, hosts without
KVM — is refused from one list naming the phase that brings it. Snapshots
record their hypervisor and an online one refuses to restore under the other;
a vmlab-hv VM with a GPU takes none (HV D9).
**The legacy agent tier is built** (#125–#131): `agent_transport` on the
profile, `windows-xp` and `windows-9x` on `isa-serial`, the C agent under
`guest/agent-legacy` serving `exec` and `set_time` only over COM1, the bootstrap ISO's
`legacy/` builds and install scripts, and the Rust agent's own serial fallback
for old Linux. Verified live on FreeDOS; the NT build is verified under Wine,
and the 9x build live on Windows 95 (950, OSR2, OSR2.5) and 98 (FE, SE).
**The macOS agent is built** (Intel, §7.4): the Rust agent cross-built for
`x86_64-apple-darwin` with cargo-zigbuild (no Apple SDK) and ad-hoc signed,
serving terminal, exec, fileops, tail, metrics, set_time and the console
user's pasteboard over `/dev/cu.vmlab.agent.0` — no watch, tree or apps —
logins through `su -l`/`pam_launchd`, installed offline from the Recovery
Terminal by the bootstrap ISO's `macos/install.sh`, which also arms a
one-shot first-boot `sysadminctl` administrator. Verified live on Tahoe 26.7.2.
**Shared apps are built** (§20, ADR-0016): `vmlab app run` puts one guest
program's windows on the host desktop over waypipe, X11 or RDP RemoteApp,
carried on the agent channel. **USB passthrough is built** (§5.2.1):
`usb = true` boots a VM with a hot-plug xHCI (`usbx`), and `vmlab usb
list/attach/detach` (wscript surface 3) hand it host USB devices at runtime —
runtime-only attachments in the lab daemon, holders gathered by the
supervisor's `usb.list` from every lab, unplugs swept from sysfs, online
snapshots refused while any are attached. **Worktree labs are built** (§21,
ADR-0019, #174–#178): `vmlab worktree freeze <snapshot>` in the main checkout
makes each disk read-only at an offline snapshot under `.vmlab/freeze/<n>/`
and puts the main lab back on its current state as an overlay written by copy
plus safe `qemu-img rebase` (only the post-snapshot delta); `.vmlab/worktree.json`
names the newest generation. A linked worktree's `up` clones each matching
machine from it, carrying MACs, firmware variables, TPM state, volumes and the
freeze-point record, and owing an `@dev` re-seed. Snapshots taken before the
freeze are **locked** until `thaw`; restoring the freeze point resets a lab
and keeps later offline snapshots. A frozen family runs under **worktree
isolation**: NAT refuses host addresses, forwards bind loopback, and a
worktree's global segments join `<segment>@<worktree>`.

Module map under `src/`:

- `config/` — WCL schema, typed model, §5.1 validation, host config, profiles;
  `projection.rs` reflects `schema.wcl` into the Schema projection (ADR-0005);
  `hypervisor.rs` is which hypervisor a machine runs on (§22, ADR-0021) — its
  own `hypervisor`, else the host config's default for its kind — and
  `vmlab_refusals`, the one list of what vmlab-hv cannot run yet, read by
  validation and again by the lab daemon.
- `instance.rs` — lab instances (ADR-0011's amendment, PRD §3.2): the
  effective name `<declared>-<instance>` a directory runs its lab under, from
  `--instance`, `VMLAB_INSTANCE` or the linked git worktree, slugged to a DNS
  label; the CLI applies it on load, and the lab daemon `adopt`s the name it
  is started with.
- `freeze/` — worktree labs (§21, ADR-0019): the freeze layout, the
  `worktree.json` record and how a lab root finds its main checkout or its
  linked worktrees without the git binary (`mod.rs`); freeze, thaw and
  freeze-point restore planned as step lists with undos (`plan.rs`) run by one
  rolling-back executor (`steps.rs`); the facts and dependants they need, and
  `worktree status` (`inspect.rs`).
- `dev/` — dev machines (§19.1): who carries `@dev`, which one is the lab's
  default, and the `@dev` > profile > floor resolution of its arguments —
  deliberately separate from the hardware resolver ADR-0008 owns. `select.rs`
  is the fixed ladder a `vmlab dev` verb picks its machine by (§19.7):
  argument, `VMLAB_DEV_MACHINE`, the default `@dev`, else an error listing
  the candidates.
- `profiles/` — guest OS profiles (WCL data, user-overridable).
- `qemu/` — hardware resolution (VM>template>profile), cmdline builder,
  firmware lookup, process management; `container.rs` builds the micro-VM
  argv for lab containers (§18); `hostgpu.rs` picks a VM's host GPU and its
  emulator environment, and `vk_layer.rs` embeds the Vulkan PCI filter
  (`vk_pci_filter.c`, compiled by `build.rs`) that leaves a Venus guest only
  that GPU (§5.2); `keepawake.rs` embeds `vmlab-gpu-keepawake`
  (`gpu_keepawake.c`, also compiled by `build.rs`), the helper the
  supervisor's `supervisor/keepawake.rs` runs to hold a host GPU out of
  runtime suspend between labs (ADR-0018).
- `qmp/` — the QMP client, used only below the Hypervisor seam by its QEMU
  adapter.
- `appshare/` — shared apps (§20): the CLI-side host end, because only the
  user's desktop session reaches the compositor. `select_mode` picks the
  protocol from the guest family and the host session; `waypipe.rs` and
  `rdp.rs` drive the host clients, `x11.rs` the cookie-checking X11 proxy.
  `cli/app.rs` is the `vmlab app` verb over it.
- `template/` — store, qemu-img, builds, artefact cache, registry catalog
  search; `cli.rs` is the `vmlab template` surface, a pure protocol client
  since the supervisor took ownership of the store (ADR-0010).
- `oci/image/` — standard container-image pull: docker/OCI manifests,
  layer flatten (whiteouts → squashfs via sqfstar), digest-addressed cache.
- `guest_asset.rs` + `guest/` — the container micro-VM kernel/initramfs:
  `vmlab-cinit` (guest PID 1), `cinit-proto` (host↔init contract, shared
  crate), `build-asset.sh` (pinned Alpine, rootless build).
- `guest/fetch-firmware.sh` — the UEFI firmware vmlab ships (§5.2): Debian's
  OVMF/AAVMF, plain and secure-boot with keys enrolled, pinned by version and
  sha256 into `guest/dist/firmware/<arch>/`. `qemu/firmware.rs` prefers the
  host config's `firmware_dir`, then these, then the host distro's.
- `guest/agent-legacy` + `guest/build-agent-legacy.sh` — `vmlab-agent-legacy`,
  the C89 agent for guests with no virtio-serial (§7.4's legacy tier: NT4
  through XP/2003, Windows 9x/ME, DOS), speaking the same protocol over a
  16550 on COM1 (`agent_transport = "isa-serial"`) and advertising `exec`
  and `set_time` only; one polling loop (`agent.c`) behind three adapters (`plat_win32.c`,
  `plat_dos.c`, `plat_posix.c`). Conformance runs the POSIX build through
  the daemon's own client in `src/labd/legacy_agent_tests.rs`. Staged on
  the bootstrap ISO under `legacy/` with an install script each.
- `guest/agent-templeos` — `VmlabAgt.HC`, the same agent for TempleOS in
  HolyC (the guest compiles it). A command is HolyC source run through
  `ExePrint`; it reaches the guest by being typed at its shell
  (`vmlab::templeos_agent_script`), TempleOS reading no ISO 9660. Output
  is captured by a hook in the kernel's StdOut key-device chain that claims
  what the agent's own task prints while a command runs.
- `agent_asset.rs` + `guest/agent`, `guest/agent-proto` — `vmlab-agent`, the
  in-guest agent on the `vmlab.agent.0` virtio-serial port: interactive
  terminals (PTY/ConPTY), streaming exec, tail, metrics, clipboard, the
  handle-based file RPC session every transfer runs over (`fileops.rs`,
  §19.5), the recursive tree `watch` backing the workspace syncer
  (`watch/`, §19.5; Linux and Windows only) and the shared-app sessions
  (`app.rs`, §20.3: waypipe and X11 connections parked and announced on
  Linux, RDP dialled to the guest's own 3389, which `windows/rdp.rs`
  prepares) — none of it touches the guest network. `spawn.rs` is
  the one seam every guest process is created through, and that a file
  session borrows
  its identity from (ADR-0015); each platform half mints §19.2's declared
  logins behind it:
  `windows/logon.rs` a token and a loaded profile, `unix/login.rs` a real
  login (PAM via `su -l`, else `setuid`) and the container floor. `unix/` is
  the one half Linux and macOS share (PTY, exec, logins, port), with what
  differs in `unix/linux.rs` (`/proc`, `/sys`, systemd, the features only
  Linux serves), `unix/macos.rs` (Mach host, `getmntinfo`, `settimeofday`,
  the directory service, `pthread_setugid_np`, the console pasteboard) and
  the Linux-only `unix/container.rs` and `unix/shepherd.rs`; `darwin.rs` is
  the macOS policy as values, compiled on every test host. Baked into
  templates by
  `template/agent_install.rs`; spawned by cinit inside container micro-VMs;
  `labd/vm_agent.rs` is the host-side client; `build-agent.sh` builds the
  per-target binaries (musl, windows-gnu, and apple-darwin through
  cargo-zigbuild and rcodesign).
- `media/` — folder → ISO/floppy with content-addressed cache.
- `vision/` — screenshot, template matching, OCR.
- `hvdisplay.rs` — vmlab-hv's native frame protocol as vmlab consumes it
  (§22, phase 2b): one-shot captures, the input connection and the
  recorder's `NativeStream`, all on a VM's `display.sock`; `hvdisplay/fake.rs`
  is vmlab-hv's own server core behind a test-painted frame source.
- `record/` — console recording (§11.1): the pure core (`core.rs`: framebuffer
  rectangles, cursor shapes and the inputs vmlab sent in; letterboxed frames
  on a constant clock, the pointer at the last position vmlab sent, click
  ripples, idle cuts and the timeline out), the encoder plan (`encoder.rs`:
  ffmpeg discovery against the host config `ffmpeg` key and the argv for
  `.mp4`/`.webm` as values), and the executor (`executor.rs`: the RFB read
  loop over the VM's VNC socket that reconnects across a `vm restart` or a
  restore, the ffmpeg child and the clock); `labd/record.rs` the lab daemon's
  registry and verbs (one recording per machine, armed in the paused window
  of a boot, finalised by whoever ends the machine or the run); `cli/record.rs`
  the `vmlab record` verb and the `--record` flags on `script`, `up`,
  `vm start` and `vm restart`.
- `usb/` — host USB passthrough (§5.2.1), pure over its inputs: the sysfs
  scan (root injectable), selectors and their resolution, and `plan_attach`,
  every attach refusal plus the `device_add` arguments as a value (ADR-0003).
- `net/` — userspace fabric: frame codecs, L2 switch, DHCP, DNS, gateway,
  NAT engine, L3 rules; `nat/guard.rs` is worktree isolation's host-address
  filter (§21.4).
- `proto/` — JSON-lines daemon wire protocol (client + server). `vocab.rs`
  holds the typed request vocabulary every surface constructs through
  (ADR-0007) and `error.rs` the error codes replies carry; `report.rs`
  generates `docs/protocol.md` from them (`just proto-generate`).
- `supervisor/` — `vmlabd`: lab registry, global segments, watchdogs;
  `templates.rs` runs lab-scoped builds/pushes as background operations
  (`vmlab template build` rides it) and `store.rs`
  the store- and registry-scoped operations behind every `vmlab template` verb
  (ADR-0010); `backing.rs` walks every lab disk's whole backing chain, frozen
  generations and unregistered worktrees included, for the store's in-use
  check (§21.5).
- `labd/` — per-lab daemon: lifecycle, snapshots, network assembly, events,
  SMB integration, the lab runtime the wscript host binds to;
  `hypervisor/` is the Hypervisor seam (ADR-0001, ADR-0020): it receives a
  typed `Launch` (resolved machine plus runtime paths) and hands back a
  `Process` and a `Control`, whose capabilities — snapshots, input, frames,
  hot-plug, media — are reported or `None`; `qemu.rs` composes the argv with
  the pure builders in `qemu/` and translates each capability to QMP;
  `vmlab_hv/` is the second production adapter (§22, ADR-0021) — `describe.rs`
  composes vmlab-hv's `MachineDescription` from a container or VM launch,
  `control.rs` is its control protocol as a `Control` (two connections, one
  for commands and one read for events; hot-plug among its capabilities) — chosen per machine by
  `for_machine`; and `fake.rs` is the in-memory adapter the lifecycle tests
  drive, which can also put the vmlab-hv control client in front of a fake
  protocol server;
  `provisioning.rs` is §7.2's per-machine provisioning: each machine's steps
  run in parallel once it is ready, gated by `depends_on_provision`, with
  machine-prefixed output and the first failure cancelling the rest (wscript
  provisions stop at their next host call through `scripting/cancel.rs`);
  `worktree.rs` runs freeze, thaw and freeze-point restore under the state
  lock (§21.1, §21.3), `freeze_clone.rs` plans and makes a worktree machine's
  clone from the freeze (§21.2), and `isolation.rs` decides worktree isolation
  once at daemon start (§21.4);
  `container.rs`/`container_ctl.rs` run OCI containers as micro-VMs (§18);
  `app_share.rs` is `machine.app_open` (§20.3), the broker that opens a
  shared app's agent session and hands the CLI its control and connection
  sockets;
  `boot_clock.rs` is the guest-clock check every fresh agent handshake runs
  under `clock = "host"` (§7.3, #186): `get_time`, and `set_time` past 2 s;
  `boot_id.rs` is the per-family boot identity read over the agent's `exec`
  and the loop `wait_rebooted` waits on it with (§10.3);
  `usb.rs` is USB passthrough's lab-wide half (§5.2.1) — selector resolution
  against the host, who else holds a device (via the supervisor), the
  `usb.*` events and the sysfs watch that drops unplugged devices; each
  machine attaches and tracks its own (`Machine::usb_*`);
  `agent_repair.rs` is `machine.repair_agent` (§19.4), the plan for replacing
  a guest's agent binary over its own channel and the divergence it records;
  `workspace/` is the workspace syncer (§19.6, ADR-0014) — the layered
  ignore rules and the prune list they yield (`ignore.rs`), the host-side sync
  ledger (`ledger.rs`), the one bidirectional reconciliation as a value
  (`plan.rs`), the three walks (`scan.rs` — the host descent, the guest probe
  and the stat-walk) over the one guest-side seam (`guest.rs`),
  temp-then-rename applies on both sides (`apply.rs`), the host watcher and
  the per-path debounce both directions use (`watcher.rs`), §19.6's three
  Windows actions as a value computed before the loop (`windows.rs`), the
  halt and the guest marker file it writes (`halt.rs`), `.git`'s mutable set
  and the deferral a held lock buys it (`locks.rs`), the two copies
  `dev sync diff` brings together (`diff.rs`), what a **snapshot restore** does
  instead of a reconciliation (`reseed.rs` — the host-only, digest-based
  reconcile that carries a rewound guest back to the canonical copy and cannot
  emit a guest→host action), what capture and restore **refuse** and the words
  they refuse in (`bracket.rs`), and the lab-daemon-owned loop
  (`syncer.rs`), which drains the guest's dirty set, holds the rescan and
  re-seed barriers and the halt, serves the `dev sync` verbs, and is the one
  piece of vmlab's machinery running as the machine's default login.
  Decisions the runtime used to make mid-flight are values computed before
  execution (ADR-0003): `plan.rs` (wave ordering), `share_plan.rs` (share
  transports, gateway rules, the smbd port), `forward_plan.rs` (every port
  forward), `pull_ledger.rs` (deferred download lifecycle).
- `scripting/` — wscript host module (lab/VM/segment API), provisions, handlers;
  `as_login`/`as_account` are §19.2's wscript rung, a second `MachineHandle`
  carrying a resolved logon so a provision can write into a login's home
  before that account has ever logged on (§19.8).
- `smb/` — bundled-smbd shared folders; `steps.rs` owns the guest-side mount
  plan (virtiofs + SMB, per guest OS) the lab runtime only executes.
- `oci/` — OCI registry push/pull (chunked, multi-arch).
- `cli/` — the `vmlab` verb surface; `dev.rs` is `vmlab dev` (§19.7), which
  holds only what is meaningless for a machine that is not `@dev` — the
  `sync` verbs a halt is resolved through (§19.6), which live here because
  ADR-0013 leaves no guest→host control path to offer them from. `usb.rs` is
  `vmlab usb` (§5.2.1): `list` from the supervisor (or sysfs alone when none
  runs), `attach`/`detach` to the VM's lab daemon. `worktree.rs` is `vmlab
  worktree freeze/thaw/status` (§21); `status` reads files only and needs no
  daemon.

The user manual is a wdoc book at `docs/manual/` (`just manual-build`,
`just manual-serve`; audited with `/technical-book audit`, whose surface
extractor is `scripts/manual-surface.py`). The website entry is `docs/main.wcl`
(`just docs-build` renders the landing page and the manual under `/manual/`).

`docs/vmlab-prd.md` remains the binding contract; section refs (`§N`) appear
throughout the code and commit messages.

## Conventions

- Changes land on a branch and merge to `main` via a pull request.
- **just** as command runner: `just build` / `just test`, and `just ci::check`
  for the merge bar — the `ci` module (`.just/ci/mod.just`) is the single
  definition of what a change must pass, and `ci.yml` calls its recipes rather
  than inlining equivalents. Justfile follows the norms in the justfile skill
  (groups, doc comments, `[private]`, noun-verb naming, the norm-14 gate).
- Standard Rust toolchain: `cargo build`, `cargo test`, `cargo clippy`,
  `cargo fmt`.
- `just e2e::run` is the end-to-end suite: every feature in
  `e2e/features.py`, driven through the CLI inside a privileged container
  with /dev/kvm (`e2e/README.md`). It sits outside the merge bar.
  `--hypervisor vmlab` runs it on vmlab-hv, skipping each feature past the
  phase vmlab-hv has delivered (its tag in `features.py`).
- `just buildbox::dist` builds every release artefact (binary, strict guest
  bundle, BPF objects) into `target/dist/` inside the `vmlab-build` container
  (`build/Dockerfile`, toolchains from `scripts/guest-toolchains.sh`), as the
  host uid; `buildbox::shell` runs `just ci::check` there (`build/README.md`).

## Agent skills

### Issue tracker

Issues live in GitHub Issues on `VMLabDev/vmlab`, driven via the `gh` CLI.
See `docs/agents/issue-tracker.md`.

### Triage labels

The five canonical triage roles, each label string equal to its name
(`needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`,
`wontfix`). See `docs/agents/triage-labels.md`.

### Domain docs

Single-context: `CONTEXT.md` + `docs/adr/` at the repo root.
See `docs/agents/domain.md`.
