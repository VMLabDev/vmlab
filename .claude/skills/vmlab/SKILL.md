---
name: vmlab
description: "Complete vmlab reference — the lab file, wscript, the CLI, templates, networking, containers, dev machines. Read before writing or editing a `vmlab.wcl` or a `.ws` script, running any `vmlab` command, or answering a question about how vmlab behaves."
allowed-tools:
  - Bash
  - Read
disable-model-invocation: false
---

# vmlab

<overview>
vmlab is a declarative QEMU/KVM VM-lab orchestrator. A lab — machines plus the
virtual networks between them — is declared in WCL in a `vmlab.wcl` file and
brought up with one command. Disk templates are built locally and distributed
over OCI registries. OCI containers join a lab as micro-VM machines. Guest
automation is written in wscript, which drives power, exec, files, keystrokes,
screen matching and OCR. Any machine can be marked `@dev`, which gives it a
workspace synced both ways with a host directory. A linked git worktree can run
its own copy of a lab, cloned from the main checkout's frozen, provisioned disks.
A supervisor daemon plus one
daemon per lab run behind the CLI, which is the only front end.

`references/` holds the whole product reference. It is self-contained: the
answer is in these files, so read the one that covers the question rather than
recalling vmlab behaviour or reading vmlab's source.
</overview>

## Which file answers it

