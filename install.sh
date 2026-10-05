#!/bin/sh
# install.sh — install the `vmlab` CLI and its guest assets from a GitHub release.
#
#   curl -fsSL https://vmlab.io/install.sh | sh                       # latest stable
#   curl -fsSL https://vmlab.io/install.sh | sh -s -- --pre           # latest pre-release
#   curl -fsSL https://vmlab.io/install.sh | sh -s -- --version 0.2.0-alpha
#
# vmlab is pre-release only for now, so use --pre (or --version) — a plain run
# targets stable, which does not exist yet.
#
# Options / environment:
#   --version <X>     install version X (e.g. 0.2.0-alpha); or set VMLAB_VERSION
#   --pre             install the newest pre-release
#   --bin-dir <dir>   install into <dir> (default: $VMLAB_INSTALL_DIR or ~/.local/bin)
#   --guest-dir <dir> install the guest assets into <dir> (default: $VMLAB_GUEST_DIR,
#                     else ~/.local/share/vmlab/guest — where vmlab looks for them)
#   --no-guest        install the binary only, not the guest assets
#   --skip-checks     do not report missing runtime tools after installing
#   --help            show this help
#
#   VMLAB_RELEASE_BASE_URL  where releases are downloaded from, as
#                     <base>/v<version>/<asset> (default:
#                     https://github.com/VMLabDev/vmlab/releases/download);
#                     point it at a local server to test the installer
#
# The guest assets — the container micro-VM kernel/initramfs, every in-guest
# agent build and the UEFI firmware vmlab boots its VMs with (OVMF/AAVMF,
# secure boot included) — come from the same release as the binary
# (vmlab-guest-<version>.tar.gz, checked against its .sha256) and replace
# whatever an older install left in the guest directory. A release without
# them, or a failed download, costs only the guest assets: the binary stays.
#
# vmlab drives QEMU/KVM, so the prebuilt binary is Linux x86_64 only (run it on
# Linux, or on Windows via WSL 2). It needs /dev/kvm plus QEMU and the
# usual guest tooling at runtime — see https://vmlab.io for the full list.

set -eu

REPO="VMLabDev/vmlab"
SOURCE_BUILD="cargo install --git https://github.com/VMLabDev/vmlab --locked"

VERSION="${VMLAB_VERSION:-}"
BIN_DIR="${VMLAB_INSTALL_DIR:-$HOME/.local/bin}"
GUEST_DIR="${VMLAB_GUEST_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/vmlab/guest}"
BASE_URL="${VMLAB_RELEASE_BASE_URL:-https://github.com/$REPO/releases/download}"
PRE=0
SKIP_CHECKS=0
NO_GUEST=0

err() { printf 'error: %s\n' "$1" >&2; exit 1; }
warn() { printf '\nwarning: %s\n' "$1" >&2; }

usage() {
  # The header comment, up to the first blank line.
  sed -n '2,/^$/p' "$0" | sed -n 's/^# \{0,1\}//p'
  exit "${1:-0}"
}

# ── Parse args ──────────────────────────────────────────────────────────────
while [ $# -gt 0 ]; do
  case "$1" in
    --version) [ $# -ge 2 ] || err "--version needs an argument"; VERSION="$2"; shift 2 ;;
    --version=*) VERSION="${1#--version=}"; shift ;;
    --pre) PRE=1; shift ;;
    --bin-dir) [ $# -ge 2 ] || err "--bin-dir needs an argument"; BIN_DIR="$2"; shift 2 ;;
    --bin-dir=*) BIN_DIR="${1#--bin-dir=}"; shift ;;
    --guest-dir) [ $# -ge 2 ] || err "--guest-dir needs an argument"; GUEST_DIR="$2"; shift 2 ;;
    --guest-dir=*) GUEST_DIR="${1#--guest-dir=}"; shift ;;
    --no-guest) NO_GUEST=1; shift ;;
    --skip-checks) SKIP_CHECKS=1; shift ;;
    -h|--help) usage 0 ;;
    -*) err "unknown option: $1 (try --help)" ;;
    *) [ -z "$VERSION" ] || err "unexpected argument: $1"; VERSION="$1"; shift ;;
  esac
