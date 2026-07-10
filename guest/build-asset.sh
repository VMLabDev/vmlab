#!/usr/bin/env bash
# Build the micro-VM guest boot asset: a kernel + initramfs pair per
# architecture, under guest/dist/<arch>/{vmlinuz,initramfs.img,VERSION}.
#
# The initramfs contains:
#   /init                 vmlab-cinit (static musl, built from guest/cinit)
#   /bin/busybox (+sh)    busybox-static: sh, modprobe, ip, udhcpc, ifconfig...
#   /etc/udhcpc/...       the udhcpc hook script cinit drives (see cinit net.rs)
#   /sbin/mkfs.ext4       e2fsprogs (dynamic, against the bundled musl + libs)
#   /usr/bin/qemu-ga      Alpine qemu-guest-agent + its full .so closure (the
#                         closure — glib, pcre2, libintl, ... — is pinned
#                         explicitly below rather than resolved at build time,
#                         so every byte in the image is checksum-verified)
#   /lib/modules/...      trimmed module tree (squashfs/overlay/ext4/9p/virtio
#                         + deps from modules.dep), loaded via /etc/vmlab-modules
#
# All Alpine packages are pinned (version + sha256) and fetched from the
# v3.22 main/community repos. apk files are plain tar.gz — unpacked without
# root; the cpio owner flag maps everything to root:root.
#
# Usage: guest/build-asset.sh [arch...]      (default: x86_64 aarch64)

set -euo pipefail

ALPINE_VERSION=3.22
MIRROR="${VMLAB_ALPINE_MIRROR:-https://dl-cdn.alpinelinux.org/alpine}"

# Pinned packages: repo|name|version|sha256(x86_64)|sha256(aarch64)
PACKAGES=(
  "main|linux-virt|6.12.95-r0|b4f49a5454dfecc406e92591d8deb4f7203b7c64e7531f8cd217f2e76371a4cb|3606bf25a62c8c9d3e6aa27a018e644f03d5b6cb0b3a3dea155e873bcc3689d4"
  "main|busybox-static|1.37.0-r20|488ad6efd04b5a722719e79f8e0dcc2c24afd6758867af3ce41b04839e60c74b|ee469aee2958feffd7f64dd96655704025ff60af614f77a1d7323dc237c34da2"
  "main|musl|1.2.5-r12|4990a5e0ba312e478f94cfe431a70efef1538004eb361c8ae424516848be45bb|ac281d1e7f9e9c447c51e309317b975f48be6edaf3ab91ae73b959cf86703782"
  "main|e2fsprogs|1.47.2-r2|4e59067f388ba8338dca3babd5784799d2d434eb6c19076d17ff96e50fbb1a49|3f57194ef8a75326d95ea2ab72ef92a82349230b1686c988c09e7ee869a1d24f"
  "main|e2fsprogs-libs|1.47.2-r2|4aeee327c9d8465ca403e4283e07055c353a2e3446f69113e624c91eab2e7e9e|43118bb26b5b631affeca249dc394055b14240f40a1754b093322bf9f1623344"
  "main|libcom_err|1.47.2-r2|bb138d264fbf92ae95b355b43b0cf2ab59ce4abcb0594024bb687e3eecc490a4|f0f5ea8a160fe0ff6d0b18eff222f80a6a83eb1ca6abb913db7a902d5bd25c43"
  "main|libblkid|2.41-r9|e82d3a09cd31de5d051b726c7f9081b8e0bd4d5159e368f3834a273b040c0adc|24b7016cb609ca653f787406de13576561f0f293beaf063dbd194fec13addef0"
  "main|libuuid|2.41-r9|063219dac6aedcf18d4531e17bb0f8a5a511bf64d4f71a5cf7cbb0b5ff04d81b|81ff69d3a6a7d0cb5ccd6a799bf775597df28efe490cbaff3b1f531f6a46bc2e"
  "main|libmount|2.41-r9|e8012b56a1da9804f56dd2f1ff47b7745db5cc538d148259332cfcc1c881a16e|aa7e09aa526ffef46aca603862cbaa9b46942ce8ee441becde800ba2bdcbc594"
  "main|libeconf|0.6.3-r0|fe4fbf324f84f7c8b475cfc708c5505aa630bced7b5f6289bf5ca36d9b587829|cdd4311aff72d53056134468b68a68f22829f995627b434d2f88960884319de9"
  "main|glib|2.84.4-r0|f9ec9e7d1f348fe2a87a124261f560909d1a4ac3020964c4ec372ff50f8ffad5|339817d41dc00fd898e8a24dc4a36b8e04608c3fd8642ecc97b00b6f5581e8bc"
  "main|libffi|3.4.8-r0|9a75cb9024693c1e52c3d8d7c9afb7c79e6e20f6c08df28effdb8dd816095083|9391f60a14c146655deaf65115563bc8dcd749cf0f93ec567e6443f2ed7d3bfc"
  "main|libintl|0.24.1-r0|1e9900a63a851e790b28d201d5c11872a5ff74322fd998c06ac952c6a2ed1ce4|17bacc149386d7dcb2ed4488afddc2dba644081cbc134b5839fbbe5e1b875bf9"
  "main|pcre2|10.46-r0|cfb8ad103a101fa6a31769e50e188dab9c60124705682d01b3de268795db58ad|62fdc4a3d6b48ca211cf6480c5da55664b489ec2b192ca8942e5b1d60ebe9496"
  "main|zlib|1.3.2-r0|1f3d5f463f490dad3a68097376711bfe5e8156e9e8daff3070513aa4378cdeca|7a39a917e4dab3c7a45537210ee5b5f17bf75f5e7777809a20cddd0afe074187"
  "main|numactl|2.0.18-r0|1a6d27d89c567ab20d548d72bb338b9274fceefdce4f3f7dd0dfec94f9d47666|ccf946ba49b04da45f8b7a71e51bca68d5201a4139a588901023cf3186c6e5ab"
  "main|liburing|2.9-r0|d8220b52497635bef2b6526c921c70a0275352c6fc6c4090ef1abaee17d2788f|fd94adf963411e907d62e644fe184f672bef95dd1568c55642ae77421be93621"
  "community|qemu-guest-agent|10.0.0-r1|c5c5ef9e80cf65d5f21060f1072a531082c4b692180418ab55bd3d38f1c08a97|18e3c96b169125017f63345632f71337bb2d3b32da2f2988195098db4d523fb1"
)

