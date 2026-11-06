# End-to-end suite

This suite exercises every user-facing vmlab feature through the `vmlab` CLI. It runs inside a Docker container that holds vmlab built from this checkout and every host tool vmlab drives.

```sh
just e2e::run                        # every scenario, about 16 minutes
just e2e::run --only core guest      # named scenarios only
just e2e::run --only dev --keep      # leave the labs up for inspection
just e2e::run --list                 # scenario names
just e2e::run --hypervisor qemu      # explicit QEMU coverage
```

The host needs Docker and a usable `/dev/kvm`. The container runs privileged, with a 24 GiB memory limit and no swap. If `~/.local/share/config-weave/bin` exists, the playbook scenario mounts it into the container. GPU rendering is skipped unless `VMLAB_E2E_RENDER_NODE` names a host render device designated for testing, such as `/dev/dri/renderD128`. The suite never selects a host GPU automatically.

The image recipe first builds the release toolchain image from `build/Dockerfile`, using `VMLAB_BUILD_IMAGE` or `vmlab-build`. The firmware stage inherits those toolchains and `VMLAB_BUILD_ENV=1`. It copies only the firmware scripts, patches, regression check, Secure Boot manifest and lockfile, iPXE pins and configuration, SeaBIOS inputs, and OVMF bitmap. Docker reuses that stage when unrelated source files change. Verified downloads and Python dependencies use BuildKit caches. The iPXE checkout stays in the firmware layer so a changed pin gets a fresh checkout.

The main build sets `VMLAB_SKIP_FIRMWARE=1` for `build-asset.sh` and builds only the kernel/initramfs and agent. The final image copies firmware directly from the firmware stage. The skip is opt-in; `guest-install`, release bundles and CI retain their normal firmware build. The runtime carries the same branded OVMF variants, Secure Boot VARS, SeaBIOS and iPXE ROMs as `just guest-install`, plus AAVMF for the QEMU administration checks.

## How a run works

- **Base template.** `templates/alpine` builds `x86_64/e2e-alpine` from Alpine's NoCloud cloud image. The build adds the vmlab agent and nothing else: no package installs and no updates. A build takes about 45 seconds once the image is cached.
- **Download cache.** The Docker volume `vmlab-e2e-cache` keeps vmlab's downloaded artefacts between runs. `just e2e::cache-clean` drops the volume.
- **Registry.** A plain registry runs on `localhost:5000` inside the container for push and pull. vmlab speaks plain HTTP only to `localhost`.
- **Scenarios.** Each scenario in `scenarios/` copies its lab from `labs/` into the container, drives it, and destroys it afterwards. `run.py` runs the scenarios in the order listed in its `SCENARIOS`. When `template` is not selected, the runner builds the base template first.
- **Results.** Each scenario reports every feature it covers against the catalogue in `features.py` as pass, fail, or skip with a reason. A feature that no scenario reported counts as missing, and a missing feature fails the run.
- **Output.** A run writes to `e2e/results/`:
  - `results.json`: one row per feature, naming the hypervisor the run used.
  - `junit.xml`: the same rows as JUnit XML, one test case per feature, for `terrarium test push`. A failed or missing feature is a failure, a skipped one is skipped.
  - `e2e.log`: every command the run executed, with its output.
  - `registry.log`: the registry's log.

## Hypervisors

A run defaults to `--hypervisor vmlab` (PRD §22); `--hypervisor qemu` retains QEMU coverage. The image builds `vmlab-hv` from the vmlab-hypervisor commit that `Cargo.lock` pins `vmlab-hv-proto` to, so the binary always speaks the protocol this vmlab was built against. Its virtio-gpu needs virglrenderer 1.3, newer than Debian trixie's, so the image builds that from source under `/opt/virgl` and `vmlab-hv` finds it by rpath; QEMU keeps the distro's. It is built without PipeWire, so an audio VM plays into vmlab-hv's null sink: `vm.hw.audio` checks the guest's sound card and the node vmlab names, and the host stream is a live check.

The runner writes an explicit host config for both machine kinds in every scenario. On `vmlab`, containers use vmlab-hv, and VMs do too when the scenario covers a delivered VM phase. Other VMs are explicitly pinned to QEMU. A QEMU run pins both kinds to QEMU. `QEMU_SCENARIOS` names exceptions and their reasons. `fastpath` remains pinned to QEMU until N6-N8 lands. A VM feature it reports is skipped on a vmlab run, naming the reason, and never overwrites another scenario's pass.