done

# ── HTTP helper (curl or wget) ──────────────────────────────────────────────
if command -v curl >/dev/null 2>&1; then
  http_get()      { curl -fsSL "$1"; }
  download_file() { curl -fsSL -o "$2" "$1"; }
elif command -v wget >/dev/null 2>&1; then
  http_get()      { wget -qO- "$1"; }
  download_file() { wget -qO "$2" "$1"; }
else
  err "need curl or wget on PATH"
fi

# Pull the first "tag_name": "..." out of a GitHub API JSON response.
first_tag() { sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' | head -n1; }

# ── Detect platform ─────────────────────────────────────────────────────────
os="$(uname -s)"
arch="$(uname -m)"
case "$arch" in
  x86_64|amd64) arch="x86_64" ;;
esac
case "$os" in
  Linux)
    [ "$arch" = "x86_64" ] || err "no prebuilt binary for Linux/$arch — build from source:
  $SOURCE_BUILD"
    suffix="linux-x86_64" ;;
  Darwin)
    err "no macOS build — vmlab drives QEMU/KVM and runs on Linux (or Windows via WSL 2)." ;;
  *)
    err "unsupported platform: $os/$arch — vmlab runs on Linux (or Windows via WSL 2)." ;;
esac

# ── Resolve version ─────────────────────────────────────────────────────────
if [ -n "$VERSION" ]; then
  tag="v${VERSION#v}"
elif [ "$PRE" -eq 1 ]; then
  tag="$(http_get "https://api.github.com/repos/$REPO/releases" | first_tag)"
  [ -n "$tag" ] || err "could not find any release for $REPO"
else
  tag="$(http_get "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null | first_tag || true)"
  [ -n "$tag" ] || err "no stable release published yet.
vmlab is pre-release only for now — re-run with --pre to get the newest pre-release:
  curl -fsSL https://vmlab.io/install.sh | sh -s -- --pre
See $( printf 'https://github.com/%s/releases' "$REPO" )"
fi

ver="${tag#v}"
asset="vmlab-${ver}-${suffix}"
BASE_URL="${BASE_URL%/}"
url="$BASE_URL/$tag/$asset"

# ── Download + install ──────────────────────────────────────────────────────
printf 'Installing vmlab %s to %s\n' "$ver" "$BIN_DIR"

tmp="$(mktemp)"
trap 'rm -f "$tmp"' EXIT INT TERM
download_file "$url" "$tmp" || err "download failed: $url
The release may not exist or may lack a $suffix asset. See https://github.com/$REPO/releases"

chmod +x "$tmp"
mkdir -p "$BIN_DIR"
mv "$tmp" "$BIN_DIR/vmlab"
trap - EXIT INT TERM

printf 'Installed: %s\n' "$("$BIN_DIR/vmlab" --version 2>/dev/null || echo "$BIN_DIR/vmlab")"

# ── PATH hint ───────────────────────────────────────────────────────────────
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) printf '\n%s is not on your PATH. Add it, e.g.:\n  export PATH="%s:$PATH"\n' "$BIN_DIR" "$BIN_DIR" ;;
esac

