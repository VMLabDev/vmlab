# Official vmlab runtime image (PRD §14): the `vmlab` CLI + the `vmlab-web` UI
# server plus their full runtime dependency set. The userspace network fabric
# means the container needs NO --privileged, no extra capabilities, and no host
# network mode — `--device /dev/kvm` is the only host grant needed for
# acceleration (without it, vmlab falls back to TCG with a loud warning).
#
# By default the container runs `vmlab-web` bound to 0.0.0.0:7878 (with
# --no-auth) so the web UI is reachable through a published port with no login.
# Set VMLAB_WEB_USER + VMLAB_WEB_PASSWORD to require a login instead — supplied
# credentials take precedence over --no-auth:
#
#   docker run --rm -p 7878:7878 --device /dev/kvm \
#     -e VMLAB_WEB_USER=admin -e VMLAB_WEB_PASSWORD=secret \
#     -v "$PWD":/lab vmlab
#
# The CLI is still available by overriding the command:
#
#   docker run --rm --device /dev/kvm -v "$PWD":/lab vmlab vmlab up
#   docker exec <container> vmlab status
#
# Build:  docker build -t vmlab -f Containerfile .      (context = this dir)
#    or:  just image      /      docker compose build
#
# WCL + wscript are git dependencies (fetched during the cargo build), so the
# build context is just this repository — no sibling checkouts required.

# ---- frontend ---------------------------------------------------------------
# Build the SolidJS web UI; the output is embedded into vmlab-web (rust-embed).
# pnpm (via corepack, pinned by web-ui's packageManager field): the @forge/*
# deps are git-subdir deps, which npm cannot install.
FROM node:22-bookworm-slim AS web
WORKDIR /web
RUN corepack enable
# No lockfile on purpose (see web-ui/.gitignore) — fresh resolution extracts
# the @forge/* git-subdir deps correctly; their revs are pinned in package.json.
COPY web-ui/package.json web-ui/pnpm-workspace.yaml ./
RUN pnpm install
COPY web-ui/ ./
RUN pnpm build

# ---- builder ----------------------------------------------------------------
FROM rust:1.92-bookworm AS builder
WORKDIR /build/vmlab
COPY . .
# Supply the built web assets so rust-embed can bake them into vmlab-web.
COPY --from=web /web/dist ./web-ui/dist
# No --locked: release CI stamps the package version into Cargo.toml, which the
# lockfile would otherwise reject. Deps are still pinned by Cargo.lock.
RUN cargo build --release --features web --bin vmlab --bin vmlab-web

# ---- runtime ----------------------------------------------------------------
FROM debian:bookworm-slim
# QEMU system emulators, firmware, swtpm, OCR, NAT, ISO/floppy tooling, SMB
# server (PRD §14).
RUN apt-get update && apt-get install -y --no-install-recommends \
        qemu-system-x86 \
        qemu-system-arm \
        qemu-utils \
        ovmf \
        seabios \
        swtpm \
        tesseract-ocr \
        passt \
        xorriso \
        mtools \
        dosfstools \
        samba \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# vmlab-web spawns the `vmlab` binary for the supervisor/lab daemons (it locates
# it as a sibling), so both must sit in the same directory.
COPY --from=builder /build/vmlab/target/release/vmlab     /usr/local/bin/vmlab
COPY --from=builder /build/vmlab/target/release/vmlab-web /usr/local/bin/vmlab-web

# Documented volume mounts (PRD §14):
#   /root/.local/share/vmlab/templates  — the template store
#   /var/lib/vmlab/work                  — lab working data (disk clones, media)
#   /lab                                — the lab directory (holds vmlab.wcl)
#   /share                              — host files shared into guest VMs
#                                         (bind-mount from the host; referenced
#                                          as host = "/share" in vmlab.wcl)
# Everything else is container-ephemeral by design. /share is intentionally not
# a declared VOLUME: it is only useful bind-mounted from the host, and a bare
# `docker run` should not mint a stray anonymous volume for it.
#
# The lab's working data (linked disk clones, built ISOs, TPM + lab state) is
# normally written to `<lab>/.vmlab`. With /lab bind-mounted from the host that
# puts heavy, write-churning I/O on the host filesystem — and on Windows that
# bind mount is a slow virtiofs/9p bridge, so disk clones crawl (issue #2).
# VMLAB_WORK_DIR relocates that data onto a container-native volume instead,
# leaving only the editable vmlab.wcl on the (read-mostly) bind mount.
ENV VMLAB_WORK_DIR=/var/lib/vmlab/work
VOLUME ["/root/.local/share/vmlab/templates", "/var/lib/vmlab/work"]
WORKDIR /lab
EXPOSE 7878

# Auto-start the mounted /lab on startup so it is already running when the UI is
# opened. Set VMLAB_WEB_UP=0 to leave it stopped instead — the UI then lists it
# and the user starts it with the "up" button.
ENV VMLAB_WEB_UP=1

# Default: serve the web UI with no login (--no-auth). VMLAB_WEB_UP (above)
# controls whether the lab auto-starts. Setting VMLAB_WEB_USER +
# VMLAB_WEB_PASSWORD overrides --no-auth and requires a login. No ENTRYPOINT, so
# the command is overridable for CLI/one-shot use (e.g. `docker run vmlab vmlab up`).
CMD ["vmlab-web", "--bind", "0.0.0.0", "--port", "7878", "--no-auth"]
