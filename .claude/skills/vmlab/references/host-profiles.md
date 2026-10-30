# Host configuration, guest OS profiles, and `vmlab osinfo`

Per-host settings live in one optional host configuration file; per-guest-OS
hardware defaults live in guest OS profiles. Everything else is in the lab file
(see lab-file.md).

## The host config file

vmlab reads `config.wcl` from its XDG config directory: `~/.config/vmlab/config.wcl`,
honouring `XDG_CONFIG_HOME`.

- The file is optional. Absent file means every default applies.
- Every field is an override; an absent field leaves the default in place.
- It must start with `import <vmlab-host.wcl>`, and carries one `host { … }` block
  and zero or more `gpu "<name>" { … }` host GPUs.
  A file without the import is rejected with an error naming the missing line.
- A malformed value is reported at its line, with the same wording a lab file gets.
  Every mistake in the file is reported in one pass.
- Errors surface where a lab is loaded, so a broken file fails `vmlab up` with its
  lines named. The daemon processes fall back to defaults for a file they cannot read.
- The daemons read the file when they start, so a change takes effect on the next
  `vmlab up` after the supervisor restarts.

```wcl
# ~/.config/vmlab/config.wcl
import <vmlab-host.wcl>

host {
  subnet_pool        = "10.99.0.0/16"
  dns_suffix         = "lab.local"
  disk_low_percent   = 5
  viewer             = "remote-viewer vnc://{}"
  oci_chunk_size     = 128MiB
  workspace_max_file = 2GiB
}
```

```wcl
# ~/.config/vmlab/config.wcl
import <vmlab-host.wcl>

host {
  subnet_pool = "10.99.0.0/16"
  dns_suffix  = "lab.local"
  psk         = "shared-secret"
  viewer      = "vncviewer {}"
}
```

### `host {}`

The block of host-wide settings; the file carries at most one.