# Modules cinit needs (deps resolved from modules.dep below). virtio core,
# virtio-pci and virtio_console are built into linux-virt; virtio_blk and
# virtio_net are not. crc32c_generic is a runtime (request_module) dependency
# of ext4 that modules.dep cannot see — without it the scratch mount fails
# with "Cannot load crc32c driver". af_packet backs udhcpc's raw DHCP socket.
WANTED_MODULES=(
  virtio_blk virtio_net af_packet squashfs overlay crc32c_generic ext4 9p 9pnet_virtio
)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="$SCRIPT_DIR/.cache/apk"
DIST_DIR="$SCRIPT_DIR/dist"

die() {
  echo "build-asset: error: $*" >&2
  exit 1
}

log() {
  echo "build-asset: $*" >&2
}

need() {
  local tool
  for tool in "$@"; do
    command -v "$tool" >/dev/null 2>&1 || die "missing host tool: $tool"
  done
}

rust_target_for() {
  case "$1" in
    x86_64) echo "x86_64-unknown-linux-musl" ;;
    aarch64) echo "aarch64-unknown-linux-musl" ;;
    *) die "unsupported arch '$1' (supported: x86_64 aarch64)" ;;
  esac
}

# Download (once, into the cache) and checksum-verify one package.
# Prints the cached file path.
fetch_pkg() {
  local repo="$1" name="$2" ver="$3" sha="$4" arch="$5"
  local file="$CACHE_DIR/${arch}--${name}-${ver}.apk"
  if [[ ! -f "$file" ]]; then
    log "fetching $name-$ver ($arch)"
    curl -fsSL -o "$file.tmp" "$MIRROR/v$ALPINE_VERSION/$repo/$arch/$name-$ver.apk" \
      || die "download failed: $name-$ver ($arch)"
    mv "$file.tmp" "$file"
  fi
  echo "$sha  $file" | sha256sum -c --quiet - >/dev/null 2>&1 \
    || die "sha256 mismatch for $file (delete it to re-download)"
  echo "$file"
}

build_cinit() {
  local target="$1"
  rustup target list --installed | grep -qx "$target" \
    || die "rust target $target not installed — run: rustup target add $target"
  local -a extra_env=()
  # Cross-linking the static binary: use rustc's bundled lld unless the user
  # already configured a linker for the target.
  if [[ "$target" == aarch64-* && "$(uname -m)" != "aarch64" ]]; then
    if [[ -z "${CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER:-}" ]]; then
      extra_env+=("CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld")
    fi
  fi
  log "building vmlab-cinit for $target"
  env "${extra_env[@]}" cargo build --release --target "$target" \
    --manifest-path "$SCRIPT_DIR/cinit/Cargo.toml" \
    || die "cargo build for $target failed"
}

