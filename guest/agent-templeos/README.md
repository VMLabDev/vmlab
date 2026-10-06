# vmlab-agent for TempleOS

`VmlabAgt.HC` is the vmlab guest agent for TempleOS (PRD §7.4, the legacy
tier): the same wire as `guest/agent-proto`, version 2, over the 16550 at COM1
that the `templeos` profile (`agent_transport = "isa-serial"`) wires to the
host socket. It is HolyC, compiled by TempleOS itself; there is no build, only
a stamp (`build-agent-legacy.sh templeos` writes `VA_VERSION` and copies the
source to `guest/dist/agent/templeos/`).

## Status

Verified live on a `templeos` clone: the agent compiles in the guest,
installs, starts again after a reboot, answers the handshake over COM1,
reports `exec` as its only feature, and returns a command's output. `vmlab
exec temple -- 'Dir;'` prints the listing; a compile error exits 1 with the
compiler's report. `vmlab shell` and `vmlab cp` refuse by name.

## What it does

One feature, `exec`. A command is HolyC source: the argv joined by spaces,
compiled and run by `ExePrint`, and what it prints becomes the channel's
stdout. TempleOS offers no per-task redirection, so the agent adds a hook to
the kernel's StdOut key-device chain (`KeyDevAdd`) ahead of the DolDoc one:
while a command runs, the hook claims everything the agent's own task prints
and lets every other task's output through. DolDoc markup in the captured
text is reduced to the text the screen would show. An exception, including a
compile error, is reported as exit code 1 with the OS's own message in the
output. `os_info`, `net_info` (empty; TempleOS has no
network by design) and `shutdown` are answered — power-off is a write to the
PIIX4 sleep register, reboot is `Reboot` — and every other open is refused by
name, so `vmlab shell` and `vmlab cp` say what is missing.

```
vmlab exec temple -- '"hello %d\n",42;'
vmlab exec temple -- 'Dir("~");'
```

Ring 0, polled UART, no interrupts: one task spawned at boot. A command runs
in that task, so nothing else is answered until it returns.

## Getting it into the guest

TempleOS reads no ISO 9660 (its install CD is a RedSea image in an ISO
wrapper) and has no network, so the bootstrap ISO cannot carry it in. The way
in is the screen: `vmlab::templeos_agent_script()` returns the source as
`A("…")` statements plus the `FileWrite`, the `#include` and
`VmlabAgentInstall`, and a template's provision types it at the shell:

```wscript
vm.type_text_paced(vmlab::templeos_agent_script(), 40)
```

Roughly twelve thousand keystrokes; at 40 ms each, about eight minutes, once
per template build. `VmlabAgentInstall` appends the include and the spawn to
`~/MakeHome.HC.Z`, which `StartOS.HC` includes last at every boot, and starts
the agent immediately, so the build verifies the handshake without a reboot.

Input must be the QMP transport (the profile's default). Over VNC, TempleOS
sees every shift a keystroke late, so shifted characters land on the wrong
key.

The code lines carry no `$`: typed at the DolDoc shell, a dollar opens a
command instead of landing as text, so the source writes it as `0x24`. Comment
lines are not typed and may use it. `agent_asset.rs` has a test for this.
