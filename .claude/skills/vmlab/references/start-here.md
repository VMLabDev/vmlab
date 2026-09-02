# Getting started with vmlab

Install the CLI and its runtime tools, then run a container, a VM from a
registry template, a template built from installer media, and a provision
script.

## Install

### Prerequisites

- A Linux host, or Windows with WSL 2. There is no macOS build, because vmlab
  drives QEMU/KVM.
- `/dev/kvm` present and writable by your user. Without it every guest is
  emulated, which is far slower. On WSL 2 this needs the nested-virtualisation
  setting below.
- A package manager that can install QEMU. The package names below are Debian
  and Ubuntu names. Other distributions ship the same tools under different
  names.
- `curl` or `wget` for the installer, and a Rust toolchain only if you build
  from source.

### Install the CLI

The installer downloads a prebuilt Linux x86_64 binary from the GitHub release
and places it in `~/.local/bin`. vmlab publishes pre-releases only, so pass
`--pre`. Without it the installer looks for a stable release, finds none, and
stops with a message telling you to add the flag.

```sh
curl -fsSL https://vmlab.io/install.sh | sh -s -- --pre
```

To pin a version, pass `--version <X>` with a release tag instead of `--pre`,
or set `VMLAB_VERSION`. To install somewhere other than `~/.local/bin`, pass
`--bin-dir <dir>` or set `VMLAB_INSTALL_DIR`. If the target directory is not on
your `PATH`, the installer prints the `export` line to add.

### Build from source instead

The prebuilt binary exists for Linux x86_64 only. On any other Linux
architecture, or for the current main branch, build with cargo. The source tree
pins its WCL and wscript dependencies by git revision, so no sibling checkouts
are needed.

```sh
cargo install --git https://github.com/VMLabDev/vmlab --locked
```

From a checkout, `just build` runs `cargo build` and `just install` runs
`cargo install --path . --locked`, which places the binary in `~/.cargo/bin`.

Confirm the binary runs:

```sh
vmlab --version
```

Success: `vmlab --version` prints a version string. If the shell reports
`command not found`, add the install directory to your `PATH` and open a new
shell.

### Install the runtime tools

vmlab does not bundle QEMU. It finds the emulators, firmware and helper tools
on your `PATH` and in the standard firmware directories. The package list below
is the one vmlab's own test suite installs on Ubuntu. It covers every guest
architecture and every documented feature.

```sh
sudo apt-get update
sudo apt-get install -y --no-install-recommends \
  qemu-system-x86 qemu-system-arm qemu-system-misc qemu-utils \
  ovmf seabios swtpm \
  qemu-efi-aarch64 qemu-efi-riscv64 \
  tesseract-ocr passt \
  xorriso mtools dosfstools samba
```

| Package | Provides | Needed for |
| --- | --- | --- |
| `qemu-system-x86`, `qemu-system-arm`, `qemu-system-misc` | `qemu-system-x86_64`, `qemu-system-aarch64`, `qemu-system-riscv64` | Running guests. Install only the architectures you use. |
| `qemu-utils` | `qemu-img` | Every disk operation: clones, snapshots, template builds |
| `ovmf`, `seabios`, `qemu-efi-aarch64`, `qemu-efi-riscv64` | UEFI and BIOS firmware per architecture | Booting any guest; `firmware = "ovmf"` is the default on modern profiles |
| `swtpm` | A software TPM 2.0 | Guests with `tpm = true`, which Windows 11 and Server 2025 require |
| `passt` | Userspace networking helper | The network fabric |
| `xorriso`, `mtools`, `dosfstools` | ISO and floppy builders | `media {}` blocks and the bootstrap ISO every template build attaches |
| `samba` | `smbd` | Shared folders on guests without virtiofs support |
| `tesseract-ocr` | `tesseract` | `vmlab vm ocr` and `wait_for_text` in scripts |

Two more tools are optional. `sqfstar` from `squashfs-tools` is required the
first time you run a container, because vmlab flattens the image's layers into
a squashfs. A VNC viewer is required for `vmlab console` and `gui = true`;
`remote-viewer` from `virt-viewer` is preferred because it dials the display's
unix socket directly, and `gvncviewer` or `vncviewer` work over a local TCP
bridge. Shared folders use `virtiofsd` when the host and guest both support it
and fall back to the bundled `smbd` otherwise, so installing both covers every
guest.

vmlab checks for a missing tool when it first needs it, not at install time.
`vmlab validate` does not probe the host. A missing emulator is reported by
`vmlab up`, a missing `sqfstar` by the first container start, and a missing
viewer by `vmlab console`.