# Resolve the wanted module names to a dependency-first, de-duplicated list
# of module paths (as they appear in modules.dep).
resolve_modules() {
  local depfile="$1"
  shift
  local -A seen=()
  local -a order=()

  resolve_one() {
    local path="$1"
    [[ -n "${seen[$path]:-}" ]] && return 0
    seen[$path]=1
    local deps d
    deps="$(awk -F': ' -v p="$path" '$1 == p { print $2 }' "$depfile")"
    local -a dep_list=()
    read -ra dep_list <<<"$deps" || true
    for d in "${dep_list[@]}"; do
      resolve_one "$d"
    done
    order+=("$path")
  }

  local want path
  for want in "$@"; do
    path="$(awk -v w="$want" -F: '{
        p = $1; n = p
        sub(/^.*\//, "", n); sub(/\.ko(\.gz)?$/, "", n); gsub(/-/, "_", n)
        if (n == w) { print p; exit }
      }' "$depfile")"
    [[ -n "$path" ]] || die "module $want not found in $depfile"
    resolve_one "$path"
  done
  printf '%s\n' "${order[@]}"
}

# The udhcpc hook: applies the lease and records ip + dns for cinit.
# See guest/cinit/src/net.rs for why udhcpc rather than a native DHCP client.
write_udhcpc_script() {
  local root="$1"
  mkdir -p "$root/etc/udhcpc"
  cat >"$root/etc/udhcpc/default.script" <<'EOF'
#!/bin/sh
# vmlab micro-VM udhcpc hook. busybox exports: $1 (event), $interface, $ip,
# $subnet, $router, $dns. Applies the lease and records it under
# /run/vmlab-net/ for vmlab-cinit (resolv.conf + the net_up event).
BB=/bin/busybox
STATE=/run/vmlab-net
$BB mkdir -p "$STATE"
case "$1" in
  deconfig)
    $BB ifconfig "$interface" 0.0.0.0
    ;;
  bound|renew)
    $BB ifconfig "$interface" "$ip" netmask "${subnet:-255.255.255.0}"
    if [ -n "${router:-}" ]; then
      while $BB route del default gw 0.0.0.0 dev "$interface" 2>/dev/null; do :; done
      for r in $router; do
        $BB route add default gw "$r" dev "$interface" && break
      done
    fi
    printf '%s\n' "$ip" > "$STATE/$interface.ip"
    printf '%s\n' ${dns:-} > "$STATE/$interface.dns"
    ;;
esac
exit 0
EOF
  chmod 0755 "$root/etc/udhcpc/default.script"
}

