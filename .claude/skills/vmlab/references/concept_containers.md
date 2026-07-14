# Containers

_vmlab runs in Docker/Podman with /dev/kvm; optional eBPF networking adds /dev/net/tun, CAP_BPF and CAP_NET_ADMIN._

vmlab runs unprivileged. The container image is defined by `Containerfile`; WCL and
wscript are git dependencies (fetched during the build), so the \*\*build context is
just the vmlab repo\*\*. The image bundles the `vmlab` CLI and the `vmlab-web` UI
server, and is published per release as `ghcr.io/<owner>/vmlab:<version>` (and
`:latest`).


```console
docker build -t vmlab -f Containerfile .   # from the repo (or: just image)

docker run --rm -it --device /dev/kvm --device /dev/net/tun \
  --cap-add BPF --cap-add NET_ADMIN -e VMLAB_FASTPATH=auto \
  -v ~/.local/share/vmlab/templates:/root/.local/share/vmlab/templates \
  -v "$PWD":/lab -w /lab vmlab vmlab up
```

`--device /dev/kvm` is the **only host grant needed for KVM**. Without it,
vmlab falls back to slow TCG emulation with a loud warning. The optional eBPF
network fast path additionally uses `/dev/net/tun`, `CAP_BPF`, and
`CAP_NET_ADMIN`; it probes the host and falls back to userspace when unavailable.
Neither mode needs `--privileged` or host networking. By default the container
serves the web UI (`vmlab-web` on :7878 — see the `docker compose` stack); override
the command for one-shot/CI CLI use, or drive a running container via
`docker exec <ctr> vmlab ...`.


## Related

- [Daemon model](../references/concept_daemon_model.md)

- [Networking model](../references/concept_networking.md)

- [WSL2](../references/concept_wsl2.md)

[← Back to SKILL.md](../SKILL.md)