Success: `qemu-system-x86_64 --version` and `qemu-img --version` both print a
version. `ls /dev/kvm` shows the device.

### Guest assets

Two things run inside guests that are not part of the `vmlab` binary:

- The **micro-VM guest asset** — a pinned Alpine kernel and an initramfs
  holding vmlab's own init, which boots every lab container.
- The **agent binary**, `vmlab-agent`, one per guest OS and architecture, which
  a template build bakes into the image from a bootstrap ISO.

vmlab looks for both under one directory tree, in this order.

| Location | When it is used |
| --- | --- |
| `$VMLAB_GUEST_ASSET_DIR/` | An explicit override, when the variable is set |
| `/usr/share/vmlab/guest/` | A system-wide install |
| `~/.local/share/vmlab/guest/` | The per-user data directory |

Inside that tree the micro-VM asset lives at `<arch>/vmlinuz`,
`<arch>/initramfs.img` and `<arch>/VERSION`, and the agent at
`agent/<os>-<arch>/vmlab-agent` or `vmlab-agent.exe` with its own `VERSION`.

The release installer ships the CLI binary only. It does not place guest
assets, so on a fresh host there are none. Build them from a source checkout
with one recipe, which runs both build scripts and copies the result into
`~/.local/share/vmlab/guest/`.

```sh
git clone https://github.com/VMLabDev/vmlab
cd vmlab
just guest-install
```

`guest/build-asset.sh` fetches pinned Alpine packages, verifies their
checksums, and assembles the kernel and initramfs for x86_64 and aarch64. It
runs without root and needs `curl`, `tar`, `gzip`, `cpio`, `sha256sum`,
`cargo`, `rustup` and `git` on the host, plus the
`x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` Rust targets
installed with `rustup target add`. `guest/build-agent.sh` cross-compiles the
agent for Linux x86_64, aarch64 and riscv64 and for Windows x86_64. The Windows
target needs `x86_64-w64-mingw32-gcc` from mingw-w64 and is skipped with a
warning when it is absent. The riscv64 target is best-effort and skipped the
same way.

Assets are not needed for every workflow:

- A VM cloned from a template pulled from a registry needs neither. The agent
  is already inside the pulled image.
- A container needs the micro-VM asset for its architecture. Without it the
  first container start fails, naming every directory it searched and the build
  script to run.
- A template build from an ISO needs the agent binary for the guest's
  architecture, unless the template sets `agent = false`. Without it the build
  fails before booting anything, with the same kind of message.

Success: `ls ~/.local/share/vmlab/guest/` lists `x86_64` and `agent`.
`cat ~/.local/share/vmlab/guest/x86_64/VERSION` prints a version stamp.

### On WSL 2

vmlab treats WSL 2 as a first-class host. Its network fabric creates no tap or
bridge devices and needs no privileges, which is what makes the Windows kernel
a non-issue. Two things are specific to WSL 2.

**Nested virtualisation must be on.** `/dev/kvm` appears inside the WSL
distribution only when the WSL VM itself exposes virtualisation. Add the
setting to `%UserProfile%\.wslconfig` on the Windows side, then restart WSL
with `wsl --shutdown`.

```ini
# .wslconfig
[wsl2]
nestedVirtualization=true
```

**Viewers live on the Windows side.** `vmlab console --tcp` bridges a VM's VNC
display to a localhost port and prints the address, so a Windows VNC client can
attach through WSL's localhost forwarding. Port forwards declared in the lab
file reach the Windows side the same way.

vmlab creates `$XDG_RUNTIME_DIR` at daemon start when a WSL session lacks it.
Host settings, including the viewer choice and the template store location, are
in host-profiles.md.

Warning: if `/dev/kvm` is missing, vmlab still runs guests under pure
emulation. A Windows installer under emulation takes hours rather than minutes,
and template builds time out. Confirm the device exists before building
anything.

## Your first lab: one container on one NAT segment

One segment and one container running nginx: bring it up, reach it from the
host through a port forward, run a command inside it, take it down. Covers
every lifecycle verb: `validate`, `up`, `status`, `down`, `destroy`.

A container is the quickest machine to start with because there is nothing to
build. vmlab pulls the image like a docker client would and runs it inside a
small micro-VM with vmlab's own init (containers.md).

### Prerequisites

- vmlab is installed and `vmlab --version` works.
- The micro-VM guest asset for x86_64 is in place under
  `~/.local/share/vmlab/guest/x86_64/`. Containers boot from it.
