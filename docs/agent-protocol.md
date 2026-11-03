# Guest agent protocol v2

This reference specifies the byte stream used by the production host client in
`src/labd/vm_agent.rs` and the Rust, C legacy, and HolyC agents. It describes the
current implementation. The findings below identify differences that an
independent implementation must not mistake for shared guarantees. A separate
bug-fix commit corrects legacy unsupported-open refusals, F3 and F12. The Rust
agent's behavior is unchanged.

The generated inventory covers all serializable wire messages and supporting
types in `guest/agent-proto/src/{lib,fileops,watch,app}.rs`. The guest-internal
`ContainerConfig` is excluded. The handwritten sections define message direction,
state, side effects, and limits that field types cannot express.

## Transport and trust

| Transport | Guest endpoint | Host endpoint and guarantees |
|---|---|---|
| Virtio serial | Port named `vmlab.agent.0`; Linux `/dev/virtio-ports/vmlab.agent.0`, macOS `/dev/cu.vmlab.agent.0`, Windows virtio-serial device | QEMU chardev Unix socket, one host client. Ordered duplex bytes, no message boundaries. A snapshot restore can replay or lose bytes. |
| ISA serial | 16550 COM1, I/O base `0x3f8`, 115200 baud, eight data bits, no parity, one stop bit, no hardware or software flow control | Same chardev Unix socket. Serial timing limits throughput to about 11 KiB/s. COM2 carries the serial console when COM1 carries the agent. |
| Container micro-VM | Same virtio port, agent launched by cinit with `--container <config>` | Same host client and framing. There is no agent TCP listener. Exec and terminal enter the container namespaces, or chroot in idle mode. File operations resolve inside the rootfs. |
| Host-process conformance | Rust `--port <raw-tty>` or legacy POSIX `--listen <unix-socket>` | A raw pty bridged to a Unix socket, or a direct Unix stream. No UART rate or snapshot guarantees are simulated. |

The Rust Linux agent falls back to COM1 when it finds no virtio-serial controller.
`--port` overrides discovery on Unix. The tty configuration is raw, 115200 8N1,
with `CLOCAL` and `CREAD`. C Win32 asserts DTR and RTS; DOS and TempleOS also set
OUT2. These signals are not credit flow control.
Sources: `guest/agent/src/unix/mod.rs:146`, `guest/agent/src/unix/linux.rs:49`,
`guest/agent-legacy/src/plat_win32.c:98`, `guest/agent-legacy/src/plat_dos.c:59`.

There is no authentication, encryption, checksum, or authorization layer on this
stream. The VM boundary and host socket permissions supply access control.
Paths and commands are guest-native, not a sandbox. Credentials cross in plaintext
on the local pipe. The protocol is not a security boundary.

## Outer framing

All integer header fields are unsigned and little-endian. The header is 13 bytes.

| Offset | Size | Value |
|---|---|---|
| 0 | 4 | ASCII `VMLB`, hexadecimal `56 4d 4c 42` |
| 4 | 4 | Payload length only, uint32 LE, 0 through 65536 |
| 8 | 1 | Kind: 0 control, 1 data, 2 stderr data |
| 9 | 4 | Channel ID, uint32 LE |
| 13 | length | Payload bytes |

Control frames use channel 0 and contain one UTF-8 JSON object, without a newline
or trailing NUL. Host objects have a `cmd` discriminator; agent objects have an
`event` discriminator. Nonzero channels carry opaque bytes. Only agent exec output
uses kind 2. Rust tolerates host kind 2 as stdin, but a host must send kind 1.
An empty data frame is valid and is not EOF. Receivers accept arbitrarily split
headers, split payloads, and multiple frames in a read.

The Rust and C decoders scan for magic after noise. If a candidate header has an
unknown kind or payload length over 65536, they discard one byte and resume the
scan. With no magic, they retain at most three trailing bytes for split magic.
A plausible but incomplete frame waits for its declared payload. There is no
timeout or checksum that distinguishes such a frame from noise. Magic embedded
inside a valid frame's payload has no special meaning. TempleOS differs on
invalid kinds, as recorded in F4.
Source: `guest/agent-proto/src/lib.rs:270`, `guest/agent-legacy/src/wire.c:59`.

JSON integers must fit the widths in the generated tables. uint64 values require
exact integer arithmetic, including values above JavaScript's exact-number range.
JSON strings use JSON escaping. Raw file and process bytes need not be UTF-8.
Unknown fields are ignored by the Rust serde types. Unknown variants are errors.
Omitted defaults and nullability are listed separately in the tables. `env` is an
array of two-element string arrays, not a JSON object.

## Handshake, epochs, and identity

The host sends `hello` with `proto_version = 2` and a fresh opaque `token`.
The agent removes existing channels and replies `hello`, echoing that token,
reporting its own version, build stamp, OS tag, and feature strings.
The host accepts a hello only when both token and version match. A mismatched
agent version times out rather than negotiating a lower version. The production
host uses a 16-character hexadecimal token. The legacy token limit is 255 decoded
bytes. Implementations need not support unbounded tokens.

The token echo is the epoch barrier. During recovery the host discards stale
channel state and ignores old hello tokens. A new hello on the same transport
must release the previous epoch's channels, handles, and process resources.
An agent must tolerate another hello without a disconnect. The Rust agent emits
an error on a host version mismatch but still sends its own hello. Legacy and
TempleOS ignore the requested version. The host is the actual version gate.
Sources: `src/labd/vm_agent.rs:254`, `src/labd/vm_agent.rs:2197`,
`guest/agent/src/mux.rs:409`, `guest/agent-legacy/src/agent.c:468`.

Full agents include `instance` in hello and pong. It is an opaque agent-process
identity, stable across re-handshakes and changed on agent restart. A host ping
answered under a different instance requires a fresh handshake. Legacy and
TempleOS omit it, so ping cannot detect replacement on those tiers.

OS boot identity is a separate host-side exec probe, not a wire message or feature.
Linux reads `/proc/sys/kernel/random/boot_id`; macOS reads `kern.bootsessionuuid`.
Windows reads registry `BootId`, falling back to WMI last boot time. DOS,
Windows 9x/ME, and TempleOS refuse boot identity by name.
Source: `src/labd/boot_id.rs:1`.

## Features and degradation

