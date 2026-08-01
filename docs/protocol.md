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
| `ping` | — | `cli`, `daemon`, `web` | Liveness check; answers `"pong"`. |
| `version` | — | `cli` | The supervisor's own build version. |
| `fastpath` | — | `cli`, `web` | Which network fast-path tier this host selected (PRD §9.1), and why the skipped tiers were unavailable. |
| `status` | — | `cli`, `web` | Every lab in the registry. |
| `lab.ensure` | `name: String`, `root: std::path::PathBuf` | `cli` | Spawn (or find) a lab's daemon; answers with its socket path. |
| `lab.release` | `name: String` | `cli` | Stop a lab's daemon, after `down` or `destroy`. |
| `lab.restart` | `name: String`, `root: std::path::PathBuf` | `web` | Restart a lab's daemon so it re-reads its config; answers with the new socket path. |
| `global.attach` | `name: String`, `subnet: Option<Ipv4Net>`, `peer: Option<String>` | `daemon` | Join a global segment (PRD §9.2), creating it on first use; answers with the trunk socket to bridge to. |
| `global.detach` | `name: String` | `daemon` | Leave a global segment. |
| `global.list` | — | `daemon` | Every global segment this host knows. |
| `template.list` | `lab: String`, `root: std::path::PathBuf` | `web` | The templates a lab declares, with their store and build state. |
| `template.remote` | `lab: String`, `root: std::path::PathBuf`, `template: String`, `arch: Option<String>` | `web` | What the registry holds for one declared template. |
| `template.build` | `lab: String`, `root: std::path::PathBuf`, `template: String`, `arch: Option<String>` | `web` | Start building one declared template. |
| `template.stop_build` | `lab: String`, `arch: String`, `template: String` | `web` | Abort a running build. |
| `template.push` | `lab: String`, `root: std::path::PathBuf`, `template: String`, `arch: Option<String>`, `version: Option<String>` | `web` | Start pushing one built template to its registry. |
| `template.op_status` | `lab: String` | `web` | Which template builds and pushes are in flight for one lab. |
| `template.console_path` | `lab: String`, `arch: String`, `template: String` | `web` | The socket serving a running build's console, for the web viewer. |
| `shutdown` | — | `cli`, `daemon` | Tear the supervisor down; the reply is sent before it exits. |

## A lab daemon's socket