| Question | File |
|----------|------|
| Installing, and the first lab, VM, template or provision end to end | `references/start-here.md` |
| Daemons, processes, guest channels, on-disk paths, the wire protocol and its error codes | `references/architecture.md` |
| The lab file's syntax and evaluation, and the `lab {}` block | `references/lab-file.md` |
| The `vm {}` block and every child block: attributes, types, defaults | `references/vm.md` |
| GPU acceleration: `gpu {}` modes, host GPUs, Venus, keeping a GPU awake | `references/vm.md`, `references/host-profiles.md` |
| A guest that sleeps: the `suspended` state, `prevent_sleep`, waking with `vm start` | `references/vm.md`, `references/troubleshooting.md` |
| A slow Windows guest under KVM: Hyper-V enlightenments (`hyperv`), on by default for Windows profiles from Vista on; an idle Windows guest's host CPU cost | `references/vm.md`, `references/troubleshooting.md` |
| macOS guests: the `macos` profile, `apple_smc`, `input = "usb"`, `tsc = "host"`, `ignore_msrs`, and what the macOS family changes (shares, logins, playbooks, `app run`) | `references/host-profiles.md`, `references/vm.md`, `references/troubleshooting.md` |
| The guest clock: `clock`, the RTC, what a restore sets | `references/vm.md`, `references/snapshots-vision.md` |
| Containers as lab machines, the micro-VM model, the `container {}` block | `references/containers.md` |
| Segments, DHCP, DNS, NAT, routing, traffic rules, the eBPF fast path | `references/networking.md` |
| Port forwards and their host ports: automatic ports, port slots, `vmlab port` | `references/networking.md`, `references/cli-machine.md` |
| Templates, the store, builds, `source {}`, linked clones, scratch VMs, OCI distribution, and the published templates anyone can pull (`ghcr.io/vmlabdev/vmlab-templates`, Windows included) | `references/templates.md` |
| The wscript language: types, control flow, matching, modules, stdlib | `references/wscript-language.md` |
| wscript `Lab`, `Segment`, `Term` and `Recording` methods, and the shared types (`Match`, `ExecResult`, `Login`, `GuestStats`, `UsbDevice`, `Event`) | `references/wscript-lab-api.md` |
| wscript `Machine` methods — lifecycle, exec, files, input, vision, snapshots, host USB, console recording (`record`) | `references/wscript-machine-api.md` |
| Provisions, `on "event" {}` handlers, the event catalogue, playbooks | `references/automation.md` |
| Ordering machines: `depends_on` (start order) and `depends_on_provision` (provisioning order), parallel provisioning | `references/lab-file.md`, `references/automation.md` |
| `@dev`, the workspace syncer, and `vmlab dev sync` | `references/dev-machines.md` |
| Lab instances (`--instance`, `VMLAB_INSTANCE`, a linked worktree's name) and moving a lab to another directory (`vmlab lab move`) | `references/architecture.md`, `references/cli-lab.md` |
| Worktree labs: freezing the main lab, worktree clones, the freeze point, locked snapshots, thaw, worktree isolation (`vmlab worktree`) | `references/worktrees.md`, `references/cli-lab.md` |
| `login {}`, who runs as whom, minted Windows logons, Linux and macOS sessions, `as_login` | `references/logins.md` |
| The macOS agent: what it serves and refuses, installing it from Recovery (`macos/install.sh`), the first-boot admin | `references/templates.md` (bootstrap ISO) |
| Snapshots, screenshots, console recording, keyboard and mouse input, image matching, OCR | `references/snapshots-vision.md` |
| Shared folders, `vmlab cp`, clipboard transfer (the `media {}` block is in `vm.md`) | `references/shares-media.md` |
| The host config file, WSL 2, guest OS profiles, `vmlab osinfo` | `references/host-profiles.md` |
| Old and odd guests: the legacy agent tier (DOS, Windows 9x, NT4–XP), TempleOS, `cpu_model`, `accel` | `references/host-profiles.md`, `references/templates.md` |
| Host USB passthrough: `usb = true`, `vmlab usb`, selectors | `references/cli-machine.md`, `references/vm.md` |
| Lab-level CLI verbs: `up`, `down`, `status`, `validate`, `destroy`, `pull`, `lab`, `worktree`, `logs`, `eventlog`, `tail`, `dns`, `fastpath`, `playbook`, `script` | `references/cli-lab.md` |
| Per-machine CLI verbs: `vm`, `machine`, `container`, `exec`, `shell`, `app`, `console`, `record`, `cp`, `clipboard`, `snapshot`, `usb`, `port`, `dev` | `references/cli-machine.md` |
| Console recording: `vmlab record`, `--record` on `script`/`up`/`vm start`/`vm restart`, the pointer rule, idle compression, the timeline, what ends a recording | `references/cli-machine.md` (`vmlab record`), `references/snapshots-vision.md` (Recording) |
| Shared apps: a guest program's windows on the host desktop over waypipe, X11 or RDP (`vmlab app run`) | `references/cli-machine.md` (`vmlab app`), `references/troubleshooting.md` |
| `vmlab template` and its subcommands | `references/cli-template.md` |
| A command that failed, or a message the user is asking about | `references/troubleshooting.md` |
| What a vmlab term means | `references/glossary.md` |
| A worked lab to copy from | `references/examples.md` |

<boundaries>
<always>
- Read the reference file covering the surface before writing WCL or wscript, so
  attribute names, defaults and signatures come from the file rather than memory.
- Run `vmlab validate` after editing a `vmlab.wcl`, before `vmlab up`.
- Write a wscript script (`vmlab script x.ws`) for guest automation of more than
  one step, so the run is one program against the machine handle rather than a
  chain of `vmlab exec` calls.
- Name the reference file an answer came from, so the user can check it.
- Confirm the target lab or template before acting when more than one
  `vmlab.wcl` or store version is plausible.
</always>

<never>
- Run `vmlab destroy` or `vmlab template rm` without the user asking for it in
  this session — both delete state (a lab's clones, a store image).
- State that a WCL attribute, wscript method or CLI flag exists without finding
  it in `references/`.
</never>
</boundaries>

<maintenance>
These files are maintained by hand against vmlab's manual. Inside the vmlab
source tree, `docs/manual/` is where a correction lands first, `docs/vmlab-prd.md`
is the binding contract, `src/config/schema.wcl` is every lab-file attribute
that exists, and `src/scripting/mod.rs` every wscript function.
</maintenance>