`net_info`, `os_info`, `shutdown`, `ping`, and hello are v2 baseline messages.
Every tier also implements exec. A feature advertisement is a capability, not a
promise that a request will succeed with missing programs, permissions, or an
unavailable desktop. Unknown feature strings may be ignored by production hosts.
The conformance runner flags them as untested instead of certifying them.

| Tier | Required current advertisement | Conditional or unavailable |
|---|---|---|
| Full Linux | `terminal`, `exec`, `fileops`, `tail`, `metrics`, `watch`, `tree`, `set_time`, `get_time`, `app_waypipe`, `app_x11`, `app_log_split` | `clipboard` and `clipboard_reply` depend on the startup display-server probe. No `eventlog` or `app_rdp`. |
| Full Windows | `terminal`, `exec`, `fileops`, `tail`, `metrics`, `watch`, `tree`, `set_time`, `get_time`, `clipboard`, `clipboard_reply`, `eventlog`, `app_rdp` | Clipboard can fail while nobody is logged on. RDP can fail for guest edition, policy, logon, or listener readiness. |
| Full macOS | `terminal`, `exec`, `fileops`, `tail`, `metrics`, `set_time`, `get_time` | Clipboard and reply depend on platform probe. No watch, tree, eventlog, or apps. |
| Legacy C | `exec`, `set_time` | No terminal, files, watch, metrics, clipboard, tree, get_time, or apps. No logon minting. |
| TempleOS | `exec` | No optional vocabularies or logon minting. Commands are HolyC source. |

Sources: `guest/agent/src/unix/mod.rs:288`, `guest/agent/src/unix/linux.rs:23`,
`guest/agent/src/windows/mod.rs:113`, `guest/agent-legacy/src/agent.c:137`,
`guest/agent-templeos/VmlabAgt.HC:353`.

The host's degradation rules are operation-specific:

| Capability absent | Host behavior |
|---|---|
| Agent handshake | No agent readiness. Screen-driven operations remain available. |
| `fileops` | File transfer and repair fail by name. No old whole-file fallback. |
| `watch` | Workspace watch refuses. There is no periodic stat-walk substitute for a supported watch. |
| `tree` | Tree-dependent operations refuse; ordinary exec remains usable. |
| `set_time` | Clock setting refuses. Restore cannot use this capability. |
| `get_time` | Initial clock validation is skipped. |
| `clipboard` | Gets and sets refuse immediately in the client. |
| `clipboard_reply`, with clipboard present | Set is fire-and-forget. Get waits for text or its timeout. |
| Selected `app_*` | Shared-app selection refuses that mode. |
| `terminal`, `tail`, `eventlog` | Opens reach the agent and must return an ID-bearing error when unsupported. |

Some client methods perform their own feature check; `open_terminal`, `open_app`,
`open_tail`, metrics, and eventlog do not all do so. A replacement must not rely
on the client to filter every unsupported message. Channel-less controls without
request IDs cannot return correlated generic errors. TempleOS app opens retain
an existing exception, F3. The C agent's corresponding bug is fixed separately.

## Channel lifetime, ordering, and flow control

Only the host opens channels. IDs are nonzero uint32 values chosen by the host
and unique among live channels. The Rust agent refuses zero and duplicate IDs.
An open replies `opened` only after setup succeeds, or `error` with that ID.
Output follows `opened`. A channel error ends that channel; generic errors have
no structured error code. Fileops has its own structured operation errors.

Per-channel bytes preserve order. Channels may interleave. There is no global
ordering between stdout and stderr, different channels, or independent control
requests. The production client serializes clipboard, tree-status, and clock
requests because their replies carry no request ID. Implementers should likewise
avoid multiple outstanding operations whose replies cannot be distinguished.

Each direction starts with 262144 bytes of credit per opened channel. Sending
data consumes payload bytes only, including both stdout and stderr against one
shared window. A sender may send at most its available credit, in frames of at
most 65536 bytes. `window_adjust` adds credit. Receivers replenish after consuming
at least 131072 bytes, granting the consumed amount. Header and control bytes do
not consume credit. Nested record headers do consume credit as channel data.

EOF is a control message and a half-close. It does not spend credit. Exec input
must drain before stdin closes. Exec output drains before the agent's `eof`,
which precedes `exited`. No more bytes follow output EOF, but input may continue.
Terminal output merges stderr into the PTY stream and ends with `exited` without
an EOF message. Terminal input EOF is ignored.

`close` has no acknowledgement. It cancels a channel and releases its resources.
Closing exec kills its direct process, not arbitrary descendants. `tree` counts
descendants but does not make close a tree-kill operation. Closed IDs must emit
no new frames, although already-written bytes can still arrive at the host.
The Rust mux cancels pending channel frames and suppresses later sends from
session workers, including EOF, exit status, errors, and credit grants.
Processes that finish before close still report their output and exit normally.
Unknown close, EOF, data, and credit for vanished Rust channels are ignored.

Credit prevents one stream from growing without limit. It does not guarantee
fair scheduling. Rust uses bounded writer and input queues; the host closes a
session whose 2048-event queue stays full for ten seconds. The host open timeout
is 15 seconds. These are implementation policies, not negotiated wire fields.
Sources: `guest/agent/src/mux.rs:147`, `guest/agent/src/exec.rs:61`,
`src/labd/vm_agent.rs:130`, `src/labd/vm_agent.rs:1018`.

## Vocabulary semantics

### Exec and terminal

`open_exec` passes argv directly to process creation. A shell runs only when argv
explicitly names one. Environment entries override the selected identity's
environment. Absent cwd uses that identity's default. Empty argv and setup
failures produce an ID-bearing error. POSIX legacy failures after fork instead
open and exit 127. Unix signal death reports `128 + signal`. Windows reports its
process exit code as int32.

`open_terminal` allocates a PTY or ConPTY, applies rows and columns, and starts
the selected shell or command. `resize` changes its dimensions. Terminal bytes
include the platform's terminal conventions and agent banner. A host must not
expect byte-identical terminal output across operating systems.

`tree` is an optional named process-tree membership on exec. Valid names contain
1 through 64 ASCII letters, digits, hyphens, or underscores. `tree_status` returns
the live count, zero for an unknown or drained tree. Linux uses a subreaper;
Windows uses job membership. Container orphan handling differs, F7.