- `sqfstar` from `squashfs-tools` is on your `PATH`. vmlab flattens the pulled
  image with it.
- Internet access from the host, to pull the `nginx:1.27` image from Docker Hub.
- Host port 18081 is free.

### The lab file

Create an empty directory and put a file named `vmlab.wcl` in it. vmlab finds
the lab file by walking up from the current directory, the way git finds
`.git`, so every command runs from inside that directory.

```wcl
// vmlab.wcl
import <vmlab.wcl>

lab "first-lab" {

  segment "lan" {
    subnet = "10.90.0.0/24"
    nat    = true
  }

  container "web" {
    image   = "nginx:1.27"
    profile = "container"
    nic { segment = "lan" }
    port { host = 18081 container = 80 }
    healthcheck {
      command  = ["curl", "-fsS", "http://localhost/"]
      interval = 5s
    }
  }
}
```

- `import <vmlab.wcl>` brings in the schema. Every lab file starts with it, and
  it is what lets `validate` reject a misspelt field.
- `lab "first-lab"` names the lab. The name is a DNS label. It appears in
  `vmlab lab list` and in the DNS suffix guests resolve each other under.
- `segment "lan"` declares a virtual L2 network with the subnet
  `10.90.0.0/24`. DHCP is on by default. `nat = true` gives machines on the
  segment internet egress through the host.
- `container "web"` runs the image `nginx:1.27`. `profile = "container"`
  supplies the micro-VM's CPU and memory defaults. A container must get its
  size from a profile or declare `cpus` and `memory` itself.
- `nic { segment = "lan" }` attaches the container to the segment with a
  dynamic DHCP lease.
- `port { host = 18081 container = 80 }` forwards host port 18081 to port 80
  inside the container. It is shorthand for a `forward {}` block on the segment.
- `healthcheck {}` runs `curl` inside the container every five seconds. The
  container is **ready** once the probe passes for the first time. Without a
  healthcheck a container is ready as soon as its process starts.

Every field of these blocks, with its type and default, is in lab-file.md and
containers.md.

### Validate

`vmlab validate` parses the file, checks it against the schema, and runs the
semantic checks: the segment the NIC names exists, the port is unique across
the lab, the container has a size. It starts nothing and writes nothing.

```sh
vmlab validate
```

A clean run prints one line, `ok: lab "first-lab" — 0 vm(s), 1 container(s), 1
segment(s)`, and exits 0. An error names the file, the line and what is wrong.
Misspelling `segment` as `segmnet` inside the `nic` block shows the shape of a
schema error.

### Up

```sh
vmlab up
```

The first `up` starts the supervisor daemon, `vmlabd`, if it is not already
running, starts a lab daemon for this lab, pulls the nginx image, flattens it,
boots the micro-VM, and waits for the healthcheck to pass. Progress streams to
the terminal. The pull happens once; the flattened image is cached by digest
and later runs skip it.

`up` returns when every machine is ready and every provision script has
finished. This lab has no provision scripts, so it returns as soon as the
healthcheck passes. The lab keeps running after the command exits. It is owned
by the lab daemon, not by your terminal.

### Inspect

```sh
vmlab status
```

`status` prints one row per machine with what it is doing and its IP address,
and one row per segment. Add `-v` for the raw power state, the readiness flag
and the container's image, health and last exit code.

```sh
vmlab status -v
```

Two narrower verbs: the container's address on its own, and the log of its
console, which carries the kernel messages and the process's stdout and stderr.

```sh
vmlab container ip web
vmlab container logs web -n 50
```

Success: `vmlab status` shows `web` running and ready with an address in
`10.90.0.0/24`.

### Reach it from the host

The `port {}` block made the host listen on 18081 and forward to the container.

```sh
curl http://localhost:18081/
```

The response is nginx's welcome page. The forward is served by the lab daemon's
userspace network stack, so nothing on the host was configured to make this
work: no iptables rule, no bridge, no capability.

### Run a command inside it

Commands reach the guest over the agent's virtio-serial channel, not over the
network. `vmlab container exec` runs one command and mirrors its output and
exit code. Everything after `--` is the command.

```sh
vmlab container exec web -- nginx -v
```

`vmlab exec` does the same thing and accepts a container name as well as a VM
name. The two verbs share one implementation. Both wait up to 120 seconds by
default; `--timeout <SECS>` changes that.

```sh
vmlab exec web -- cat /etc/os-release
```

For an interactive session, attach a shell. Press Ctrl-] to detach and leave
the container running.

```sh
vmlab container shell web
```

