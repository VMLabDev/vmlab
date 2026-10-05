# syntax=docker/dockerfile:1.7
#
# vmlab-build: every toolchain the release artefacts need — the host `vmlab`
# binary, the strict guest bundle (`just guest-package`) and the BPF objects
# (`just ebpf-build`) — plus what the merge bar (`just ci::check`) needs, on
# the Ubuntu CI runs on. The checkout is never in the image: `just buildbox::dist`
# and `just buildbox::shell` bind-mount it and run as the host uid:gid.
#
# scripts/guest-toolchains.sh is the single definition of the toolchains; each
# of its stages is one layer here, and each layer copies in only the files that
# stage reads, so a source edit invalidates nothing and a toolchain edit
# invalidates only what follows it. The build context is those files alone
# (build/Dockerfile.dockerignore).
#
# Toolchains live under /opt, readable by any uid. CARGO_HOME moves to a
# writable cache path at the end, because cargo writes locks and the registry
# into it; the installed binaries stay on PATH from /opt/rust/cargo/bin.

FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive \
    RUSTUP_HOME=/opt/rust/rustup \
    CARGO_HOME=/opt/rust/cargo \
    WATCOM=/opt/open-watcom-v2 \
    VMLAB_MSVCRT_PREFIX=/opt/mingw-msvcrt \
    PATH=/opt/rust/cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin

# The host tools vmlab's tests probe for: the same list as ci.yml's check job
# ("Install runtime deps"), so `just ci::check` inside the container runs what
# CI runs. First, because nothing in the checkout changes it.
RUN apt-get update -q \
 && apt-get install -y -q --no-install-recommends \
      qemu-system-x86 qemu-system-arm qemu-system-misc qemu-utils \
      ovmf seabios swtpm \
      qemu-efi-aarch64 qemu-efi-riscv64 \
      tesseract-ocr passt \
      xorriso mtools dosfstools samba \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/vmlab-build
COPY scripts/guest-toolchains.sh scripts/

RUN ./scripts/guest-toolchains.sh apt && rm -rf /var/lib/apt/lists/*

COPY ebpf/rust-toolchain.toml ebpf/
RUN ./scripts/guest-toolchains.sh rust

# The merge bar's extras over the bundle's toolchains: ci.yml's check job adds
# rustfmt + clippy, its guest-crates job the windows-gnu target the agent's
# Windows code is linted against (`ci::guest-windows-check`).
RUN rustup component add --toolchain stable rustfmt clippy \
 && rustup target add --toolchain stable x86_64-pc-windows-gnu

RUN ./scripts/guest-toolchains.sh ebpf \
 && rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git"

RUN ./scripts/guest-toolchains.sh watcom

RUN VMLAB_BIN_DIR=/usr/local/bin ./scripts/guest-toolchains.sh just

COPY guest/build-mingw-msvcrt.sh guest/
RUN ./scripts/guest-toolchains.sh msvcrt

# Runtime: any uid. HOME and CARGO_HOME are world-writable cache paths, and
# build/run.sh mounts named volumes over CARGO_HOME and the checkout's target
# dirs so rebuilds stay incremental. git refuses a repository owned by another uid;
# the container runs as the checkout's owner, so it never has to be told.
RUN mkdir -p /var/cache/vmlab-build/cargo /home/build \
 && chmod 1777 /var/cache/vmlab-build/cargo /home/build
ENV HOME=/home/build \
    CARGO_HOME=/var/cache/vmlab-build/cargo
WORKDIR /
CMD ["bash"]