`core` keeps the four-machine `core-lab` on QEMU. On vmlab-hv it runs three labs in sequence. Normal cleanup keeps at most two VMs running at once. `core-hv` covers lifecycle, hardware and parallel provisions with two machines. `core-hv-provision` checks a provision dependency with two machines. `core-hv-scratch` checks the blank-disk boot and forced stop with one machine. The labs reuse `core-lab`'s media and provision script. QEMU arguments and both floppy features retain their QEMU-only or phase-4 skip reasons. Reload checks edit QEMU's SMBIOS arguments on QEMU and memory on vmlab-hv. They check the live launch and the guest, including a pending edit, restart, a stopped-machine rebuild, and an unchanged peer PID. `core-unmountable` runs on both hypervisors.

`admin` keeps its aarch64 `admin-lab` and emulation check on QEMU. On vmlab-hv it uses the x86_64 scratch VM in `admin-hv` for the same administration checks and skips only the phase-3 emulation feature. `admin-roots` runs on both. Process checks identify a live QEMU by its lab name or a live vmlab-hv by its exact `--machine` path, so a leftover description file cannot pass a shutdown check.

`core-admin-coverage.md` lists the 33 formerly suppressed features, their original labs, and the reason each was skipped.

The suite's VM template is Alpine's UEFI image, except on a vmlab run delivered through 2a or 2b, where it is Alpine's BIOS image with `firmware = "seabios"`.

Every feature in `features.py` carries a phase tag in `PHASES`:

- `host`: no hypervisor takes part, such as the template store, validation or the supervisor. The feature runs on both.
- `1`: container micro-VMs.
- `2a` to `2f`: modern x86_64 VMs: the q35 platform on SeaBIOS (2a), the native frame protocol (2b), OVMF, secure boot, the TPM and Windows (2c), GPU (2d), virtio-fs and audio (2e), USB and vfio (2f).
- `3`: TCG guests.
- `4`: the legacy PC.

`QEMU_ONLY` lists the features no other hypervisor runs, each with its reason: `qemu_args` and the external-virtiofsd checks.

On `vmlab`, a feature past the delivered phase (`VMLAB_DELIVERED`, or `--vmlab-phase`) is reported as skipped with the reason `vmlab-hv phase N`, whatever its scenario did. A pass from a VM that ran on QEMU says nothing about vmlab-hv. A scenario with no feature in scope does not run. Every feature still passes, fails or skips with a reason, and each row in `results.json` names the run's hypervisor. vmlab-hv's parity gate for a phase is a `--hypervisor vmlab` run that passes every feature in that phase.

## Adding coverage

To cover a new feature, add it to `features.py` and tag its phase, then report it from a scenario with `h.ok`, `h.check` or `h.skip`. `features.py` refuses to load while a feature has no tag. `harness.py` documents the API.

Run `python3 -m unittest discover -s e2e -p 'test_*.py'` to check hypervisor routing and result accounting without booting VMs.

Keep labs small: 1 CPU, 512 MiB to 1 GiB of memory, and the e2e template or small container images. `vmlab up` returns once machines have started, before they are ready. Call `h.wait_ready` before talking to a guest.

## Legacy PC gate

The `legacy` scenario boots FreeDOS 1.3, Windows 98 SE, Windows 7 7600.0.1 and
TempleOS sequentially. It checks agent readiness, exec, a disk write/read,
a PNG screenshot and emulator exit status zero after `vm stop`. Windows 98
may reach the emulator-quit rung after ignoring guest shutdown. That is a
flushed emulator exit, not proof that Windows shut itself down.

The scenario uses these installed templates and disables automatic agent
refresh:

```text
x86/freedos-1.3@1.3.0
x86/windows-98-se@4.10.2222.5
x86_64/windows-7@7600.0.1
x86_64/templeos@5.03.2
```

If a template is missing, the scenario skips with the missing reference.
`VMLAB_E2E_LEGACY_STORE` can name a populated source store. The scenario copies
only its four template versions into the run's store, using sparse files and
reflinks where available. With `just e2e::run`, set `VMLAB_E2E_LEGACY_STORE` on
the host to the source store; the recipe mounts it read-only at `/legacy-store`
and sets the variable to that path inside the container, so the source is never
written. The recipe's 24 GiB container limit applies.
`--only legacy` does not build the unrelated Alpine base template.

Before each boot, the scenario waits until fewer than two project VMs run on
the host, excluding `qemu-system-* -avd` Android emulators. Containers need
`--pid=host` for that count. On a shared development host, run the VM-owning
processes inside `systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0`.
The four `vm.legacy.*` features remain tagged phase 4 and skip on vmlab-hv
until that phase is delivered.
