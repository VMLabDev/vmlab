# Architecture, paths, and the wire protocol

vmlab is a single-host lab orchestrator. A **lab** — machines and the virtual
networks between them — is declared in one file and booted on QEMU/KVM, driven
directly over QMP with no libvirt in between.

## Three processes

vmlab is a two-tier daemon system with a thin client in front of it. The
`vmlab` command is a client. It never runs a VM itself.

- **The supervisor, `vmlabd`.** One per user, started automatically by the CLI
  the first time it is needed. It keeps the registry of running labs, owns the
  global segments that span labs (see networking.md), serialises every write to
  the template store (see templates.md), runs host-level watchdogs such as
  `host.disk_low`, and aggregates the event stream of every lab on the host.
- **The lab daemon.** One per running lab, spawned by the supervisor on
  `vmlab up` and reaped on `down` or `destroy`. It owns everything lab-scoped:
  the QEMU processes, their QMP and agent channels, the lab's segments with
  their DHCP, DNS, NAT and rules, clones, snapshots, lab state, the wscript
  runtime, and the workspace syncer of any dev machine (see dev-machines.md).
- **The CLI.** It connects to the supervisor for discovery and host-scoped
  verbs, then talks directly to the lab daemon's socket for anything
  lab-scoped. Nothing proxies the hot path.

Topology:

```
vmlab CLI ──discover──> vmlabd (supervisor)
vmlabd ──spawn / reap──> lab daemon (lab A)
vmlabd ──spawn / reap──> lab daemon (lab B)
vmlab CLI ──lab ops──> lab daemon (lab A)
lab daemon (lab A) ──QMP──> QEMU per machine
lab daemon (lab B) ──QMP──> QEMU per machine
```

The split exists for fault and contention isolation. A lab daemon that dies
takes only its lab with it. The supervisor notices, emits `lab.daemon_crashed`,
and marks the lab failed. It does not restart the lab on its own. Other labs,
and the supervisor itself, are unaffected.

### A lab name is a host-wide identity

The supervisor's registry, the lab's runtime directory, its control socket and
its process markers are all keyed by the lab's declared name, not by the
directory it lives in. Two directories that declare the same lab name cannot
run at once on one host. On `up` of the second, the supervisor answers with a
`conflict` error naming the other root and the two remedies: stop that lab, or
rename this one. This rule is what makes the SSH aliases
`vmlab-<lab>-<machine>` unambiguous (see logins-and-ssh.md).

## What a machine is

A lab boots two kinds of machine. A **VM** is booted from a template: a sealed
qcow2 in the store (see templates.md). A **container** is booted from an OCI
image inside a micro-VM (see containers.md). Both attach to the same segments,
register in the same DNS, take the same snapshots and are driven through the
same agent. *Machine* means either.

### Linked clones

A VM's disk is a qcow2 **linked clone**: a copy-on-write overlay whose backing
file is the template's disk in the store. The template is never written to.
Clones live in `.vmlab/` and are disposable. `vmlab down` powers the lab off
and keeps them, `vmlab destroy` deletes them, and the next `up` makes fresh
ones. Because a clone leans on its template, removing a template still backing
a clone is refused unless forced.

### The guest agent

Every machine runs `vmlab-agent`, reached over one multiplexed virtio-serial
port named `vmlab.agent.0`. No guest network is involved. The agent is the
channel for readiness, streaming command execution, interactive terminals, file
operations, log tailing, metrics, clipboard, OS information, per-NIC address
reporting and graceful shutdown. Templates bake it in during the build.
Container micro-VMs get it from vmlab's own init, which spawns it beside the
workload.

A machine is **ready** when its agent answers the handshake. A lab is **up**
when every machine is ready and every provision script has completed. A VM
built without an agent still works for screen-driven automation (see
snapshots-vision.md), but it never reports ready, so scripts targeting it must
wait on the screen or on time.