DOS executes synchronously and sends `opened` only after the command returns.
It spools stdout and stderr, discards stdin, and cannot answer pings or cancel a
running program. TempleOS joins argv with spaces, appends a semicolon when
needed, and executes it with `ExePrint`. It ignores env and cwd, strips DolDoc
markup, captures only stdout, and truncates output at 65535 bytes. Compile errors
and uncaught exceptions exit 1. It cannot cancel or answer control requests while
HolyC runs. These tiers do not satisfy full-agent streaming semantics.
Sources: `guest/agent-legacy/src/plat_dos.c:146`,
`guest/agent-templeos/VmlabAgt.HC:514`.

### Fileops

`open_file_ops` opens one RPC session. Its data stream contains records:

```text
metadata_length uint32 LE | data_length uint32 LE | JSON metadata | raw data
```

Metadata is at most 8388608 bytes. Raw data is at most 131072 bytes. Records may
span outer frames or share a frame. A malformed, oversized, or undecodable
record fails the channel. There is no inner resynchronization marker.

A request is `{id: uint64, ...Op}`. A response is `{id: uint64, ...Reply}`.
The `op` or `reply` discriminator is flattened beside `id`, never nested.
IDs match replies to requests. Multiple requests may be outstanding, and replies
may arrive out of order. The Rust server has four workers and a 64-request queue.
The caller must await dependencies such as open before read, and write before
digest, rename, or close. Pipelining alone supplies no execution barrier.

Handles are uint64 values scoped to a session. They begin at 1 in Rust and are
released on close or epoch reset. File and directory handles are distinct.
Unknown, closed, and wrong-kind handles return `bad_handle`.

| Operation | Result and semantics |
|---|---|
| `open` | `handle`. All-false flags mean read-only. Exclusive implies create. Append ignores write offsets. Mode is Unix permission bits and is ignored on Windows. See F6 for existing-file mode. |
| `close` | `ok`. Releases one handle. This is distinct from control-channel close of the whole session. |
| `read` | `data` plus raw bytes. Reads at the requested offset. A short or empty result means EOF, not an arbitrary partial read. A length above 131072 is refused. |
| `write` | `ok` after all raw bytes are written at offset, or at EOF for append. No implicit truncate or flush-to-durable-storage guarantee. |
| `fstat`, `stat`, `lstat` | `attrs`. Stat follows symlinks; lstat describes the link. Size is bytes; times are signed epoch nanoseconds. |
| `fsetstat`, `setstat` | `ok`. Only supplied attributes change. Size truncates or extends. Mode is ignored on Windows. |
| `open_dir` | Directory `handle`. |
| `read_dir` | `entries`, at most 512 at a time, plus `eof`. No `.` or `..`. Entry attributes describe links without following them. Order is filesystem-dependent. |
| `mkdir` | `ok`. Optional mode and case-sensitivity requirement. Windows must enable the NTFS flag while empty or report unsupported. Already-case-sensitive filesystems satisfy it directly. |
| `rmdir` | `ok` for an empty directory only. |
| `remove` | `ok` for a file or symlink. No recursive removal. |
| `rename` | `ok`. Overwrite behavior follows the guest OS. |
| `realpath` | `name` containing a guest-native canonical path. |
| `symlink` | `ok`. Target text is preserved. `kind` selects file or directory creation on Windows, including dangling links. |
| `readlink` | `name` containing the target text. |
| `digest` | Lowercase hexadecimal SHA-256 and byte length of the file on disk. No base64 or host-computed substitute. |

An operation failure returns `reply:error` with `code` and descriptive `msg` and
leaves the session usable. Codes are `no_such_file`, `permission_denied`,
`already_exists`, `not_a_directory`, `is_a_directory`, `not_empty`, `bad_handle`,
`unsupported`, and catch-all `failure`. OS mappings are in
`guest/agent-proto/src/fileops.rs:343`. Message prose is not a stable API.

### Tail and eventlog

`open_tail` sends the last 65536 bytes of an existing file, then live appends.
It polls every 250 ms. Truncation reopens at byte zero. Unix detects replacement
by inode and device; Windows only detects shrinkage, F5. A missing path fails the
open. The logon remains held across reopens. Close stops the follower.

`open_event_log` is Windows-only. Its optional filter is an XPath query, default
`*` on the System channel. Output is XML-rendered event bytes on the data channel.
It runs as the agent identity and carries no logon. Unix and legacy refuse it.

### Watch

`open_watch` registers a recursive watcher before `opened`. Its channel carries
`uint32 LE length | UTF-8 JSON` records, each at most 33554432 bytes. Prune entries
are root-relative directory prefixes with `/` separators. The host computes
ignores. The agent normalizes separators and `.` components, and refuses a prune
that covers the root. It does not follow directory symlinks.

The agent coalesces dirty paths. The empty-to-nonempty transition sends one
`dirty` nudge. The host sends `drain`, with at most one outstanding. The agent
atomically swaps the dirty set and sends one `batch` containing each path's
current kind, size, and nanosecond mtime. Absent `stat` is a tombstone. There are
no event types, request IDs, or batch acknowledgements. Later changes accumulate
for the next drain.

A kernel overflow, more than 20000 dirty paths, or lost subtree coverage replaces
the batch with `rescan`. The host must stat-walk before resuming synchronization.
A lost channel likewise requires a stat-walk. Root disappearance fails the
channel. Wrong-direction or malformed records fail the channel.
Source: `guest/agent/src/watch/mod.rs:45`.

### Metrics, clipboard, and clocks

`subscribe_metrics` starts a connection-wide sampler. Interval is clamped to
1 through 3600 seconds. The first measurement establishes a CPU baseline, and
the first report arrives after an interval. CPU is percent, memory and disk
amounts are bytes. Disk mount names are guest-native. `unsubscribe_metrics`
parks the sampler. Metrics subscriptions survive hello in the current Rust agent,
even though channels do not. They are not channel resources.

`get_clipboard` returns text. With `clipboard_reply`, set returns `clipboard_set`
only after success, and either operation may return `clipboard_failed` with a
reason. Windows may also send unsolicited clipboard changes. A guest can advertise
clipboard while no user desktop currently exists. Without the reply feature,
set success has no acknowledgement. Clipboard operations have no request ID.

