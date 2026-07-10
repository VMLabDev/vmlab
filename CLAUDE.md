# CLAUDE.md

Project context for Claude Code.

## Project Purpose

**vmlab** is a VM lab management tool written in Rust. This is a fresh
rewrite; the product requirements live in `docs/vmlab-prd.md` — read that first;
it is the source of truth for design and scope.

Many earlier attempts are archived under
github.com/wiltaylor/.graveyard-private — notably `vmlab_qemu` (QMP/QGA
driver crate), `vmlab_oci` (OCI registry client for VM disk images), and
`vmlab_floppy` (pure-Rust FAT for floppy images), all buried 2026-06-12.
Consult them for prior art only — the PRD overrides anything they did.

## Status

PRD implemented (M1–M6). Module map under `src/`:

- `config/` — WCL schema, typed model, §5.1 validation, host config, profiles.
- `profiles/` — guest OS profiles (WCL data, user-overridable).
- `qemu/` — hardware resolution (VM>template>profile), cmdline builder,
  firmware lookup, process management; `container.rs` builds the micro-VM
  argv for lab containers (§18).
- `qmp/`, `qga/` — QMP and guest-agent clients.
- `template/` — store, qemu-img, builds, artefact cache, store/OCI CLI.
- `oci/image/` — standard container-image pull: docker/OCI manifests,
  layer flatten (whiteouts → squashfs via sqfstar), digest-addressed cache.
- `guest_asset.rs` + `guest/` — the container micro-VM kernel/initramfs:
  `vmlab-cinit` (guest PID 1), `cinit-proto` (host↔init contract, shared
  crate), `build-asset.sh` (pinned Alpine, rootless build).
- `media/` — folder → ISO/floppy with content-addressed cache.
- `vision/` — screenshot, template matching, OCR.
- `net/` — userspace fabric: frame codecs, L2 switch, DHCP, DNS, gateway,
  NAT engine, L3 rules.
- `proto/` — JSON-lines daemon wire protocol (client + server).
- `supervisor/` — `vmlabd`: lab registry, global segments, watchdogs.
- `labd/` — per-lab daemon: lifecycle, snapshots, network assembly, events,
  SMB integration, the lab runtime the wscript host binds to;
  `container.rs`/`container_ctl.rs` run OCI containers as micro-VMs (§18).
- `scripting/` — wscript host module (lab/VM/segment API), provisions, handlers.
- `smb/` — bundled-smbd shared folders.
- `oci/` — OCI registry push/pull (chunked, multi-arch).
- `cli/` — the `vmlab` verb surface.
- `web/` — the `vmlab-web` binary (Actix-web): REST + WebSocket API over the
  proto client, an embedded SolidJS console UI (`web-ui/`, rust-embed), live
  noVNC over a `vnc.sock` WebSocket bridge, and username/password auth. Behind
  the optional `web` feature; the crate also exposes a `[lib]` so this binary
  reuses `proto`/`paths`/`cli`.

`docs/vmlab-prd.md` remains the binding contract; section refs (`§N`) appear
throughout the code and commit messages.

## Conventions

- Trunk-based development: commit directly to `main`, no branches or PRs
  unless explicitly asked.
- **just** as command runner: `just build` / `just test` / `just check`
  (lint + fmt check + tests). Justfile follows the norms in the justfile
  skill (groups, doc comments, `[private]`, noun-verb naming).
- Standard Rust toolchain: `cargo build`, `cargo test`, `cargo clippy`,
  `cargo fmt`.
