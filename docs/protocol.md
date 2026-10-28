# The vmlab wire protocol

<!-- generated from `src/proto/vocab.rs` — run `just proto-generate` -->

JSON lines over a unix socket: a request is a `cmd` string and an `args` object, and a
reply carries either `ok` or an `err` message with a machine-readable `code`. Callers
inside this repo construct requests through the vocabulary in `src/proto/vocab.rs`
rather than spelling the strings; the strings are what goes on the wire, and are what an
out-of-repo client writes.

## Error codes

A failure says why by code. The message is prose and may be reworded freely;
the code is the contract.

| code | HTTP status | `vmlab` exit code |
|---|---|---|
| `unknown_command` | 400 Bad Request | 2 |
| `invalid_argument` | 400 Bad Request | 2 |
| `not_found` | 404 Not Found | 4 |
| `conflict` | 409 Conflict | 5 |
| `unsupported` | 501 Not Implemented | 6 |
| `failed` | 502 Bad Gateway | 1 |
| `internal` | 500 Internal Server Error | 1 |

## The supervisor socket (`vmlabd`)

| command | arguments | called by | what it does |
|---|---|---|---|
| `ping` | — | `cli`, `daemon` | Liveness check; answers `"pong"`. |
| `version` | — | `cli` | The supervisor's own build version. |
| `fastpath` | — | `cli` | Which network fast-path tier this host selected (PRD §9.1), and why the skipped tiers were unavailable. |
| `status` | — | `cli` | Every lab in the registry. |
| `lab.ensure` | `name: String`, `root: std::path::PathBuf` | `cli` | Spawn (or find) a lab's daemon; answers with its socket path. |
| `lab.release` | `name: String` | `cli` | Stop a lab's daemon, after `down` or `destroy`. |
| `lab.restart` | `name: String`, `root: std::path::PathBuf` | `cli` | Restart a lab's daemon so it re-reads its config; answers with the new socket path. |
| `global.attach` | `name: String`, `subnet: Option<Ipv4Net>`, `peer: Option<String>`, `lab: Option<String>`, `members: Vec<GlobalMember>`, `mtu: Option<u16>` | `daemon` | Join a global segment (PRD §9.2), creating it on first use; answers with the trunk socket to bridge to. |
| `global.detach` | `name: String`, `lab: Option<String>` | `daemon` | Leave a global segment. |
| `global.list` | — | `daemon` | Every global segment this host knows. |
| `usb.list` | — | `cli`, `daemon` | Every USB device plugged into this host (PRD §5.2.1), hubs hidden, each with the `lab/vm` holding it, if one does. |
| `gpu.keepawake` | `gpu: String`, `address: String` | `daemon` | Hold a host GPU out of runtime suspend until the supervisor stops (ADR-0018), starting its keep-awake helper unless one runs; answers with the helper's `pid`. `failed` when the helper cannot open the GPU. |
| `template.list` | `lab: String`, `root: std::path::PathBuf`, `file: Option<std::path::PathBuf>` | `cli` | The templates a file declares, with their store and build state. |
| `template.build` | `lab: String`, `root: std::path::PathBuf`, `template: String`, `arch: Option<String>`, `version: Option<String>`, `file: Option<std::path::PathBuf>`, `variants: Option<Vec<String>>` | `cli` | Start building one declared template. |
| `template.stop_build` | `lab: String`, `arch: String`, `template: String` | `cli` | Abort a running build. |
| `store.list` | `remote: bool` | `cli` | Every template in the store, with its size and, on request, whether that exact version is published. |
| `store.remove` | `reference: String`, `force: bool` | `cli` | Remove one exact store version `<arch>/<name>@<version>`. |
| `store.prune` | `filter: Option<String>`, `keep: usize`, `apply: bool`, `force: bool` | `cli` | Plan a prune of superseded builds, and carry it out when `apply`. |
| `store.export` | `reference: String`, `out: std::path::PathBuf` | `cli` | Write one store version to a portable archive. |
| `store.import` | `archive: std::path::PathBuf`, `overwrite: bool` | `cli` | Read a template back out of an archive. |
| `store.pull` | `target: String`, `arch: Option<String>`, `overwrite: bool`, `all_variants: bool` | `cli` | Download a published template into the store. |
| `store.push` | `reference: String`, `target: Option<String>`, `source: Option<String>`, `prerelease: bool`, `lab: String` | `cli` | Start uploading one store version to an OCI registry. |
| `store.stop_push` | `lab: String`, `arch: String`, `template: String` | `cli` | Abort a running store push. |
| `registry.search` | `query: Option<String>`, `namespace: Option<String>`, `arch: Option<String>`, `containers: bool` | `cli` | Search one OCI namespace, or every configured one, for published templates or container images. |
| `registry.login` | `registry: String`, `username: String`, `password: String` | `cli` | Store credentials for an OCI registry host. |
| `registry.namespaces` | — | `cli` | The searchable OCI namespaces this host is configured with. |
| `registry.namespace_add` | `namespace: String`, `use_for: crate::template::registries::RegistryUse` | `cli` | Add or update a searchable namespace. |
| `registry.namespace_remove` | `namespace: String` | `cli` | Remove a searchable namespace. |
| `shutdown` | — | `cli`, `daemon` | Tear the supervisor down; the reply is sent before it exits. |