**The host opens channels; the guest answers.** Every channel to a guest is
opened from the host side and answered by the agent. Nothing in a guest can
open a connection back to vmlab. This one rule is why the SSH facade refuses
`ssh -R`, why a halted workspace can only be resolved from the host, and why a
guest with no network is still fully driveable.

## What happens on `vmlab up`

The CLI locates `vmlab.wcl`, validates it (see lab-file.md), and asks the
supervisor to ensure a lab daemon for that name. The supervisor pre-pulls any
registry templates or container images the lab needs, then spawns the daemon.
The daemon computes its plan as a value before it starts anything: the waves of
machines `depends_on` implies, the share plan (see shares-media.md), and the
forward plan (see networking.md). It assembles the network fabric, creates the
clones, launches one QEMU per machine, waits for each agent, mounts shares,
runs each machine's provision steps in declaration order, and reports the lab
up. Provision output streams live to the terminal and into the lab log.

`down` walks the stop ladder on every machine — agent shutdown, then ACPI, then
a kill after a timeout — and asks the supervisor to release the daemon.
`destroy` does the same and then removes everything in `.vmlab/`.

## Where things live

vmlab follows the XDG base directory convention, and every path honours the
corresponding environment variable.

| Path | What is there |
| --- | --- |
| `<lab>/vmlab.wcl` | The lab definition. The CLI finds it by walking up from the current directory, the way git finds a repository. |
| `<lab>/.vmlab/` | Lab-local working data: linked-clone disks, snapshot data, built ISO and floppy images, TPM state, persisted lab state, the workspace sync ledger and the `dev use` selection. Safe to delete when the lab is down. Gitignore it. |
| `~/.local/share/vmlab/templates/` | The template store, laid out as `<arch>/<name>/<version>/`. |
| `~/.local/share/vmlab/oci/` | The digest-addressed cache of pulled container images. |
| `~/.local/state/vmlab/` | Daemon state, per-lab and per-machine logs as JSON lines, event history, SSH host keys. |
| `~/.config/vmlab/` | The host configuration file and user profile overrides (see host-profiles.md). |
| `$XDG_RUNTIME_DIR/vmlab/` | `vmlabd.sock`, and `labs/<lab>/` holding each lab daemon's `control.sock` and its per-machine QMP, agent, NIC and VNC sockets. |

The runtime directory is created private to the user, because a client that can
reach a control socket can run scripts in the lab and read and write guest
files. Where `XDG_RUNTIME_DIR` is unset, which some WSL setups leave it, vmlab
falls back to a uid-scoped directory under `/tmp` and refuses one owned by
anyone else.

Set `VMLAB_WORK_DIR` to move every lab's `.vmlab/` under one base directory.
Each lab gets a subdirectory named after its root plus a short hash, so two
labs never collide. The lab file stays where it is.

## The four roots

| Root | Default | Override | Holds |
| --- | --- | --- | --- |
| Data | `~/.local/share/vmlab` | `XDG_DATA_HOME` | The template store, the container-image cache, guest assets, build caches. |
| State | `~/.local/state/vmlab` | `XDG_STATE_HOME` | Daemon logs, per-lab logs and event history, the lab registry, SSH host keys. |
| Config | `~/.config/vmlab` | `XDG_CONFIG_HOME` | The host configuration file, registry namespaces, profile overrides. |
| Runtime | `$XDG_RUNTIME_DIR/vmlab` | `XDG_RUNTIME_DIR` | Every control socket. Falls back to `/tmp/vmlab-<uid>` when the variable is unset, as on some WSL setups. |

An XDG variable that is set but empty is treated as unset. `HOME` unset
resolves to `/`. The runtime root is created private to the user, mode 0700,
and vmlab refuses to put sockets into one owned by somebody else, because a
client that can connect to a control socket can run scripts in the lab and read
and write guest files. The data, state and config roots are not tightened,
since they hold no control interface and are legitimately shared in some
deployments.

### Data root

