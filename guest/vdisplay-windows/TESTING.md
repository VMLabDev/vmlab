# Windows display verification

Verified on 2026-11-04 with the local `x86_64/windows-11#standard` template:
Windows 11 Enterprise Evaluation, build 26100, x64, Test Mode, Secure Boot off.
All VMs were headless, used private runtime/state/config directories, and ran
under `MemoryMax=24G` with `MemorySwapMax=0`. No host PCI or USB device was used.

## Builds

The preferred route passed end to end:

```sh
VMLAB_BUILD_IMAGE=vmlab-build-vdisp python3 guest/vdisplay-windows/rebuild.py --route container
```

The image was built from this checkout's `build/Dockerfile`. Ubuntu Clang 18
compiled both binaries with `/W4 /WX`; lld linked the UMDF and IddCx stubs.
The build lab generated the catalog, signed the package, and returned its ZIP
through `vmlab cp`. Source and package manifests verified successfully.

The alternative MSVC build also compiled and signed successfully through
`build.ps1 -Mode msvc` in that lab. The playbook installed Visual Studio Build
Tools 17.14.37827.9 with MSVC 14.44.35207 and SDK/WDK packages 10.0.26100.1.
A second `vmlab up` reported the toolchain already configured. The MSVC package
was pulled with `vmlab cp`, normalized from Windows ZIP paths, and verified.
Live driver checks used the Clang-built package.

Windows reported valid Authenticode signatures for the installed DLL, generated
catalog, and pinned upstream IVSHMEM catalog. The display device reported `OK`,
and the console service reported `Running` and `Automatic`. Reinstallation also
passed. The installed final driver version was `1.0.52624.1918`.

## Live display paths

QEMU used the task's 128 MiB memfd and `ivshmem-plain` stand-in. C2 publication
showed the lock screen and password screen, changing frame sequences, heartbeat,
cursor shape/position, and acknowledged 1920x1080, 1366x768, 1280x720, and
1600x900 requests. Reboot restored the lock screen without a user login.

The H1 end-to-end check used `vmlab-hv` commit `8d6331f`, launched through
`test-hv-launch.py` with no VFIO devices. `vmlab vm screenshot` captured the
lock screen, password screen, and logged-in desktop through the native display
socket. C2 requests changed 1920x1080 to 1366x768 and then 1280x720 with matching
acknowledgements. Mouse input at 500,300 produced hotspot coordinates 500,300,
a visible 41x39 cursor, and hotspot 3,3. Fourteen sampled frames rotated through
buffers 0,1,2; sequences 1062 through 1067 used 0,1,2,0,1,2. A guest reboot
restored publication and the console service before login, at the default
1920x1080 after H1 reset its header.

## Automated checks

- `clang++ -std=c++17 -Wall -Wextra -Werror -fsanitize=address,undefined guest/vdisplay-windows/check.cpp`: passed validation, row pitch, rotation, sequence wrap, modes, and EDID checks.
- `python3 -m unittest discover -s guest/vdisplay-windows -p 'test_*.py'`: two package-transfer tests passed, including Windows separators and traversal rejection.
- `cargo test --lib cli::display_install::tests`: both opt-in and incomplete-package tests passed.
- `python3 -m py_compile guest/vdisplay-windows/*.py` and `bash -n guest/package.sh`: passed.
- `package.py` staged the verified package into a guest-bundle directory; a tar round trip preserved its members.
- `actionlint` 1.7.12 with ShellCheck disabled accepted `.github/workflows/ci.yml`; a YAML parse checked the new artifact-job dependencies and release gating.
- `just ci::check`: passed once, including fmt, clippy, host and guest checks, BPF verification, and agent conformance. Host library tests reported 2121 passed and 11 ignored; conformance reported 22 passed and no skips.

The actual `machine install-display` command refused the non-opted-in test VM
before transferring files. Positive live installation used the SYSTEM agent and
the same package script directly because this no-GPU test lab deliberately
cannot satisfy the production passthrough configuration.

## Checks left to the owner

No real passthrough-GPU check ran because no test PCI device was designated.
The full hypervisor gate was not run, per the owner's merge-batch policy. No
hypervisor source changed. The full multi-architecture guest release bundle was
not rebuilt; its changed Windows staging and archive path were exercised alone.
The new release-only GitHub Actions jobs were validated locally, but no release
was triggered to run them on GitHub-hosted runners.
