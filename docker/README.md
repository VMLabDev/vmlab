# Running vmlab in Docker

The image (`Containerfile` at the repo root) ships both the `vmlab` CLI and the
`vmlab-web` UI server. By default a container runs the web UI on port `7878`.

## Quick start (compose)

```sh
docker compose up --build
```

Then open <http://localhost:7878> and sign in. Credentials come from the
environment (defaults `vmlab` / `vmlab`); override them:

```sh
VMLAB_WEB_USER=me VMLAB_WEB_PASSWORD=s3cret docker compose up --build
```

> The web UI binds `0.0.0.0` inside the container so the published port works,
> and vmlab-web requires a login on any non-loopback bind. **Change the default
> password before exposing this beyond your machine.**

## Your lab

Put your `vmlab.wcl` in `docker/lab/` — it's bind-mounted to `/lab` in the
container, so edit it on the host with your normal editor. A sample lab is
provided (`docker/lab/vmlab.wcl`, an Alpine VM from a public OCI template).
Templates are pulled on the first `up` into the persistent `vmlab-templates`
volume.

KVM acceleration uses the host's `/dev/kvm` (mapped in `compose.yaml`); remove
that device mapping to fall back to slower TCG emulation.

The Compose stack also enables vmlab's optional eBPF network fast path by
mapping `/dev/net/tun` and adding only `CAP_BPF` and `CAP_NET_ADMIN`. It does
not use privileged or host-network mode. `VMLAB_FASTPATH=auto` probes the AF_XDP
tier at startup and falls back to the userspace fabric if the host cannot use
it. Inspect the selected tier with:

```sh
docker compose exec vmlab-web vmlab fastpath
```

The available modes are `auto` (recommended), `afxdp` (force the AF_XDP probe),
`sockmap` (evaluation only; currently slower than userspace), and `off`.

## The Active Directory demo lab (config-weave playbooks)

Besides the default `/lab`, the Compose stack mounts `docker/labs/ad-demo`
into the managed labs home, so **ad-demo** shows up in the web UI's lab
picker. It demonstrates vmlab's `playbook {}` blocks — [config-weave]
playbooks copied into the guests and applied during `up`:

- `dc01` is promoted to the forest root of `corp.example.com` (AD DS role →
  `Install-ADDSForest` → wait for AD to answer) and serves DNS for the
  segment.
- `srv01` declares `depends_on = ["dc01"]`, so it boots only after the DC's
  playbook has converged, then joins the domain.

Both plays end in reboot-required resources; vmlab reboots each guest and
re-runs the apply until config-weave reports it converged. Watch it live on
each machine's **Playbook** tab, re-run `check`/`apply` from there (the
playbook folder is re-pushed on every run), and open the playbook's file
tree in the built-in editor from the designer's playbook node.

Two prerequisites:

1. Drop the config-weave guest binaries in `docker/config-weave/` — see
   [docker/config-weave/README.md](config-weave/README.md).
2. The Windows Server 2025 template (~12 GB) is pulled from ghcr on the
   first `up`.

Bring it up from the UI (pick *ad-demo*, press *Start all*) or:

```sh
docker compose exec vmlab-web sh -c 'cd /root/.local/share/vmlab/labs/ad-demo && vmlab up'
```

The first `up` takes a while: two Windows clones specialize, then the DC
promotion and the domain join each cross a reboot. Sign-in after: pick the
VM's Console tab — `CORP\Administrator` / `vmlab123!` (demo credentials, set
in `docker/labs/ad-demo/playbooks/active-directory/playbook.wcl`).

[config-weave]: https://github.com/Configweave/config-weave

## Sharing files into your VMs

Drop files in `docker/share/` on the host — it's bind-mounted to `/share` in
the container (`compose.yaml`) — and share that path into a VM from your
`vmlab.wcl`:

```wcl
vm "alpine" {
  # ...
  share { host = "/share" guest = "/mnt/share" }   // Linux guest
}

vm "winsrv" {
  # ...
  share { host = "/share" guest = "S:" }           // Windows guest (drive)
}
```

vmlab serves the directory over SMB and auto-mounts it in the guest once the
guest agent responds. The `host` path is absolute, so it's used as-is; a
relative `host = "./sub"` would instead resolve against the lab directory
(`/lab`).

**Guest prerequisites (Linux only).** Windows mounts the share natively. A
Linux guest needs two things the mount step does *not* provide for you:

- `cifs-utils` installed (the `mount.cifs` helper — kernel CIFS alone isn't
  enough), and
- the mount point (e.g. `/mnt/share`) to already exist.

The shipped sample lab handles both in `docker/lab/provision.ws`
(`apk add --no-cache cifs-utils` + `mkdir -p /mnt/share`); copy that pattern
for your own Linux guests, or bake the prerequisites into the template.

**Write-back ownership caveat.** The in-container `smbd` runs as `root`, so
files a guest *writes* into the share land on the host owned by `root:root`
(you may need `sudo` to remove them). If you need host-user ownership, run the
container as your uid (add `user: "${UID}:${GID}"` to the compose service, with
matching host-file permissions) or append `uid=`/`gid=` to the guest's cifs
mount options via a provision.

## Plain `docker run`

```sh
# Web UI (add -v "$PWD/share":/share to share host files into the VMs)
docker run --rm -p 7878:7878 --device /dev/kvm --device /dev/net/tun \
  --cap-add BPF --cap-add NET_ADMIN -e VMLAB_FASTPATH=auto \
  -e VMLAB_WEB_USER=admin -e VMLAB_WEB_PASSWORD=secret \
  -v "$PWD":/lab -v "$PWD/share":/share vmlab:latest

# CLI (override the default command)
docker run --rm --device /dev/kvm --device /dev/net/tun \
  --cap-add BPF --cap-add NET_ADMIN -e VMLAB_FASTPATH=auto \
  -v "$PWD":/lab vmlab:latest vmlab up
```
