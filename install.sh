#!/bin/sh
# install.sh — install the `vmlab` CLI from a GitHub release.
#
#   curl -fsSL https://vmlab.io/install.sh | sh                       # latest stable
#   curl -fsSL https://vmlab.io/install.sh | sh -s -- --pre           # latest pre-release
#   curl -fsSL https://vmlab.io/install.sh | sh -s -- --version 0.2.0-alpha
#
# vmlab is pre-release only for now, so use --pre (or --version) — a plain run
# targets stable, which does not exist yet.
#
# Options / environment:
#   --version <X>   install version X (e.g. 0.2.0-alpha); or set VMLAB_VERSION
#   --pre           install the newest pre-release
#   --bin-dir <dir> install into <dir> (default: $VMLAB_INSTALL_DIR or ~/.local/bin)
#   --skip-checks   do not report missing runtime tools after installing
#   --help          show this help
#
# vmlab drives QEMU/KVM, so the prebuilt binary is Linux x86_64 only (run it on
# Linux, or on Windows via WSL 2). It needs /dev/kvm plus QEMU and the
# usual guest tooling at runtime — see https://vmlab.io for the full list.

set -eu

REPO="VMLabDev/vmlab"
SOURCE_BUILD="cargo install --git https://github.com/VMLabDev/vmlab --locked"

VERSION="${VMLAB_VERSION:-}"
BIN_DIR="${VMLAB_INSTALL_DIR:-$HOME/.local/bin}"
PRE=0
SKIP_CHECKS=0

err() { printf 'error: %s\n' "$1" >&2; exit 1; }

usage() {
  sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
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
url="https://github.com/$REPO/releases/download/$tag/$asset"

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

# The guest assets are not in the release: the binary ships alone. A pulled
# template already carries its agent, so this only bites a container start or
# a template build — the two things that need an asset from the host.
asset_dir=''
for d in "${VMLAB_GUEST_ASSET_DIR:-}" /usr/share/vmlab/guest "$HOME/.local/share/vmlab/guest"; do
  [ -n "$d" ] && [ -d "$d" ] && { asset_dir=$d; break; }
done
if [ -z "$asset_dir" ]; then
  printf '\nNo guest assets found. VMs cloned from a published template need none,\n'
  printf 'but a lab container and a template build both do. Build them from source:\n'
  printf '  git clone https://github.com/VMLabDev/vmlab && cd vmlab && just guest-install\n'
fi
