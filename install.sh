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
#   --fastpath        grant the eBPF network fast path's helper (vmlab-fastpath)
#                     CAP_BPF and CAP_NET_ADMIN through sudo, without asking
#   --no-fastpath     do not offer them; or set VMLAB_FASTPATH_CAPS=yes|no
#   --skip-checks     do not report missing runtime tools after installing
#   --insecure-skip-signature  skip cosign authentication (unsafe), but still
#                     verify SHA256SUMS; or VMLAB_INSECURE_SKIP_SIGNATURE=1
#                     Without cosign the default is to refuse installation.
#                     Skipping permits an attacker to replace both assets and
#                     checksums. Use only with an independently trusted source.
#   --release-key <file>  verify a key-signed release with this cosign public
#                     key, not the one in this script; or VMLAB_RELEASE_KEY.
#                     For testing a local build: whoever picks the key picks
#                     whom the installer trusts.
#   --fetch <asset>   install nothing: download the named asset of the release
#                     into the current directory, after the same signature
#                     check and against the signed SHA256SUMS. For the
#                     vmlab-hv bundle, which this script does not install.
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
# (vmlab-guest-<version>.tar.gz, checked against signed SHA256SUMS) and replace
# whatever an older install left in the guest directory. A release without
# them, or a failed download, costs only the guest assets: the binary stays.
#
# The network fabric needs no privilege. Its optional eBPF fast path needs
# CAP_BPF and CAP_NET_ADMIN, held by one small helper, vmlab-fastpath,
# installed beside vmlab. The daemons start it and ask it, over a closed
# request protocol, to create taps and to load, attach and update vmlab's own
# BPF programs; it refuses anything else. An install as an ordinary user can
# give the helper those capabilities as file capabilities (`sudo setcap`). The
# grant goes to a copy root holds and has checked against the signed checksum,
# never to whatever is at the helper's path at that moment. Run
# from a terminal, the installer asks with a default of no; non-interactive
# installs grant nothing unless --fastpath is explicit. The vmlab binary itself
# never gets a capability. Replacing the helper drops them, so every install
# requires consent again. File capabilities go to whoever executes the file,
# so the helper is installed mode 0700, and it refuses a caller that is not
# the owner of its file.
#
# vmlab drives QEMU/KVM, so the prebuilt binary is Linux x86_64 only (run it on
# Linux, or on Windows via WSL 2). It needs /dev/kvm plus QEMU and the
# usual guest tooling at runtime — see https://vmlab.io for the full list.

set -eu

REPO="VMLabDev/vmlab"
SOURCE_BUILD="cargo install --git https://github.com/VMLabDev/vmlab --locked"

# ── Release signatures ──────────────────────────────────────────────────────
# A release is authenticated in one of two ways, by the signature file it has:
#   SHA256SUMS.bundle      keyless cosign, the GitHub OIDC identity of ci.yml on
#                          main: every release that GitHub built.
#   SHA256SUMS.key.bundle  a cosign key pair: a release built outside GitHub
#                          (scripts/release-sign.sh). Verified with
#                          `cosign verify-blob --key` against RELEASE_KEY.
# RELEASE_KEY is the switch for the second way, and it is on: it holds the PEM
# public key of the releases, the same text as build/release-signing.pub. A
# release signed by another key is refused. Empty, it would trust no key and
# refuse every key-signed release. `just release::trust <cosign.pub>` writes
# it and the file together (docs/agents/release-gate.md).
RELEASE_KEY='-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEdJtVT5BAlVyiYuqEcgPFxsP5/90K
i7p71qR5nuN4Kci629Qf9sz2FIm3LIaHNbIQm2JHDJBkMLgM+K2O/M0l7Q==
-----END PUBLIC KEY-----'

