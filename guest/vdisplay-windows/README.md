# Windows shared-memory display

This x64 UMDF 2 driver creates one IddCx monitor and publishes its frames and
hardware cursor through the version-1 `VMLABDSP` IVSHMEM protocol. The binding
contract is `.scratch/plans/vfio-display/CONTRACT.md` in the main vmlab checkout.
The driver validates the mapping before writing to it. Frame publication uses
three rotating buffers and a hardware fence that also orders write-combined
stores before the sequence increment and doorbell.

## Build and sign a package

The host needs Python 3.11 or later, Docker for the container route, a built
vmlab CLI, the local Windows 11 `standard` template, and the config-weave Windows
binary in vmlab's configured search path. No passthrough GPU is required.

From the repository root:

```sh
cargo build --bin vmlab
docker build -f build/Dockerfile -t vmlab-build .
python3 guest/vdisplay-windows/rebuild.py --route container
```

The container compiles with Clang's clang-cl driver and lld-link against
hash-locked SDK, WDK, and CRT packages. Windows runs the pinned Inf2Cat and
SignTool binaries to generate the catalog and test-sign the DLL and catalog.
Each build creates a test certificate. Signatures therefore differ between builds.

The Windows-only alternative uses the same sources and kits:

```sh
python3 guest/vdisplay-windows/rebuild.py --route windows
```

`build-vm/vmlab.wcl` declares the build machine. Its `toolchain` config-weave
playbook installs the pinned Visual Studio Build Tools C++ workload and SDK,
then installs the matching WDK and SDK NuGet packages. The bootstrapper, channel,
catalog, and kit downloads are SHA256-checked. `vmlab up` converges those tools.
`build.ps1` invokes MSVC directly, then generates and signs the package.

`rebuild.py` creates a private runtime and state directory, boots headlessly in a
24 GiB systemd scope without swap, transfers inputs with `vmlab cp`, and builds
with `vmlab exec`. It pulls the signed ZIP with `vmlab cp`, checks its manifest
against the current source files, and stages `target/signed-package/`.
It stops its build lab and isolated supervisor on exit. Do not run two rebuilds
against the same build-lab checkout concurrently.

The preferred route is container compilation with Windows catalog generation.
The compiler and linker work on Linux. Inf2Cat and SignTool are Windows SDK/WDK
executables; the Windows lab makes that packaging step repeatable without Wine.

`build.py` alone produces an unsigned package. `guest/package.sh` requires a
verified signed package and includes it under `vdisplay-windows/x86_64/` in the
guest bundle. Set `VMLAB_VDISPLAY_PACKAGE_DIR` to use another signed package.
Source-derived INF versions prevent Windows from reusing an older Driver Store
copy when testing a rebuilt DLL.

Release CI uses the same split. Its `vdisplay-build` job compiles on Ubuntu,
`vdisplay-sign` catalogs and signs on Windows, and `guest-assets` downloads the
signed package before building the bundle. Those jobs run only for a release.

## Install on an opted-in guest

The companion host-device change supplies `gpu.virtual_display`. Enable it on a
Windows passthrough VM and boot that VM in Test Mode with Secure Boot disabled.
Then run this command from its lab:

```sh
vmlab machine install-display win
```

For a development package, add `--package` with the signed package directory.
The command checks the opt-in and the running configuration before transferring
anything. It uses the SYSTEM agent channel, not the general bootstrap ISO.
Guests without the opt-in receive no driver, certificate, or configuration change.

The installer verifies the pinned Red Hat catalog signer before trusting its
publisher for unattended installation. It trusts the generated test certificate,
installs IVSHMEM, creates or updates the persistent root display device, and
checks that the device starts. Console selection persists the indirect monitor
as the sole active display so Windows can put sign-in there too. The
`VMLabDisplayConsole` LocalSystem service maintains that selection after boot,
console-session changes, and host mode requests. Its worker runs in the console
session, attaches to the input desktop, and changes the display configuration
only when the active topology or preferred geometry differs.

The IVSHMEM binary is from upstream virtio-win 0.1-161, driver version
100.76.104.16100. Its archive hash is in `dependencies.json`. The standard
virtio-win 0.1.271 ISO and upstream 0.1-304 archive do not contain this driver.

## Check publication without a passthrough GPU

Use an isolated Windows VM with QEMU's `ivshmem-plain` device and a shared
128 MiB memfd. Find that VM's `memory-backend-memfd` descriptor under `/proc`.
Do not select a descriptor from another VM.

`shm.py init <descriptor>` initializes an empty region. `shm.py read <descriptor>`
reports frame geometry, sequence, buffer index, heartbeat, cursor fields, and a
frame hash. `shm.py request <descriptor> --width 1366 --height 768` requests a
mode. The guest acknowledges a request only after publishing that geometry.
The memfd must be initialized again if the QEMU process is replaced.

`check.cpp` exercises header validation, row pitch, buffer rotation, sequence
wrap, mode selection, and generated EDID. Compile it with AddressSanitizer and
UndefinedBehaviorSanitizer for the portable checks. Live Windows checks remain
necessary for IddCx, cursor updates, sign-in, and mode requests.

Startup diagnostics use the Windows Application log's `VMLabDisplay` source.
PnP failures also appear in the Kernel-PnP configuration log. A successful driver
package install alone does not prove that the display device started.

## Check H1 without a host GPU

`test-hv-launch.py` adds H1's real virtual-display device to a generated machine
description. Use it only in an isolated test lab with `hypervisor = "vmlab"`,
`gui = false`, and no `gpu {}` block. It refuses descriptions containing VFIO
devices. This bypasses the production configuration's passthrough requirement
for driver testing; it does not emulate the shared-memory device.

Set `VMLAB_HV_TEST_BINARY` to the H1 `vmlab-hv` executable before starting the
isolated supervisor. In that runtime's private `XDG_CONFIG_HOME/vmlab/config.wcl`,
set `host.vmlab_hv` to the absolute path of `test-hv-launch.py`. Boot inside the
same memory-capped scope used for the QEMU check. Install the signed package
through the SYSTEM agent with `install.ps1 -VirtualDisplayEnabled`.

H1 initializes its own C2 header. Read its `vmlab-vdisplay` memfd without running
`shm.py init`. Use `vmlab vm screenshot`, `mouse-move`, and `sendkeys` to check the
native display path without opening a viewer. Stop the lab and its isolated
supervisor when finished. See `TESTING.md` for the checks run for this change.

## Sources and licenses

The IddCx lifecycle follows Microsoft's sample at
https://github.com/microsoft/Windows-driver-samples/tree/main/video/IndirectDisplay.
That repository currently licenses the sample under MS-PL, not MIT.
`driver.cpp` retains that license and attribution in `LICENSE-Microsoft.txt`.
The IVSHMEM ABI and binary use the virtio-win project's public interface and
BSD-style license, reproduced in `LICENSE-IVSHMEM.txt`.
