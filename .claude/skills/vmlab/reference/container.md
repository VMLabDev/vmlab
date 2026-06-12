# Container image & WSL2 reference

## Build

The image is defined by `Containerfile` in the vmlab repo. vmlab depends on
sibling `WCL/` and `wisp/` workspaces via path deps, so the **build context
is the parent directory** containing all three:

```sh
docker build -t vmlab -f vmlab/Containerfile .   # run from the parent dir
# or, from inside vmlab/:
just image
```

Published per release as `ghcr.io/<owner>/vmlab:<version>`.

## Run

```sh
docker run --rm -it --device /dev/kvm \
  -v ~/.local/share/vmlab/templates:/root/.local/share/vmlab/templates \
  -v "$PWD":/lab -w /lab vmlab vmlab up
```

- `--device /dev/kvm` is the **only host grant needed**. Without it vmlab
  falls back to TCG emulation (slow but functional) with a loud warning.
- **No `--privileged`, no extra capabilities, no host network mode** — the
  network fabric is entirely userspace.
- Volumes: the template store (persistent across runs) and the lab
  directory (where `vmlab.wcl` and `.vmlab/` live). Everything else is
  container-ephemeral by design.

## Entrypoint model

`ENTRYPOINT ["vmlab"]`, `CMD ["daemon", "start"]` — by default the
container runs the supervisor in the foreground with lab daemons as
children. Two usage modes:

- **Long-running:** start the container with the default CMD, then drive
  the CLI via `docker exec <ctr> vmlab ...` (or a second container sharing
  the socket volume).
- **One-shot / CI:** override the command, e.g.
  `docker run ... vmlab vmlab up && vmlab run test.wisp` and exit.

## What's inside

QEMU system emulators (x86 + ARM), `qemu-utils`, OVMF + SeaBIOS firmware,
`swtpm`, `tesseract-ocr`, `passt` (NAT), `xorriso` (ISO builds), `mtools` +
`dosfstools` (floppy builds), `samba` (SMB shares), CA certs. This is also
the checklist of host tools needed when running vmlab *outside* the
container.

## WSL2 (PRD §13)

WSL2 is a first-class host — vmlab needs no tap/bridge/macvlan and no
`CAP_NET_ADMIN`, which is what makes this work:

- Enable **nested virtualisation** in `.wslconfig` (KVM needs it).
- Windows-side access to guests: declare port forwards
  (`forward {}` in WCL or `vmlab net forward`) — WSL's localhost
  forwarding bridges them to Windows.
- `vmlab console <vm> --tcp` bridges the VNC display to a localhost TCP
  port for a Windows-side viewer.
- `$XDG_RUNTIME_DIR` is created at daemon start if absent (some WSL setups
  lack it; falls back to `/tmp/vmlab-<uid>`).
- Watch disk: the ext4 VHDX grows as `.vmlab/` linked clones grow — the
  `host.disk_low` watchdog matters more here.

Source of truth: PRD §13–14; `Containerfile`; `README.md`.