`get_time` returns signed UTC epoch milliseconds. `set_time` returns `time_set` or
`time_set_failed` with a guest reason. Full agents use the platform system clock.
Legacy only accepts nonnegative milliseconds with whole seconds fitting uint32
and discards fractional seconds. Its DOS adapter writes UTC to DOS date/time.
The host client cannot send a pre-1970 time, even though the wire type is signed.
Sources: `guest/agent/src/metrics.rs:41`, `guest/agent/src/clipboard.rs:27`,
`guest/agent-legacy/src/agent.c:247`, `src/labd/vm_agent.rs:852`.

### Apps

`open_app` flattens `mode` and, for X11, `cookie` into the open object. App session
data is `uint32 LE length | UTF-8 JSON`, capped at 1048576 bytes per record.
`ready` precedes connection announcements. Agent records are `ready`, `pending`,
`log`, and negotiated `diagnostic`. App `exited` and `failed` records are
daemon-to-CLI translations and must never come from an agent. The control
channel carries agent exit or error.

For waypipe, the agent listens privately and starts
`waypipe --socket <endpoint> server -- <argv>`. For X11, it allocates a display,
writes Xauthority with the host's 32-lowercase-hex-digit cookie, and starts argv
with `DISPLAY` and `XAUTHORITY`. stdout and stderr become lossy UTF-8 log lines.
When the agent advertises `app_log_split`, the host may set `split_logs = true`
on `open_app`. For waypipe, the agent redirects the program's stderr to its
original stdout through an argument-preserving shell wrapper. Waypipe's own
stderr stays on a separate pipe and becomes `diagnostic` records, hidden by
the CLI unless `--verbose` is set. The program's stdout and stderr remain
`log` records. No line-pattern filtering is used. Without the feature, the
host omits `split_logs` and the agent keeps the existing mixed output.
Each accepted connection is parked and announced by one `pending`. A host
`open_app_conn` adopts the oldest parked connection. Unadopted connections expire
after 30 seconds. With nothing parked, connection open fails.

For RDP, logon is required and argv is empty. The agent prepares RemoteApp policy
and waits up to 120 seconds for the guest listener before ready. Each connection
open dials guest loopback port 3389, with a ten-second dial budget. It cannot dial
an arbitrary guest or host destination. Each app connection has independent
credit and independent half-closes. Adopted connections can outlive the app
session. Sources: `guest/agent/src/app.rs:35`, `:483`, `:609`.

### Logins and logon lifetime

The wire has no logon-handle allocation vocabulary. `logon` is a self-contained
object on each exec, terminal, fileops, tail, or app open. It contains account,
secret, and optional Windows elevation. A host resolves login labels before
sending this object. Absent logon means agent identity, or the configured
container floor. Invalid credentials must fail by name, with no silent fallback.
Watch, metrics, OS information, and eventlog run as the agent identity.

The full agent caches minted identities per account and secret inside one agent.
Windows calls LogonUser, loads the profile, and selects a linked token for
elevation. Channels hold references so token and profile outlive all users.
Idle grace is ten minutes, ticket lifetime ten hours, sweep interval one minute.
Active channels retain an aged identity until they close. Unix resolves accounts
and uses `su -l` with PAM where available, otherwise setuid and an assembled
login environment. Unix does not authenticate the supplied secret. There is no
portable wire guarantee of a reusable OS logon ID. Elevation cache behavior has
a known disagreement, F8. Legacy and TempleOS refuse object-valued logons.
Sources: `guest/agent/src/logon.rs:24`, `guest/agent/src/unix/login.rs:13`,
`guest/agent/src/windows/logon.rs:143`.

### OS information, network information, and shutdown

`os_info` returns OS ID, name, version, kernel, architecture, and hostname.
`net_info` returns non-loopback interfaces with optional lowercase MAC and IP
addresses without prefix lengths or IPv6 zone IDs. Legacy and TempleOS return
an empty network list rather than synthesizing unavailable interfaces.

`shutdown` accepts `powerdown`, `reboot`, or `halt`. The agent sends
`shutting_down` with that mode before invoking the OS operation. Windows and
macOS map halt to powerdown. The connection may vanish after acknowledgement.
The Rust agent delays OS action 200 ms. Later OS failures can produce a
channel-less error. The host helper treats a missing acknowledgement as success
after a successful send; a conformance check must inspect the actual wire reply.
Sources: `guest/agent/src/mux.rs:536`, `src/labd/vm_agent.rs:897`.

## Implementation limits and findings

Legacy C has eight sessions, 256 JSON tokens, 64 argv entries, 64 environment
entries, a shared 16384-byte decoded-string pool, a 4096-byte control-output
buffer, and 4096-byte pump chunks. These limits can reject an otherwise valid v2
message. TempleOS is synchronous and has the output cap described above.