With no `login {}` block on the machine, `exec` and `shell` run as the agent's
own identity, which is root in a container. Declaring the account a surface
attaches as is in logins-and-ssh.md.

### Down

```sh
vmlab down
```

`down` stops every machine gracefully and keeps their state. For a container
that means its scratch disk survives, so files written inside it are still
there after the next `up`. The lab daemon exits. `vmlab status` from this
directory now prints `lab "first-lab": not running`.

```sh
vmlab up
curl http://localhost:18081/
vmlab down
```

The second `up` is faster. The image is cached and there is no healthcheck
history to wait for beyond the first pass.

### Destroy

```sh
vmlab destroy
```

`destroy` stops the lab if it is running and then deletes everything vmlab
created for it: the container's scratch state, the lab-local `.vmlab/`
directory beside the lab file, and any dynamically added network rules. The lab
file itself and the cached image are untouched. The next `up` starts from a
fresh container.

Warning: `destroy` is not undoable. Anything written inside a machine's disk is
gone after `destroy`. Keep data you care about on the host, in a `share {}` or
a `volume {}`, or copy it out with `vmlab cp` first.

Success: the directory holds only `vmlab.wcl`. `vmlab lab list` no longer shows
`first-lab`.

## Your first VM: a registry template

Run a full virtual machine without building anything. The VM's `template` field
names a template published on a public OCI registry, and `vmlab up` pulls it
into the local store the first time.

### Prerequisites

- vmlab is installed with the runtime tools. No guest assets are needed: the
  pulled template already carries the agent.
- `/dev/kvm` is available. The template is x86_64, so it runs under KVM on an
  x86_64 host.
- Internet access from the host, to pull from `ghcr.io`. The template is small,
  and the pull takes a few tens of seconds.
- A VNC viewer for the console step. `remote-viewer` from `virt-viewer` is the
  one vmlab prefers. The console step also works with no viewer at all.

### The lab file

```wcl
// vmlab.wcl
import <vmlab.wcl>

lab "first-vm" {

  vm "alp" {
    template = "ghcr.io/vmlabdev/vmlab-templates/alpine-3.23"
    arch     = "x86_64"
    memory   = 1GiB
    nic { nat = true }
  }
}
```

`template` is an OCI registry reference of the form
`host/owner/[group/]name[:tag]`. With no tag it tracks the moving `latest` tag,
the newest stable version of the published template. `:latest-prerelease`
tracks pre-releases and `:<version>` pins one. A registry reference always
needs `arch`, because one tag can carry several architectures and vmlab never
assumes the host's.

`nic { nat = true }` is shorthand for attaching the VM to the lab's built-in
NAT segment, so no segment declaration is needed. The guest gets a DHCP lease
and internet egress. `memory = 1GiB` overrides the size the template recorded;
`cpus` and everything else are inherited from the template and, below it, from
its guest OS profile. The whole surface is in vm.md.

### Up

```sh
vmlab validate
vmlab up
```

`up` resolves the tag, finds no matching version in the local store, pulls the
template, installs it under `x86_64/alpine-3.23@<version>`, and creates the VM
as a linked clone of it. The clone is a qcow2 file whose backing file is the
sealed template, so it starts small and the template is never modified. The VM
boots and `up` waits until the guest agent answers, which is what **ready**
means.

A cached version is reused on every later `up` and never re-pulled implicitly.
To fetch a template without starting anything, run `vmlab pull`. To see what
the store holds, list it.

```sh
vmlab status
vmlab template list
```

Success: `vmlab status` shows `alp` running and ready with an address.
`vmlab template list` shows `x86_64/alpine-3.23` with the version that was
pulled.

### Open a shell

`vmlab shell` attaches an interactive terminal inside the guest over the
agent's virtio-serial channel. No SSH and no guest network are involved, so it
works even on a VM with no NIC. This VM declares no `login {}`, so the shell
runs as the agent's identity, which is root on Linux.

```sh
vmlab shell alp
```

Inside the guest, detach with Ctrl-]. Detaching leaves the VM running.

```sh
cat /etc/alpine-release
ip addr show eth0
```

For a single command, use `exec` instead. Its exit code becomes the exit code
of `vmlab exec`.

```sh
vmlab exec alp -- uname -a
```

### Attach the console

Every VM has a VNC display served on a unix socket whether or not anyone is
looking at it. `vmlab console` attaches a viewer to it. vmlab picks the viewer
from the host configuration, else the first of `remote-viewer`, `gvncviewer`
and `vncviewer` on `PATH`. Closing the viewer window only disconnects. The VM
keeps running.