VERSION="${VMLAB_VERSION:-}"
BIN_DIR="${VMLAB_INSTALL_DIR:-$HOME/.local/bin}"
GUEST_DIR="${VMLAB_GUEST_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/vmlab/guest}"
BASE_URL="${VMLAB_RELEASE_BASE_URL:-https://github.com/$REPO/releases/download}"
PRE=0
SKIP_CHECKS=0
INSECURE_SKIP_SIGNATURE="${VMLAB_INSECURE_SKIP_SIGNATURE:-0}"
RELEASE_KEY_FILE="${VMLAB_RELEASE_KEY:-}"
NO_GUEST=0
FETCH=""
FASTPATH="${VMLAB_FASTPATH_CAPS:-ask}"

err() { printf 'error: %s\n' "$1" >&2; exit 1; }
warn() { printf '\nwarning: %s\n' "$1" >&2; }

usage() {
  printf '%s\n' \
    'Usage: install.sh [--version X | --pre] [options]' \
    '  --bin-dir DIR       binary destination (default ~/.local/bin)' \
    '  --guest-dir DIR     guest assets destination' \
    '  --no-guest          binary only' \
    '  --fastpath          request eBPF capabilities (privileged)' \
    '  --no-fastpath       do not request capabilities' \
    '  --skip-checks       skip runtime tool checks' \
    '  --insecure-skip-signature  unsafe: skip cosign, still check SHA256SUMS' \
    '  --release-key FILE  cosign public key for a key-signed release (testing)' \
    '  --fetch ASSET       only download ASSET, verified, into this directory' \
    '  --help              show this help'
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
    --fastpath) FASTPATH=yes; shift ;;
    --no-fastpath) FASTPATH=no; shift ;;
    --skip-checks) SKIP_CHECKS=1; shift ;;
    --insecure-skip-signature) INSECURE_SKIP_SIGNATURE=1; shift ;;
    --release-key) [ $# -ge 2 ] || err "--release-key needs an argument"; RELEASE_KEY_FILE="$2"; shift 2 ;;
    --release-key=*) RELEASE_KEY_FILE="${1#--release-key=}"; shift ;;
    --fetch) [ $# -ge 2 ] && [ -n "$2" ] || err "--fetch needs an argument"; FETCH="$2"; shift 2 ;;
    --fetch=*) FETCH="${1#--fetch=}"; [ -n "$FETCH" ] || err "--fetch needs an argument"; shift ;;
    -h|--help) usage 0 ;;
    -*) err "unknown option: $1 (try --help)" ;;
    *) [ -z "$VERSION" ] || err "unexpected argument: $1"; VERSION="$1"; shift ;;
  esac
done

# A fetched asset is written under its own name into the current directory:
# one plain file name, never a path.
case "$FETCH" in
  [!A-Za-z0-9]*|*[!A-Za-z0-9._-]*) err "--fetch takes the file name of one release asset, not '$FETCH'" ;;
esac

case "$FASTPATH" in
  ask|yes|no) ;;
  *) err "VMLAB_FASTPATH_CAPS must be yes, no or ask, not '$FASTPATH'" ;;
esac

have() { command -v "$1" >/dev/null 2>&1; }

case "$INSECURE_SKIP_SIGNATURE" in
  0|1) ;;
  *) err "VMLAB_INSECURE_SKIP_SIGNATURE must be 0 or 1" ;;
esac

# ── HTTP helper (curl or wget) ──────────────────────────────────────────────
if command -v curl >/dev/null 2>&1; then
  http_get()      { curl -fsSL "$1"; }
  download_file() { curl -fsSL -o "$2" "$1"; }
  latest_url() { curl -fsSL -o /dev/null -w '%{url_effective}' "https://github.com/$REPO/releases/latest"; }
elif command -v wget >/dev/null 2>&1; then
  http_get()      { wget -qO- "$1"; }
  download_file() { wget -qO "$2" "$1"; }
  latest_url() {
    wget --server-response -O /dev/null "https://github.com/$REPO/releases/latest" 2>&1 |
      sed -n 's/^[[:space:]]*[Ll]ocation: \([^[:space:]]*\).*/\1/p' | tail -n1
  }
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
  if [ -z "$tag" ]; then
    resolved="$(latest_url 2>/dev/null || true)"
    case "$resolved" in
      https://github.com/"$REPO"/releases/tag/v*) tag="${resolved##*/}" ;;
    esac
  fi
  [ -n "$tag" ] || err "no stable release published yet.
