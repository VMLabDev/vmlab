# Handoff — interactive terminals for VMs & containers (vmlab-agent)

_Status as of 2026-07-14 (third session). Branch: `main`, **12 commits, not
pushed**, plus in-flight work this session (see "Third session progress")._

## Third session progress (2026-07-14)

- **Item 1 ran and caught three real bugs** (all fixed, uncommitted at the
  time of writing):
  1. *Layered builds seal mid-specialize.* The build "succeeded" (`agent:
     installed and answering`, `agent_version` in meta) but a smoke-test clone
     booted into "The computer restarted unexpectedly". The layered build seeds
     the source disk directly (`SeedDisk::CopyFrom`), bypassing clone
     instantiation, so the source's `first_boot` script (the WS2025
     sysprep-marker wait + settle reboot) never ran; QGA answers during
     specialize, so the agent hook ran and the build sealed a half-specialized
     image. Fix in `src/template/build.rs`: capture the layered source's
     `meta.first_boot_script` and inject it into the build VM's `TemplateParts`
     before `up()` — the up wave already runs `run_first_boot` before the
     pre-provision (agent) hook.
  2. *`wait_ready`/`is_ready` deadlock inside first-boot scripts.* The re-run
     then hit `run_first_boot`'s 1800 s ceiling: during a pending first boot the
     ready flag is deferred until the script returns, so the WS2025 script's
     own `vm.is_ready()` drop-check and final `vm.wait_ready(1800)` can never
     see it — **plain `vmlab up` of a fresh modern-Windows clone has been
     deadlocking the same way since the Jul 1 template rebuilds added the
     settle reboot** (`vmlab-templates` c7b5023). Fix: `VmHandle` now knows it
     targets the VM whose own first-boot script is running
     (`first_boot_gated`), and `is_ready`/`wait_ready` there mean a **live**
     QGA ping (`VmInstance::agent_answering`/`wait_agent_answering` — the
     sticky `agent_up` flag never drops across an in-guest reboot). No template
     rebuild needed; stored templates work as-is.
  3. *Failed builds leak the build VM.* `run_build` only cleaned the workdir on
     error; QEMU + swtpm kept running (found two orphaned 8 GiB build VMs
     fighting over the same `build-<name>` run-dir sockets). Fix: boot→seal is
     now one fallible step; any error runs `runtime.down` before propagating.
  - Side observation (not fixed): two concurrent builds of the same template
    name share `/run/user/1000/vmlab/labs/build-<name>/` socket paths — no
    lock prevents the collision, and a dying QEMU unlinks the other's socket
    files (QEMU removes socket paths blindly on exit).
  - **Item 1 PASSED end-to-end** (build 5, all fixes in): the layered build
    sealed only after the first-boot gate (`first-boot: build ready`), printed
    "agent: installed and answering", and the sealed meta carries
    `agent_version = "agent=2658b0e"`. A clone booted READY in ~2.5 min with:
    `vmlab exec` → `nt authority\system`, wscript `terminal()` send/expect into
    ConPTY PowerShell, `stats()` returning real cpu/mem, and a byte-identical
    5 MB `vmlab cp` round-trip. Throwaway template + smoke lab deleted; no
    stray processes.
- **All fixes committed and PUSHED** (`c49896a` scripting readiness,
  `bc3a9f1` template build fixes, `ffb335a` wskill docs — 15 commits total on
  origin/main). Item 7 done. Note: SSH-agent signing was broken in this
  session; pushed over HTTPS with the gh credential helper.
- Still open: item 2 (Windows clipboard with a logged-in console user — the
  session-1 win-e2e leftover lab is gone), item 3 (template rebuild wave,
  deferred to tonight — combine with the virtiofs flag flip), item 4 (users
  rebuild guest assets), item 5 (breaking-change release note when the next
  release trailer goes out).
- **Item 6 done (uncommitted):** `shell`/`cp`/`tail`/`eventlog`/`container shell`
  in the wskill CLI reference; `terminal()`/`stats()` + `Term` handle +
  agent-first transport in `fact/vm_agent.wcl`; container_api updated (also
  fixed stale "not snapshottable" claim); glossary `vmlab-agent` term; template
  build concept notes the automatic agent bake. `just docs-build` +
  `just skill-build` regenerated `docs/_site` + `.claude/skills/vmlab`.