## A lab daemon's socket

| command | arguments | called by | what it does |
|---|---|---|---|
| `ping` | — | `cli`, `daemon` | Liveness check; answers `"pong"`. |
| `version` | — | `cli` | This daemon's build (`vmlab::version::BUILD`): what the CLI compares with its own before it drives the daemon, since one build cannot drive another's. |
| `status` | — | `cli` | The whole lab's runtime status: machines, segments, readiness. |
| `dns.table` | — | `cli` | The DNS zones the lab's segments serve. |
| `up` | `machines: Vec<String>`, `record: Vec<RecordRequest>` | `cli` | Bring the lab up, or just the named machines (empty = all). Streams provisioning output. |
| `pull` | `machines: Vec<String>` | `cli` | Download every pending template and image without starting anything, over the code path `up` runs first. |
| `pull.cancel` | `machine: String` | `cli` | Abort one machine's running download; whatever waits on it fails with "download cancelled". |
| `run` | `script: String`, `record: Vec<RecordRequest>` | `cli` | Run an ad-hoc wscript against the lab (PRD §12), streaming output. |
| `down` | `machines: Vec<String>`, `force: bool` | `cli` | Stop the lab, or just the named machines (empty = all). |
| `destroy` | — | `cli` | Stop the lab and delete everything it materialised. |
| `machine.start` | `machine: String`, `record: Vec<RecordRequest>` | `cli` | Start one machine, pulling its template or image first. |
| `machine.stop` | `machine: String`, `force: bool` | `cli` | Stop one machine; `force` kills instead of the graceful ladder. |
| `machine.restart` | `machine: String`, `force: bool`, `record: Vec<RecordRequest>` | `cli` | Stop one machine, wait for it to settle, and boot it again. |
| `machine.destroy` | `machine: String` | `cli` | Stop one machine and delete everything it materialised. |
| `machine.capabilities` | `machine: String` | `cli` | What this machine can do beyond the universal commands, probed live: a display, a console log, in-place reboot, and whichever features its agent negotiated. |
| `machine.ip` | `machine: String`, `nic: Option<usize>` | `cli` | The machine's guest IP, optionally for one NIC index. |
| `machine.screenshot` | `machine: String`, `path: String` | `cli` | Write a PNG of the machine's framebuffer to a host path. |
| `machine.sendkeys` | `machine: String`, `keys: String` | `cli` | Send a key chord to the machine's display. |
| `machine.mouse_move` | `machine: String`, `x: i64`, `y: i64` | `cli` | Move the pointer to an absolute framebuffer position. |
| `machine.mouse_click` | `machine: String`, `button: String`, `x: Option<i64>`, `y: Option<i64>` | `cli` | Click a mouse button, optionally moving there first (both `x` and `y`, or neither). |
| `machine.mouse_drag` | `machine: String`, `x1: i64`, `y1: i64`, `x2: i64`, `y2: i64` | `cli` | Press at one point, drag, release at another. |
| `machine.ocr` | `machine: String`, `region: Option<Region>` | `cli` | Read text off the machine's display, whole screen or one region. |
| `machine.find_image` | `machine: String`, `image: String`, `threshold: f64`, `region: Option<Region>` | `cli` | Find a template image on the machine's display; null when no match scores above `threshold`. |
| `machine.exec` | `machine: String`, `cmd: String`, `args: Vec<String>`, `timeout: u64`, `user: Option<String>`, `password: Option<String>` | `cli` | Run a command in the guest through the agent and collect its output. |
| `machine.osinfo` | `machine: String`, `timeout: u64` | `cli` | What the guest OS says it is. |
| `machine.tty_open` | `machine: String`, `cols: u16`, `rows: u16`, `user: Option<String>`, `password: Option<String>` | `cli` | Open an interactive terminal, re-exposed as a raw-byte unix socket the caller connects to. Every open gets its own shell. |
| `machine.repair_agent` | `machine: String` | `cli` | Push the host's shipped vmlab-agent into a running machine and mark it **diverged** (PRD §19.4). Never fires by itself: an automatic refresh would make the template's sealed `agent_version` a lie. |
| `machine.usb_attach` | `machine: String`, `selector: String`, `force: bool` | `cli` | Hot-plug the host USB device `selector` names into a running VM booted with `usb = true` (PRD §5.2.1): `<vid>:<pid>`, `<vid>:<pid>@<serial>` or `port:<bus>-<port>`. A HID keyboard or mouse is refused without `force`. Answers with the attachment. |
| `machine.usb_detach` | `machine: String`, `selector: Option<String>`, `all: bool` | `cli` | Unplug a host USB device from a VM — the one `selector` names, or with `all` every one it holds. Answers with what was detached. |
| `machine.usb_attached` | `machine: Option<String>` | `daemon` | The host USB devices one machine holds — or, with `machine` omitted, every machine in the lab. |
| `machine.record_start` | `machine: String`, `path: Option<String>`, `options: RecordOptions` | `cli` | Record one VM's console to an MP4 or WebM file (§11.1): the path's extension picks the format, `options` the rate, size, idle compression, timeline and cap. One per machine (`conflict`); a container has no display (`unsupported`); missing ffmpeg or encoder is `failed`. Answers with the live recording's status. |
| `machine.record_stop` | `machine: String` | `cli` | Finalise one VM's recording to a playable file. Answers with the path and the video's length. |
| `machine.record_status` | `machine: Option<String>` | `cli` | Every live recording in the lab — or, with `machine` named, that machine's: path, start time, elapsed time and options. |
| `machine.tty_resize` | `machine: String`, `session: u32`, `cols: u16`, `rows: u16` | `cli` | Resize an open terminal session. |
| `machine.app_open` | `machine: String`, `mode: String`, `argv: Vec<String>`, `cwd: Option<String>`, `cookie: Option<String>`, `user: Option<String>`, `password: Option<String>` | `cli` | Open a **shared app** session (PRD §20): start `argv` in the guest and carry its windows' protocol to the caller, which owns the host desktop end. The reply is `{"session", "control", "conns"}`, plus `"rdp": {"user", "password"}` in `rdp` mode. `control` is a unix socket relaying the session's length-prefixed app records, closed by the caller to end the session; every connection to `conns` becomes one transport connection of the session. In `waypipe` mode a guest with no `waypipe` for that identity answers `unsupported`. |
| `machine.push_file` | `machine: String`, `to: String`, `from: Option<String>`, `data: Option<String>`, `mode: Option<u32>` | `cli` | Copy a file into the guest: either `from`, a host path the daemon can see, or `data`, base64 for a caller that holds bytes. |
| `machine.pull_file` | `machine: String`, `from: String`, `to: Option<String>` | `cli` | Copy a file out of the guest: to `to`, a host path the daemon can write, or — with `to` omitted — back inline as base64, for a caller that wants the bytes rather than a file on the daemon's host. |
| `machine.tail` | `machine: String`, `path: String` | `cli` | Follow a guest file (`tail -F` semantics), streamed as chunks until the caller hangs up or the machine stops. |
| `machine.eventlog` | `machine: String`, `filter: Option<String>` | `cli` | Follow the Windows event log, streamed as chunks. |
| `machine.stats` | `machine: String` | `cli` | Latest guest metrics; subscribes the sampler on first use. |
| `machine.clipboard_get` | `machine: String` | `cli` | Read the guest clipboard. |
| `machine.clipboard_set` | `machine: String`, `text: String` | `cli` | Write the guest clipboard. |
| `machine.logs` | `machine: String`, `lines: usize`, `follow: bool` | `cli` | The machine's console log: the last `lines`, then, with `follow`, streamed growth until the machine stops. |
| `playbook.list` | — | `cli` | Every playbook assignment in the lab, one row per (machine, block). |
| `playbook.check` | `machine: String`, `playbook: Option<String>`, `play: Option<String>` | `cli` | Dry-run a playbook against one machine, streaming its output. |
| `playbook.apply` | `machine: String`, `playbook: Option<String>`, `play: Option<String>` | `cli` | Apply a playbook to one machine, streaming its output. |
| `snapshot.take` | `name: String`, `machine: Option<String>` | `cli` | Snapshot one machine, or the whole lab when `machine` is omitted. |
| `snapshot.restore` | `name: String`, `machine: Option<String>`, `discard: bool` | `cli` | Restore one machine, or every machine when `machine` is omitted. `discard` is §19.6's explicit discard flag: a restore rewinds a dev machine's workspace and re-converges it from the host, which destroys the guest copy of every conflicting path, so a halted workspace refuses until this says so. It defaults to `false` and means nothing for a machine without a workspace. |
| `snapshot.delete` | `machine: String`, `name: String` | `cli` | Delete one machine's snapshot. |
| `snapshot.list` | `machine: String` | `cli` | One machine's snapshots. |
| `worktree.freeze` | `snapshot: String` | `cli` | Freeze the stopped lab at the offline snapshot `snapshot` on every machine, as the next generation worktree labs clone from (PRD §21.1). |
| `worktree.thaw` | `drop_online_snapshots: bool` | `cli` | Fold the freeze back into the stopped main lab's disks and unlock its earlier snapshots (PRD §21.3). Refused while any worktree lab backs onto it. `drop_online_snapshots` gives up the online snapshots taken since a freeze, whose machine state no commit can carry, rather than refusing. |
| `workspace.flush` | `machine: String` | `cli` | Run a workspace sync pass now and answer with what it decided (PRD §19.6). What `vmlab dev sync flush` and `status --wait` are. |
| `workspace.resolve` | `machine: String`, `paths: Vec<String>`, `all: bool`, `winner: String` | `cli` | Say which side wins at halted paths, and carry it out (§19.6). `paths` empty with `all` set takes the whole batch — the 30 000-file case is one `.vmlabignore` edit away and nobody is going to type it. |
| `workspace.diff` | `machine: String`, `paths: Vec<String>` | `cli` | Bring the guest's copy of one workspace path to the host (§19.6). The host copy is a plain directory on the developer's own workstation, so only the *guest* side is behind the seam — which is the whole reason this verb exists rather than "attach and look". |
| `shutdown` | — | `cli`, `daemon` | Tear the lab daemon down; the reply is sent before it exits. |