vmlab is pre-release only for now — re-run with --pre to get the newest pre-release:
  curl -fsSL https://vmlab.io/install.sh | sh -s -- --pre
See $( printf 'https://github.com/%s/releases' "$REPO" )"
fi

ver="${tag#v}"
case "$ver" in
  ''|*[!0-9A-Za-z.-]*) err "invalid release version" ;;
esac
if [ "$INSECURE_SKIP_SIGNATURE" -eq 1 ]; then
  warn "INSECURE: skipping release signature verification. An attacker can replace both the assets and SHA256SUMS to execute malicious code. Checksums alone do not authenticate the release."
else
  have cosign || err "need cosign (v2.6.1 or newer) on PATH to verify release signatures; install it from https://docs.sigstore.dev/cosign/system_config/installation/"
fi
have sha256sum || err "need sha256sum on PATH to verify release assets"
asset="vmlab-${ver}-${suffix}"
BASE_URL="${BASE_URL%/}"
url="$BASE_URL/$tag/$asset"

# ── Download + install ──────────────────────────────────────────────────────
[ -n "$FETCH" ] || printf 'Installing vmlab %s to %s\n' "$ver" "$BIN_DIR"

verify_dir="$(mktemp -d)"
[ -n "$verify_dir" ] && [ "$verify_dir" != / ] || err "unsafe verification directory"
bin_stage=""
cleanup_release() {
  if [ -n "$bin_stage" ] && [ "$bin_stage" != / ]; then
    rm -rf -- "$bin_stage"
  fi
  [ -n "$verify_dir" ] && [ "$verify_dir" != / ] || return 1
  rm -rf "$verify_dir"
}
trap cleanup_release EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
download_file "$BASE_URL/$tag/SHA256SUMS" "$verify_dir/SHA256SUMS" || err "release has no checksum manifest"

# The public half of the release key as a file, or nothing when the switch is off.
release_key() {
  if [ -n "$RELEASE_KEY_FILE" ]; then
    [ -s "$RELEASE_KEY_FILE" ] || err "no release key at $RELEASE_KEY_FILE"
    warn "verifying with the release key $RELEASE_KEY_FILE, not the one of this installer."
    printf '%s\n' "$RELEASE_KEY_FILE"
  elif [ -n "$RELEASE_KEY" ]; then
    printf '%s\n' "$RELEASE_KEY" > "$verify_dir/release-key.pub"
    printf '%s\n' "$verify_dir/release-key.pub"
  fi
}

# verify_signature: authenticate SHA256SUMS, or end the install. A key-signed
# release is taken only when a release key is known. The signature of a key
# pair is in no transparency log, so that check is turned off for it alone.
verify_signature() {
  key="$(release_key)" || exit 1
  if download_file "$BASE_URL/$tag/SHA256SUMS.key.bundle" "$verify_dir/SHA256SUMS.key.bundle" 2>/dev/null; then
    [ -n "$key" ] || err "this release is signed with a release key, and this installer has none to verify it with. Get the current installer from https://vmlab.io/install.sh"
    cosign verify-blob --key "$key" --bundle "$verify_dir/SHA256SUMS.key.bundle" \
      --insecure-ignore-tlog=true "$verify_dir/SHA256SUMS" \
      || err "release signature verification failed"
    return 0
  fi
  [ -z "$RELEASE_KEY_FILE" ] || err "release has no key signature (SHA256SUMS.key.bundle) for --release-key"
  download_file "$BASE_URL/$tag/SHA256SUMS.bundle" "$verify_dir/SHA256SUMS.bundle" \
    || err "release has no signature bundle. An unsigned release installs only with --insecure-skip-signature"
  cosign verify-blob --bundle "$verify_dir/SHA256SUMS.bundle" \
    --certificate-identity 'https://github.com/VMLabDev/vmlab/.github/workflows/ci.yml@refs/heads/main' \
    --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
    "$verify_dir/SHA256SUMS" || err "release signature verification failed"
}