- `just check` green after the build.rs fix.
- Template rebuild wave (item 3) deferred until tonight per Wil.
- Note: the handoff's build snippet needed fixing: `template build -f <file>`
  (not a positional path), and the store ref is `x86_64/windows-server-2025`
  (short name, not the ghcr path).

## What this is

A new first-party guest agent, **`vmlab-agent`**, that gives full VMs and OCI
container micro-VMs real interactive shells (Linux bash, **Windows PowerShell via
ConPTY**) plus streaming exec, file transfer, `tail -F`, metrics, clipboard and
Windows event-log — all over **one virtio-serial port `vmlab.agent.0`**, with **no
guest network involved**. Containers were unified onto the same agent (cinit's old
`vmlab.tty.0` PTY code is gone). The plan lives at
`~/.claude/plans/can-you-create-a-prancy-reef.md`.

## Commit map (oldest → newest on `main`)

| commit | what |
|---|---|
| `52dd03b` | `guest/agent-proto` wire contract + `vmlab.agent.0` VM channel in cmdline |
| `1ca0cdd` | `vmlab-agent` Linux half (PTY terminal, exec, files, tail, metrics) |
| `a09d0d8` | host `AgentHandle` (`labd/vm_agent.rs`) + `vm.tty_*`/exec/file/tail RPCs + CLI `shell`/`cp`/`tail` |
| `9df3b1b` | Windows half — vioserial open, ConPTY PowerShell, SCM service, eventlog, clipboard |
| `75e7097` | container unification onto vmlab-agent (cinit tty retired, ctl proto v6) |
| `bd08017` | template bake (`agent_install.rs`, `agent_asset.rs`, `build-agent.sh`, meta `agent_version`) |
| `1865ba9` | **(pre-existing user work)** web pull-progress on machine views — committed separately, not mixed in |
| `80fd908` | web VM terminal tab, guest-metrics meters, clipboard buttons |
| `89773d9` | wscript `terminal()` send/expect + `stats()` + agent-first exec/copy |
| `caeaaf8` | CI job for guest crates + cross targets; Containerfile ships the agent dist |
| `2658b0e` | **bug fix** — fully close the channel socket on shutdown/replace (see below) |
| `3eb14f4` | **new this session** — Windows agent installs to space-free `C:\ProgramData\vmlab`, unquoted `binPath=` tokens (the sc-create quoting fix) |

## Verification status

### Linux + containers — fully verified live (2026-07-14)
Drove a real air-gapped (no NIC) ubuntu VM built from a layered template:
- Template build **baked and verified the agent** ("agent: installed and answering").
- `vmlab shell` — MOTD, root bash, clean exit; `vmlab exec` as root; two concurrent shells.
- `vmlab cp` both directions, 30 MB, sha256 identical each way.
- `vmlab tail` followed a growing guest log.
- wscript `terminal()` send/expect (verified `stty size` = 120×32 default), `stats()`, container `terminal()` **inside the container PID namespace** (workload = PID 1).
- Online snapshot create/restore (incl. with a leaked `tail` session held open), guest reboot reconnect, degradation error for an agent-less template with QGA fallback still working.
- Web WebSocket `/tty` (prompt + command echo), `/stats`, `agent_version` in status.
- `just check` green (515 lib tests; still green after `3eb14f4`).