## Coverage

- `cli` — the `vmlab` verb surface (`src/cli`, `src/template/cli.rs`, `src/appshare`)
- `daemon` — one daemon calling another (`src/labd`, `src/supervisor`)

Asymmetry is not automatically wrong — some commands only make sense from one place.
The lists below exist so that each one is a decision somebody made rather than a gap
nobody noticed. Every command reachable from a single surface says which it is, beside
its declaration in the vocabulary, and the build fails while one says neither — so the
open gaps below are a worklist rather than a list somebody has to re-derive.

### Reachable from no surface

Every command has a caller.

### Reachable only from `cli`

Deliberate, with the reason recorded beside the declaration:

- `version` — The build check is the CLI's: it is the one process that can be a different build from the daemon it connects to, and a script runs inside the daemon that would answer.
- `run` — A scratch script is a shell verb: it comes from a file the caller already has and streams its output back to the terminal that ran it. What the console runs is declared playbooks.
- `machine.ip` — A scripting shortcut over data the console already holds: the lab status projection carries every machine's address, so the console reads it there rather than asking a second time.
- `machine.mouse_move` — The console drives a machine through a live VNC canvas, where a human moves the pointer themselves. Scripted pointer input is for callers that have no canvas.
- `machine.mouse_click` — Scripted input, for the same reason as `machine.mouse_move`: a console user clicks the VNC canvas directly.
- `machine.mouse_drag` — Scripted input, for the same reason as `machine.mouse_move`: a console user drags on the VNC canvas directly.
- `machine.ocr` — Reading text off the framebuffer is a script's substitute for looking at it. The console shows the framebuffer to somebody who can already read it.
- `machine.find_image` — How a script finds a control it cannot see. A console user clicks the one they can, on the VNC canvas.
- `machine.exec` — The scripted counterpart to the console's interactive terminals: one command, its output collected, an exit code to branch on. The console opens a shell and lets a human type instead.
- `machine.osinfo` — A live guest probe with a timeout, so it does not belong on a panel that refreshes; the status projection already carries what the console shows about a machine. Its CLI help calls it fit for scripting, which is what it is.
- `machine.repair_agent` — A deliberate, machine-changing act with a rebuild as its alternative — the console offering it as a button would invite exactly the reflex the verb exists to keep manual, and its audience is whoever is iterating on the agent itself, at a terminal.
- `machine.usb_attach` — A script attaches through the lab runtime it is bound to, not over the socket, so the verb is the one caller that crosses it.
- `machine.usb_detach` — The other half of `machine.usb_attach`, and one-way for the same reason.
- `machine.record_start` — A script records through the lab runtime it is bound to              (`m.record`), not over the socket, so the verb is the one caller              that crosses it.
- `machine.record_stop` — The other half of `machine.record_start`, and one-way for the same reason.
- `machine.record_status` — `status` folds the same entries into each machine's line; this              is the recording-only view `vmlab record status` shows.
- `machine.app_open` — The host end of a shared app is a window-protocol client running in the user's own desktop session, which only a process started there has; the daemon is headless by design (§11).
- `machine.tail` — An open-ended stream of an arbitrary guest path, which is what a terminal is for. The console follows a machine's console log through `machine.logs`.
- `machine.eventlog` — A stream into a terminal, for the same reason as `machine.tail`.
- `playbook.list` — One flat table is the shape a shell wants. The console builds its playbook list from the lab's declarations directly and asks the daemon only which runs are in flight.
- `worktree.freeze` — Freezing is done once, by hand, in the main checkout: the machines' disks become read-only files every worktree of the repository depends on. A lab script has no reason to take a decision with that reach, and nothing to gain from it.
- `worktree.thaw` — The counterpart of `worktree.freeze`, for the same reason.
- `workspace.flush` — The console already has the answer: a syncer's report is part of the machine's status projection, which the console polls. What this adds is *waiting for a pass*, which is a terminal's idiom — a page that blocks for up to two minutes on a guest that has stopped answering is a page nobody wants.
- `workspace.resolve` — §19.6 states it outright: the console reads the halt and does not act on it. Resolution is a per-path judgement about a developer's own working copy, made beside the two directories in question, and the copy that loses is not recoverable from vmlab — which is a decision for a terminal in the lab directory, not a button.
- `workspace.diff` — It answers with the guest's bytes for a host-side `diff`, whose audience is a terminal. A console showing two versions of a source file is a diff viewer, which is the editor's job — and the editor is already attached into the guest.
- `version` — What `vmlab daemon status` prints: which build of the supervisor is running on this host, asked by whoever is standing in front of it.
- `lab.ensure` — Spawning-or-finding a lab daemon belongs in one place, and that place is the helper in `src/cli/daemon.rs` — the web layer calls it rather than asking the supervisor itself. One call site is the decision; the scan reports it as the CLI because that is where the helper lives.
- `lab.release` — The other half of `lab.ensure`, and a shell's alone: a command finishes and gives the daemon back. The console does not finish, and leaves it up for the next request.
- `store.list` — The store is host-wide; every template command the console has is scoped to the lab it has open, and it has no view of the store as a whole to hang this on. Giving it one is a separate decision from putting the operations on the protocol, which is what this namespace does.
- `store.remove` — Store management, for the reason on `store.list`.
- `store.prune` — Store management, for the reason on `store.list`.
- `store.export` — Store management, for the reason on `store.list`.
- `store.import` — Store management, for the reason on `store.list`.
- `store.pull` — Store management, for the reason on `store.list`.
- `store.push` — Store management, for the reason on `store.list`.
- `store.stop_push` — The other half of `store.push`.
- `registry.search` — The console searches a namespace through its own REST endpoint, which runs in the web process rather than over this socket — `GET /api/catalog/oci`. Routing it here is #37's business, not this command's.
- `registry.login` — The console has its own login endpoint in the web process (`POST /api/registries/login`), for the same reason as `registry.search`.
- `registry.namespaces` — Namespace settings reach the console through the web process's own `/api/registries` endpoints, for the same reason as `registry.search`.
- `registry.namespace_add` — Namespace settings, for the reason on `registry.namespaces`.
- `registry.namespace_remove` — Namespace settings, for the reason on `registry.namespaces`.