build_arch() {
  local arch="$1"
  local target
  target="$(rust_target_for "$arch")"
  build_cinit "$target"

  local work root extract kextract
  work="$(mktemp -d "${TMPDIR:-/tmp}/vmlab-guest-asset.XXXXXX")"
  # shellcheck disable=SC2064  # expand $work now, not at trap time
  trap "rm -rf '$work'" RETURN
  root="$work/root"
  extract="$work/extract"
  kextract="$work/kernel"
  mkdir -p "$root" "$extract" "$kextract"

  # -- fetch + unpack the pinned packages (apk = tar.gz, no root needed) -----
  local entry repo name ver sha_x86 sha_arm sha apk
  for entry in "${PACKAGES[@]}"; do
    IFS='|' read -r repo name ver sha_x86 sha_arm <<<"$entry"
    if [[ "$arch" == "x86_64" ]]; then sha="$sha_x86"; else sha="$sha_arm"; fi
    apk="$(fetch_pkg "$repo" "$name" "$ver" "$sha" "$arch")"
    local dest="$extract"
    [[ "$name" == "linux-virt" ]] && dest="$kextract"
    tar -xzf "$apk" -C "$dest" --warning=no-unknown-keyword 2>/dev/null \
      || die "unpack failed: $apk"
  done

  # -- initramfs skeleton -----------------------------------------------------
  local d
  for d in bin sbin etc lib usr/bin usr/lib run proc sys dev tmp \
    rootfs rootfs-ro scratch vmlab-cfg; do
    mkdir -p "$root/$d"
  done

  # /init: the static cinit binary.
  install -m 0755 "$SCRIPT_DIR/cinit/target/$target/release/vmlab-cinit" "$root/init"

  # busybox + the /bin/sh the udhcpc hook needs (cinit calls applets as
  # `busybox <applet>`, so no other symlinks are required).
  install -m 0755 "$extract/bin/busybox.static" "$root/bin/busybox"
  ln -sf busybox "$root/bin/sh"
  write_udhcpc_script "$root"

  # mkfs.ext4 (+ config) and every shared library the pinned packages carry:
  # the musl loader in /lib and the dependency closure of mkfs.ext4/qemu-ga.
  cp -a "$extract/sbin/mke2fs" "$extract/sbin/mkfs.ext4" "$root/sbin/"
  cp -a "$extract/etc/mke2fs.conf" "$root/etc/"
  cp -a "$extract/lib/." "$root/lib/"
  find "$extract/usr/lib" -maxdepth 1 -name '*.so*' -exec cp -a {} "$root/usr/lib/" \;

  # qemu-ga: primary control is the vmlab.ctl.0 channel; the agent rides
  # along for exec/file-copy parity with full VMs.
  install -m 0755 "$extract/usr/bin/qemu-ga" "$root/usr/bin/qemu-ga"

  # -- kernel + trimmed module tree -------------------------------------------
  local kver kmoddir vmlinuz
  kmoddir="$(find "$kextract/lib/modules" -mindepth 1 -maxdepth 1 -type d | head -n1)"
  [[ -n "$kmoddir" ]] || die "no module tree in linux-virt package"
  kver="$(basename "$kmoddir")"
  vmlinuz="$kextract/boot/vmlinuz-virt"
  [[ -f "$vmlinuz" ]] || die "no boot/vmlinuz-virt in linux-virt package"

  local moddest="$root/lib/modules/$kver"
  mkdir -p "$moddest"
  local meta
  for meta in modules.dep modules.alias modules.builtin modules.builtin.modinfo \
    modules.symbols modules.order; do
    [[ -f "$kmoddir/$meta" ]] && cp "$kmoddir/$meta" "$moddest/"
  done

  local -a module_paths=()
  mapfile -t module_paths < <(resolve_modules "$kmoddir/modules.dep" "${WANTED_MODULES[@]}")
  # die inside the process substitution can't stop the script — check here.
  [[ ${#module_paths[@]} -ge ${#WANTED_MODULES[@]} ]] \
    || die "module resolution failed (got ${#module_paths[@]} paths)"
  : >"$root/etc/vmlab-modules"
  local mpath mname
  for mpath in "${module_paths[@]}"; do
    mkdir -p "$moddest/$(dirname "$mpath")"
    cp "$kmoddir/$mpath" "$moddest/$mpath"
    mname="$(basename "$mpath")"
    mname="${mname%.gz}"
    mname="${mname%.ko}"
    echo "${mname//-/_}" >>"$root/etc/vmlab-modules"
  done
  log "$arch: modules: $(tr '\n' ' ' <"$root/etc/vmlab-modules")"

  # -- pack --------------------------------------------------------------------
  local out="$DIST_DIR/$arch"
  mkdir -p "$out"
  (cd "$root" && find . -print0 | LC_ALL=C sort -z |
    cpio --null -o -H newc --owner=+0:+0 --quiet) | gzip -9 >"$out/initramfs.img"
  cp "$vmlinuz" "$out/vmlinuz"

  local cinit_rev="unknown"
  if git -C "$SCRIPT_DIR" rev-parse --short HEAD >/dev/null 2>&1; then
    cinit_rev="$(git -C "$SCRIPT_DIR" rev-parse --short HEAD)"
    # Untracked files count as dirty too (git diff alone would miss them).
    if [[ -n "$(git -C "$SCRIPT_DIR" status --porcelain -- "$SCRIPT_DIR" 2>/dev/null)" ]]; then
      cinit_rev="$cinit_rev-dirty"
    fi
  fi
  printf 'alpine=%s kernel=%s cinit=%s\n' "$ALPINE_VERSION" "$kver" "$cinit_rev" >"$out/VERSION"

  log "$arch: $(du -h "$out/initramfs.img" | cut -f1) initramfs, kernel $kver → $out"
}

main() {
  need curl tar gzip cpio sha256sum awk cargo rustup git install find
  mkdir -p "$CACHE_DIR" "$DIST_DIR"
  local -a arches=("$@")
  [[ ${#arches[@]} -gt 0 ]] || arches=(x86_64 aarch64)
  local arch
  for arch in "${arches[@]}"; do
    log "building guest asset for $arch"
    build_arch "$arch"
  done
  log "done"
}

main "$@"
