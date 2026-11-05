# syntax=docker/dockerfile:1.7
#
# The end-to-end test image: vmlab, its container guest asset and the Linux
# agent and vmlab-hv built from this checkout, beside every host tool
# vmlab drives. Run it
# privileged with /dev/kvm passed through (`just e2e::run`).

ARG VMLAB_BUILD_IMAGE=vmlab-build
# Only firmware inputs invalidate this stage. VMLAB_BUILD_ENV=1 is inherited
# from the release toolchains, so no nested Docker invocation is needed.
FROM ${VMLAB_BUILD_IMAGE} AS firmware
WORKDIR /src
COPY guest/build-firmware.sh guest/build-ovmf.sh guest/build-secureboot.sh \
     guest/fetch-firmware.sh guest/build-seabios.sh \
     guest/enroll-secureboot.py guest/enroll-secureboot.py.lock guest/
COPY guest/ovmf/boot-without-waits.patch guest/ovmf/host-resolution.patch \
     guest/ovmf/reproducible-iscsi-pcd.patch guest/ovmf/reproducible-debug-path.patch \
     guest/ovmf/reproducible-source-path.patch guest/ovmf/test-genfw-reproducible.py guest/ovmf/
COPY guest/secureboot/microsoft-2023.json guest/secureboot/
COPY guest/logo/Logo.bmp guest/logo/
COPY guest/seabios/* guest/seabios/
COPY guest/ipxe/build.py guest/ipxe/upstream.json guest/ipxe/
COPY guest/ipxe/config/ guest/ipxe/config/
# Cache verified downloads. Keep iPXE's checkout in the layer itself so a
# changed upstream pin starts with a fresh checkout rather than a stale cache.
RUN --mount=type=cache,id=vmlab-e2e-firmware-deb,target=/src/guest/.cache/deb,sharing=locked \
    --mount=type=cache,id=vmlab-e2e-firmware-edk2,target=/src/guest/.cache/edk2,sharing=locked \
    --mount=type=cache,id=vmlab-e2e-firmware-seabios,target=/src/guest/.cache/seabios,sharing=locked \
    --mount=type=cache,id=vmlab-e2e-firmware-secureboot,target=/src/guest/.cache/secureboot,sharing=locked \
    --mount=type=cache,id=vmlab-e2e-firmware-uv,target=/src/guest/.cache/uv,sharing=locked \
    UV_CACHE_DIR=/src/guest/.cache/uv bash guest/build-firmware.sh x86_64 aarch64

FROM ${VMLAB_BUILD_IMAGE} AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/var/cache/vmlab-build/cargo \
    --mount=type=cache,target=/src/target \
    --mount=type=cache,target=/src/guest/agent/target \
    --mount=type=cache,target=/src/guest/cinit/target \
    set -eux; \
    # The target caches are shared by every checkout's image build, and cargo
    # judges freshness by mtime: stamp this checkout's sources as newest so a
    # build never ships another checkout's binary.
    find src guest hv -type f -exec touch {} +; \
    cargo build --release --locked --bin vmlab; \
    install -D target/release/vmlab /out/vmlab; \
    VMLAB_SKIP_FIRMWARE=1 ./guest/build-asset.sh x86_64; \
    test ! -e guest/dist/firmware; \
    ./guest/build-agent.sh linux-x86_64; \
    VMLAB_AGENT_STAMP=agent=e2e-stale VMLAB_AGENT_DIST_DIR=/out/stale-agent \
      ./guest/build-agent.sh linux-x86_64; \
    mkdir -p /out/guest; \
    cp -r --sparse=always guest/dist/x86_64 guest/dist/agent /out/guest/
# virglrenderer, which vmlab-hv's virtio-gpu builds against (phase 2d) and
# trixie does not ship at the pinned version. The pin, its patches and its
# Meson options are build/hv-pins.toml's, built by build/hv.py as CI builds
# them. Installed under /opt/virgl, apart from the distro's: vmlab-hv finds it
# by rpath, and QEMU keeps trixie's.
RUN python3 build/hv.py virgl-deps-install && rm -rf /var/lib/apt/lists/*
RUN python3 build/hv.py virgl-build --prefix /opt/virgl
# vmlab-hv (PRD §22) shares this checkout's workspace and protocol.
RUN --mount=type=cache,target=/var/cache/vmlab-build/cargo \
    --mount=type=cache,target=/src/target \
    set -eux; \
    export PKG_CONFIG_PATH=/opt/virgl/lib/pkgconfig \
      RUSTFLAGS="-C link-arg=-Wl,-rpath,/opt/virgl/lib"; \
    cargo build --release --locked --no-default-features -p vmlab-hv; \
    install -D target/release/vmlab-hv /out/vmlab-hv

FROM registry:2 AS registry

FROM debian:trixie-slim
# No `ovmf` or `qemu-efi-aarch64`: vmlab boots the UEFI firmware it ships
# under /usr/share/vmlab/guest/firmware, and the suite proves it needs nothing
# else.
RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      qemu-system-x86 qemu-system-arm qemu-system-modules-opengl \
      libegl1 libegl-mesa0 libgl1 libgl1-mesa-dri libgbm1 libopengl0 libepoxy0 libvulkan1 \
      qemu-utils \
      swtpm swtpm-tools virtiofsd \
      samba xorriso mtools dosfstools squashfs-tools tesseract-ocr ffmpeg \
      python3 util-linux procps iproute2 curl ca-certificates git \
      xvfb x11-utils \
 && rm -rf /var/lib/apt/lists/*
# Ubuntu 24.04's virtiofsd 1.10.0, which predates --migration-mode and
# --readonly, for the old-virtiofsd snapshot check. Pinned by digest.
RUN set -eux; \
    deb=/tmp/virtiofsd.deb; \
    curl -fsSL -o "$deb" http://archive.ubuntu.com/ubuntu/pool/universe/r/rust-virtiofsd/virtiofsd_1.10.0-1ubuntu0.1_amd64.deb; \
    echo "8069325e87cd4485fdb4dd2dde0e54dc68345847c92a1f5d9e9916dadd549b07  $deb" | sha256sum -c -; \
    dpkg-deb -x "$deb" /tmp/virtiofsd-old; \
    install -D /tmp/virtiofsd-old/usr/libexec/virtiofsd /usr/local/lib/vmlab-e2e/virtiofsd-1.10.0; \
    rm -rf "$deb" /tmp/virtiofsd-old
COPY --from=registry /bin/registry /usr/local/bin/registry
COPY --from=build /out/vmlab /usr/local/bin/vmlab
COPY --from=build /out/vmlab-hv /usr/local/bin/vmlab-hv
COPY --from=build /opt/virgl /opt/virgl
COPY --from=build /out/guest /usr/share/vmlab/guest
COPY --from=firmware /src/guest/dist/firmware /usr/share/vmlab/guest/firmware
# A second Linux agent with a different stamp, for the agent-refresh check.
COPY --from=build /out/stale-agent /usr/share/vmlab/e2e/stale-agent
COPY e2e /e2e
# Catch missing runtime libraries before a suite waits for guests that cannot
# start. The hypervisor has no host audio session and uses its null backend.
RUN vmlab --version && vmlab-hv --help > /dev/null
ENV XDG_RUNTIME_DIR=/run/user/0 \
    PYTHONUNBUFFERED=1
WORKDIR /work
ENTRYPOINT ["python3", "/e2e/run.py"]
