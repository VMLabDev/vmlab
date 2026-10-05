# syntax=docker/dockerfile:1.7
#
# The end-to-end test image: vmlab, its container guest asset and the Linux
# agent built from this checkout, beside every host tool vmlab drives. Run it
# privileged with /dev/kvm passed through (`just e2e::run`).

FROM rust:1-trixie AS build
RUN apt-get update \
 && apt-get install -y --no-install-recommends cpio curl git xz-utils \
 && rm -rf /var/lib/apt/lists/*
RUN rustup target add x86_64-unknown-linux-musl
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/src/guest/agent/target \
    --mount=type=cache,target=/src/guest/cinit/target \
    set -eux; \
    # The target caches are shared by every checkout's image build, and cargo
    # judges freshness by mtime: stamp this checkout's sources as newest so a
    # build never ships another checkout's binary.
    find src guest -type f -exec touch {} +; \
    cargo build --release --locked --bin vmlab; \
    install -D target/release/vmlab /out/vmlab; \
    ./guest/build-asset.sh x86_64; \
    # vmlab's own UEFI firmware for the aarch64 admin VM too: the runtime
    # image carries no OVMF/AAVMF package, so every UEFI boot proves the
    # bundled set is all a host needs.
    ./guest/fetch-firmware.sh aarch64; \
    ./guest/build-agent.sh linux-x86_64; \
    VMLAB_AGENT_STAMP=agent=e2e-stale VMLAB_AGENT_DIST_DIR=/out/stale-agent \
      ./guest/build-agent.sh linux-x86_64; \
    mkdir -p /out/guest; \
    cp -r --sparse=always guest/dist/x86_64 guest/dist/agent guest/dist/firmware /out/guest/

FROM registry:2 AS registry

FROM debian:trixie-slim
# No `ovmf` or `qemu-efi-aarch64`: vmlab boots the UEFI firmware it ships
# under /usr/share/vmlab/guest/firmware, and the suite proves it needs nothing
# else.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      qemu-system-x86 qemu-system-arm qemu-system-modules-opengl \
      libegl1 libegl-mesa0 libgl1 libgl1-mesa-dri libgbm1 libopengl0 \
      qemu-utils \
      swtpm swtpm-tools virtiofsd \
      samba xorriso mtools dosfstools squashfs-tools tesseract-ocr \
      python3 util-linux procps iproute2 curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=registry /bin/registry /usr/local/bin/registry
COPY --from=build /out/vmlab /usr/local/bin/vmlab
COPY --from=build /out/guest /usr/share/vmlab/guest
# A second Linux agent with a different stamp, for the agent-refresh check.
COPY --from=build /out/stale-agent /usr/share/vmlab/e2e/stale-agent
COPY e2e /e2e
ENV XDG_RUNTIME_DIR=/run/user/0 \
    PYTHONUNBUFFERED=1
WORKDIR /work
ENTRYPOINT ["python3", "/e2e/run.py"]