[ "$INSECURE_SKIP_SIGNATURE" -eq 1 ] || verify_signature

verify_asset() {
  # Match one exact basename, never interpret paths supplied by a manifest.
  want="$(awk -v name="$1" '$2 == name && NF == 2 {print $1}' "$verify_dir/SHA256SUMS")"
  [ "${#want}" -eq 64 ] || err "missing or duplicate checksum for $1"
  case "$want" in *[!0-9a-f]*) err "invalid checksum for $1" ;; esac
  got="$(sha256sum "$2" | cut -d' ' -f1)" || err "cannot compute SHA256 for $1"
  [ "$want" = "$got" ] || err "checksum mismatch for $1"
}

# ── Fetch one asset ─────────────────────────────────────────────────────────
# --fetch: the asset reaches the current directory only after its bytes match
# the line of the authenticated SHA256SUMS. The .sha256 file a release has
# beside an asset is covered by no signature and is never read here.
if [ -n "$FETCH" ]; then
  fetch_tmp="$verify_dir/$FETCH"
  download_file "$BASE_URL/$tag/$FETCH" "$fetch_tmp" || err "download failed: $BASE_URL/$tag/$FETCH"
  verify_asset "$FETCH" "$fetch_tmp"
  [ -n "$fetch_tmp" ] && [ "$fetch_tmp" != / ] || err "fetch_tmp is empty or /"
  mv "$fetch_tmp" "./$FETCH"
  printf 'Fetched: %s (verified against SHA256SUMS of vmlab %s)\n' "$FETCH" "$ver"
  exit 0
fi

tmp="$verify_dir/$asset"
download_file "$url" "$tmp" || err "download failed: $url
The release may not exist or may lack a $suffix asset. See https://github.com/$REPO/releases"
verify_asset "$asset" "$tmp"

# ── Fast-path helper ────────────────────────────────────────────────────────
# vmlab-fastpath, the only binary the fast path's capabilities go to. The
# daemons look for it beside vmlab. A release without it costs only the fast
# path: the userspace fabric carries every lab without it.
#
# It is downloaded and verified before vmlab is replaced: the daemons and the
# helper exchange no version, so a helper this script refuses must not end
# with the new vmlab beside the helper of the earlier install.
helper_asset="vmlab-fastpath-${ver}-${suffix}"
helper_tmp="$verify_dir/$helper_asset"
HELPER=""
HELPER_SHA=""
if download_file "$BASE_URL/$tag/$helper_asset" "$helper_tmp" 2>/dev/null; then
  verify_asset "$helper_asset" "$helper_tmp"
  HELPER_SHA="$want"
  HELPER="$BIN_DIR/vmlab-fastpath"
fi

[ -n "$BIN_DIR" ] && [ "$BIN_DIR" != / ] || err "BIN_DIR is empty or /"
mkdir -p -- "$BIN_DIR"

# Finish both copies before touching the installed binaries. The final
# renames stay on the destination filesystem even when TMPDIR is elsewhere.
bin_stage="$(mktemp -d -- "$BIN_DIR/.vmlab-install.XXXXXX")"
[ -n "$bin_stage" ] && [ "$bin_stage" != / ] || err "unsafe binary staging directory"
[ -n "$tmp" ] && [ "$tmp" != / ] || err "tmp is empty or /"
cp -- "$tmp" "$bin_stage/vmlab"
chmod +x "$bin_stage/vmlab"
if [ -n "$HELPER" ]; then
  [ -n "$helper_tmp" ] && [ "$helper_tmp" != / ] || err "helper_tmp is empty or /"
  cp -- "$helper_tmp" "$bin_stage/vmlab-fastpath"
  # Only the owner may execute a helper that holds file capabilities.
  chmod 0700 "$bin_stage/vmlab-fastpath"
fi

