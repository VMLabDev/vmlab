# End-to-end suite

This suite exercises every user-facing vmlab feature through the `vmlab` CLI. It runs inside a Docker container that holds vmlab built from this checkout and every host tool vmlab drives.

```sh
just e2e::run                        # every scenario, about 16 minutes
just e2e::run --only core guest      # named scenarios only
just e2e::run --only dev --keep      # leave the labs up for inspection
just e2e::run --list                 # scenario names
just e2e::run --hypervisor vmlab     # the machines on vmlab-hv, by phase
```

The host needs Docker and a usable `/dev/kvm`. The container runs privileged. If `~/.local/share/config-weave/bin` exists, the playbook scenario mounts it into the container.

## How a run works

- **Base template.** `templates/alpine` builds `x86_64/e2e-alpine` from Alpine's NoCloud cloud image. The build adds the vmlab agent and nothing else: no package installs and no updates. A build takes about 45 seconds once the image is cached.
- **Download cache.** The Docker volume `vmlab-e2e-cache` keeps vmlab's downloaded artefacts between runs. `just e2e::cache-clean` drops the volume.
- **Registry.** A plain registry runs on `localhost:5000` inside the container for push and pull. vmlab speaks plain HTTP only to `localhost`.
- **Scenarios.** Each scenario in `scenarios/` copies its lab from `labs/` into the container, drives it, and destroys it afterwards. `run.py` runs the scenarios in the order listed in its `SCENARIOS`. When `template` is not selected, the runner builds the base template first.
- **Results.** Each scenario reports every feature it covers against the catalogue in `features.py` as pass, fail, or skip with a reason. A feature that no scenario reported counts as missing, and a missing feature fails the run.
- **Output.** A run writes to `e2e/results/`:
  - `results.json`: one row per feature, naming the hypervisor the run used.
  - `e2e.log`: every command the run executed, with its output.
  - `registry.log`: the registry's log.

## Hypervisors

A run is on one hypervisor (PRD §22): `--hypervisor qemu`, the default, or `--hypervisor vmlab`. The image builds `vmlab-hv` from the vmlab-hypervisor commit that `Cargo.lock` pins `vmlab-hv-proto` to, so the binary always speaks the protocol this vmlab was built against. Its virtio-gpu needs virglrenderer 1.3, newer than Debian trixie's, so the image builds that from source under `/opt/virgl` and `vmlab-hv` finds it by rpath; QEMU keeps the distro's. It is built without PipeWire, so an audio VM plays into vmlab-hv's null sink: `vm.hw.audio` checks the guest's sound card and the node vmlab names, and the host stream is a live check.

On `vmlab`, the runner writes a host config whose `hypervisor {}` block puts containers on vmlab-hv, and a scenario's VMs too when it covers a VM feature of a delivered phase (2a onward). It is rewritten per scenario. `QEMU_SCENARIOS` names the scenarios whose VMs stay on QEMU, with the reason: `core` proves most of what it covers from QEMU's argv and its lab declares `qemu_args` and floppies, `admin` declares an emulated aarch64 VM, and `fastpath` attaches its VM NICs as taps, which vmlab-hv does not take. A VM feature such a scenario reports is skipped on a vmlab run, naming the reason, and never overwrites another scenario's pass.

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

Keep labs small: 1 CPU, 512 MiB to 1 GiB of memory, and the e2e template or small container images. `vmlab up` returns once machines have started, before they are ready. Call `h.wait_ready` before talking to a guest.
