# Run vmlab in a container

## Purpose

Run a lab unprivileged inside Docker/Podman with only /dev/kvm.

## Prerequisites

- The host exposes /dev/kvm (else vmlab falls back to slow TCG).
- The vmlab image is built or pulled.

## Flowchart

![diagram](../_wdoc/process_run_in_container-diagram-1.svg)

## Steps

### Step 1: Build (or pull) the image

```console
$ docker build -t vmlab -f Containerfile .   # from the repo (or: just image)
$ docker pull ghcr.io/<owner>/vmlab:latest   # or a published release
```

> [!NOTE]
> **Build context**
> WCL and wscript are git dependencies (fetched during the build), so the context is just the vmlab repo. The image bundles the `vmlab` CLI and the `vmlab-web` UI server.

Build with `just image` (or the command above), or skip building — every release is published to GHCR as `ghcr.io/<owner>/vmlab:<version>` (and `:latest`).

### Step 2: Run a lab

```console
$ docker run --rm -it --device /dev/kvm \
    -v ~/.local/share/vmlab/templates:/root/.local/share/vmlab/templates \
    -v "$PWD":/lab -w /lab vmlab vmlab up
```

> [!TIP]
> **Only /dev/kvm**
> No --privileged, no extra capabilities, no host network mode — the fabric is entirely userspace.

Mount the template store (persistent) and the lab directory, grant `--device /dev/kvm`, and run a vmlab verb (the command above overrides the default). By default the container serves the web UI (`vmlab-web` on :7878 — see the `docker compose` stack); for CLI use, override the command or drive a running container via `docker exec <ctr> vmlab ...`.

> [!TIP]
> **Verification**
> `vmlab status` (via `docker exec` or in the one-shot command) reports the lab running; no KVM-fallback warning appears in the logs.

## Related

- [Containers](../references/concept_containers.md)

- [WSL2](../references/concept_wsl2.md)

- [Daemon model](../references/concept_daemon_model.md)

[← Back to SKILL.md](../SKILL.md)