# ── Guest assets ────────────────────────────────────────────────────────────
# The container micro-VM kernel/initramfs, every in-guest agent build and the
# bundled UEFI firmware, as one tarball from the same release: a lab container
# and a template build both need them from the host, and a UEFI VM boots the
# bundled firmware before the host's own OVMF. Nothing here can fail the
# install — the binary is already in place — so every failure is a warning
# naming the source-build fallback.
guest_fallback() {
  warn "$1
The vmlab binary is installed; the guest assets are not. A VM cloned from a
published template needs none (a UEFI one falls back to the host's OVMF), but
a lab container and a template build both do. Build them from source instead:
  git clone https://github.com/$REPO && cd vmlab && just guest-install"
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | cut -d' ' -f1
  else
    return 1
  fi
}

install_guest() {
  gname="vmlab-guest-${ver}.tar.gz"
  gurl="$BASE_URL/$tag/$gname"
  printf '\nInstalling guest assets to %s\n' "$GUEST_DIR"

  # Staged beside the target, so the swap below is a rename on one filesystem.
  gparent="$(dirname "$GUEST_DIR")"
  mkdir -p "$gparent" || { guest_fallback "cannot create $gparent"; return 0; }
  gtmp="$(mktemp -d "$gparent/.vmlab-guest.XXXXXX")" \
    || { guest_fallback "cannot create a staging directory in $gparent"; return 0; }
  trap 'rm -rf "$gtmp"' EXIT INT TERM

  if ! download_file "$gurl" "$gtmp/$gname" || ! download_file "$gurl.sha256" "$gtmp/$gname.sha256"; then
    guest_fallback "could not download $gurl (or its .sha256) — releases before the guest bundle do not carry one."
    return 0
  fi

  want="$(sed -n '1s/^\([0-9a-fA-F]\{64\}\).*/\1/p' "$gtmp/$gname.sha256")"
  got="$(sha256_of "$gtmp/$gname")" || {
    guest_fallback "need sha256sum or shasum to verify $gname; refusing to install it unverified."
    return 0
  }
  if [ -z "$want" ] || [ "$want" != "$got" ]; then
    guest_fallback "checksum mismatch for $gname (expected ${want:-nothing}, got $got); refusing to install it."
    return 0
  fi

  mkdir "$gtmp/new"
  if ! tar -xzf "$gtmp/$gname" -C "$gtmp/new" || [ ! -d "$gtmp/new/agent" ]; then
    guest_fallback "$gname did not unpack into a guest directory."
    return 0
  fi

  # Swap: the old directory moves aside whole (nothing from an older version
  # survives in the new one), the new one renames into its place, and the old
  # one is removed. Put the old one back if the second rename fails.
  if [ -e "$GUEST_DIR" ] || [ -L "$GUEST_DIR" ]; then
    mv "$GUEST_DIR" "$gtmp/old" || { guest_fallback "cannot move the old $GUEST_DIR aside."; return 0; }
  fi
  if ! mv "$gtmp/new" "$GUEST_DIR"; then
    [ -e "$gtmp/old" ] && mv "$gtmp/old" "$GUEST_DIR"
    guest_fallback "cannot move the new guest assets into $GUEST_DIR."
    return 0
  fi
  rm -rf "$gtmp"
  trap - EXIT INT TERM

  printf 'Installed guest assets: %s\n' "$(ls "$GUEST_DIR" | tr '\n' ' ')"

  # vmlab searches $VMLAB_GUEST_ASSET_DIR, then /usr/share/vmlab/guest, then
  # the per-user directory — say so when this install is not the one it finds.
  default_dir="${XDG_DATA_HOME:-$HOME/.local/share}/vmlab/guest"
  if [ -n "${VMLAB_GUEST_ASSET_DIR:-}" ] && [ "$VMLAB_GUEST_ASSET_DIR" != "$GUEST_DIR" ]; then
    printf 'Note: VMLAB_GUEST_ASSET_DIR=%s is set and takes precedence over %s.\n' "$VMLAB_GUEST_ASSET_DIR" "$GUEST_DIR"
  elif [ "$GUEST_DIR" != "$default_dir" ] && [ "$GUEST_DIR" != /usr/share/vmlab/guest ]; then
    printf 'Note: vmlab does not look in %s by itself; set VMLAB_GUEST_ASSET_DIR=%s\n' "$GUEST_DIR" "$GUEST_DIR"
  elif [ "$GUEST_DIR" = "$default_dir" ] && [ -d /usr/share/vmlab/guest ]; then
    printf 'Note: /usr/share/vmlab/guest exists and takes precedence over %s.\n' "$GUEST_DIR"
  fi
}

[ "$NO_GUEST" -eq 1 ] || install_guest

# ── Runtime tools ───────────────────────────────────────────────────────────
# vmlab bundles none of these: it looks each one up on PATH the first time it
# needs it, so a host missing one fails at the first `up`, container start or
# template build instead of here. Report what is absent now, while the person
# who can install it is still watching. Missing tools are a warning, never an
# error — most of them matter only to the features that use them.
[ "$SKIP_CHECKS" -eq 1 ] && exit 0

have() { command -v "$1" >/dev/null 2>&1; }

# Package name per manager, for the one-line install hint below.
pkg_for() {
  case "$1" in
    qemu-system-x86_64) apt=qemu-system-x86; dnf=qemu-system-x86;   pac=qemu-system-x86 ;;
    qemu-img)           apt=qemu-utils;      dnf=qemu-img;          pac=qemu-img ;;
    xorriso)            apt=xorriso;         dnf=xorriso;           pac=libisoburn ;;
    mcopy)              apt=mtools;          dnf=mtools;            pac=mtools ;;
    mkfs.vfat)          apt=dosfstools;      dnf=dosfstools;        pac=dosfstools ;;
    swtpm)              apt=swtpm;           dnf=swtpm;             pac=swtpm ;;
    smbd)               apt=samba;           dnf=samba;             pac=samba ;;
    tesseract)          apt=tesseract-ocr;   dnf=tesseract;         pac=tesseract ;;
    sqfstar)            apt=squashfs-tools;  dnf=squashfs-tools;    pac=squashfs-tools ;;
    virtiofsd)          apt=virtiofsd;       dnf=virtiofsd;         pac=virtiofsd ;;
    remote-viewer)      apt=virt-viewer;     dnf=virt-viewer;       pac=virt-viewer ;;
    *)                  apt="$1";            dnf="$1";              pac="$1" ;;
  esac
}