| Path | Holds |
| --- | --- |
| `templates/` | The template store, laid out as `<arch>/<name>/<version>/` with `disk.qcow2` and `template.wcl` in each. See templates.md. |
| `templates/.lock` | The advisory lock every store mutation holds. |
| `templates/.oci-pull/` | Staging for a registry pull in progress. |
| `oci/` | The digest-addressed container-image cache: `blobs/sha256/`, `images/sha256/<manifest>/` with `manifest.json`, `config.json` and `rootfs.sqfs`, and `refs/<host>/<repo>/<tag>` recording the last digest a tag resolved to. See containers.md. |
| `guest/<arch>/` | The container micro-VM kernel and initramfs, and `guest/agent/<os>-<arch>/` the guest agent binaries, when installed here rather than under `/usr/share/vmlab/guest`. |
| `cache/artefacts/` | Content-addressed downloads a template build's `source {}` fetched. |
| `cache/builds/` | Working directories of template builds. |
| `cache/oci-push/` | Working directory of a registry push. |
| `~/.local/share/config-weave/bin` | Where the playbook engine's guest binaries are looked for, unless `VMLAB_CONFIG_WEAVE_DIR` says otherwise. See automation.md. |

### State root

| Path | Holds |
| --- | --- |
| `vmlabd.log` | The supervisor's log. |
| `labd-<lab>.log` | One lab daemon's log. |
| `labs.json` | The supervisor's lab registry. |
| `labs/<lab>/events.jsonl` | The lab's event history, one JSON object per line. |
| `labs/<lab>/lab.log` | Provision and script output. |
| `labs/<lab>/vms/<vm>/` | `serial.log`, `qemu.log` and `swtpm.log` for one VM. |
| `labs/<lab>/containers/<name>/console.log` | The micro-VM kernel log with the container's stdout and stderr. |
| `ssh/known_hosts` | The `known_hosts` the managed SSH block points clients at. |
| `ssh/<lab>/<machine>` | The SSH facade's host key for one machine. See logins-and-ssh.md. |

`events.jsonl` and `lab.log` roll over at 16 MiB, keeping one previous
generation as `<name>.1`. `vmlab logs` reads these files directly; there is no
daemon call for logs.

### Configuration root

| Path | Holds |
| --- | --- |
| `config.wcl` | The host configuration file. See host-profiles.md. |
| `registries.json` | The searchable OCI namespaces `vmlab template registry` manages. See cli-template.md. |
| `profiles/` | User overrides of the shipped guest OS profiles. See host-profiles.md. |
| `~/.docker/config.json` | Registry credentials, read and written Docker-style so an existing login works. `DOCKER_CONFIG` names a different directory. Credential helpers named there are invoked. |
| `~/.ssh/config` | The managed block `vmlab ssh-config` writes between its markers. The host config's `ssh_config` field moves it. |

### Runtime sockets

| Path | Holds |
| --- | --- |
| `vmlabd.sock` | The supervisor's control socket. |
| `labs/<lab>/control.sock` | One lab daemon's control socket. |
| `labs/<lab>/vms/<vm>/` | `qmp.sock`, `agent.sock`, `vnc.sock`, `tpm.sock`, one `nic<i>.sock` per NIC, one `vfs<i>.sock` per virtiofs share, and a `term-<id>.sock` per open terminal. |
| `labs/<lab>/containers/<name>/` | `qmp.sock`, `ctl.sock`, `agent.sock`, `nic<i>.sock`, `vfs<i>.sock` and `term-<id>.sock`. |
| `global/<segment>.sock` | The trunk socket a lab daemon bridges to for a global segment. See networking.md. |
| `ssh/` | The `ControlPath` multiplexer sockets the managed SSH block names, and `config.lock`, the lock its writer takes. |

## The lab directory

A lab is any directory holding a `vmlab.wcl`. Every lab-scoped verb finds it by
walking up from the current directory. Beside it, vmlab keeps `.vmlab/`, which
should be in the lab's `.gitignore`.