# No helper from this release: the helper of an earlier install, which may
# still hold file capabilities, goes away before the new vmlab arrives. Its
# daemons would start it otherwise, an older privileged binary than the user
# was told about. Removing the file removes its capabilities with it.
if [ -z "$HELPER" ]; then
  old_helper="$BIN_DIR/vmlab-fastpath"
  if [ -e "$old_helper" ] || [ -L "$old_helper" ]; then
    rm -f -- "$old_helper" \
      || err "this release gave no $helper_asset, and the vmlab-fastpath of the earlier install at $old_helper cannot be removed. Nothing was installed. Remove it and install again."
    warn "this release gave no $helper_asset, so the eBPF fast path stays off; the userspace network fabric needs no privilege. Removed the vmlab-fastpath of the earlier install ($old_helper) and its capabilities with it: it is not the helper of this vmlab."
  else
    warn "this release has no $helper_asset, so the eBPF fast path stays off; the userspace network fabric needs no privilege."
  fi
fi

mv -fT -- "$bin_stage/vmlab" "$BIN_DIR/vmlab"
if [ -n "$HELPER" ]; then
  mv -fT -- "$bin_stage/vmlab-fastpath" "$HELPER"
fi
rmdir -- "$bin_stage"
bin_stage=""

printf 'Installed: %s\n' "$("$BIN_DIR/vmlab" --version 2>/dev/null || echo "$BIN_DIR/vmlab")"

# ── PATH hint ───────────────────────────────────────────────────────────────
case ":$PATH:" in
  *":$BIN_DIR:"*) ;;
  *) printf '\n%s is not on your PATH. Add it, e.g.:\n  export PATH="%s:$PATH"\n' "$BIN_DIR" "$BIN_DIR" ;;
esac

# ── eBPF fast path ──────────────────────────────────────────────────────────
# The kernel fast-path tiers need CAP_BPF and CAP_NET_ADMIN (vmlab fastpath),
# granted to the helper only. Every daemon proves a tier works before using
# it and falls back to the userspace fabric otherwise, so this is an
# optimisation, never a requirement. Nothing here can fail the install: the
# binaries are already in place.
CAPS="cap_bpf,cap_net_admin+ep"

fastpath_hint() {
  printf '\nThe eBPF network fast path needs CAP_BPF and CAP_NET_ADMIN on its helper. To grant them:\n'
  printf '  sudo setcap %s %s\n' "$CAPS" "$HELPER"
  printf 'Without them vmlab uses its userspace network fabric, which needs no privilege.\n'
}

fastpath_warning() {
  printf '%s\n' 'NOTE: only the vmlab-fastpath helper gets CAP_BPF and CAP_NET_ADMIN, not vmlab. Any program running as you can start it and ask it to create tap devices and load and update vmlab'"'"'s own XDP and sockmap programs on them; it refuses every other request. It is installed for you alone (mode 0700) and serves no other user.'
}

# Ask on the terminal: under `curl | sh` the script itself is stdin.
fastpath_ask() {
  fastpath_warning >/dev/tty
  printf '\nGrant the vmlab-fastpath helper CAP_BPF and CAP_NET_ADMIN for the eBPF network fast path?\n' >/dev/tty
  printf 'This runs, under sudo: setcap %s on a copy of %s that root checked against the release checksum.\n' "$CAPS" "$HELPER" >/dev/tty
  printf '[y/N] ' >/dev/tty
  read -r answer </dev/tty || answer=""
  case "$answer" in
    [Yy]|[Yy][Ee][Ss]) return 0 ;;
    *) return 1 ;;
  esac
}

