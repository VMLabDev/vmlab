# Debug feature

`vmlab debug <machine> <operation>` debugs a whole x86 KVM VM on vmlab-hv.
`<machine>` is `[lab/]machine`. The machine must already be running and report
`debug: true` in `vmlab machine capabilities <machine> --json`.
QEMU, emulated guests, and selecting an OS process by PID are unsupported.
No guest agent is needed.

The command inventory is `src/cli/debug.rs`. Guest operations pass through
`machine.debug` and the optional hypervisor debugger capability.

## Commands

| Operation | Arguments | Behavior |
| --- | --- | --- |
| `status` | None | Run state, stop reason, vCPUs, and breakpoints |
| `pause` | None | Stops all vCPUs and waits for pending exits to complete |
| `continue` | None | Resumes every vCPU and retains breakpoints |
| `step` | None | Executes one instruction on `--vcpu` and waits up to five seconds for a stop. A timeout cancels the step and leaves the VM paused |
| `detach` | `--leave-paused` optional | Clears debugger settings. Resumes unless `--leave-paused` is present. Requires a paused VM |
| `read` | `<address> <length>` | Reads memory as hex bytes |
| `write` | `<address> <hex-bytes>` | Writes RAM |
| `search` | `<address> <length> <hex-pattern>` | Lists matching addresses, including overlapping matches |
| `dump` | `<address> <length> <path>` | Writes raw bytes to a new file. Preserves existing files and publishes no partial dump on failure |
| `registers` | `[name=value ...]` | Inspects registers or edits named registers before inspecting them |
| `break` | `<address> [--kind execute\|write\|read-write] [--length N]` | Installs a linear-address hardware breakpoint on every vCPU. Defaults are `execute` and length 1 |
| `unbreak` | Same arguments as `break` | Removes the matching breakpoint |
| `gdb` | `[--listen 127.0.0.1:1234]` | Serves one GDB or LLDB connection. Only loopback addresses are accepted. Port 0 selects a free port |

Global options are `--json`, `--vcpu <id>` with default 0, and `--virtual`.
Memory addresses default to physical addresses. `--virtual` uses linear
addresses in the selected vCPU's current page-table context. Addresses and
register values accept decimal or a `0x` hexadecimal prefix. Byte sequences
are contiguous hex, such as `0012ff`. Lengths are decimal byte counts.

Memory, register, breakpoint, and detach operations require a paused VM.
They do not pause it implicitly. A step stops only the selected vCPU's instruction
execution while other vCPUs remain parked. A watchpoint encountered during the
step is reported as a breakpoint stop.

The default register view shows general registers, selectors, and system
registers. `--json` also includes x87 and XMM state. GDB or LLDB can edit that
extended state. Protected-mode segment-selector edits, arbitrary MSRs, and
AVX upper registers are unsupported.

## Limits

Reads and writes carry at most 65,536 bytes. Search ranges are at most 16 MiB,
patterns at most 65,536 bytes, and results at most 1,024 matches.
`--json` exposes the search's `truncated` flag. Dumps stream in bounded chunks.

Reads cover guest RAM and firmware ROM. Writes cover mapped RAM and reject
ROM, legacy apertures, MMIO, and holes. A rejected range changes no bytes.
Virtual writes can modify read-only guest pages for debugging.
Pausing stops vCPUs, not external device workers or DMA.

There are four shared hardware slots. Execution breakpoints require length 1.
Data watchpoints require aligned lengths of 1, 2, 4, or 8 bytes. `read-write`
detects either access. A read-only watchpoint is unavailable on x86.
Watchpoints detect CPU accesses, not device DMA or host memory edits.
Addresses are not filtered by process or CR3.

## Example invocation

With a running `lab/vm`, these commands stop it, inspect memory, install a
write watchpoint, and resume it. The example addresses are illustrative and
must name RAM used by the software being debugged.

```sh
vmlab debug lab/vm pause
vmlab debug lab/vm read 0x9000 16
vmlab debug lab/vm registers rax=0x1234
vmlab debug lab/vm break 0x9000 --kind write --length 2
vmlab debug lab/vm continue
vmlab debug lab/vm status --json
```

## Debugger attachment

```sh
vmlab debug lab/vm gdb --listen 127.0.0.1:1234
gdb -ex 'target remote 127.0.0.1:1234'
```

The listener reports its bound address on stderr. Connecting pauses the VM.
The listener has no authentication. Any local process able to connect can control
the VM while it is listening.
GDB uses `hbreak *<address>`, `watch`, `awatch`, `stepi`, and `detach`.
LLDB connects with `gdb-remote 127.0.0.1:1234` and uses its normal register,
memory, breakpoint, and instruction-step commands. Each vCPU is a debugger thread.
The PC is linear in segmented modes.
The register description stays x86-64 with SSE. Disassembly mode does not
automatically follow 16-bit or 32-bit guest code.

Ordinary debugger breakpoints also consume hardware slots. Guest instructions
are not patched. Clean detach resumes the VM after clearing debugger settings.
A lost connection, a debugger kill request, or Ctrl-C in the bridge clears
debugger settings and leaves the VM paused. Ctrl-C before attachment closes
the listener without changing the VM. Guest memory and register edits remain.
Use one controlling debugger session per VM.

Normal command results go to stdout. `--json` prints the typed response.
Errors go to stderr with the existing CLI exit-code mapping. Invalid arguments
use exit 2, invalid run state uses exit 5, and unavailable capability uses exit 6.
The GDB listener's data stream is Remote Serial Protocol rather than JSON.

## Verification

`tests/cli_debug.rs` runs the actual CLI through a typed lab-socket relay to
the real hypervisor when `VMLAB_HV_DEBUG_TEST_BINARY` names a built `vmlab-hv`.
It tests memory reads and writes, search, dumps, watchpoint stops, and GDB
attachment. Without that variable it prints a named skip reason.
The hypervisor's S1 debugger tests additionally exercise page translation,
SMP stepping, interruption, disconnect cleanup, and GDB or LLDB clients.
The LLDB check skips with its startup error if the host's LLDB cannot run.