### Windows — partially verified live (2026-07-14, WS2025, air-gapped)
Cross-built `vmlab-agent.exe` with mingw, **hand-installed over QGA** (not via a real
template build), started the SCM service, then:
- **ConPTY PowerShell terminal: WORKS** — `whoami` = `nt authority\system`, `$PSVersionTable` = 5.1.26100, clean `exit`.
- **`vmlab cp` round-trip: WORKS** — 10 MB, sha256 identical (agent transport).
- **Event-log tail: WORKS** — wrote a System event, saw it stream.
- **`vm.stats`: WORKS** — real CPU/mem/disk (`C:\`).
- **Clipboard `set`: accepted; `get`: not verified** — needs an interactive console session (see open items).

## Open items / things to finish

### 1. Run the Windows template-bake verification build (highest priority — half done)
The sc-create quoting snag from the first session is **fixed in `3eb14f4`**: the agent
now installs to space-free `C:\ProgramData\vmlab\vmlab-agent.exe`, with `binPath=` and
the path passed as **separate unquoted argv tokens** through QGA guest-exec — the exact
form that was verified by hand (the quoted `binPath= "C:\Program Files\…"` form made sc
register a mangled path; `StartService FAILED 87`).

What remains is the e2e proof: **`src/template/agent_install.rs::install_windows` has
still never been run end-to-end.** A minimal layered verification template was prepared
(build was about to start when the session ended). Recreate it anywhere and run:

```sh
mkdir -p /tmp/agent-bake-test && cat > /tmp/agent-bake-test/vmlab.wcl <<'EOF'
import <vmlab.wcl>

template "agent-bake-test" {
  arch    = "x86_64"
  version = "0.0.1"
  profile = "windows-server"
  cpus    = 4
  memory  = 8GiB
  disk    = 60GiB

  source "template" { from = "x86_64/ghcr.io/vmlabdev/vmlab-templates/windows-server-2025" }
}
EOF
target/debug/vmlab template build /tmp/agent-bake-test/vmlab.wcl
```

Notes from scoping this: template builds run **in-process in the CLI** (no daemon), so
the freshly built `target/debug/vmlab` is all you need. Layered source = store ref
(`x86_64/<name>[@version]`; WS2025 is in the store at
`ghcr.io/vmlabdev/vmlab-templates/windows-server-2025`, latest `26100.1742.2`). A
provision-less template is fine — build.rs seals with its own graceful shutdown. The
agent hook waits up to 600 s for QGA, which must cover the sysprepped image's specialize
pass; if it times out, that's a real finding, not a flake. Success = the build log
prints **"agent: installed and answering"** and the sealed meta carries `agent_version`.
Delete the throwaway template from the store afterwards (`vmlab template rm`).

### 2. Clipboard on Windows is unverified on the happy path
`set` returns ok but the guest-side helper only reaches the clipboard from an **active
console session** — the WS2025 template has no logged-in user, so `get` times out (this is
the designed-in behaviour: no session → "no clipboard"). The self-spawn path
(`WTSQueryUserToken` + `CreateProcessAsUserW` into the console session, named-pipe bridge,
`AddClipboardFormatListener`) has **not been exercised with a real logged-in user**.
Also: after manually launching `--clipboard-helper` via QGA (which ran in session 0, not a
console), a subsequent `vm.clipboard_get` returned **"no vmlab-agent answered on the agent
channel"** — i.e. the agent channel stopped handshaking. **Investigate whether poking the
helper wedged the channel**, or whether that was just the ping-revalidation reconnect
racing. Reproduce with a logged-in RDP/console user before trusting clipboard.

### 3. Template rebuild wave
Every stored template predates the agent (`agent_version` absent → no terminal, QGA
fallback only). Rebuild them — good to combine with the pending Windows virtiofs-flag
rebuild noted in the `virtiofs-share-transport` memory — and push to ghcr.

### 4. Guest asset reinstall (containers)
cinit is now proto v6 and needs `vmlab-agent` in the initramfs. Anyone on an old asset
keeps QGA exec but has **no container shell** until they rebuild:
`just guest-install` (now builds both the boot asset and the agent binaries).

### 5. Breaking change to release-note
The new `virtserialport` changes the guest device tree, so **online internal snapshots
taken before this upgrade will not load** (VMs *and* containers). Offline snapshots are
fine.

### 6. Docs / wskill
`vmlab shell` / `cp` (now bidirectional) / `tail` / `eventlog` and the wscript `terminal()`
+ `stats()` API aren't in the wskill/docs yet.

### 7. Not pushed
All 12 commits are local on `main`. Push when ready (suggest: after item 1 passes).

## Environment leftovers

A supervisor and a `win-e2e` labd from the first session are still running out of
`target/debug/vmlab` (lab root under that session's now-dead scratchpad,
`…/6c9a2503-…/scratchpad/win-e2e-lab`). They predate `3eb14f4` and hold the
hand-installed-agent WS2025 VM — potentially handy for the clipboard item (#2), otherwise
`vmlab destroy` the lab and stop the stray supervisor before it confuses anything.

## The one bug verification already caught (fixed in `2658b0e`)

After an online snapshot restore the agent reconnect timed out forever. QEMU's
`server=on` chardev serves **one client at a time** and frees the slot only when *its*
read side sees EOF — but a session-held `AgentHandle` clone (a `vm.tail` labd loop whose
proto client had gone away) kept the socket's write half open while the reader task sat
blocked in `read()` holding the read half, so the fresh post-restore connect queued in the
listen backlog until its handshake timed out. Fix: `AgentHandle::shutdown()` now aborts the
reader **and** shuts the write half down explicitly; `Inner::drop` aborts the reader for the
plain last-drop case; every handle-replacement site (`teardown`, online restore, dead-ping
replacement) goes through `shutdown()`. Regression test:
`labd::vm_agent::tests::dropped_and_shutdown_handles_free_the_one_client_slot`.

## How to resume

```sh
# Build everything (agent binaries need these once):
rustup target add x86_64-unknown-linux-musl aarch64-unknown-linux-musl \
    riscv64gc-unknown-linux-musl x86_64-pc-windows-gnu     # mingw-w64-gcc already installed
./guest/build-asset.sh x86_64            # boot asset (cinit + agent in initramfs)
./guest/build-agent.sh                   # per-target agent binaries → guest/dist/agent/
just guest-install                       # installs both into ~/.local/share/vmlab/guest

cargo build                              # debug host binary at target/debug/vmlab
just check                               # clippy -D warnings + fmt + tests + web-ui tsc
```

Then item #1 above (the layered WS2025 verification build) is the next action.

Windows hand-install recipe from the first session, for reference (a real template build
should replace this once item #1 passes) — note it already used the ProgramData path that
`3eb14f4` now codifies:
```sh
# with an air-gapped WS2025 VM up and agent-ready (poll `vmlab exec win -- whoami`):
vmlab exec win -- cmd.exe /c "mkdir C:\ProgramData\vmlab"
vmlab cp guest/dist/agent/windows-x86_64/vmlab-agent.exe "win:C:/ProgramData/vmlab/vmlab-agent.exe"
vmlab exec win -- sc.exe create vmlab-agent binPath= C:\ProgramData\vmlab\vmlab-agent.exe start= auto
vmlab exec win -- sc.exe start vmlab-agent
vmlab shell win           # → SYSTEM PowerShell over ConPTY
```

## Key files

- Wire contract: `guest/agent-proto/src/lib.rs` (frames, credit windows, `HostMsg`/`AgentMsg`, `ContainerConfig`).
- Guest agent: `guest/agent/src/{mux,exec,files,tail,metrics}.rs` (portable), `linux.rs`, `windows/` (port, conpty, eventlog, metrics, clipboard, service).
- Host client: `src/labd/vm_agent.rs` (the one place the QEMU-one-client gotcha lives).
- VM/container wiring: `src/labd/vm.rs`, `src/labd/container.rs`, `src/qemu/cmdline.rs`, `src/qemu/container.rs`.
- RPCs: `src/labd/mod.rs` (`vm.tty_*`, `vm.stats`, `vm.clipboard_*`, `container.tty_*`, …).
- CLI: `src/cli/lab.rs` (`cmd_shell`, `cmd_cp`, `cmd_tail`, `cmd_eventlog`), `src/cli/tty_attach.rs`.
- Template bake: `src/template/agent_install.rs`, `src/agent_asset.rs`, `guest/build-agent.sh`, `src/template/build.rs` (pre_provision hook), meta `agent_version`.
- Web: `src/web/tty.rs` (VM + container), `src/web/api.rs` (`vm_stats`/`clipboard`/`container_stats`), `web-ui/src/components/{TerminalPanel,GuestStats,MachineView,ConsoleScreen}.tsx`.
- Scripting: `src/scripting/terminal.rs` + `terminal()`/`stats()` in `src/scripting/mod.rs`.

Memory: `~/.claude/projects/-home-wil-dev-vmlab/memory/vmlab-agent.md` (and the updated
`vmlab-containers.md`) carry the durable version of this.