# What root runs for the grant: $1 the capabilities, $2 the bin directory, $3
# the SHA-256 the signed SHA256SUMS names for the helper, stdin the helper.
# The bin directory is the user's, so any program running as the user can
# rename another file onto the helper's path while the installer waits at its
# prompt or sudo at its password. Root therefore never grants a path there. It
# copies stdin into a directory only root can enter, checks the digest of that
# copy, grants the copy, and renames it onto the helper's path: the rename
# keeps the capabilities, and the staging directory is in the bin directory so
# that it is one filesystem. Exit 3: the bytes are not the helper. Exit 4: the
# staging directory is not the one made here.
FASTPATH_GRANT='
set -eu
umask 077
caps=$1 want=$3
cd -- "$2"
bin=$(pwd -P)
stage=$(mktemp -d "$bin/.vmlab-fastpath.XXXXXX")
cd -- "$stage"
# The name was looked up in the user'"'"'s directory a second time: prove that
# this is a fresh directory of ours, directly in the bin directory. A user
# cannot move a directory they may not write to another parent, so ".." is
# the bin directory from here on.
here=$(pwd -P)
case "$here" in
  "$bin"/.vmlab-fastpath.??????) ;;
  *) exit 4 ;;
esac
[ -O . ] && [ "$(stat -c %a .)" = 700 ] && [ -z "$(ls -A .)" ] || exit 4
name=${here##*/}
[ -n "$name" ] || exit 4
trap '"'"'rm -f ./vmlab-fastpath; cd .. && rmdir "./$name"'"'"' EXIT
cat > ./vmlab-fastpath
got=$(sha256sum ./vmlab-fastpath | cut -d" " -f1)
[ "$got" = "$want" ] || exit 3
# chown drops file capabilities, so it comes before setcap.
chown "$(stat -c %u:%g ..)" ./vmlab-fastpath
# Owner only: file capabilities go to whoever executes the file (F2).
chmod 700 ./vmlab-fastpath
setcap "$caps" ./vmlab-fastpath
mv -fT ./vmlab-fastpath ../vmlab-fastpath
'

fastpath_caps() {
  # root's daemons hold every capability already.
  [ "$(id -u)" -eq 0 ] && return 0
  [ "$FASTPATH" = no ] && return 0
  [ -n "$HELPER" ] || return 0
  [ "$FASTPATH" != yes ] || fastpath_warning >&2
  if grep -qi microsoft /proc/sys/kernel/osrelease 2>/dev/null; then
    [ "$FASTPATH" = yes ] && printf '\nWSL 2 cannot run the eBPF fast path, so no capabilities were granted.\n'
    return 0
  fi
  if ! have setcap; then
    printf '\nsetcap is not installed (libcap2-bin on Debian/Ubuntu, libcap elsewhere).\n'
    fastpath_hint
    return 0
  fi
  # The kernel ignores file capabilities on a nosuid mount.
  if have findmnt; then
    case ",$(findmnt -no OPTIONS -T "$BIN_DIR" 2>/dev/null)," in
      *,nosuid,*)
        printf '\n%s is on a nosuid mount, where file capabilities do nothing; the eBPF\n' "$BIN_DIR"
        printf 'fast path needs vmlab and its helper installed elsewhere (--bin-dir).\n'
        return 0 ;;
    esac
  fi
  if [ "$FASTPATH" = ask ]; then
    # In a subshell: a failed redirect on `:`, a special builtin, would end
    # this shell.
    if ! ( : </dev/tty ) 2>/dev/null; then
      printf '\nNo terminal: no capabilities granted. Use --fastpath to grant them to the vmlab-fastpath helper.\n'
      return 0
    fi
    fastpath_ask || { printf 'Not granted. '; fastpath_hint; return 0; }
  fi
  if ! have sudo; then
    printf '\nsudo is not installed.\n'
    fastpath_hint
    return 0
  fi
  # sudo asks for its password on the terminal, not on stdin.
  # The helper is opened here, as the user, and root reads only that stream.
  grant=0
  sudo sh -c "$FASTPATH_GRANT" sh "$CAPS" "$BIN_DIR" "$HELPER_SHA" < "$HELPER" || grant=$?
  case "$grant" in
    0)
      printf 'Granted: %s\n' "$(getcap "$HELPER" 2>/dev/null || echo "$CAPS")"
      printf 'Daemons already running keep their old privileges: run `vmlab daemon stop`,\n'
      printf 'then `vmlab fastpath` shows the tier the next daemon selects.\n' ;;
    3)
      warn "no capabilities granted: the file at $HELPER is not the helper of this release (its SHA-256 differs from the signed checksum). Something replaced it after it was installed. Find out what before you install again." ;;
    4)
      warn "no capabilities granted: something changed $BIN_DIR while the grant ran. Find out what before you install again." ;;
    *)
      warn "setcap failed; the helper is installed without the fast path's capabilities."
      fastpath_hint ;;
  esac
}

