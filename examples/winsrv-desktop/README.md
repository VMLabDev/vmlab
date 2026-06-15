# winsrv-desktop — watch a Windows Server 2025 desktop

The smallest useful lab: one Windows Server 2025 VM on a NAT'd segment,
with its display surfaced on the host so you can drive the desktop by hand.
It exercises vmlab's console access (PRD §11) — `gui = true` to auto-open
QEMU's window, or `vmlab console` to attach a viewer on demand.

Prerequisite — build the template first:

```sh
(cd ../templates/windows-server-2025 && ./fetch-deps.sh && vmlab template build)
```

Run it:

```sh
vmlab validate
vmlab up                # boots winsrv; with gui = true its window opens
vmlab status            # winsrv ready at 10.80.0.10
vmlab console winsrv    # or attach a VNC viewer yourself, any time
vmlab down              # clone retained; `vmlab destroy` deletes it
```

Guest credentials: `Administrator` / `vmlab123!`.

## Showing the UI

`gui = true` (set on the lab, inherited by every VM) makes `vmlab up`
open QEMU's own display window per guest, so the Server 2025 desktop
renders live on the host. Drop it to `false` on a VM to keep that one
headless while still reachable.

Even when headless, every VM always serves a VNC display on a unix
socket — `vmlab console winsrv` launches your configured viewer against
it. On WSL2 (where the viewer lives on the Windows side) it bridges the
socket to a localhost TCP port and prints the address; add `--tcp` to
force that path anywhere.