| ID | Disagreement and evidence | Status |
|---|---|---|
| F1 | Legacy README says exec only (`guest/agent-legacy/README.md:13`), but hello includes set_time (`guest/agent-legacy/src/agent.c:152`). | Source-confirmed documentation drift. |
| F2 | Rust emits an error then hello on version mismatch (`guest/agent/src/mux.rs:416`); C and HolyC ignore host version (`guest/agent-legacy/src/agent.c:468`, `guest/agent-templeos/VmlabAgt.HC:595`). | Host rejects a mismatched reply. |
| F3 | Legacy and TempleOS unknown app opens emitted an error without ID (`guest/agent-legacy/src/agent.c:518`, `guest/agent-templeos/VmlabAgt.HC:625-636`). The host cannot resolve an open waiter from a channel-less error (`src/labd/vm_agent.rs`, `handle_ctrl`). | Reproduced on C POSIX. C fixed in separate commit `a9b80fa`; TempleOS remains unchanged. |
| F4 | TempleOS consumes payload for an invalid kind (`guest/agent-templeos/VmlabAgt.HC:75-82`); Rust and C reject the candidate header before payload (`guest/agent-proto/src/lib.rs:299`, `guest/agent-legacy/src/wire.c:86`). | Different noise recovery behavior. |
| F5 | Protocol promises tail rotation survival (`guest/agent-proto/src/lib.rs:474`); Windows same_file always returns true (`guest/agent/src/tail.rs:113-128`). | Equal-size or larger replacement is not detected on Windows. Source-confirmed, not executed here. |
| F6 | File open mode is described as creation-only (`guest/agent-proto/src/fileops.rs:76`); Rust reapplies it to existing files when create or exclusive is requested (`guest/agent/src/fileops.rs:508-516`). | Preserve implementation behavior pending a contract decision. |
| F7 | Tree orphan retention promise (`guest/agent-proto/src/lib.rs:419`) exceeds container behavior (`guest/agent/src/unix/shepherd.rs:22-24`). | Container orphans go to namespace init and leave the counted tree. |
| F8 | Cache key omits elevation (`guest/agent/src/logon.rs:42-53`, `guest/agent/src/windows/logon.rs:115`), while mint applies it once (`guest/agent/src/windows/logon.rs:174`). | Same credentials can reuse the first elevation selection. Source-confirmed, Windows not executed here. |
| F9 | TempleOS comment says stdin is credited (`guest/agent-templeos/VmlabAgt.HC:476`); main loop discards data without credit (`:650-653`). | Input larger than initial window can stall. |
| F10 | Legacy accepts 64 argv values into an array of 64 pointers without a reserved NULL slot (`guest/agent-legacy/src/agent.c:301`, `:349-358`), then passes it to execvp (`guest/agent-legacy/src/plat_posix.c:187`). | Source concern; boundary not reproduced. |
| F11 | Exec and terminal reapers emitted EOF or exit after host close, contrary to `HostMsg::Close` (`guest/agent-proto/src/lib.rs`). | Resolved. The Rust mux cancels queued frames and suppresses worker replies for closed channels across exec, terminal, tail, watch, fileops, apps, and eventlog. Regression tests cover delayed exit and channel-ID reuse. Quiet cancellation is a required conformance pass. |
| F12 | Legacy refusal dispatch used `open_fileops` and `open_eventlog`, while serde sends `open_file_ops` and `open_event_log` (`guest/agent-proto/src/lib.rs:1168`, `:1250`; corrected dispatch in `guest/agent-legacy/src/agent.c:510-517`). TempleOS has the same spellings (`guest/agent-templeos/VmlabAgt.HC:627`, `:633`). | Reproduced C timeout. C fixed in `a9b80fa`, retaining old aliases. TempleOS not changed or executed. |
| F13 | C errors take signed `long` and omit negative IDs (`guest/agent-legacy/src/agent.c:93-101`), but the wire permits all nonzero uint32 IDs. Dispatch casts an unsigned channel ID to `long` (`:509-519`). | On 32-bit legacy targets, IDs above 2147483647 lose error correlation. Source concern; those targets were not executed. |

These findings are not silently normalized into requirements for every tier.
A new full agent should implement the shared semantics and explicitly choose
compatibility where a listed implementation disagrees. The C refusal dispatch
and Rust quiet close are fixed. Other findings remain unchanged.

## Run conformance

The runner drives the production `AgentHandle` over a Unix socket. Its pty mode
bridges a raw tty for binaries that open a serial device. Listen mode starts a
binary with a Unix listener. The default arguments are `--port {endpoint}` and
`--listen {endpoint}` respectively. `VMLAB_CONFORMANCE_ARGS` overrides them with
a JSON array of strings; `{endpoint}` is replaced in each argument.

Process fixtures require Linux user namespaces, bubblewrap, Python 3, and standard
POSIX utilities in the agent environment. The positive `app_waypipe` setup case
also requires `waypipe` on the agent's PATH. CI installs and checks that dependency.
The process sees a read-only host tree,
private PID/network/IPC/user namespaces, private `/run` and `/tmp`, no capabilities,
and one writable temporary fixture directory, also mounted at `/root` for the
agent's terminal home. This protects host power state,
clock, display, and existing files. The runner kills its process group on exit.
It never boots a VM.

```sh
CARGO_BUILD_JOBS=8 cargo build --manifest-path guest/agent/Cargo.toml
CARGO_BUILD_JOBS=8 just agent-conformance guest/agent/target/debug/vmlab-agent pty full
```

The C POSIX build and the same cases run with:

```sh
legacy_build_dir=$(mktemp -d)
cc -std=c99 -Wall -Wextra -Werror -O1 '-DAGENT_VERSION="agent-legacy=test"' \
  -o "$legacy_build_dir/agent" guest/agent-legacy/src/{json,wire,agent,plat_posix}.c
CARGO_BUILD_JOBS=8 just agent-conformance "$legacy_build_dir/agent" listen legacy
```

A future implementation needs no runner changes:

```sh
CARGO_BUILD_JOBS=8 just agent-conformance /absolute/path/opticlang-agent pty full
VMLAB_CONFORMANCE_ARGS='["--serial-device","{endpoint}"]' just agent-conformance /absolute/path/agent
just agent-conformance /absolute/path/agent listen legacy
```

For an already-listening Unix socket, the root argument must name an empty,
writable guest scratch directory. The runner sends exec commands and creates
and removes files there. The target must be disposable and POSIX-compatible.
The socket runner cannot establish target isolation, so it skips clock writes
and shutdown rather than risking a real machine.

```sh
just agent-socket-conformance /absolute/path/agent.sock /guest/scratch full
```

The tier argument is `full`, `legacy`, `templeos`, or `auto`. It checks the tier's
minimum advertisement and tests advertised capabilities. Current executable
fixtures use POSIX commands. DOS, Windows, macOS, and TempleOS runtime semantics
are documented here but are not certified by a Linux POSIX run. A non-POSIX
endpoint reports command failures; selecting its tier does not translate commands.

Each case prints `CONFORMANCE <name> PASS`, `FAIL <reason>`, or `SKIP <reason>`.
Every deviation fails, including frames emitted after close. SKIP is counted
separately from PASS. Any failure gives a nonzero exit.
`AGENT` records the build stamp, OS, and advertisement. Cases cover baseline
metadata, injected noise in both directions, false magic, exec fields and errors,
multiplexing, a stalled receive window, duplex megabyte transfers, cancellation,
fileops and pipelining, malformed nested framing, terminal input and resize,
tail truncation/rotation, watch prune/drain/tombstone, metrics unsubscribe,
clock refusal, logon refusal, app records, all three shutdown acknowledgement
modes, and same-stream re-handshake. The X11 case exchanges real socket bytes
and checks an independent half-close. The waypipe case checks setup and records;
it does not certify a rendered Wayland application.
Clipboard and Windows-specific tests depend on advertisement and target support.