fastpath_caps

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

install_guest() {
  [ -n "$GUEST_DIR" ] && [ "$GUEST_DIR" != / ] || err "unsafe guest directory"
  # Never delete an arbitrary directory supplied through --guest-dir. Adopt
  # older unmarked installs only when ownership and the bundle layout match.
  if [ -e "$GUEST_DIR" ] || [ -L "$GUEST_DIR" ]; then
    if [ -L "$GUEST_DIR" ] || [ ! -d "$GUEST_DIR" ] \
      || [ "$(stat -c %u -- "$GUEST_DIR" 2>/dev/null || true)" != "$(id -u)" ]; then
      guest_fallback "refusing to replace symlink or non-owned guest directory $GUEST_DIR."
      return 0
    fi
    if [ ! -d "$GUEST_DIR/agent" ] || [ ! -d "$GUEST_DIR/firmware" ] \
      || [ ! -f "$GUEST_DIR/agent/linux-x86_64/vmlab-agent" ] \
      || [ ! -f "$GUEST_DIR/x86_64/vmlinuz" ] \
      || [ ! -f "$GUEST_DIR/x86_64/initramfs.img" ]; then
      guest_fallback "refusing to replace unrelated guest directory $GUEST_DIR: expected the vmlab guest bundle layout."
      return 0
    fi
    touch "$GUEST_DIR/.vmlab-guest" || { guest_fallback "cannot mark $GUEST_DIR."; return 0; }
  fi
  gname="vmlab-guest-${ver}.tar.gz"
  gurl="$BASE_URL/$tag/$gname"
  printf '\nInstalling guest assets to %s\n' "$GUEST_DIR"

  # Staged beside the target, so the swap below is a rename on one filesystem.
  gparent="$(dirname "$GUEST_DIR")"
  mkdir -p "$gparent" || { guest_fallback "cannot create $gparent"; return 0; }
  gtmp="$(mktemp -d "$gparent/.vmlab-guest.XXXXXX")" \
    || { guest_fallback "cannot create a staging directory in $gparent"; return 0; }
  [ -n "$gtmp" ] && [ -d "$gtmp" ] && [ "$gtmp" != / ] \
    || { guest_fallback "the staging directory in $gparent is missing"; return 0; }
  # The old guest directory while it is moved aside, else empty. It may be a
  # long source build (`just guest-install`), so no exit deletes it: it goes
  # back to $GUEST_DIR, or it stays in the staging directory under a name the
  # warning gives.
  gold=""
  gkept=0
  restore_guest() {
    [ -n "$gold" ] || return 0
    [ "$gkept" -eq 0 ] || return 1
    # -T: a $GUEST_DIR that something made again is not moved into.
    if { [ -e "$gold" ] || [ -L "$gold" ]; } && ! mv -T -- "$gold" "$GUEST_DIR"; then
      gkept=1
      warn "cannot put the previous guest directory back at $GUEST_DIR. It is saved at $gold"
      return 1
    fi
    gold=""
  }
  cleanup_guest() {
    [ -n "$gtmp" ] && [ "$gtmp" != / ] || return 1
    if restore_guest; then
      rm -rf -- "$gtmp"
    else
      rm -rf -- "$gtmp/new" "$gtmp/$gname"
    fi
    cleanup_release
  }
  trap cleanup_guest EXIT

  if ! download_file "$gurl" "$gtmp/$gname"; then
    guest_fallback "could not download $gurl."
    return 0
  fi

  verify_asset "$gname" "$gtmp/$gname"

  mkdir "$gtmp/new"
  if ! tar -xzf "$gtmp/$gname" -C "$gtmp/new" || [ ! -d "$gtmp/new/agent" ]; then
    guest_fallback "$gname did not unpack into a guest directory."
    return 0
  fi
  touch "$gtmp/new/.vmlab-guest"

  # Swap: the old directory moves aside whole (nothing from an older version
  # survives in the new one), the new one renames into its place, and the old
  # one is removed. `gold` is set before the first rename and cleared after
  # the second, so a failure or a signal anywhere between them reaches
  # restore_guest, here or in the exit trap.
  if [ -e "$GUEST_DIR" ] || [ -L "$GUEST_DIR" ]; then
    gold="$gtmp/old"
    mv -T -- "$GUEST_DIR" "$gold" || { guest_fallback "cannot move the old $GUEST_DIR aside."; return 0; }
  fi
  if ! mv -T -- "$gtmp/new" "$GUEST_DIR"; then
    restore_guest || true
    guest_fallback "cannot move the new guest assets into $GUEST_DIR."
    return 0
  fi
  gold=""
  rm -rf -- "$gtmp"
  trap cleanup_release EXIT

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
want remote-viewer      "vmlab console and gui = true" gvncviewer vncviewer

# virtiofsd ships as an internal helper off PATH on most distros, so look
# where vmlab looks: $VMLAB_VIRTIOFSD, PATH, then the distro locations.
virtiofsd_bin=''
for c in "${VMLAB_VIRTIOFSD:-}" "$(command -v virtiofsd 2>/dev/null || true)" \
         /usr/lib/virtiofsd /usr/libexec/virtiofsd /usr/lib/qemu/virtiofsd; do
  if [ -n "$c" ] && [ -f "$c" ] && [ -x "$c" ]; then virtiofsd_bin=$c; break; fi
done
virtiofsd_old=''
if [ -z "$virtiofsd_bin" ]; then
  missing="${missing}virtiofsd	shared folders over virtiofs (smbd is the fallback)
"
else
  # Older releases still serve shares, without the flags two features need.
  virtiofsd_help=$("$virtiofsd_bin" --help 2>&1 || true)
  case "$virtiofsd_help" in
    *--shared-dir*) ;;
    *) virtiofsd_old="cannot serve shares at all (it has no --shared-dir)" ;;
  esac
  if [ -z "$virtiofsd_old" ]; then
    case "$virtiofsd_help" in
      *--migration-mode*) ;;
      *) virtiofsd_old="no --migration-mode: VMs with virtiofs shares cannot be snapshotted online (needs 1.11.0)" ;;
    esac
    case "$virtiofsd_help" in
      *--readonly*) ;;
      *) virtiofsd_old="${virtiofsd_old:+$virtiofsd_old
  }no --readonly: read-only shares cannot use virtiofs (needs 1.13.0)" ;;
    esac
  fi
fi

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

if [ -n "$virtiofsd_old" ]; then
  virtiofsd_version=$("$virtiofsd_bin" --version 2>/dev/null | head -n 1)
  printf '\n%s (%s) is too old for every vmlab feature:\n  %s\n' \
    "${virtiofsd_version:-virtiofsd}" "$virtiofsd_bin" "$virtiofsd_old"
  printf 'Install virtiofsd 1.13.0 or later (https://gitlab.com/virtio-fs/virtiofsd/-/releases)\n'
  printf 'and point VMLAB_VIRTIOFSD at it if it is not the one above.\n'
fi

if [ ! -e /dev/kvm ]; then
  printf '\n/dev/kvm is missing: guests will run under emulation, which is very slow.\n'
  printf 'Enable virtualisation in the BIOS, or load kvm_intel / kvm_amd.\n'
elif [ ! -r /dev/kvm ] || [ ! -w /dev/kvm ]; then
  printf '\n/dev/kvm exists but this user cannot use it. Add yourself to the kvm group:\n'
  printf '  sudo usermod -aG kvm "$USER"    # then log out and back in\n'
fi