```sh
vmlab console alp
```

The Alpine login prompt appears on the virtual screen. With no viewer
installed, or on WSL 2 where the viewer lives on the Windows side, ask for a
TCP bridge instead. vmlab forwards the display to a localhost port and prints
the address for any VNC client.

```sh
vmlab console --tcp alp
```

To open a viewer automatically on every `up`, set `gui = true` on the VM or on
the lab. The VM still runs headless; the viewer is a separate client process.

### Take a screenshot

The same display the console shows can be captured to a PNG at any moment. This
is the basis of screen-driven automation, where a script waits for an image or
a piece of text to appear.

```sh
vmlab vm screenshot alp login.png
```

`login.png` holds the same login prompt. With `tesseract` installed,
`vmlab vm ocr alp` reads the text off the screen instead, and
`vmlab vm sendkeys alp <chord>` types into it. The whole surface is in
snapshots-vision.md.

### Snapshot and restore

A snapshot records a machine's disk, and if the machine is running, its RAM and
device state too. Every snapshot remembers the power state it was taken in.
Restoring an online snapshot resumes the guest exactly where it was; restoring
an offline one leaves it powered off.

```sh
vmlab exec alp -- sh -c 'echo before > /root/marker'
vmlab snapshot create clean --vm alp
vmlab snapshot list alp
```

`--vm alp` narrows the snapshot to one machine. Without it, `snapshot create`
captures every VM and container in the lab under one name. Consistency across
machines in a lab-wide snapshot is best-effort, not coordinated.

```sh
vmlab exec alp -- rm /root/marker
vmlab snapshot restore clean --vm alp
vmlab exec alp -- cat /root/marker
```

The last command prints `before`. The VM was running when the snapshot was
taken, so it is running again after the restore, with the marker back.

```sh
vmlab snapshot delete alp clean
```

Note: snapshots are not a workspace backup. On a dev machine the workspace is
re-converged from the host after a restore rather than rolled back with the
disk. The source of truth for a workspace is the host directory
(dev-machines.md).

### Clean up

```sh
vmlab down
vmlab destroy
```

`destroy` deletes the clone and the lab-local state. The pulled template stays
in the store, so the next `up` of any lab that names it starts without a
download. To remove it, find its version with `vmlab template list` and remove
that reference.

```sh
vmlab template rm x86_64/alpine-3.23@<version>
```

## Build a template from an ISO

A template is a sealed base disk in the local store, keyed by architecture,
name and version. Every VM in a lab is a linked clone of one. This builds a
template from the Ubuntu Server 24.04 installer ISO using the example shipped
with vmlab, then clones a VM from it.

The example does an unattended install. A `cloudinit/` folder is packed into a
small ISO that Ubuntu's installer picks up as an autoinstall source, and a
short wscript answers the one confirmation prompt by reading the screen. The
same pattern — installer media plus answer file plus a script that drives the
screen — builds every template under `examples/templates/`, including Windows
Server.

### Prerequisites

- vmlab is installed with the runtime tools. `xorriso` and `mtools` are needed
  here, because the build packs two ISOs: the cloud-init folder and vmlab's own
  bootstrap ISO.
- The agent binary for Linux x86_64 is installed under
  `~/.local/share/vmlab/guest/agent/linux-x86_64/`. The build stages it on the
  bootstrap ISO and refuses to start without it.
- `/dev/kvm` is available. The install takes several minutes under KVM and far
  longer under emulation.
- About 3 GB of free disk for the ISO download and 20 GB for the build disk.
  The sealed template is much smaller than the working disk.
- A checkout of the vmlab source, for the example directory:
  `git clone https://github.com/VMLabDev/vmlab`.

### The template definition

```sh
cd vmlab/examples/templates/ubuntu-24.04
```

```wcl
// examples/templates/ubuntu-24.04/vmlab.wcl
// Ubuntu Server 24.04 template (PRD §6.1). The installer ISO is downloaded
// and sha256-verified into the artefact cache; cloudinit/ is packed into a
// CIDATA ISO that subiquity picks up as a NoCloud autoinstall source. The
// provision script answers the autoinstall confirmation and waits for the
// installer to power the VM off. Build with:
//
//   vmlab template build        (run from this directory)

import <vmlab.wcl>

template "ubuntu-24.04" {
  arch    = "x86_64"
  version = "24.04.4"
  profile = "linux-modern"
  cpus    = 2
  memory  = 4GiB
  disk    = 20GiB

  source "iso" {
    url    = "https://releases.ubuntu.com/24.04/ubuntu-24.04.4-live-server-amd64.iso"
    sha256 = "e907d92eeec9df64163a7e454cbc8d7755e8ddc7ed42f99dbc80c40f1a138433"
  }

  media { kind = "iso" from = "./cloudinit/" label = "CIDATA" }
  nic { nat = true }

  provision "scripts/install.ws" { }
}
```