| Path under `.vmlab/` | Holds |
| --- | --- |
| `state.json` | Persisted lab state: snapshot records, pinned artefacts, agent repairs. |
| `vms/<vm>/` | `disk0.qcow2`, the linked clone; `OVMF_VARS.fd`; `tpm-state/`; `firstboot.done` once the first-boot provision has run. |
| `containers/<name>/` | `scratch.qcow2`, the writable overlay, and `container.json`. |
| `media/` | ISO and floppy images built from `media {}` folders, content-addressed. See vm.md. |
| `volumes/` | Named container volumes. |
| `smb/` | The bundled smbd's configuration and state. See shares-media.md. |
| `workspace/<machine>.json` | The workspace syncer's ledger for one dev machine. See dev-machines.md. |
| `screenshots/` | Where `Machine.screenshot` writes when given no path. |
| `dev-machine` | The selection `vmlab dev use` records. |

`VMLAB_WORK_DIR` relocates the whole directory: with it set, the lab's working
data lives at `$VMLAB_WORK_DIR/<lab-dir-name>-<hash>/`, where the hash is
twelve hex characters of the lab root's canonical path, so several labs can
share one base without colliding. The lab file itself stays where it is. This
keeps disk clones off a slow filesystem such as a bind mount.

## Environment variables

| Variable | Effect |
| --- | --- |
| `XDG_DATA_HOME`, `XDG_STATE_HOME`, `XDG_CONFIG_HOME`, `XDG_RUNTIME_DIR` | Move the four roots above. |
| `HOME` | The base of every default root. |
| `VMLAB_WORK_DIR` | Relocates every lab's `.vmlab/` under one base, as described above. |
| `VMLAB_GUEST_ASSET_DIR` | Searched first for the micro-VM kernel and initramfs (`<arch>/`) and the agent binaries (`agent/<os>-<arch>/`), before `/usr/share/vmlab/guest` and the data root's `guest/`. |
| `VMLAB_CONFIG_WEAVE_DIR` | Where the playbook engine's guest binaries are, instead of `~/.local/share/config-weave/bin`. |
| `VMLAB_VIRTIOFSD` | The `virtiofsd` binary to use, before searching `PATH`. |
| `VMLAB_FASTPATH` | Overrides the host config's `fastpath`: `auto`, `off`, `sockmap` or `afxdp`. A malformed value is ignored with a warning. See host-profiles.md. |
| `VMLAB_DEV_MACHINE` | Which dev machine `vmlab dev` verbs mean, second on the selection ladder after an explicit argument. See dev-machines.md. |
| `DOCKER_CONFIG` | The directory holding `config.json` with registry credentials. |
| `PATH` | Searched for the emulator, `qemu-img`, `swtpm`, `virtiofsd` and `ssh`. |

## The wire protocol

Every control connection is a unix domain socket carrying JSON lines. The
supervisor listens on `vmlabd.sock` and each lab daemon on its lab's
`control.sock`, both under the runtime directory.

Each line is one object tagged by `type`. A request is `req`, with a
client-chosen `id`, a `cmd` string and an `args` object. The final answer is
`resp` for the same `id`, carrying either `ok` with the result, or `err` with a
prose message and a machine-readable `code`. Commands that produce output as
they run, such as `up` and `machine.logs` with `follow`, send `stream` messages
with a `chunk` before the final `resp`. A connection that has subscribed
receives `event` messages on the same line stream. The supervisor and the lab
daemons speak this protocol to each other as well.

```json
{"type":"req","id":1,"cmd":"machine.ip","args":{"machine":"dc01","nic":null}}
{"type":"resp","id":1,"ok":"10.0.0.10"}
{"type":"req","id":2,"cmd":"snapshot.restore","args":{"name":"nope","machine":"dc01","discard":false}}
{"type":"resp","id":2,"err":"\"dc01\" has no snapshot \"nope\"","code":"not_found"}
```

The message may be reworded between releases; the code is the contract, and it
decides the CLI's exit status. Inside the repository every surface builds
requests through the typed vocabulary in the source rather than spelling the
strings, and the generated reference `docs/protocol.md` (every command, its
arguments, and why it is reachable from where it is) is produced from that
vocabulary, so the two cannot drift.