```wcl
host {
  subnet_pool          = "10.213.0.0/16"
  dns_suffix           = "vmlab.internal"
  dns_upstream         = "1.1.1.1:53"
  disk_low_percent     = 10
  psk                  = "…"
  trunk_port           = 13947
  viewer               = "vncviewer {}"
  ffmpeg               = "/opt/ffmpeg/bin/ffmpeg"
  fastpath             = "auto"
  oci_chunk_size       = 512MiB
  config_weave_bin_dir = "~/.local/share/config-weave/bin"
  workspace_max_file   = 256MiB
  firmware_dir         = "/opt/vmlab-firmware"
  vmlab_hv             = "/opt/vmlab-hv/bin/vmlab-hv"
  hypervisor { container = "qemu" vm = "qemu" }
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `subnet_pool` | utf8 | `10.213.0.0/16` | CIDR the automatic /24 segment subnets are carved from; segments with no `subnet` are auto-allocated one /24 each (see networking.md). |
| `dns_suffix` | utf8 | `vmlab.internal` | Suffix for auto-registered machine names in segment DNS, `<vm>.<lab>.<suffix>`. |
| `dns_upstream` | utf8 | host resolver | Upstream resolver as `ip[:port]` for queries the lab DNS cannot answer. |
| `disk_low_percent` | i64 | `10` | Free-space percentage, 0 to 100, below which the `host.disk_low` watchdog fires (see automation.md). |
| `psk` | utf8 | none | Pre-shared key authenticating cross-host segment trunks. Set the same value on both hosts. |
| `trunk_port` | i64 | `13947` | TCP port the supervisor listens on for inbound cross-host segment trunks, 1 to 65535. |
| `viewer` | utf8 | none (auto-detected) | VNC viewer command; `{}` is replaced by the target. |
| `waypipe` | utf8 | `waypipe` on `PATH` | Path to the host's waypipe client for shared apps from Linux guests (cli-machine.md, `vmlab app`). Read by the CLI itself, so no restart; not checked until `app run` starts it. |
| `freerdp` | utf8 | `xfreerdp3`, then `xfreerdp`, on `PATH` | Path to the FreeRDP 3 client for shared apps from Windows guests (cli-machine.md, `vmlab app`). Read by the CLI itself, so no restart; not checked until `app run` starts it. |
| `ffmpeg` | utf8 | `ffmpeg` on `PATH` | Path to the ffmpeg that encodes console recordings (cli-machine.md, `vmlab record`); needs `libx264` for `.mp4`, `libvpx-vp9` for `.webm`. Read by the lab daemon when a recording starts and checked then (`-encoders`); never looked for at `up` without `--record`. |
| `fastpath` | utf8 | `auto` | Network fast path: `auto` probes, `off` forces userspace, `sockmap` and `afxdp` force a kernel tier. |
| `oci_chunk_size` | ByteSize | `512MiB` | OCI layer chunk size for `vmlab template push` (see templates.md). |
| `config_weave_bin_dir` | utf8 | `~/.local/share/config-weave/bin` | Directory holding the config-weave guest binaries playbooks push (see automation.md). |
| `workspace_max_file` | ByteSize | `256MiB` | Workspace syncer per-file size guard. A larger file is refused by name (see dev-machines.md). |
| `vmlab_hv` | utf8 | `vmlab-hv` on PATH | The vmlab-hv binary machines on `hypervisor = "vmlab"` run (PRD §22). |
| `hypervisor {}` | child | `qemu` for both | Default hypervisor per machine kind, `container` and `vm`, each `qemu` or `vmlab`; a machine's own `hypervisor` wins. |
| `firmware_dir` | utf8 | none | UEFI firmware override, `<dir>/<arch>/` with the bundled set's file names; searched before vmlab's bundled firmware and the host's (see "UEFI firmware" below). |

Parser rules, all violations reported in one pass:

- `subnet_pool` is a well-formed CIDR.
- `disk_low_percent` is between 0 and 100.
- `trunk_port` is between 1 and 65535.
- `fastpath` is one of `auto`, `off`, `sockmap`, `afxdp`. The `VMLAB_FASTPATH`
  environment variable selects the same values.
- `oci_chunk_size` and `workspace_max_file` are non-negative sizes.
- A field the schema does not name is rejected with its position; the removed
  `gpu_address` is rejected with a message pointing at `gpu "<name>" {}`.

`config_weave_bin_dir` is a location knob rather than a switch: it is the first
rung of a three-rung lookup, ahead of the `VMLAB_CONFIG_WEAVE_DIR` environment
variable and the XDG default.

`workspace_max_file` is host config rather than a `@dev` argument because the cap
is about this developer's link to the guest, not the lab everyone shares; the
refusal message names the field.

### `hypervisor {}`

Child of `host {}`, at most one: the hypervisor a machine runs on when its own
`vm {}` or `container {}` names none, one default per machine kind (PRD §22,
ADR-0021). Keyed by kind because vmlab-hv takes machines over one phase at a
time; a phase that is ready flips the default for its own machines only.

```wcl
host {
  vmlab_hv = "/opt/vmlab-hv/bin/vmlab-hv"
  hypervisor {
    container = "vmlab"
    vm        = "qemu"
  }
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `container` | utf8 | `qemu` | Hypervisor for every `container {}` without `hypervisor`: `qemu` or `vmlab`. |
| `vm` | utf8 | `qemu` | Hypervisor for every `vm {}` without `hypervisor`: `qemu` or `vmlab`. vmlab-hv runs no VMs yet, so `vmlab` here fails validation for every VM that names none. |

- Any value other than `qemu` or `vmlab` is a parse error naming it.
- A machine's own `hypervisor` wins. The field never resolves through a
  template or a profile.
- A default is checked like a declared value. `vmlab validate` (and the lab
  daemon again before it builds the machine) refuses what vmlab-hv cannot run
  yet, naming the machine and the reason: any VM, `qemu_args`, a non-x86_64
  arch, `accel = "tcg"`, a container where `/dev/kvm` is not usable.
- Changing a default moves every machine it reaches on its next start; an
  online snapshot taken under the old hypervisor then refuses to restore
  (snapshots-vision.md), an offline one restores under either.

### QEMU or vmlab-hv

QEMU runs every machine unless one says otherwise. vmlab-hv is vmlab's own
hypervisor (one process per machine, driven over its own control protocol). It
runs only container micro-VMs today, x86_64 under KVM, with no TCG fallback.
Pick it for a container to try it on one machine inside an otherwise-QEMU lab.
Keep QEMU for VMs, for `qemu_args`, and for any container that must also run
on a host without KVM. What differs on a vmlab-hv machine:

- `up` checks for the binary (`vmlab_hv`, else `vmlab-hv` on `PATH`) before it
  starts anything, as it does `qemu-system-<arch>`.
- Its log is `vmlab-hv.log` beside the console log, where QEMU's is `qemu.log`.
- `vmlab status -v` shows `hypervisor=vmlab` (every machine shows its value).
- Online snapshots are in vmlab-hv's own format and restore only under
  vmlab-hv. `qemu-img` is still needed for the scratch disk and offline
  snapshots.

### gpu "<name>" {}

A **host GPU**, declared beside `host {}`, zero or more of them. A lab's
`gpu {}` names one with `host_gpu` or takes the default (vm.md, `gpu {}`).

```wcl
gpu "AMD1"    { address = "0000:72:00.0" }
gpu "Nvidia1" {
  address     = "0000:01:00.0"
  default     = true
  qemu_binary = "/opt/qemu-vmlab/bin/qemu-system-x86_64"  // patched egl-headless
  qemu_env    = { VKR_DEBUG: "validate" }                 // record literal: `:` and commas
}
gpu "Nvidia2" { address = "0000:05:00.0"  vulkan_icd = "/usr/share/vulkan/icd.d/nvidia_icd.json" }
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| label | utf8 | required | The name; matched case-insensitively against a lab's `host_gpu`. |
| `address` | utf8 | required | Host PCI address (`lspci -D`, `/sys/bus/pci/devices/`). |
| `vulkan_icd` | utf8 | none | Vulkan ICD JSON a `vulkan` guest's renderer loads (`VK_DRIVER_FILES`), overriding the driver-derived filter. |
| `default` | bool | `false` | The GPU a lab gets when it names none. At most one; a lone block is the default without it. |
| `qemu_binary` | utf8 | none | Absolute path to the QEMU every VM on this GPU runs (all modes, passthrough too) instead of `qemu-system-<arch>` on PATH. |
| `qemu_env` | record of utf8 | none | `{ KEY: "value" }` set in that QEMU's environment, applied last: beats the derived `VK_LOADER_DRIVERS_SELECT`/`VK_DRIVER_FILES` and the PCI-filter variables (`VMLAB_VK_PCI_FILTER_DISABLE: "1"` turns the layer off). |
| `keepawake` | utf8 | `auto` | Supervisor holds the GPU out of runtime suspend from the first virgl/vulkan VM on it until `vmlab daemon stop`. `auto`: an NVIDIA GPU that can runtime-suspend; `on`: always (any EGL driver); `off`: never. |

Parsing rejects a block with no label or no `address`, two labels differing
only in case, a second `default = true`, a `qemu_binary` that is not an absolute
path, a `qemu_env` value that is not a string (each key is a WCL identifier, so
never empty and never containing `=`), and a `keepawake` other than `auto`, `on`
or `off`. The rest is checked when a lab uses the GPU (validate
and VM start), naming it: the address must be a PCI device, with a render node
for virgl/vulkan, `vulkan_icd` must exist, and `qemu_binary` must be an
executable file — one named `qemu-system-<a>` for the VM's QEMU arch (another
arch is refused naming both; any other file name is taken as given). Read at
each VM start.

With `qemu_binary` set, the VM also gets `-L <stock data dir>` —
`<prefix>/share/qemu` of the `qemu-system-<arch>` on PATH, only if it exists —
so a minimal build finds firmware and ROMs; QEMU still searches its own data
dir after it. `qemu.log`'s `vmlab: env:` line lists every variable vmlab set,
`qemu_env` included; `vmlab status -v` shows `qemu=<path>` after `host_gpu=`,
then the same variables as `env=NAME=value,...` by name (values in full), and
`status --json` carries them as the VM's `qemu_env` object — both as recorded
at the start, not the host config as it is now.
No host-wide default, no CLI override; template builds (no `gpu {}`) always run
the stock QEMU. Use: NVIDIA's EGL takes a LINEAR dma-buf only as
`GL_TEXTURE_EXTERNAL_OES`, so stock QEMU 11.1's egl-headless shows a black
console for a Venus guest; the patched QEMU
(`VMLabDev/qemu`, branch `vmlab/egl-headless-external-dmabuf`) fixes it.

Stale bands: a Windows Venus guest's VNC console and `vmlab vm screenshot` can
show horizontal bands of an earlier frame across a GPU-drawn window that was
changing, until the guest presents again (more often on a busy or slow host
GPU). The bands are in the frame the guest's display driver flips to the
scanout, not a QEMU readback race: the scanout does not change after the flush,
and AMD on stock QEMU shows them too. RDP and an in-guest capture compose their
own frame and are clean. Not fixed (VMLabDev/vmlab#170); for an exact view, RDP
through a segment `forward { host_port = 13389 to = "<vm>:3389" }`.

Identical cards: two RTX 5090s both match the `*nvidia*` driver filter, the
loader cannot filter by PCI address, and Mesa's device-select layer breaks
Venus. So every `vulkan` VM on a picked GPU gets vmlab's own implicit layer,
`VK_LAYER_VMLAB_pci_filter` (C, built into the `vmlab` binary, written to
`~/.local/share/vmlab/vk-layer/<digest>/` at VM start). It answers
`vkEnumeratePhysicalDevices`/`vkEnumeratePhysicalDeviceGroups` with only the
device whose `VK_EXT_pci_bus_info` address is the GPU's `address`, so the guest
sees exactly one Venus device, the card `egl-headless` scans out from. Always
on, identical cards or not. `qemu.log` shows `VK_ADD_IMPLICIT_LAYER_PATH` and
`VMLAB_VK_PCI_ADDRESS` on the `vmlab: env:` line, then the layer's own
`vmlab-pci-filter: 0000:05:00.0: kept NVIDIA GeForce RTX 5090, hid 1`. No such
line after the guest used Vulkan means the host loader is too old to honour
`VK_ADD_IMPLICIT_LAYER_PATH`; `qemu_env = { VMLAB_VK_PCI_FILTER_DISABLE: "1" }`
turns the layer off.

Runtime power management (ADR-0018). With `power/control=auto` the driver
powers an idle GPU down, and between `vmlab down` and the next `up` nothing uses
it; on NVIDIA the resume reboots the GSP, and a failed one (Xid 119, often then
Xid 79) loses the GPU until the host reboots — QEMU then aborts in
`libnvidia-eglcore` some 40 s after the VM was reported up. vmlab:
- refuses a GPU whose `power/runtime_status` is `error` (validate and VM start:
  ``host GPU `Nvidia1`: gpu address 0000:01:00.0: the device is in a failed
  power state (power/runtime_status=error) …``) — reboot the host;
- warns once on `up` for an NVIDIA virgl/vulkan GPU that can runtime-suspend:
  `power/control=auto` and `/proc/driver/nvidia/gpus/<addr>/power` not saying
  `Runtime D3 status: Disabled` (desktop cards report it disabled while
  `control` stays `auto`, so they get no warning). NVIDIA only: that resume is
  the failure seen, and `auto` is the kernel default elsewhere;
- under `keepawake` (same condition by default) asks the supervisor, before the
  emulator spawns, to run `vmlab-gpu-keepawake` on the GPU (C, embedded in the
  binary, written to `~/.local/share/vmlab/gpu-keepawake/<digest>/`): an EGL
  device-platform GLES context plus a 16x16 clear every 100 ms (a context alone
  does not hold an NVIDIA GPU awake). No root, ~6 MiB VRAM; one per GPU,
  reused across labs; exits with the supervisor, even a killed one.
  `vmlab status -v` shows `keepawake=on|off|lost|failed` after `host_gpu=`.
  A failed helper is a warning in `labd-<lab>.log`, never a refused start.

The helper does not cover a supervisor restart. The host-side fixes (root) keep
the GPU powered always: a udev rule
`ACTION=="add|bind", SUBSYSTEM=="pci", ATTR{vendor}=="0x10de", ATTR{class}=="0x030000", ATTR{power/control}="on"`
in `/etc/udev/rules.d/99-nvidia-no-runtime-pm.rules`, or
`options nvidia NVreg_DynamicPowerManagement=0x00` in `/etc/modprobe.d/`
(reboot; regenerate the initramfs if it loads nvidia), or for one boot
`echo on | sudo tee /sys/bus/pci/devices/<addr>/power/control`. All cost idle
battery on a laptop.

### UEFI firmware

vmlab ships its own UEFI firmware in the guest assets, under
`<guest asset dir>/firmware/<arch>/` — Debian's edk2 builds, pinned by version
and sha256 (`firmware/VERSION` names the packages):

| Arch | Plain UEFI | Secure boot |
| --- | --- | --- |
| x86_64 | `OVMF_CODE_4M.fd` + `OVMF_VARS_4M.fd` | `OVMF_CODE_4M.secboot.fd` + `OVMF_VARS_4M.ms.fd` (Microsoft 2011 + 2023 CAs and Debian keys enrolled) |
| aarch64 | `AAVMF_CODE.fd` + `AAVMF_VARS.fd` | `AAVMF_CODE.secboot.fd` + `AAVMF_VARS.ms.fd` (keys enrolled) |

A host needs no `ovmf` / `qemu-efi-aarch64` package. Lookup order, for plain UEFI
and secure boot alike:

1. `firmware_dir` from the host config, `<dir>/<arch>/`. Authoritative for an
   arch it has a directory for: a missing pair is an error, not a fall-through.
2. The bundled set, under `$VMLAB_GUEST_ASSET_DIR`, `/usr/share/vmlab/guest`,
   `~/.local/share/vmlab/guest`, in that order.
3. The host's distro firmware at its well-known paths (riscv64 always lands
   here — nothing is bundled for it).

- A secure-boot CODE is paired with its enrolled VARS by default; a blank store
  boots in setup mode and enforces nothing, so vmlab refuses it — unless the VM
  opts in with `secure_boot_keys = "setup"` (secboot CODE + blank
  `OVMF_VARS_4M.fd`; the guest enrolls its own keys, e.g. systemd-boot
  auto-enroll). `secure_boot_vars = "<path>"` boots a user-supplied pre-enrolled
  VARS file (e.g. from `virt-fw-vars --set-pk … --add-db …`) under the secboot
  CODE; it must be the size of that CODE's VARS template.
- Each VM copies its VARS template once, at first start, to
  `.vmlab/vms/<vm>/OVMF_VARS.fd`, and keeps it. A VM whose store was created
  under a firmware of another flash layout (a host's 2 MiB OVMF) keeps booting
  a build of that layout.
- Templates record `firmware`, `secure_boot` and `secure_boot_keys`, never a
  firmware path. A template built with `secure_boot_vars` stores the file as
  `OVMF_VARS.fd` beside its disk (`secure_boot_keys = "vars"`); export/import and
  push/pull carry it, and clones start from it.
- Changing the key store of a VM that has already started means deleting its
  `OVMF_VARS.fd` (losing its boot entries): an existing copy is never replaced.
- To see which firmware a running VM booted, read the `-drive if=pflash`
  arguments of its QEMU process.

Fast path note: `auto` never selects `sockmap`; it was measured slower than the
userspace fabric and exists for explicit evaluation. Both kernel tiers need
CAP_BPF and CAP_NET_ADMIN, and a daemon that cannot prove a tier works on its host
falls back to userspace silently. `vmlab fastpath` reports the tier in use.

### The viewer

`vmlab console` and `gui = true` launch a viewer chosen the same way:

- An explicit `viewer` in the host config wins and is dialled at the VNC unix
  socket directly.
- Otherwise vmlab takes the first of `remote-viewer`, `gvncviewer` and `vncviewer`
  found on `PATH`; all three are driven over a localhost TCP bridge to the socket,
  held open by a detached helper that exits when the viewer window closes, so
  neither command ties up the terminal.
- With no viewer at all, or with `--tcp`, `vmlab console` bridges the socket to a
  localhost port and prints the address to point any VNC client at.

Closing a viewer only disconnects; the VM keeps running, always headless behind VNC.

## Directories

vmlab follows the XDG layout and honours each variable that overrides it.

| Path | Holds |
| --- | --- |
| `~/.config/vmlab/` | `config.wcl` and the `profiles/` directory of user profiles. |
| `~/.local/share/vmlab/` | The template store under `templates/` and the container image cache under `oci/`. |
| `~/.local/share/config-weave/bin/` | Where config-weave's own install puts its guest binaries. |
| `~/.local/state/vmlab/` | Daemon state, per-lab logs and event history. |
| `$XDG_RUNTIME_DIR/vmlab/` | Control sockets: the supervisor's, each lab's, and per-VM QMP, agent and VNC sockets. |
| `<lab>/.vmlab/` | The lab's own working data: disk clones, built media, TPM state, persisted state and sync ledgers. Gitignore it. |

The runtime directory is a full-privilege interface, since a client that can connect
to a lab socket runs scripts in the lab and reads and writes guest files. vmlab
creates it `0700`, tightens an existing one, and refuses one owned by someone else.

`VMLAB_WORK_DIR` relocates every lab's `.vmlab/` under one base, namespaced by lab
name and a hash of its root, keeping the write-heavy working data off a slow
filesystem while the lab file stays put.

## WSL 2

vmlab is clean on WSL 2 for the same reasons it needs no privileges elsewhere: the
network fabric is userspace with no tap, bridge or macvlan, and the only kernel
grant KVM needs is `/dev/kvm`. Four things are specific to WSL 2.

- **Nested virtualisation must be enabled** in `.wslconfig` for `/dev/kvm` to exist
  inside the distribution. Without it every VM runs under TCG.
- **`XDG_RUNTIME_DIR` may be missing.** Some WSL setups do not set it; vmlab falls
  back to `/tmp/vmlab-<uid>`, created private and refused if owned by anyone else.
- **The viewer lives on the Windows side.** Run `vmlab console --tcp` to get a
  localhost address and point a Windows VNC client at it; WSL's localhost forwarding
  carries it across. Host access to guest services works the same way, through port
  forwards and localhost forwarding.
- **The disk-space watchdog matters more.** WSL 2's ext4 VHDX grows and does not
  shrink, and linked clones grow with use, so `disk_low_percent` and the
  `host.disk_low` event are worth handling.

## KVM or TCG

For each machine vmlab picks the accelerator once: KVM when `/dev/kvm` can be opened
and the guest architecture is the host's, TCG otherwise. TCG is full emulation, slow
but functional, and vmlab warns loudly when it falls back, naming the machine and the
architecture. Two cases hit it on purpose: a foreign-architecture guest such as an
aarch64 or riscv64 template on an x86_64 host, which can only ever be emulated, and a
host with no `/dev/kvm`. Give an emulated guest a couple of minutes to boot.

Nested virtualisation inside a guest is separate: a KVM guest on x86 sees VMX or SVM
only when its VM sets `nested = true`, and only when the host's own `kvm_intel` or
`kvm_amd` module has nested virtualisation on. `vmlab up` warns with the fix when it
is off.

### macOS guests under KVM

A macOS guest needs the `kvm` module's `ignore_msrs` on: XNU reads Intel MSRs KVM
does not emulate, KVM answers each with a #GP, and the guest panics or hangs early
in boot (every boot on an AMD host). `vmlab up` warns, without refusing, for every VM
it starts under KVM whose profile name starts with `macos` or whose `apple_smc`
resolves on, when `/sys/module/kvm/parameters/ignore_msrs` is not `Y`. The fix:

```sh
echo "options kvm ignore_msrs=1 report_ignored_msrs=0" | sudo tee /etc/modprobe.d/kvm-msrs.conf
echo 1 | sudo tee /sys/module/kvm/parameters/ignore_msrs   # until the next boot
```

`tsc = "host"` (vm.md) needs KVM too: the frequency is read from KVM at the VM's
first KVM start and pinned in `.vmlab/vms/<vm>/tsc-hz`; under TCG `up` warns and the
VM boots without it, which macOS does not survive.

If every VM is slow and `vmlab logs` shows the TCG fallback warning for x86_64
guests, the host lacks KVM: on WSL 2 enable nested virtualisation in `.wslconfig`,
on a bare host check that your user can open `/dev/kvm` (see troubleshooting.md).

## Guest OS profiles

A profile is a named bundle of known-good hardware defaults for one family of guest
operating system. Profiles are data, shipped as WCL and extensible from a directory
in the config.

### What a profile decides

| Field | Decides |
| --- | --- |
| `machine` | The QEMU machine type: `q35` or `pc` (i440fx). |
| `firmware`, `secure_boot`, `tpm` | OVMF or SeaBIOS, secure boot under OVMF, and an swtpm 2.0 device. |
| `disk_bus`, `nic_model`, `display` | The devices the guest can drive: `virtio`, `ide` or `sata` disks; a NIC model such as `virtio-net-pci`, `e1000` or `pcnet`; a display device such as `virtio-vga`, `std`, `cirrus-vga` or `vmware` (VMware SVGA II). |
| `cpu_model` | The QEMU `-cpu` model the guest sees instead of the host CPU (KVM) or `max` (TCG), for guests a modern CPUID breaks. Only `macos` sets one; a template pins it for its clones. |
| `hyperv` | The Hyper-V enlightenments under KVM, which put a Windows guest's clock on the reference TSC page instead of an emulated timer. Set by `windows-11`, `windows-10`, `windows-server` and `windows-legacy`; `windows-xp` and `windows-9x` leave it off. |
| `apple_smc`, `input`, `tsc` | What macOS needs and no other guest does: the Apple SMC and an SMBIOS type 2 table; a USB keyboard beside the USB tablet (`input = "usb"`), because macOS reads no PS/2 keyboard; an invariant TSC pinned at the host's frequency (`tsc = "host"`). Only `macos` sets them. |
| `accel` | The accelerator: `kvm` (required, no fallback) or `tcg` (always emulated, even where KVM works, for guests KVM cannot run reliably). Unset, KVM when usable, else TCG. No shipped profile sets one. |
| `cpus`, `memory` | The hardware floor a VM or container inherits when neither its block nor its template says. |
| `agent_transport` | The device the guest agent's channel rides: `virtio-serial` (default), `isa-serial` for a guest with no virtio drivers, where the legacy agent speaks over COM1, or `none` for a guest nothing can run an agent on. The older `agent_channel` bool still loads as an alias. |
| `input_transport` | How `send_keys` and the mouse reach the guest: `qmp` (default) or `vnc` (see snapshots-vision.md). |
| `acpi_shutdown` | Whether the guest shuts down on an ACPI power-button press (default `true`). The stop ladder plans from it: off, a stop goes through the agent alone and waits for a booting guest's agent. Only `macos` turns it off. |
| `virtiofs` | Whether the guest mounts virtiofs natively, which makes it a candidate for `transport = "auto"` shares (see shares-media.md). |
| `rtc` | What the guest reads the real-time clock as: `utc` (default), or `localtime` for DOS, Windows 9x and NT4 through XP/2003, which cannot read it as UTC (see snapshots-vision.md). |
| `workspace_guest` | The guest path an `@dev` workspace lands at when the decorator names none (see dev-machines.md). |
| `playbook_dir` | The guest directory a `playbook {}` block pushes config-weave and its playbook folders into when the block names no `guest_dir`, for a guest whose default location is not writable (see automation.md). |

The profile's name also classifies the guest: `windows*` is Windows, `linux*` is
Linux, `macos*` is macOS. The `login {}` validation rules, the Windows preconditions
of the workspace syncer, share mounting, playbooks, `boot_id`, `app run` and
`repair-agent` key on it. macOS follows the Linux login rules; any other name is
unknown and gets none of them.

A profile does not install the agent; that happens once, at template build, when the
build stages the agent binaries on the bootstrap ISO and the template's
unattended-install hook runs the install script (see templates.md).

### Where profiles live

The shipped profiles are compiled into the binary. User profiles are `*.wcl` files
in the `profiles` directory of vmlab's XDG config directory,
`~/.config/vmlab/profiles/` by default. vmlab loads the shipped set, then every
`*.wcl` file there in sorted filename order.

Each file must start with `import <vmlab-profile.wcl>` and contains
`profile "<name>" { … }` blocks. A file without the import is rejected, and so is any
file with an unknown field, a bad keyword or a wrong type, with the line named.

A user profile whose name matches a shipped one **replaces it entirely**, field by
field from scratch, rather than merging. A file holding only
`profile "windows-11" { machine = "pc" }` produces a `windows-11` with no firmware,
no TPM, no disk bus, no memory and no device choices. Copy the full shipped block and
edit it. A new name extends the set and is usable from any lab's `profile =`.

A profile file is validated with the same wording as a lab file: an unknown field, a
value outside its set such as `machine = "vax"`, a `cpus` below 1 or a missing import
are all reported at their line, and every mistake in a file is reported in one pass.
A lab naming a profile that does not exist fails `vmlab validate`; validation reports
an unknown profile name on the block that names it.

Dropping your own `container` profile into the profiles directory raises the micro-VM
defaults for every lab on this host, which is simpler than adding `memory = …` to
each container block.

```wcl
# ~/.config/vmlab/profiles/mine.wcl
import <vmlab-profile.wcl>

profile "freebsd" {
  description = "FreeBSD: q35, SeaBIOS, virtio disk and NIC"
  machine     = "q35"
  firmware    = "seabios"
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "std"
  cpus        = 2
  memory      = 2GiB
  hyperv      = true
  workspace_guest = "/usr/src"
}
```

```wcl
# ~/.config/vmlab/profiles/mine.wcl
import <vmlab-profile.wcl>

profile "freebsd" {
  description = "FreeBSD 14: q35, SeaBIOS, virtio disk and NIC"
  machine     = "q35"
  firmware    = "seabios"
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "std"
  cpus        = 2
  memory      = 2GiB
}
```

### `profile {}`

One profile. Every field is optional. A field left unset means the QEMU default
applies for that device, which is what the shipped `custom` profile relies on.

```wcl
profile "<name>" {
  description     = "…"
  machine         = "q35"
  firmware        = "ovmf"
  secure_boot     = false
  tpm             = false
  disk_bus        = "virtio"
  nic_model       = "virtio-net-pci"
  display         = "virtio-vga"
  cpu_model       = "pentium3"
  hyperv          = true
  apple_smc       = false
  input           = "ps2"
  tsc             = "host"
  accel           = "tcg"
  cpus            = 2
  memory          = 4GiB
  agent_transport = "virtio-serial"
  input_transport = "qmp"
  acpi_shutdown   = true
  virtiofs        = false
  rtc             = "utc"
  workspace_guest = "/src"
  playbook_dir    = "/var/lib/vmlab/weave"
}
```

| Field | Type | Default | Meaning |
| --- | --- | --- | --- |
| `name` | utf8 (label) | required | Profile name, what a `vm`, `container` or `template` names in `profile =`; the inline block label. |
| `description` | utf8 | none | Human-readable summary. Parsed and kept; no verb surfaces it yet. |
| `machine` | utf8 | QEMU default | Machine type: `q35`, or `pc` for i440fx. |
| `firmware` | utf8 | QEMU default | Firmware: `ovmf` or `seabios`. |
| `secure_boot` | bool | unset | Enable secure boot; OVMF only. |
| `secure_boot_keys` | utf8 | unset (`microsoft`) | Key store secure boot starts from: `microsoft` or `setup`. |
| `tpm` | bool | unset | Attach a swtpm 2.0 device. |
| `usb` | bool | unset | Boot with the hot-plug xHCI controller host USB passthrough needs. A VM's own `usb` wins; unset is off. |
| `disk_bus` | utf8 | QEMU default | Primary disk bus: `virtio`, `ide` or `sata`. |
| `nic_model` | utf8 | QEMU default | NIC device model, for example `virtio-net-pci`, `e1000`, `rtl8139` or `pcnet`. |
| `display` | utf8 | QEMU default | Display device, for example `virtio-vga`, `qxl`, `std`, `cirrus-vga`, or `vmware` for VMware SVGA II (`vmware-svga`). |
| `cpu_model` | utf8 | unset | QEMU `-cpu` model, for example `pentium3`, replacing `host` under KVM and `max` under TCG. Only `macos` sets one. |
| `hyperv` | bool | unset | Hyper-V enlightenments (`hv-*` CPU flags) under KVM on x86, unless a `cpu_model` names the CPU. Unset is off. Set by `windows-11`, `windows-10`, `windows-server` and `windows-legacy`; a VM or template `hyperv` wins (vm.md). |
| `apple_smc` | bool | unset | The Apple SMC (`isa-applesmc` with Apple's OSK) and an SMBIOS type 2 table, which macOS refuses to boot without. Unset is off. x86 only. Set by `macos`. |
| `input` | utf8 | unset (`ps2`) | Keyboard and pointer: `ps2` (PS/2 keyboard, USB tablet) or `usb` (USB keyboard and tablet on an xHCI of their own, unbound so QMP `send-key` reaches the keyboard). `usb` on `macos`. |
| `tsc` | utf8 | unset | `host`: an invariant TSC pinned at the host's frequency under KVM, read at a VM's first KVM start and kept. Unset is QEMU's default. x86 only. Set by `macos`. |
| `accel` | utf8 | unset | Accelerator: `kvm` or `tcg`. Unset, KVM when usable, else TCG with a warning. No shipped profile sets one. |
| `cpus` | i64 | unset | Default vCPU count, at least 1. |
| `memory` | ByteSize | unset | Default RAM, for example `4GiB`. |
| `agent_transport` | utf8 | `virtio-serial` | The guest agent channel's device: `virtio-serial`, `isa-serial` (a 16550 on COM1 for the legacy agent; the serial log moves to COM2), or `none` (no agent; never ready by handshake). |
| `agent_channel` | bool | unset | Superseded by `agent_transport`; still accepted: `true` reads as `virtio-serial`, `false` as `none`, and `agent_transport` wins when both are present. |
| `input_transport` | utf8 | `qmp` | How scripted input reaches the guest: `qmp` send-key, or `vnc` through the VM's VNC socket. A guest that reads no PS/2 keyboard needs `input = "usb"`, not `vnc`. |
| `acpi_shutdown` | bool | `true` | The guest shuts down on an ACPI power-button press. `false` makes a graceful stop rely on the agent: it waits for a booting guest's agent as long as readiness would (10 min), skips the ACPI rung, and gives the guest 2 min to power off. Set to `false` by `macos`. |
| `virtiofs` | bool | `false` | The guest mounts virtiofs natively, so `transport = "auto"` shares use it instead of SMB. |
| `rtc` | utf8 | `utc` | What the guest reads the RTC as: `utc`, or `localtime` — the RTC then starts at the host's local time (`-rtc base=localtime`; a fixed `clock` converted to host local time). Only `windows-xp` and `windows-9x` set it. |
| `workspace_guest` | utf8 | unset | Guest path an `@dev` workspace lands at when the decorator names none. |
| `playbook_dir` | utf8 | unset | Guest directory a `playbook {}` block pushes config-weave and its playbook folders into when the block names no `guest_dir`. Must be absolute (`/…` or `C:\…`). Unset, `/var/lib/vmlab/weave` on Linux and macOS and `C:\weave` on Windows. |

The parser enforces the keyword sets above for `machine`, `firmware`, `disk_bus`,
`accel`, `input`, `tsc`, `input_transport` and `rtc`, `cpus` at least 1, and a non-negative `memory`. `nic_model`,
`display` and `cpu_model` are passed to QEMU as written; an empty `cpu_model` is
refused. A profile that sets no `workspace_guest`
still hosts a dev machine; the floor of `/src` applies. On a non-x86 `virt` machine a
`virtio-vga` display is downgraded to `virtio-gpu-pci`, which has no legacy VGA.

### The shipped set

| Profile | Machine | Firmware | Secure boot | TPM | Disk bus | NIC model | Display | CPUs | Memory | Other |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| `windows-11` | q35 | ovmf | true | true | virtio | virtio-net-pci | virtio-vga | 4 | 8GiB | `hyperv = true`. |
| `windows-10` | q35 | ovmf | false | false | virtio | virtio-net-pci | virtio-vga | 4 | 8GiB | `hyperv = true`. |
| `windows-server` | q35 | ovmf | false | true | virtio | virtio-net-pci | virtio-vga | 4 | 8GiB | `hyperv = true`. |
| `windows-legacy` | pc | seabios | false | false | ide | e1000 | std | 2 | 2GiB | `hyperv = true`. Vista, 7 and 2008-era guests; virtio-serial agent (virtio-win covers this era). |
| `windows-xp` | pc | seabios | false | false | ide | e1000 | std | 2 | 2GiB | `agent_transport = "isa-serial"`, `rtc = "localtime"`: NT4 through XP/2003, the legacy agent on COM1. |
| `windows-9x` | pc | seabios | false | false | ide | pcnet | cirrus-vga | 1 | 256MiB | `input_transport = "vnc"`, `agent_transport = "isa-serial"`, `rtc = "localtime"`. DOS, Windows 3.x to ME and 2000-era guests. |
| `templeos` | pc | seabios | false | false | ide | e1000 | std | 2 | 512MiB | `agent_transport = "isa-serial"`. TempleOS: the HolyC agent on COM1; no network by design. |
| `linux-modern` | q35 | ovmf | false | false | virtio | virtio-net-pci | virtio-vga | 2 | 4GiB | `virtiofs = true`. |
| `linux-generic` | q35 | seabios | false | false | virtio | virtio-net-pci | std | 2 | 2GiB | Older or unusual distros. |
| `macos` | q35 | ovmf | false | false | sata | virtio-net-pci | vmware | 4 | 8GiB | `cpu_model = "Skylake-Client-v4,-hle,-rtm,kvm=on,vendor=GenuineIntel,vmware-cpuid-freq=on"`, `hyperv = false`, `apple_smc = true`, `input = "usb"`, `tsc = "host"`, `acpi_shutdown = false`. Intel macOS through OpenCore, which the template carries. |
| `container` | | | | | | | | 1 | 256MiB | Micro-VM size for an OCI container; nothing else applies to a container. |
| `custom` | | | | | | | | | | Nothing assumed; supply everything on the VM or template and in `qemu_args`. |

A blank cell means the field is unset. Every Windows profile lands an `@dev`
workspace at `C:\src`, every Linux one at `/src` and `macos` at `/Users/Shared/src`
(`/` is macOS's sealed system volume); the `container` profile also
sets `workspace_guest = "/src"`, and `custom` leaves it at the floor.

`linux-modern` requests a VGA-compatible virtio GPU on x86 and downgrades to
`virtio-gpu-pci` automatically on the non-x86 `virt` machine, which has no legacy VGA.

`windows-xp` and `windows-9x` name `agent_transport = "isa-serial"`: their guests
have no virtio drivers, so the agent channel is a 16550 on COM1 and the guest runs
the legacy agent, a small C program that speaks the same protocol and serves `exec`
and `set_time` only. `vmlab exec`, readiness, the stop ladder and setting the clock
after a snapshot restore work on such a guest; a terminal and `vmlab cp` refuse by
name, and so does `vmlab app run`. On DOS the agent is the foreground program, and
a command's output arrives after it exits. Installing it is in templates.md.

The `container` profile carries only a size because a container micro-VM boots
vmlab's own guest asset directly and has no firmware, disk bus, display or NIC model
to choose. Its floor is an order of magnitude below a VM's on purpose: the micro-VM
runs one process, not an operating system. Raise it per container with `cpus` and
`memory`, or drop your own `container` profile into the user directory to change the
default for every lab.

`macos` boots Intel macOS (Tahoe and earlier) through OpenCore. OpenCore is not
vmlab's: the template carries it in its disk's EFI system partition, and OVMF boots
it, so secure boot and the TPM are off. AHCI disk, virtio-net (native from Big Sur),
VMware SVGA (no Apple virtio GPU driver on x86; the screen is OpenCore's 1280x800), a
Skylake model with the VMware CPUID leaf, `hyperv` off by name, no virtiofs. A macOS
guest is its own family across vmlab:

- **Agent**: `vmlab-agent` for macOS on virtio-serial `vmlab.agent.0` (in the guest
  `/dev/cu.vmlab.agent.0`), root under launchd.
- **Input**: QMP `send-key` reaches the unbound USB keyboard `input = "usb"` adds;
  `input_transport` stays `qmp`.
- **`boot_id`**: `/usr/sbin/sysctl -n kern.bootsessionuuid`.
- **Stop**: through the agent only (`acpi_shutdown = false`; macOS ignores the
  power button). A `down` while the guest boots waits for its agent instead of
  ending in a QMP `quit`, which the guest would see as a power cut.
- **Shares**: SMB only, `mkdir -p` then `/sbin/mount_smbfs -N [-o rdonly]
  //<user>:<pass>@<gateway>/<share> <path>`, credentials percent-encoded. A
  `transport = "virtiofs"` share is reported unmountable; `auto` picks SMB.
- **Logins**: the Linux rules — no `password` needed, `elevated` is an error.
- **Playbooks**: `guest_bin` is required (config-weave ships no macOS binary);
  `guest_dir` defaults to `/var/lib/vmlab/weave`.
- **`@dev`**: workspace at `/Users/Shared/src`.
- **`vmlab app run`**: refused by name; use `vmlab console`.
- **`vmlab machine repair-agent`**: swaps `/Library/vmlab/vmlab-agent` and runs
  `launchctl kickstart -k system/dev.vmlab.agent`.
- **Host**: `ignore_msrs` on (see "macOS guests under KVM" above).

`custom` sets nothing at all, so QEMU's defaults apply to whatever the VM and
template leave unset.

The shipped file is `src/profiles/shipped.wcl` in the repository:

```wcl
# src/profiles/shipped.wcl
import <vmlab-profile.wcl>

// Shipped guest OS profiles (PRD §5.3). Known-good hardware defaults;
// values not set on the VM or template inherit from these. Override or
// extend by dropping `*.wcl` files into `~/.config/vmlab/profiles/`.

profile "windows-11" {
  description = "Windows 11: q35, OVMF with secure boot, swtpm 2.0, virtio devices"
  machine     = "q35"
  firmware    = "ovmf"
  secure_boot = true
  tpm         = true
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "virtio-vga"
  cpus        = 4
  memory      = 8GiB
  // Where an `@dev` workspace lands when the decorator names no
  // `workspace_guest` (§19.1) — the key is guest-OS-shaped, which is why it
  // is profile-sourced.
  workspace_guest = "C:\\src"
}

profile "windows-10" {
  description = "Windows 10: q35, OVMF, virtio devices (no TPM/secure boot required)"
  machine     = "q35"
  firmware    = "ovmf"
  secure_boot = false
  tpm         = false
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "virtio-vga"
  cpus        = 4
  memory      = 8GiB
  hyperv      = true
  workspace_guest = "C:\\src"
}

profile "windows-server" {
  description = "Windows Server: q35, OVMF, swtpm 2.0, virtio devices"
  machine     = "q35"
  firmware    = "ovmf"
  secure_boot = false
  tpm         = true
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "virtio-vga"
  cpus        = 4
  memory      = 8GiB
  hyperv      = true
  workspace_guest = "C:\\src"
}

profile "windows-legacy" {
  description = "Vista/7/2008-era guests with no virtio storage/net drivers at install time: i440fx, SeaBIOS, IDE disk, e1000 NIC, std VGA; virtio-serial agent channel (virtio-win covers this era)"
  machine     = "pc"
  firmware    = "seabios"
  secure_boot = false
  tpm         = false
  disk_bus    = "ide"
  nic_model   = "e1000"
  display     = "std"
  cpus        = 2
  memory      = 2GiB
  hyperv      = true
  workspace_guest = "C:\\src"
}

profile "windows-xp" {
  description = "NT4 through XP/2003: i440fx, SeaBIOS, IDE disk, e1000 NIC, std VGA; no virtio drivers at all, so the agent channel is a 16550 on COM1 (the legacy agent)"
  machine     = "pc"
  firmware    = "seabios"
  secure_boot = false
  tpm         = false
  disk_bus    = "ide"
  nic_model   = "e1000"
  display     = "std"
  cpus        = 2
  memory      = 2GiB
  agent_transport = "isa-serial"
  workspace_guest = "C:\\src"
}

profile "windows-9x" {
  description = "DOS / Windows 3.x-ME / 2000-era PCs: i440fx, SeaBIOS, IDE disk, Cirrus VGA, AMD PCnet NIC (all drivable by these guests); cap RAM low per template"
  machine     = "pc"
  firmware    = "seabios"
  secure_boot = false
  tpm         = false
  disk_bus    = "ide"
  nic_model   = "pcnet"
  display     = "cirrus-vga"
  cpus        = 1
  memory      = 256MiB
  // Real-mode DOS/9x TUIs (fdisk, setup) drop QMP send-key events between menu
  // redraws; drive their keyboard over VNC instead, which lands reliably.
  input_transport = "vnc"
  workspace_guest = "C:\\src"
}

profile "linux-modern" {
  description = "Modern Linux: q35, OVMF, virtio everything"
  machine     = "q35"
  firmware    = "ovmf"
  secure_boot = false
  tpm         = false
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  // VGA-compatible virtio GPU on x86; auto-downgrades to virtio-gpu-pci on the
  // non-x86 `virt` machine (no legacy VGA there). See display_device_name.
  display     = "virtio-vga"
  cpus        = 2
  memory      = 4GiB
  // Kernel ≥5.4 ships the virtiofs client — `transport = "auto"` shares
  // mount natively instead of over SMB (§7.5).
  virtiofs    = true
  workspace_guest = "/src"
}

profile "linux-generic" {
  description = "Older or unusual distros: q35, SeaBIOS, virtio disk/net, conservative elsewhere"
  machine     = "q35"
  firmware    = "seabios"
  secure_boot = false
  tpm         = false
  disk_bus    = "virtio"
  nic_model   = "virtio-net-pci"
  display     = "std"
  cpus        = 2
  memory      = 2GiB
  workspace_guest = "/src"
}

profile "macos" {
  description = "Intel macOS (Tahoe and earlier) booted through OpenCore: q35, OVMF, AHCI disks, VMware SVGA, USB keyboard and tablet, Apple SMC, invariant TSC"
  machine     = "q35"
  // OpenCore is a UEFI application, and macOS has no use for secure boot or
  // a TPM under it.
  firmware    = "ovmf"
  secure_boot = false
  tpm         = false
  // macOS's own AHCI driver; AppleVirtIO's block personality is not what
  // OpenCore boots from.
  disk_bus    = "sata"
  // virtio-net is native from Big Sur on. Both it and the agent's
  // virtio-serial port land on pcie.0, where QEMU keeps them transitional,
  // the only mode AppleVirtIO drives.
  nic_model   = "virtio-net-pci"
  // There is no Apple virtio GPU driver on x86.
  display     = "vmware"
  // An Intel model macOS supports, with the TSX flags it lacks off, the
  // VMware CPUID leaf XNU reads its TSC and bus frequencies from, and no
  // KVM paravirtual extras, which stop macOS 13 on booting on AMD hosts.
  cpu_model   = "Skylake-Client-v4,-hle,-rtm,kvm=on,vendor=GenuineIntel,vmware-cpuid-freq=on"
  hyperv      = false
  apple_smc   = true
  // OpenCore and macOS read no PS/2 keyboard.
  input       = "usb"
  // QEMU fills the VMware leaf only for a known, stable TSC; pinning it at
  // the host's frequency keeps online snapshots working.
  tsc         = "host"
  cpus        = 4
  memory      = 8GiB
  // macOS does not shut down on an ACPI power-button press, so a graceful
  // stop goes through the agent and waits for a booting guest's agent.
  acpi_shutdown = false
  // `/` is the sealed system volume; `/Users/Shared` is on the Data volume
  // and any login can create a directory in it.
  workspace_guest = "/Users/Shared/src"
}

profile "container" {
  description = "Micro-VM defaults for an OCI container (PRD §18): the smallest shape that boots the guest asset and a typical service image"
  // A container micro-VM boots the guest asset directly — no firmware, disk
  // bus, display or NIC model to choose — so this profile carries only the
  // size, which is the whole reason a container names a profile. It is an
  // order of magnitude below the VM floor on purpose: the micro-VM runs one
  // process, not an operating system. Raise it per container with
  // `memory = …`, or drop your own `container` profile into
  // ~/.config/vmlab/profiles to change the default for every lab.
  cpus   = 1
  memory = 256MiB
  workspace_guest = "/src"
}

profile "custom" {
  description = "Nothing assumed — supply everything via VM/template attributes and qemu_args"
}
```

### Where a profile sits in the chain

Hardware resolves **VM block, then template, then profile**. A value set on the
`vm {}` wins; a value not set there comes from the hardware the template recorded
when it was built; a value not set there comes from the profile, and the profile's
defaults are the floor.

The profile a machine resolves against is the one it names, else the one its template
recorded, else the default. The profile is usually inherited from the template rather
than named on the VM, and it is required on a `scratch` VM, which has no template
layer. A template build resolves the build VM's hardware the same way, block over
source template, with the template's own `profile` supplying the floor.

`secure_boot = true` on a VM whose firmware resolves to SeaBIOS is a validation
error, and because either value may have been inherited, the message names the layer
each came from.

Two things a profile supplies resolve through their own chains, not the hardware one:

- A dev machine's `workspace_guest` resolves `@dev` argument, then profile, then the
  `/src` floor. A profile with no dev keys still hosts a dev machine.
- A container's `cpus` and `memory` come from its block or its profile, and one of
  the two must supply each; there is no template layer and no vmlab floor, so a
  container naming no profile must set both.

## `vmlab osinfo`

`vmlab osinfo` prints what a guest's operating system says it is, as reported by the
vmlab agent, as one JSON object. It is written for machine consumption: a tool that
drives vmlab polls it to detect agent readiness and to pick a guest binary.

```sh
vmlab osinfo <VM>
```

| Option | Meaning |
| --- | --- |
| `<VM>` | The machine, as `[lab/]name`. |
| `-h`, `--help` | Print help. |

The command asks the machine's agent with a 30-second timeout and prints the answer
on one line. The object has the fields `id`, `name`, `version`, `kernel`, `arch`, and
`hostname`. Unlike most verbs this one always prints JSON; there is no table form and
no `--json` flag.

```sh
$ vmlab osinfo nix01
{"arch":"x86_64","hostname":"nix01","id":"nixos","kernel":"6.12.20","name":"NixOS","version":"25.05"}
```

Exit status is 0 on success. `not_found` (4) means the lab declares no machine by
that name. `failed` (1) covers a machine that is not running and an agent that does
not answer within the timeout, which is what a guest still booting looks like.
