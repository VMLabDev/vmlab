# Run vmlab in a container

## Purpose

Run a lab inside Docker/Podman with KVM and optional eBPF network acceleration.

## Prerequisites

- The host exposes /dev/kvm (else vmlab falls back to slow TCG).
- For eBPF acceleration, the host exposes /dev/net/tun and permits CAP_BPF + CAP_NET_ADMIN.
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
$ docker run --rm -it --device /dev/kvm --device /dev/net/tun \
    --cap-add BPF --cap-add NET_ADMIN -e VMLAB_FASTPATH=auto \
    -v ~/.local/share/vmlab/templates:/root/.local/share/vmlab/templates \
    -v "$PWD":/lab -w /lab vmlab vmlab up
```

> [!TIP]
> **Least privilege**
> No --privileged or host network mode. The eBPF path receives only /dev/net/tun, CAP_BPF, and CAP_NET_ADMIN; it falls back to userspace when unavailable.

Mount the template store (persistent) and the lab directory, grant `/dev/kvm`, and add `/dev/net/tun` plus `CAP_BPF` and `CAP_NET_ADMIN` when eBPF acceleration is wanted. Run a vmlab verb (the command above overrides the default). By default the container serves the [web console](../references/concept_web_console.md) ([vmlab-web](../references/entity_vmlab_web.md) on :7878 — see the `docker compose` stack); for CLI use, override the command or drive a running container via `docker exec <ctr> vmlab ...`.

> [!TIP]
> **Verification**
> `vmlab status` (via `docker exec` or in the one-shot command) reports the lab running; no KVM-fallback warning appears in the logs.

## Related

- [Containers](../references/concept_containers.md)

- [WSL2](../references/concept_wsl2.md)

- [Daemon model](../references/concept_daemon_model.md)

[← Back to SKILL.md](../SKILL.md)
