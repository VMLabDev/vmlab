# vmlab-agent for TempleOS

`VmlabAgt.HC` is the vmlab guest agent for TempleOS (PRD §7.4, the legacy
tier): the same wire as `guest/agent-proto`, version 2, over the 16550 at COM1
that the `templeos` profile (`agent_transport = "isa-serial"`) wires to the
host socket. It is HolyC, compiled by TempleOS itself; there is no build, only
a stamp (`build-agent-legacy.sh templeos` writes `VA_VERSION` and copies the
source to `guest/dist/agent/templeos/`).

## Status

**Unfinished.** Verified live on the sealed `templeos` template: the agent
compiles in the guest, installs, registers itself for every boot, answers the
handshake over COM1, reports `exec` as its only feature, and the host's ladder
degrades correctly (`vmlab machine capabilities` shows `agent exec` and
`attachable no`; `vmlab shell` and `vmlab cp` refuse by name).

**Capturing a command's output does not work yet.** TempleOS has no
redirection hook — no assignable `put_s`, no `user_put_s` that fires, and
`Doc2PlainText` converts one entry rather than a document — so output has to
be read back from the document a task prints into. That works in a task with a
window (verified in the shell: swap both `Fs->put_doc` and `Fs->display_doc`
to a `DocNew`, run, walk the entries), and yields nothing in the agent's own
spawned task, which appears to have no such document. Until that is settled an
exec returns exit 0 with empty output, so the shipped TempleOS template keeps
`agent = false`.

## What it does

One feature, `exec`. A command is HolyC source: the argv joined by spaces,
compiled and run by `ExePrint` with the task's output document swapped for a
capture document, whose text becomes the channel's stdout. An exception,
including a compile error, is caught and reported as exit code 1 with the OS's
own message in the output. `os_info`, `net_info` (empty; TempleOS has no
network by design) and `shutdown` are answered — power-off is a write to the
PIIX4 sleep register, reboot is `Reboot` — and every other open is refused by
name, so `vmlab shell`, `vmlab cp` and `dev attach` say what is missing.

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
`~/MakeHome.HC`, which `StartOS.HC` includes last at every boot, and starts
the agent immediately, so the build verifies the handshake without a reboot.

Input must be the QMP transport (the profile's default). Over VNC, TempleOS
sees every shift a keystroke late, so shifted characters land on the wrong
key.
