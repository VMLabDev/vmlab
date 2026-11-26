# syntax=docker/dockerfile:1.7
#
# vmlab-build: every toolchain the release artefacts need — the host `vmlab`
# binary, the strict guest bundle (`just guest-package`), the hypervisor bundle
# (`just ci::hv-package`) and the BPF objects (`just ebpf-build`) — plus what
# the merge bar (`just ci::check`) needs, on
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
    ZIG_DIR=/opt/zig \
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
      xorriso mtools dosfstools samba liblzma-dev \
 && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/vmlab-build
COPY scripts/guest-toolchains.sh scripts/

RUN ./scripts/guest-toolchains.sh apt && rm -rf /var/lib/apt/lists/*
RUN VMLAB_BIN_DIR=/usr/local/bin ./scripts/guest-toolchains.sh uv

COPY rust-toolchain.toml ./
COPY ebpf/rust-toolchain.toml ebpf/
RUN ./scripts/guest-toolchains.sh rust

# The macOS agent's zig linker, cargo-zigbuild and rcodesign, pinned in the
# script; cargo-zigbuild lands in /opt/rust/cargo/bin, the other two here.
RUN VMLAB_BIN_DIR=/usr/local/bin ./scripts/guest-toolchains.sh zig \
 && rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git"

# The merge bar's extras over the bundle's toolchains: ci.yml's check job adds
# rustfmt + clippy, its guest-crates job the windows-gnu target the agent's
# Windows code is linted against (`ci::guest-windows-check`). The darwin
# target `ci::guest-macos-check` lints against comes with the rust stage.
RUN rustup component add --toolchain "$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)" rustfmt clippy \
 && rustup target add --toolchain "$(sed -n 's/^channel *= *"\(.*\)"/\1/p' rust-toolchain.toml)" x86_64-pc-windows-gnu

RUN ./scripts/guest-toolchains.sh ebpf \
 && rm -rf "$CARGO_HOME/registry" "$CARGO_HOME/git"

RUN ./scripts/guest-toolchains.sh watcom

RUN VMLAB_BIN_DIR=/usr/local/bin ./scripts/guest-toolchains.sh just

COPY guest/build-mingw-msvcrt.sh guest/
RUN ./scripts/guest-toolchains.sh msvcrt

# Same native dependencies as the hypervisor release job, from
# build/hv-pins.toml. virglrenderer itself is built at run time and cached
# outside target/ in the mounted checkout.
COPY build/hv.py build/hv-pins.toml build/
RUN python3 build/hv.py native-deps-install && rm -rf /var/lib/apt/lists/*

# Cross-compile the UMDF indirect display driver against hash-locked SDK/WDK packages.
RUN apt-get update -q \
 && apt-get install -y -q --no-install-recommends clang lld \
 && rm -rf /var/lib/apt/lists/*

# cosign, for the signature of a release (card 1021): `just release::sign`,
# `release::verify`, `release::publish` and `release::keygen` run it in this
# image, so a build machine needs none (scripts/release-cosign.sh). The script
# is the single pin: it downloads the binary of the sigstore/cosign release it
# names and refuses one that does not have the sha256 it names. The label
# tells a release step that the image has cosign without starting a container
# (scripts/test_release_workflow.py holds it equal to the pin). No key is in
# this image or in its history: a key reaches a container at run time only.
# Last of the toolchain layers, so adding it rebuilt none of them.
COPY scripts/install-cosign.sh scripts/
RUN ./scripts/install-cosign.sh /usr/local/bin \
 && cosign version 2>&1 | grep -q 'GitVersion: *v2\.6\.1$'
LABEL io.vmlab.cosign="v2.6.1"

# Runtime: any uid. HOME and CARGO_HOME are world-writable cache paths, and
# build/run.sh mounts named volumes over CARGO_HOME and the checkout's target
# dirs so rebuilds stay incremental. git refuses a repository owned by another uid;
# the container runs as the checkout's owner, so it never has to be told.
RUN mkdir -p /var/cache/vmlab-build/cargo /home/build \
 && chmod 1777 /var/cache/vmlab-build/cargo /home/build
ENV HOME=/home/build \
    VMLAB_BUILD_ENV=1 \
    CARGO_HOME=/var/cache/vmlab-build/cargo
WORKDIR /
CMD ["bash"]