### Error codes

A failed command answers with one of seven codes. The CLI maps each to its exit
status, so a script can branch on `$?` without parsing output.

| Code | `vmlab` exit code | Meaning |
| --- | --- | --- |
| `unknown_command` | 2 | The daemon does not know the command. Usually a version mismatch between CLI and daemon. |
| `invalid_argument` | 2 | An argument is missing, malformed or out of range. |
| `not_found` | 4 | The lab, machine, snapshot, template or path named does not exist. |
| `conflict` | 5 | The request contradicts current state: a build already running, a halted workspace, a port already claimed. |
| `unsupported` | 6 | This machine or host cannot do what was asked, such as a screen operation on a machine without a display. |
| `failed` | 1 | The operation was attempted and failed: the emulator, the agent, a registry or the guest reported an error. |
| `internal` | 1 | A fault in the daemon itself. |

A command that succeeds exits 0. A CLI-side failure before any request is sent,
such as no `vmlab.wcl` in any parent directory, also exits non-zero with a
message and no code.

### The supervisor socket

The supervisor owns the lab registry, the template store, the registry
catalogue and global segments.

| Group | Commands | Called by |
| --- | --- | --- |
| Liveness | `ping`, `version`, `fastpath`, `status`, `shutdown` | CLI, and daemons for `ping` and `shutdown` |
| Lab daemons | `lab.ensure`, `lab.release`, `lab.restart` | CLI |
| Global segments | `global.attach`, `global.detach`, `global.list` | Lab daemons only |
| Template builds | `template.list`, `template.build`, `template.stop_build` | CLI |
| The store | `store.list`, `store.remove`, `store.prune`, `store.export`, `store.import`, `store.pull`, `store.push`, `store.stop_push` | CLI |
| Registries | `registry.search`, `registry.login`, `registry.namespaces`, `registry.namespace_add`, `registry.namespace_remove` | CLI |

### A lab daemon's socket

A lab daemon owns one lab: its machines, network, snapshots, playbooks and
workspaces. Every lab-daemon command is called by the CLI.

| Group | Commands |
| --- | --- |
| Lab | `ping`, `status`, `dns.table`, `up`, `pull`, `pull.cancel`, `run`, `down`, `destroy`, `shutdown` |
| Machine lifecycle | `machine.start`, `machine.stop`, `machine.restart`, `machine.destroy`, `machine.capabilities`, `machine.ip`, `machine.osinfo`, `machine.stats`, `machine.logs`, `machine.repair_agent` |
| Display and input | `machine.screenshot`, `machine.sendkeys`, `machine.mouse_move`, `machine.mouse_click`, `machine.mouse_drag`, `machine.ocr`, `machine.find_image` |
| Guest agent | `machine.exec`, `machine.tty_open`, `machine.tty_resize`, `machine.push_file`, `machine.pull_file`, `machine.tail`, `machine.eventlog`, `machine.clipboard_get`, `machine.clipboard_set` |
| SSH facade | `machine.ssh_open` |
| Playbooks | `playbook.list`, `playbook.check`, `playbook.apply` |
| Snapshots | `snapshot.take`, `snapshot.restore`, `snapshot.delete`, `snapshot.list` |
| Workspace | `workspace.flush`, `workspace.resolve`, `workspace.diff` |

`machine.tty_open` and `machine.ssh_open` answer with the path of a second unix
socket the caller connects to and pipes bytes over: a raw terminal for the
first, an SSH connection for the second, which is what `vmlab ssh-proxy` hands
to `ssh` as its `ProxyCommand`. See logins-and-ssh.md.

### Events on the wire

A subscriber receives each event as an object with `event` and `data`. The
supervisor forwards every event from every lab daemon it has adopted unchanged,
and adds its own: `lab.daemon_crashed`, `host.disk_low` for the store's
filesystem, `segment.peer.up` and `segment.peer.down`, and the `template.op.*`
family.