| command | arguments | called by | what it does |
|---|---|---|---|
| `ping` | — | `cli`, `daemon`, `web` | Liveness check; answers `"pong"`. |
| `status` | — | `cli`, `web` | The whole lab's runtime status: machines, segments, readiness. |
| `dns.table` | — | `web` | The DNS zones the lab's segments serve. |
| `up` | `vms: Vec<String>` | `cli`, `web` | Bring the lab up, or just the named machines (empty = all). Streams provisioning output. |
| `pull` | `vms: Vec<String>` | `cli`, `web` | Download every pending template and image without starting anything, over the code path `up` runs first. |
| `pull.cancel` | `machine: String` | `web` | Abort one machine's running download; whatever waits on it fails with "download cancelled". |
| `run` | `script: String` | `cli` | Run an ad-hoc wscript against the lab (PRD §12), streaming output. |
| `down` | `vms: Vec<String>`, `force: bool` | `cli`, `web` | Stop the lab, or just the named machines (empty = all). |
| `destroy` | — | `cli`, `web` | Stop the lab and delete everything it materialised. |
| `machine.start` | `machine: String` | `cli`, `web` | Start one machine, pulling its template or image first. |
| `machine.stop` | `machine: String`, `force: bool` | `cli`, `web` | Stop one machine; `force` kills instead of the graceful ladder. |
| `machine.restart` | `machine: String`, `force: bool` | `cli`, `web` | Stop one machine, wait for it to settle, and boot it again. |
| `machine.destroy` | `machine: String` | `cli`, `web` | Stop one machine and delete everything it materialised. |
| `machine.capabilities` | `machine: String` | `web` | What this machine can do beyond the universal commands, probed live: a display, a console log, in-place reboot, and whichever features its agent negotiated. |
| `machine.ip` | `machine: String`, `nic: Option<usize>` | `cli` | The machine's guest IP, optionally for one NIC index. |
| `machine.screenshot` | `machine: String`, `path: String` | `cli`, `web` | Write a PNG of the machine's framebuffer to a host path. |
| `machine.sendkeys` | `machine: String`, `keys: String` | `cli`, `web` | Send a key chord to the machine's display. |
| `machine.mouse_move` | `machine: String`, `x: i64`, `y: i64` | `cli` | Move the pointer to an absolute framebuffer position. |
| `machine.mouse_click` | `machine: String`, `button: String`, `x: Option<i64>`, `y: Option<i64>` | `cli` | Click a mouse button, optionally moving there first (both `x` and `y`, or neither). |
| `machine.mouse_drag` | `machine: String`, `x1: i64`, `y1: i64`, `x2: i64`, `y2: i64` | `cli` | Press at one point, drag, release at another. |
| `machine.ocr` | `machine: String`, `region: Option<Region>` | `cli` | Read text off the machine's display, whole screen or one region. |
| `machine.find_image` | `machine: String`, `image: String`, `threshold: f64`, `region: Option<Region>` | `cli` | Find a template image on the machine's display; null when no match scores above `threshold`. |
| `machine.exec` | `machine: String`, `cmd: String`, `args: Vec<String>`, `timeout: u64` | `cli` | Run a command in the guest through the agent and collect its output. |
| `machine.osinfo` | `machine: String`, `timeout: u64` | `cli` | What the guest OS says it is. |
| `machine.tty_open` | `machine: String`, `cols: u16`, `rows: u16` | `cli`, `web` | Open an interactive terminal, re-exposed as a raw-byte unix socket the caller connects to. Every open gets its own shell. |
| `machine.tty_resize` | `machine: String`, `session: u32`, `cols: u16`, `rows: u16` | `cli`, `web` | Resize an open terminal session. |
| `machine.push_file` | `machine: String`, `to: String`, `from: Option<String>`, `data: Option<String>`, `mode: Option<u32>` | `cli` | Copy a file into the guest: either `from`, a host path the daemon can see, or `data`, base64 for a caller that holds bytes. |
| `machine.pull_file` | `machine: String`, `from: String`, `to: String` | `cli` | Copy a file out of the guest to a host path. |
| `machine.tail` | `machine: String`, `path: String` | `cli` | Follow a guest file (`tail -F` semantics), streamed as chunks until the caller hangs up or the machine stops. |
| `machine.eventlog` | `machine: String`, `filter: Option<String>` | `cli` | Follow the Windows event log, streamed as chunks. |
| `machine.stats` | `machine: String` | `web` | Latest guest metrics; subscribes the sampler on first use. |
| `machine.clipboard_get` | `machine: String` | `web` | Read the guest clipboard. |
| `machine.clipboard_set` | `machine: String`, `text: String` | `web` | Write the guest clipboard. |
| `machine.logs` | `machine: String`, `lines: usize`, `follow: bool` | `cli`, `web` | The machine's console log: the last `lines`, then, with `follow`, streamed growth until the machine stops. |
| `web.forward` | `machine: String`, `page: String` | `web` | Ensure a loopback forward for a declared web page and return the address to dial, plus the page's auth spec (host-side only). |
| `playbook.list` | — | `cli` | Every playbook assignment in the lab, one row per (machine, block). |
| `playbook.check` | `machine: String`, `playbook: Option<String>`, `play: Option<String>` | `cli`, `web` | Dry-run a playbook against one machine, streaming its output. |
| `playbook.apply` | `machine: String`, `playbook: Option<String>`, `play: Option<String>` | `cli`, `web` | Apply a playbook to one machine, streaming its output. |
| `playbook.op_status` | — | `web` | Which playbook runs are in flight. |
| `snapshot.take` | `name: String`, `vm: Option<String>` | `cli`, `web` | Snapshot one machine, or the whole lab when `vm` is omitted. |
| `snapshot.restore` | `name: String`, `vm: Option<String>` | `cli`, `web` | Restore one machine, or every machine when `vm` is omitted. |
| `snapshot.delete` | `machine: String`, `name: String` | `cli`, `web` | Delete one machine's snapshot. |
| `snapshot.list` | `machine: String` | `cli`, `web` | One machine's snapshots. |
| `shutdown` | — | `cli`, `daemon` | Tear the lab daemon down; the reply is sent before it exits. |

## Coverage

- `cli` — the `vmlab` verb surface (`src/cli`, `src/template/cli.rs`)
- `web` — the REST/WebSocket API, and so the console (`src/web`)
- `daemon` — one daemon calling another (`src/labd`, `src/supervisor`)

Asymmetry is not automatically wrong — some commands only make sense from one place.
The lists below exist so that each one is a decision somebody made rather than a gap
nobody noticed.

### Reachable from no surface

Every command has a caller.

### Reachable only from `cli`

- `run`
- `machine.ip`
- `machine.mouse_move`
- `machine.mouse_click`
- `machine.mouse_drag`
- `machine.ocr`
- `machine.find_image`
- `machine.exec`
- `machine.osinfo`
- `machine.push_file`
- `machine.pull_file`
- `machine.tail`
- `machine.eventlog`
- `playbook.list`
- `version`
- `lab.ensure`
- `lab.release`

### Reachable only from `web`

- `dns.table`
- `pull.cancel`
- `machine.capabilities`
- `machine.stats`
- `machine.clipboard_get`
- `machine.clipboard_set`
- `web.forward`
- `playbook.op_status`
- `lab.restart`
- `template.list`
- `template.remote`
- `template.build`
- `template.stop_build`
- `template.push`
- `template.op_status`
- `template.console_path`

### Reachable only from `daemon`

- `global.attach`
- `global.detach`
- `global.list`

## REST action segments

The REST layer projects a slice of the vocabulary onto URL path segments. The console's
action types are generated from these, so it holds no command list of its own.

`POST /api/labs/{lab}/{action}`

| segment | command |
|---|---|
| `up` | `up` |
| `down` | `down` |
| `destroy` | `destroy` |
| `pull` | `pull` |

`POST /api/labs/{lab}/machines/{machine}/{action}`

| segment | command |
|---|---|
| `start` | `machine.start` |
| `stop` | `machine.stop` |
| `restart` | `machine.restart` |
| `destroy` | `machine.destroy` |