A `template {}` block has four parts.

- **Identity and hardware.** `arch` selects the QEMU emulator. `version` is the
  version prefix the build stamps. `profile = "linux-modern"` supplies the
  machine type, firmware and device models for the build VM, and `cpus`,
  `memory` and `disk` size it. The hardware is recorded in the template's
  metadata and becomes the inheritance layer every clone starts from.
- **`source "iso"`.** What the build boots. A `url` with a `sha256` is
  downloaded into the artefact cache and verified before use. A local `path`
  works too. The other three source kinds are `qcow2`, an existing disk image;
  `template`, a layered build on top of a template already in the store; and
  `scratch`, a blank disk.
- **Attachments.** `media { kind = "iso" from = "./cloudinit/" label = "CIDATA" }`
  builds the folder into an ISO with that volume label at build time.
  `nic { nat = true }` gives the build VM egress so the installer can fetch
  packages.
- **`provision "scripts/install.ws"`.** The wscript that drives the build. It
  runs against a lab containing the single build VM, named `build`.

The script uses the screen, not the agent, because the agent does not exist
until the installer has put it there.

```rust
// examples/templates/ubuntu-24.04/scripts/install.ws
// Build provision for the ubuntu-24.04 template (PRD §6.1, §10.4).
// Subiquity finds the autoinstall config on the CIDATA ISO but, without
// `autoinstall` on the kernel command line, asks for confirmation first —
// answer it, then wait for the installer to power the VM off
// (`shutdown: poweroff` in cloudinit/user-data). The sealed image carries
// the vmlab guest agent (installed offline from the VMLAB ISO; the build
// verifies it with one extra boot), so lab clones come up "ready".

use vmlab

fn install(lab: Lab) -> Result[unit, string] {
    let vm = lab.vm("build")?

    // Nudge GRUB past its menu timeout if it is on screen.
    match vm.wait_for_text("(?i)install ubuntu", 180) {
        Ok(_) => {
            vm.send_keys("enter")?
            lab.log("selected the installer GRUB entry")
        }
        Err(e) => lab.log("no GRUB menu seen, continuing: " + e),
    }

    // Subiquity: "Continue with autoinstall? (yes|no)".
    match vm.wait_for_text("(?i)continue with autoinstall", 900) {
        Ok(_) => {
            vm.type_text("yes\n")?
            lab.log("autoinstall confirmed")
        }
        Err(e) => lab.log("no confirmation prompt seen, assuming unattended boot: " + e),
    }

    lab.log("installing (takes several minutes)...")
    vm.wait_shutdown(3600)?
    lab.log("installer powered the VM off; ready to seal")
    Ok(())
}

fn main(lab: Lab) {
    install(lab).expect("ubuntu-24.04 build failed")
}
```

`wait_for_text` OCRs the screen until a regular expression matches or the
timeout passes. `send_keys` and `type_text` inject input. `wait_shutdown`
blocks until the guest powers itself off, which the autoinstall's
`shutdown: poweroff` line does at the end of the install. The agent is
installed by the guest itself: a `late-commands` entry in `cloudinit/user-data`
mounts the bootstrap ISO vmlab attaches under the label `VMLAB` and runs its
`install.sh` into the target filesystem.

Note: vmlab attaches the bootstrap ISO to every template build and verifies the
agent's handshake before sealing. It does not push the agent in itself, because
there is no channel into a guest until the agent is running. Each OS has its
own hook: a cloud-init `late-commands` or `runcmd`, or a Windows
`FirstLogonCommands` entry in `autounattend.xml`.

### Build it

```sh
vmlab validate
vmlab template build
```

`template build` reads every `template {}` block in `./vmlab.wcl`, or the file
named with `-f`, and builds each in turn. Pass a name to build only one. The
build streams its progress, and each `lab.log` line in the script appears as it
runs. To watch the installer, set `gui = true` on the template block and a
viewer opens on the build VM.

Build stages:

1. Download the ISO into the artefact cache and verify its SHA-256, or reuse
   the cached copy.