The suite is not exhaustive. It does not certify UART timing, snapshot replay,
every filesystem error mapping, kernel watch overflow, RDP policy preparation,
valid Windows credentials, clipboard desktop integration, or non-Linux behavior.
The findings table distinguishes source analysis from runtime observations.

`just agent-proto-generate` refreshes the tables below. The ordinary agent-proto
test suite checks their exact bytes, including field types, defaults, and enum
spellings. `just ci::check` runs that check, the C conformance test, and the full
Linux process run. The daemon protocol generator remains unchanged.

## Observed host-process results

The Linux runs on 2026-11-01 used Rust agent `0.1.0` and the C POSIX build
`agent-legacy=test`. The C results include the separate refusal fix `a9b80fa`.
The full agent advertised no clipboard in the isolated, headless environment.

| Feature or check | Rust Linux, raw pty | C POSIX, listening socket |
|---|---|---|
| Handshake, baseline OS/network metadata, ping | PASS | PASS |
| Injected garbage and invalid headers | PASS | PASS |
| Exec fields, error paths, multiplexing | PASS | PASS |
| Megabyte duplex transfer and stalled-window control responsiveness | PASS | PASS |
| Cancellation, including quiet close | PASS, exec and terminal | PASS, exec |
| Terminal input, resize, exit | PASS | Not advertised; refusal checked |
| Fileops, pipelining, handle scope, flags, limits, errors, digest | PASS | Not advertised; refusal checked |
| Tail, truncation, rotation | PASS | Not advertised; refusal checked |
| Watch prune, dirty/drain, stat and tombstone | PASS | Not advertised; refusal checked |
| Metrics subscription and unsubscribe | PASS | Not advertised |
| Set time, denied in isolated namespace | PASS | PASS |
| Get time | PASS | Not advertised; host refusal checked |
| Process tree | PASS | Not advertised; host refusal checked |
| App waypipe setup and records | PASS | Not advertised; refusal checked |
| App X11 setup, connection bytes, half-close | PASS | Not advertised; refusal checked |
| App RDP | Not advertised; refusal checked | Not advertised; refusal checked |
| Eventlog | Not advertised; refusal checked | Not advertised; refusal checked |
| Clipboard and clipboard_reply | Not advertised; host clipboard refusal checked | Not advertised; host clipboard refusal checked |
| Invalid logon | PASS | PASS |
| Same-stream hello kills the old epoch; instance stable | PASS | PASS |
| Powerdown, reboot, halt acknowledgements | PASS | PASS |
| Runner totals | 21 PASS, 0 FAIL, 0 SKIP | 12 PASS, 0 FAIL, 0 SKIP |

The standalone already-listening socket recipe was also exercised against C
POSIX: 10 PASS, 2 SKIP, 0 FAIL. Its two skips are clock
writes and power operations because that runner cannot establish isolation.

`CARGO_BUILD_JOBS=8 just ci::check` passed. Later attempts intermittently hit
the unchanged `app_test::rdp_with_no_listener_fails_the_session` capture timeout
and `cli::broken_pipe::tests::a_pipe_without_a_reader_is_gone` assertion.
Both passed alone. The final complete gate passed with
`CARGO_BUILD_JOBS=8 RUST_TEST_THREADS=8 just ci::check`, limiting concurrency on
the shared host. No unrelated test fix or timing adjustment was made.

<!-- agent-proto tables begin -->

## Generated wire vocabulary

Generated from every serializable public type in `guest/agent-proto/src`
except the guest-internal `ContainerConfig`. `just agent-proto-generate`
refreshes this section. Integer widths describe JSON number ranges,
not binary encodings. Arrays of pairs encode environment entries.

### Protocol constants

| Constant | Value |
|---|---|
| Protocol version | `2` |
| Port name | `vmlab.agent.0` |
| Maximum outer payload bytes | `65536` |
| Initial channel credit bytes | `262144` |
| Replenishment threshold bytes | `131072` |
| Maximum fileops metadata bytes | `8388608` |
| Maximum fileops raw data bytes | `131072` |
| Directory entries per reply | `512` |
| Watch dirty-set capacity | `20000` |
| Maximum watch record bytes | `33554432` |
| Maximum app record bytes | `1048576` |
| Parked app connection grace seconds | `30` |
| Guest RDP port | `3389` |

### Feature strings

| Feature | Source meaning |
|---|---|
| `terminal` |  |
| `exec` |  |
| `fileops` | The handle-based, offset-addressed file RPC session ([`HostMsg::OpenFileOps`], [`crate::fileops`], PRD §19.5). It replaced the whole-file `file` vocabulary outright: an agent that predates it fails through this capability path, which is why `PROTO_VERSION` did not need to move. |
| `tail` |  |
| `metrics` |  |
| `clipboard` |  |
| `clipboard_reply` | The agent answers **every** clipboard request: `set_clipboard` with [`AgentMsg::ClipboardSet`] once the text is on the guest clipboard, `get_clipboard` with [`AgentMsg::Clipboard`], and either with [`AgentMsg::ClipboardFailed`] naming why it could not — a Windows guest nobody is logged on to has no clipboard to reach, though it still advertises [`CLIPBOARD`] because a logon can arrive at any time.  An agent without it sets silently and reports a failure as a channel-less [`AgentMsg::Error`] the host cannot attribute, so a host can only fire and forget a set and wait out a get. Being a feature rather than a version bump is what lets `PROTO_VERSION` stay put. |
| `eventlog` | Windows event-log tailing. |
| `watch` | The recursive guest tree watch backing the workspace syncer (§19.4, [`crate::watch`]). |
| `tree` | Process trees: [`HostMsg::OpenExec`] honours `tree`, and the agent answers [`HostMsg::TreeStatus`]. An agent without it ignores `tree` (unknown fields are dropped), so a host checks for this before relying on one. |
| `set_time` | The agent sets the guest's system clock: it answers [`HostMsg::SetTime`] with [`AgentMsg::TimeSet`] or [`AgentMsg::TimeSetFailed`]. The host uses it to bring a guest restored from an online snapshot back to the host's time (§7.3). |
| `get_time` | The agent reports the guest's system clock: it answers [`HostMsg::GetTime`] with [`AgentMsg::Time`]. The host reads it each time an agent handshakes, to correct a guest that booted with the wrong time (§7.3, #186). An agent without it is left unchecked. |
| `app_waypipe` | Shared apps over waypipe (PRD §20): [`HostMsg::OpenApp`] with [`crate::app::AppMode::Waypipe`]. Advertised by every Linux agent; whether the guest has a `waypipe` binary is the host's to probe. |
| `app_log_split` | Shared apps can separate waypipe's diagnostics from program output. Hosts opt in through `split_logs` on [`HostMsg::OpenApp`]. |
| `app_x11` | Shared apps over X11 (PRD §20): [`HostMsg::OpenApp`] with [`crate::app::AppMode::X11`]. Needs nothing installed in the guest. |
| `app_rdp` | Shared apps over RDP RemoteApp (PRD §20): [`HostMsg::OpenApp`] with [`crate::app::AppMode::Rdp`]. Advertised by the Windows agent; an edition with no RDP host refuses the open by name. |

