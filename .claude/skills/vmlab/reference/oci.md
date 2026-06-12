# OCI registry distribution reference

Templates distribute as **OCI artifacts** (not runnable container images)
through any OCI-compliant registry: GHCR, Docker Hub, Harbor, self-hosted.

## Login

```sh
vmlab template login ghcr.io -u myuser -p <token>
```

Validates against the registry's `/v2/` endpoint (basic → bearer token
flow) and persists to the Docker config (`~/.docker/config.json`).
Existing `docker login` credentials are reused automatically — no separate
login needed if the machine already has one.

## Push / pull

```sh
vmlab template push x86_64/linux-modern@1.0 ghcr.io/owner/linux-modern:1.0
vmlab template pull ghcr.io/owner/linux-modern:1.0                # single-arch manifest
vmlab template pull ghcr.io/owner/linux-modern:1.0 --arch x86_64  # multi-arch index: --arch required
```

The local side is always a store ref `<arch>/<name>[@<version>]`; the
remote side is a normal registry ref `host/repo:tag`. Arch is never
silently assumed from the host: pulling an ambiguous multi-arch index
without `--arch` is an error.

## Registry refs directly in labs

```wcl
vm "box" {
  template = "ghcr.io/owner/linux-modern:1.0"
  arch     = "x86_64"     // explicit arch is required with registry refs
  memory   = "4G"
}
```

`vmlab up` pulls the template into the store if absent. It never re-pulls
implicitly when present — updates are explicit via `vmlab template pull`.

## Artifact model (what's actually in the registry)

- Artifact type: `application/vnd.vmlab.template.v1` (frozen; prevents
  `docker run` misuse). Config blob:
  `application/vnd.vmlab.template.config.v1+json` (template metadata).
- The qcow2 is **chunked** into fixed-size zstd-compressed layers
  (`application/vnd.vmlab.template.chunk.v1+zstd`), default **512 MiB**
  per chunk — sized so each upload clears GHCR's 10-minute per-upload
  timeout. Configurable via `oci_chunk_size` in the host config
  (`~/.config/vmlab/config.wcl`).
- Manifest annotations (`vnd.vmlab.template.*`) record chunk count/size,
  total size, and the whole-image digest; pull reassembles in order and
  verifies the whole-image SHA-256 before installing to the store.
- Multi-arch: a standard OCI image index keyed by platform arch; push the
  same name from each arch and the index resolves per platform.

Source of truth: PRD §6.4; `src/oci/` (`media_types.rs`, `manifest.rs`,
`auth.rs`), `src/template/cli.rs`.