2. Pack `cloudinit/` and the bootstrap ISO. Create the 20 GiB working disk.
3. Boot the build VM from the installer ISO with the hardware from the profile
   and the two built ISOs attached.
4. Run `scripts/install.ws`: confirm the autoinstall, then wait for the guest
   to power off.
5. Boot once more and wait for the agent's handshake. This proves the sealed
   image will report ready as a clone.
6. Flatten the working disk and move it with its metadata into the store.

The whole run takes several minutes. A failed build leaves nothing in the
store, so the definition can be fixed and run again.
`vmlab template stop ubuntu-24.04` cancels a build in progress.

The version the build stamps is the block's `version` with a build counter
appended: the first build of `24.04.4` is `24.04.4.0`, the next is `24.04.4.1`.
Pass `--version` to pin an exact string instead. A version already in the store
is refused rather than overwritten.

```sh
vmlab template list
```

Success: `vmlab template list` shows a row for `ubuntu-24.04` with arch
`x86_64`, version `24.04.4.0` and its size on disk.

### Use it in a lab

In a new directory, declare a VM that names the template by its store
reference.

```wcl
// vmlab.wcl
import <vmlab.wcl>

lab "ubuntu-lab" {

  segment "lan" {
    subnet = "10.91.0.0/24"
    nat    = true
    forward { host_port = 12222 to = "srv:22" }
  }

  vm "srv" {
    template = "x86_64/ubuntu-24.04"
    memory   = 2GiB
    nic { segment = "lan" ip = "10.91.0.10" }
  }
}
```

A store reference is `<arch>/<name>[@<version>]`. With no version the newest
build is used. `@24.04.4` selects the newest build under that prefix, and
`@24.04.4.0` pins one exactly. `cpus`, the firmware and the device models are
inherited from the template, which recorded them at build time. Only `memory`
is overridden here.

```sh
vmlab validate
vmlab up
vmlab exec srv -- lsb_release -a
```

`validate` confirms the template exists in the store before `up` tries to clone
it. The clone boots and reports ready as soon as the baked agent answers. The
template's autoinstall created a user `vmlab` with password `vmlab`, so the
forward on port 12222 gives SSH into the guest too.

```sh
ssh vmlab@localhost -p 12222
```

Success: `vmlab status` shows `srv` ready at `10.91.0.10`, and `lsb_release`
reports Ubuntu 24.04.

### Keep the store tidy

Every build adds a version. `vmlab template clean` prunes superseded builds,
keeping the newest per template. It only prints what it would remove until
`--yes` is added, and it skips a build that still backs a clone unless
`--force` is added.

```sh
vmlab template clean
vmlab template clean --yes
```

## Automate a guest with a script

A provision script is a wscript file declared inside the machine it configures.
vmlab runs it during `vmlab up` once that machine is ready.

### Prerequisites

- Any lab with one Linux VM that reports ready. The lab file below is the
  `first-vm` one, extended.
- The guest has a display. Screenshots need one, and every VM has one. A
  container does not, so the screenshot step would fail there by naming the
  missing capability.

### A real provision

The `alpine-registry` example ships a provision that waits for readiness and
runs a command.

```rust
// examples/alpine-registry/scripts/setup.ws
// Provision for the alpine-registry lab: wait for the guest (which boots from
// a template pulled on-demand from the OCI registry), then prove it is up and
// reachable. `wait_ready` blocks until the vmlab guest agent answers.

use vmlab

fn setup(lab: Lab) -> Result[unit, string] {
    let alp = lab.vm("alp")?

    lab.log("waiting for the guest agent (template pulled from the registry on first up)...")
    alp.wait_ready(600)?
    lab.log("alp is up at " + alp.ip()?)

    let rel = alp.exec("/bin/cat", ["/etc/alpine-release"])?
    lab.log("alpine release: " + rel.stdout.trim())

    lab.log("SSH in with:  ssh vmlab@localhost -p 12222   (password: vmlab)")
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("alpine-registry setup failed")
}
```

Four things in this file are the shape of every provision script.

- `use vmlab` imports the host module, which provides `vmlab::sleep_ms` and the
  other helpers (wscript-language.md).
- `fn main(lab: Lab)` is the entry point. vmlab calls it with the Lab handle
  (wscript-lab-api.md), from which the script reaches every machine and segment
  by name.
- Methods that can fail return `Result`. The `?` operator propagates an error
  string out of `setup`, and `expect` in `main` turns it into a failed
  provision, which fails `vmlab up` with that message.
- `wait_ready(600)` blocks until the guest agent answers or 600 seconds pass. A
  provision runs after its machine is ready, so the call is instant during `up`,
  but it makes the script correct under `vmlab script` too, where nothing has
  waited for you.