### lib.Logon

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `user` | `string` | required | The guest account, e.g. `PROBE\dev`, `dev@probe.local` or `dev`. The host has already resolved a label to the account it names, so no vmlab label ever reaches the guest. |
| `secret` | `string` | required | The account's password. |
| `elevated` | `boolean` | false | Whether the session is elevated. Windows-only: it selects the account's linked token where it has one. `elevated` on a Linux-family profile is a §5.1 validation error, so a Linux guest never sees it set. |

### lib.HostMsg

#### `cmd: hello`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `proto_version` | `uint32` | required |  |
| `token` | `string` | required |  |

#### `cmd: open_terminal`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `cols` | `uint16` | required |  |
| `rows` | `uint16` | required |  |
| `command` | `array<string> or null` | null |  |
| `env` | `array<[string, string]>` | [] |  |
| `logon` | `Logon or null` | null |  |

#### `cmd: resize`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `cols` | `uint16` | required |  |
| `rows` | `uint16` | required |  |

#### `cmd: open_exec`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `argv` | `array<string>` | required |  |
| `env` | `array<[string, string]>` | [] |  |
| `cwd` | `string or null` | null |  |
| `logon` | `Logon or null` | null |  |
| `tree` | `string or null` | null |  |

#### `cmd: eof`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |

#### `cmd: open_file_ops`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `logon` | `Logon or null` | null |  |

#### `cmd: open_tail`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `path` | `string` | required |  |
| `logon` | `Logon or null` | null |  |

#### `cmd: open_watch`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `path` | `string` | required |  |
| `prune` | `array<string>` | [] |  |

#### `cmd: open_event_log`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `filter` | `string or null` | null |  |

#### `cmd: set_clipboard`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `text` | `string` | required |  |

#### `cmd: get_clipboard`

No additional fields.

#### `cmd: subscribe_metrics`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `interval_secs` | `uint64` | required |  |

#### `cmd: unsubscribe_metrics`

No additional fields.

#### `cmd: net_info`

No additional fields.

#### `cmd: os_info`

No additional fields.

#### `cmd: tree_status`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `tree` | `string` | required |  |

#### `cmd: shutdown`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `mode` | `ShutdownMode` | required |  |

#### `cmd: set_time`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `unix_ms` | `int64` | required |  |

#### `cmd: get_time`

No additional fields.

#### `cmd: open_app`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `mode` | `AppMode` | flattened into parent |  |
| `argv` | `array<string>` | [] |  |
| `env` | `array<[string, string]>` | [] |  |
| `cwd` | `string or null` | null |  |
| `logon` | `Logon or null` | null |  |
| `split_logs` | `boolean` | false | Request [`app::AppRecord::Diagnostic`] for waypipe's own stderr. Only sent when the agent advertises [`features::APP_LOG_SPLIT`]. |

#### `cmd: open_app_conn`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `app` | `uint32` | required |  |

#### `cmd: window_adjust`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `bytes` | `uint64` | required |  |

#### `cmd: close`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |

#### `cmd: ping`

No additional fields.

### lib.AgentMsg

