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
    cargo build --release --locked --bin vmlab; \
    install -D target/release/vmlab /out/vmlab; \
    ./guest/build-asset.sh x86_64; \
    ./guest/build-agent.sh linux-x86_64; \
    mkdir -p /out/guest; \
    cp -r guest/dist/x86_64 guest/dist/agent /out/guest/

FROM registry:2 AS registry

FROM debian:trixie-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      qemu-system-x86 qemu-system-arm qemu-system-modules-opengl \
      libegl1 libegl-mesa0 libgl1 libgl1-mesa-dri libgbm1 libopengl0 \
      qemu-efi-aarch64 qemu-utils ovmf \
      swtpm swtpm-tools virtiofsd \
      samba xorriso mtools dosfstools squashfs-tools tesseract-ocr \
      python3 util-linux procps iproute2 curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
COPY --from=registry /bin/registry /usr/local/bin/registry
COPY --from=build /out/vmlab /usr/local/bin/vmlab
COPY --from=build /out/guest /usr/share/vmlab/guest
COPY e2e /e2e
ENV XDG_RUNTIME_DIR=/run/user/0 \
    PYTHONUNBUFFERED=1
WORKDIR /work
ENTRYPOINT ["python3", "/e2e/run.py"]