### Write the script

In the directory holding the `first-vm` lab file, create `scripts/setup.ws` and
a file to copy in, `scripts/files/motd`.

```sh
mkdir -p scripts/files
printf 'provisioned by vmlab\n' > scripts/files/motd
```

```rust
// scripts/setup.ws
// Wait for the guest, run a command, copy a file in, capture the screen.

use vmlab

fn setup(lab: Lab) -> Result[unit, string] {
    let alp = lab.vm("alp")?

    alp.wait_ready(600)?
    lab.log("alp is ready at " + alp.ip()?)

    // exec: program and argument list, captured stdout/stderr/exit code.
    let rel = alp.exec("/bin/cat", ["/etc/alpine-release"])?
    if rel.exit_code != 0 {
        return Err("could not read the release: " + rel.stderr)
    }
    lab.log("alpine release: " + rel.stdout.trim())

    // copy_to: a host path relative to this script, to an absolute guest path.
    alp.copy_to("files/motd", "/etc/motd")?
    let motd = alp.exec("/bin/cat", ["/etc/motd"])?
    lab.log("guest motd: " + motd.stdout.trim())

    // screenshot: a PNG under the lab's .vmlab/screenshots/ when the path is "".
    let shot = alp.screenshot("")?
    lab.log("screen captured to " + shot)
    Ok(())
}

fn main(lab: Lab) {
    setup(lab).expect("first-vm setup failed")
}
```

Three details are easy to get wrong.

- `exec` takes a program and a list of arguments, not a shell line. To run a
  pipeline or use a shell feature, call the shell:
  `alp.exec("/bin/sh", ["-c", "..."])`. The default timeout is 120 seconds;
  `exec_timeout` takes a third argument in seconds.
- Relative host paths in `copy_to`, `copy_from`, `screenshot` and the
  image-matching methods resolve against the directory the script lives in, not
  the lab root. That is why the file is at `scripts/files/motd` and the script
  names it as `files/motd`.
- `screenshot("")` picks a timestamped name under the lab-local
  `.vmlab/screenshots/` directory and returns the path. Pass a path to choose
  the file yourself.

### Declare it on the machine

Add a `provision {}` block to the VM. The path is relative to the lab root.

```wcl
// vmlab.wcl
import <vmlab.wcl>

lab "first-vm" {

  vm "alp" {
    template = "ghcr.io/vmlabdev/vmlab-templates/alpine-3.23"
    arch     = "x86_64"
    memory   = 1GiB
    nic { nat = true }

    provision "scripts/setup.ws" { }
  }
}
```

`validate` compiles the script as part of validating the lab file, so a syntax
error or a call to a method that does not exist is reported before anything
boots.

```sh
vmlab validate
```

Success: `vmlab validate` reports the lab as ok with one VM. A typo in the
script is reported here, with its line.

### Run it with up

```sh
vmlab up
```

`up` boots the VM, waits until it is ready, and then runs the provision. Each
`lab.log` line appears in the terminal as the script reaches it. Every `up`
runs the steps again, whether or not the VM was already running, so keep
scripts safe to repeat. Copying the same file twice is harmless. Creating an
account twice is not; the mixed-lab example (examples.md) shows the guard
pattern, checking the guest's state before changing it.

Confirm the two side effects from the host.

```sh
vmlab exec alp -- cat /etc/motd
ls .vmlab/screenshots/
```

Success: `/etc/motd` in the guest reads `provisioned by vmlab`, and
`.vmlab/screenshots/` holds a PNG named after the VM and the time.

### Run it again on demand

Any script can be run ad hoc against the running lab. `vmlab script` takes a
path relative to the lab root and calls its `main` with the same lab handle a
provision gets.

```sh
vmlab script scripts/setup.ws
```

The one difference is ownership. A provision belongs to the machine that
declared it, and `lab.this_vm()` returns that machine. Under `vmlab script`
there is no owning machine and `this_vm()` returns an error. The script above
uses `lab.vm("alp")` so it works both ways.

Provisions and playbooks are the two kinds of setup step, and they run
interleaved in declaration order. The other place scripts run is an `on {}`
handler, which reacts to an event such as `vm.crashed` (automation.md).

Note: provisions run as the agent identity. With no `login {}` on the machine,
`exec` and `copy_to` run as root or SYSTEM. To write into a user's home as that
user, declare a login and take a second handle with `as_login`
(logins-and-ssh.md, dev-machines.md).