missing=''   # newline-separated "<cmd>\t<what it is for>"
want() {     # want <cmd> <purpose> [alternative-cmd...]
  cmd=$1; purpose=$2; shift 2
  have "$cmd" && return 0
  for alt in "$@"; do have "$alt" && return 0; done
  missing="${missing}${cmd}	${purpose}
"
}

# Running a guest at all.
want qemu-system-x86_64 "run x86_64 guests (install qemu-system-arm / -misc for other arches)"
want qemu-img           "clone disks, snapshot, and build templates"
# Per feature. Each is only needed by the labs that use it.
want xorriso            "build ISO media and the bootstrap ISO every template build attaches" genisoimage mkisofs
want mcopy              "build floppy media (mtools)" mformat
want mkfs.vfat          "format floppy images"
want swtpm              "guests with tpm = true — Windows 11 and Server 2025 require one"
want smbd               "shared folders on guests without virtiofs"
want tesseract          "vmlab vm ocr and wait_for_text in scripts"
want sqfstar            "lab containers: flattening a pulled OCI image"
want virtiofsd          "shared folders over virtiofs (smbd is the fallback)"
want remote-viewer      "vmlab console and gui = true" gvncviewer vncviewer

if [ -n "$missing" ]; then
  printf '\nMissing runtime tools — vmlab will fail at the first thing that needs one:\n\n'
  printf '%s' "$missing" | while IFS='	' read -r cmd purpose; do
    printf '  %-18s %s\n' "$cmd" "$purpose"
  done

  # One command that installs the lot, for the manager this host has.
  pkgs=''
  for cmd in $(printf '%s' "$missing" | cut -f1); do
    pkg_for "$cmd"
    if   have apt-get; then pkgs="$pkgs $apt"
    elif have dnf;     then pkgs="$pkgs $dnf"
    elif have pacman;  then pkgs="$pkgs $pac"
    fi
  done
  if have apt-get;   then printf '\n  sudo apt-get install -y%s\n' "$pkgs"
  elif have dnf;     then printf '\n  sudo dnf install -y%s\n' "$pkgs"
  elif have pacman;  then printf '\n  sudo pacman -S --needed%s\n' "$pkgs"
  else printf '\nInstall them with your package manager. See https://vmlab.io for the full list.\n'
  fi
fi

if [ ! -e /dev/kvm ]; then
  printf '\n/dev/kvm is missing: guests will run under emulation, which is very slow.\n'
  printf 'Enable virtualisation in the BIOS, or load kvm_intel / kvm_amd.\n'
elif [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
  printf '\n/dev/kvm exists but this user cannot use it. Add yourself to the kvm group:\n'
  printf '  sudo usermod -aG kvm "$USER"    # then log out and back in\n'
fi
