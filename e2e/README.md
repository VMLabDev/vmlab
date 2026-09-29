# End-to-end suite

This suite exercises every user-facing vmlab feature through the `vmlab` CLI. It runs inside a Docker container that holds vmlab built from this checkout and every host tool vmlab drives.

```sh
just e2e::run                        # every scenario, about 16 minutes
just e2e::run --only core guest      # named scenarios only
just e2e::run --only dev --keep      # leave the labs up for inspection
just e2e::run --list                 # scenario names
```

The host needs Docker and a usable `/dev/kvm`. The container runs privileged. If `~/.local/share/config-weave/bin` exists, the playbook scenario mounts it into the container.

## How a run works

- **Base template.** `templates/alpine` builds `x86_64/e2e-alpine` from Alpine's NoCloud cloud image. The build adds the vmlab agent and nothing else: no package installs and no updates. A build takes about 45 seconds once the image is cached.
- **Download cache.** The Docker volume `vmlab-e2e-cache` keeps vmlab's downloaded artefacts between runs. `just e2e::cache-clean` drops the volume.
- **Registry.** A plain registry runs on `localhost:5000` inside the container for push and pull. vmlab speaks plain HTTP only to `localhost`.
- **Scenarios.** Each scenario in `scenarios/` copies its lab from `labs/` into the container, drives it, and destroys it afterwards. `run.py` runs the scenarios in the order listed in its `SCENARIOS`. When `template` is not selected, the runner builds the base template first.
- **Results.** Each scenario reports every feature it covers against the catalogue in `features.py` as pass, fail, or skip with a reason. A feature that no scenario reported counts as missing, and a missing feature fails the run.
- **Output.** A run writes to `e2e/results/`:
  - `results.json`: one row per feature.
  - `e2e.log`: every command the run executed, with its output.
  - `registry.log`: the registry's log.

## Adding coverage

To cover a new feature, add it to `features.py`, then report it from a scenario with `h.ok`, `h.check` or `h.skip`. `harness.py` documents the API.

Keep labs small: 1 CPU, 512 MiB to 1 GiB of memory, and the e2e template or small container images. `vmlab up` returns once machines have started, before they are ready. Call `h.wait_ready` before talking to a guest.