#### `event: hello`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `proto_version` | `uint32` | required |  |
| `agent_version` | `string` | required |  |
| `os` | `string` | required |  |
| `features` | `array<string>` | required |  |
| `token` | `string` | required |  |
| `instance` | `string or null` | null | This agent process, picked fresh each time it starts and repeated in every [`AgentMsg::Pong`]. A host whose ping is answered under a different instance knows the agent it handshook has been replaced on the same channel (a service restart, a guest reboot QEMU's chardev outlived) and re-handshakes. Absent from older agents and the legacy tier, which a host cannot tell apart across a restart. |

#### `event: opened`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |

#### `event: exited`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `code` | `int32` | required |  |

#### `event: eof`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |

#### `event: metrics`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `cpu_pct` | `float32` | required |  |
| `mem_used` | `uint64` | required |  |
| `mem_total` | `uint64` | required |  |
| `disks` | `array<DiskUsage>` | required |  |

#### `event: clipboard`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `text` | `string` | required |  |

#### `event: clipboard_set`

No additional fields.

#### `event: clipboard_failed`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `msg` | `string` | required |  |

#### `event: net_info`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `interfaces` | `array<NetInterface>` | required |  |

#### `event: os_info`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `info` | `OsInfo` | required |  |

#### `event: tree_status`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `tree` | `string` | required |  |
| `alive` | `uint32` | required |  |

#### `event: shutting_down`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `mode` | `ShutdownMode` | required |  |

#### `event: time_set`

No additional fields.

#### `event: time_set_failed`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `msg` | `string` | required |  |

#### `event: time`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `unix_ms` | `int64` | required |  |

#### `event: window_adjust`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32` | required |  |
| `bytes` | `uint64` | required |  |

#### `event: error`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint32 or null` | null |  |
| `msg` | `string` | required |  |

#### `event: pong`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `instance` | `string or null` | null |  |

### lib.DiskUsage

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `mount` | `string` | required | Mount point (Unix) or drive root like `C:\` (Windows). |
| `used` | `uint64` | required |  |
| `total` | `uint64` | required |  |

### lib.ShutdownMode

#### `powerdown`

No additional fields.

#### `reboot`

No additional fields.

#### `halt`

No additional fields.

### lib.NetInterface

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `name` | `string` | required |  |
| `mac` | `string or null` | null | Lowercase colon-separated MAC, absent for interfaces without one. |
| `ipv4` | `array<string>` | [] | Dotted-quad IPv4 addresses (no prefix length). |
| `ipv6` | `array<string>` | [] | IPv6 addresses (no prefix length, no zone). |

### lib.OsInfo

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `string` | required | Machine-readable OS id: os-release `ID` on Linux, `"windows"` on Windows. |
| `name` | `string` | required | Human-readable OS name (os-release `PRETTY_NAME` / registry `ProductName`). |
| `version` | `string` | required | OS version string (os-release `VERSION_ID` / Windows display version). |
| `kernel` | `string` | required | Kernel release (`uname -r` / `major.minor.build`). |
| `arch` | `string` | required | Machine architecture (`uname -m` style: `x86_64`, `aarch64`, …). |
| `hostname` | `string` | required |  |

### fileops.Request

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint64` | required |  |
| `op` | `Op` | flattened into parent |  |

### fileops.Op

#### `op: open`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |
| `flags` | `OpenFlags` | all flags false |  |
| `mode` | `uint32 or null` | null |  |

#### `op: close`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `handle` | `uint64` | required |  |

#### `op: read`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `handle` | `uint64` | required |  |
| `offset` | `uint64` | required |  |
| `len` | `uint32` | required |  |

#### `op: write`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `handle` | `uint64` | required |  |
| `offset` | `uint64` | required |  |

#### `op: fstat`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `handle` | `uint64` | required |  |

#### `op: fsetstat`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `handle` | `uint64` | required |  |
| `attrs` | `SetAttrs` | required |  |

#### `op: stat`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |

#### `op: lstat`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |

#### `op: setstat`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |
| `attrs` | `SetAttrs` | required |  |

#### `op: open_dir`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |

#### `op: read_dir`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `handle` | `uint64` | required |  |

#### `op: mkdir`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |
| `mode` | `uint32 or null` | null |  |
| `case_sensitive` | `boolean` | false |  |

#### `op: rmdir`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |

#### `op: remove`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |

#### `op: rename`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `from` | `string` | required |  |
| `to` | `string` | required |  |

#### `op: realpath`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |

#### `op: symlink`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `target` | `string` | required |  |
| `link` | `string` | required |  |
| `kind` | `LinkKind` | required |  |

#### `op: readlink`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |

#### `op: digest`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |

### fileops.OpenFlags

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `read` | `boolean` | false |  |
| `write` | `boolean` | false |  |
| `create` | `boolean` | false | Create the file if it is absent. |
| `truncate` | `boolean` | false | Truncate an existing file to nothing. |
| `exclusive` | `boolean` | false | Fail if the file already exists (implies `create`). |
| `append` | `boolean` | false | Every write lands at the end, whatever offset it names. |

### fileops.LinkKind

#### `file`

No additional fields.

#### `dir`

No additional fields.

### fileops.Attrs

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `kind` | `EntryKind` | required |  |
| `size` | `uint64` | required |  |
| `mtime_ns` | `int64` | required | Modification time in nanoseconds since the Unix epoch. |
| `atime_ns` | `int64` | required | Access time in nanoseconds since the Unix epoch. |
| `mode` | `uint32 or null` | null | Unix permission bits. Absent where the guest has none to report. |

### fileops.SetAttrs

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `mode` | `uint32 or null` | null | Unix permission bits, ignored on Windows — the same rule the whole-file transfer's `mode` carried. |
| `size` | `uint64 or null` | null | Truncate or extend to this length. |
| `mtime_ns` | `int64 or null` | null |  |
| `atime_ns` | `int64 or null` | null |  |

### fileops.DirEntry

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `name` | `string` | required | The entry's own name, never a path. `.` and `..` are not reported. |
| `attrs` | `Attrs` | required | The entry itself, never followed through a symlink. |

### fileops.Response

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `id` | `uint64` | required |  |
| `reply` | `Reply` | flattened into parent |  |

### fileops.Reply

#### `reply: ok`

No additional fields.

#### `reply: handle`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `handle` | `uint64` | required |  |

#### `reply: data`

No additional fields.

#### `reply: attrs`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `attrs` | `Attrs` | required |  |

#### `reply: entries`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `entries` | `array<DirEntry>` | required |  |
| `eof` | `boolean` | required |  |

#### `reply: name`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required |  |

#### `reply: digest`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `sha256` | `string` | required |  |
| `len` | `uint64` | required |  |

#### `reply: error`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `code` | `ErrorCode` | required |  |
| `msg` | `string` | required |  |

### fileops.ErrorCode

#### `no_such_file`

No additional fields.

#### `permission_denied`

No additional fields.

#### `already_exists`

No additional fields.

#### `not_a_directory`

No additional fields.

#### `is_a_directory`

No additional fields.

#### `not_empty`

No additional fields.

#### `bad_handle`

No additional fields.

#### `unsupported`

No additional fields.

#### `failure`

No additional fields.

### watch.WatchRecord

#### `record: dirty`

No additional fields.

#### `record: drain`

No additional fields.

#### `record: batch`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `entries` | `array<StatRecord>` | required |  |

#### `record: rescan`

No additional fields.

### watch.StatRecord

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `path` | `string` | required | Relative to the watch root, `/`-separated on every guest OS. |
| `stat` | `Stat or null` | null |  |

### watch.Stat

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `kind` | `EntryKind` | required |  |
| `size` | `uint64` | required | Bytes for a file, the link target's length for a symlink, otherwise whatever the OS reports for the directory entry itself. |
| `mtime_ns` | `int64` | required | Modification time in nanoseconds since the Unix epoch (negative before it). |

### watch.EntryKind

#### `file`

No additional fields.

#### `dir`

No additional fields.

#### `symlink`

No additional fields.

#### `other`

No additional fields.

### app.AppMode

#### `mode: waypipe`

No additional fields.

#### `mode: x11`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `cookie` | `string` | required |  |

#### `mode: rdp`

No additional fields.

### app.AppRecord

#### `record: ready`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `display` | `string or null` | null |  |

#### `record: pending`

No additional fields.

#### `record: log`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `text` | `string` | required |  |

#### `record: diagnostic`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `text` | `string` | required |  |

#### `record: exited`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `code` | `int32` | required |  |

#### `record: failed`

| Field | JSON type | When absent | Meaning |
|---|---|---|---|
| `msg` | `string` | required |  |

<!-- agent-proto tables end -->