Neither, which the build rejects:

- `status`
- `dns.table`
- `up`
- `pull`
- `pull.cancel`
- `down`
- `destroy`
- `machine.start`
- `machine.stop`
- `machine.restart`
- `machine.destroy`
- `machine.capabilities`
- `machine.screenshot`
- `machine.sendkeys`
- `machine.tty_open`
- `machine.tty_resize`
- `machine.push_file`
- `machine.pull_file`
- `machine.stats`
- `machine.clipboard_get`
- `machine.clipboard_set`
- `machine.logs`
- `playbook.check`
- `playbook.apply`
- `snapshot.take`
- `snapshot.restore`
- `snapshot.delete`
- `snapshot.list`
- `fastpath`
- `status`
- `lab.restart`
- `template.list`
- `template.build`
- `template.stop_build`

### Reachable only from `daemon`

Deliberate, with the reason recorded beside the declaration:

- `machine.usb_attached` — The supervisor asks every lab to fold holders into `usb.list`, which is the listing the CLI shows; a per-machine view of the same rows would be a second way to read one fact.
- `global.attach` — Daemon-internal: a lab daemon joins a global segment because a lab declared one, so there is nothing for a person to ask for.
- `global.detach` — The other half of `global.attach`, and daemon-internal for the same reason.
- `global.list` — A lab daemon reads it to fold each segment's peer state into the lab status projection, which is how both other surfaces already see it.
- `gpu.keepawake` — Daemon-internal: a lab daemon asks before a `virgl`/`vulkan` VM starts on a GPU whose `keepawake` wants it, so there is nothing for a person to ask for; `vmlab status -v` shows the answer.

