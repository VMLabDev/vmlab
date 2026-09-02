# vmlab-agent-legacy

The in-guest agent for operating systems that cannot run `vmlab-agent`
(PRD §7.4): NT4 through XP/2003, Windows 95/98/ME, DOS, and Linux too old
for virtio-serial. It speaks the same wire protocol (`guest/agent-proto`,
version 2) over a 16550 UART on COM1 — the `agent_transport = "isa-serial"`
profile setting — and the lab daemon drives it through the same socket and
the same client, so readiness, `vmlab exec`, the graceful stop ladder and
`os_info` work unchanged.

It advertises one feature, `exec`. Every other open (terminal, fileops,
tunnel, tail, watch, eventlog) is refused by name on the channel that asked,
which is what the host's feature ladder (§19.4) needs to degrade truthfully:
`vmlab shell`, `vmlab cp` and `dev attach` say what is missing rather than
hang. A `logon` on an exec is refused too — nothing here mints one; every
command runs as the agent's own identity (§19.2's floor).

## Why C

Rust has no supported target for XP, 9x or DOS. The agent is C89 in one
source tree so a single core builds for every target:

| target key       | guests                | toolchain                         | binary                    |
|------------------|-----------------------|-----------------------------------|---------------------------|
| `windows-nt-x86` | NT4, 2000, XP, 2003   | mingw-w64 i686, static CRT        | `vmlab-agent-legacy.exe`  |
| `windows-9x-x86` | 95, 98, ME            | OpenWatcom v2, `win95` system     | `vmlab-agent-legacy.exe`  |
| `dos-i386`       | MS-DOS, FreeDOS       | OpenWatcom v2, DOS/32A bound in   | `VMLABAGT.EXE`            |
| `linux-x86`      | conformance; old Linux| host `cc`                         | `vmlab-agent-legacy`      |

`../build-agent-legacy.sh` builds them into `guest/dist/agent/<key>/` with
a `VERSION` stamp, skipping targets whose toolchain is absent. OpenWatcom is
found through `$WATCOM`, else `~/.local/opt/open-watcom-v2` (unpack the
project's `ow-snapshot.tar.xz` there).

## Layout

- `src/wire.[ch]` — the frame layer: the 13-byte header, and a decoder that
  resynchronises on the magic (a serial line can drop or replay bytes across
  an online snapshot restore).
- `src/json.[ch]` — a tokenizer and writer for the handful of control
  messages this agent answers. No allocation, no dependency.
- `src/agent.c` — the core: one polling loop that decodes frames, dispatches
  control messages, and pumps every live exec's pipes under the protocol's
  credit windows. No threads, so it runs under a DOS extender unchanged.
- `src/plat.h` — what the core needs from a platform: a pollable port, a
  child with pollable pipes and a non-blocking stdin queue, OS info, and a
  shutdown.
- `src/plat_win32.c` — NT and 9x from one source. ANSI APIs throughout;
  the SCM service entry points, `InitiateSystemShutdown` and
  `RegisterServiceProcess` are resolved at run time so the same code loads
  on either family. Child stdin is written from a helper thread, the one
  thing Win32 anonymous pipes cannot do without blocking.
- `src/plat_dos.c` — the UART polled directly; an exec runs synchronously
  through `COMMAND.COM` with stdout and stderr captured to files and
  streamed back afterwards; APM power-off through a DPMI real-mode call.
- `src/plat_posix.c` — `--listen <socket>` for the conformance tests
  (`src/labd/legacy_agent_tests.rs` compiles and drives it with the daemon's
  own client), and `--port /dev/ttyS0` for a Linux guest.

## What it does not do, on purpose

- **No file transfer.** QEMU times UART transmit to the baud rate, roughly
  11 KB/s at 115200; the host's feature ladder refuses `vmlab cp` by name.
- **No streaming on DOS.** DOS runs one program at a time: output arrives
  after the command exits, stdin is acknowledged and discarded, and the
  agent answers nothing else while the command runs — the host sees latency,
  not a lost agent. The agent is the foreground program; the guest is the
  agent while it runs.
- **No identity.** The NT build runs as an SCM service under SYSTEM; the 9x
  build is a RunServices process; DOS has no accounts at all.

## Running by hand

```
vmlab-agent-legacy.exe --console [--port COM1] [--log file]   (Windows)
VMLABAGT [--port COM1]                                         (DOS)
vmlab-agent-legacy --port /dev/ttyS0                           (Linux)
vmlab-agent-legacy --listen /path/agent.sock                   (host tests)
```
