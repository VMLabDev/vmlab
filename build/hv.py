#!/usr/bin/env python3
"""Build the in-tree hypervisor and its relocatable release bundle."""

import argparse
import contextlib
import gzip
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib

ROOT = Path(__file__).resolve().parent.parent
REPOSITORY = "https://github.com/VMLabDev/vmlab.git"
# The one definition of the virglrenderer pin and the native packages (D12).
PINS = tomllib.loads((ROOT / "build/hv-pins.toml").read_text())
VIRGL_VERSION = PINS["virglrenderer"]["version"]
VIRGL_SHA256 = PINS["virglrenderer"]["sha256"]
VIRGL_URL = PINS["virglrenderer"]["url"].format(version=VIRGL_VERSION)
VIRGL_MESON_OPTIONS = PINS["virglrenderer"]["meson-options"]
PATCHES = sorted((ROOT / "patches/virglrenderer").glob("*.patch"))


def patch_digest(patches):
    return hashlib.sha256(b"".join(
        patch.name.encode() + b"\0" + patch.read_bytes() for patch in patches)).hexdigest()


def options_digest(options):
    return hashlib.sha256(b"".join(option.encode() + b"\0" for option in options)).hexdigest()


def cache_key(version, sha256, patches, options):
    """The virglrenderer build's identity: a new pin, patch set or meson
    option is a new key. The compiler is not in it: a build that must not
    inherit one (a release) does not read the cache at all (`package_virgl`)."""
    return f"virglrenderer-{version}-{sha256}-{patch_digest(patches)}-{options_digest(options)[:16]}"


PATCH_SHA256 = patch_digest(PATCHES)
CACHE_KEY = cache_key(VIRGL_VERSION, VIRGL_SHA256, PATCHES, VIRGL_MESON_OPTIONS)
CACHE = Path(os.environ["VMLAB_TARGET_ROOT"]) / ".ci-cache" if os.environ.get("VMLAB_TARGET_ROOT") else ROOT / ".ci-cache"
VIRGL = CACHE / CACHE_KEY
# Where a dist build makes its own virglrenderer (`package_virgl`).
DIST_VIRGL = ROOT / "target/hv-virgl"


def run(*args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def revision():
    return subprocess.check_output(
        ["git", "-C", str(ROOT), "rev-parse", "HEAD"], text=True).strip()


def native_packages(*groups):
    """The packages of `groups` in build/hv-pins.toml, each named once."""
    packages = []
    for group in groups:
        packages += [p for p in PINS["native-deps"][group] if p not in packages]
    return packages


def sudo_prefix():
    """`sudo` unless this already runs as root (as in a container build)."""
    return [] if os.geteuid() == 0 else ["sudo"]


def apt_install(*groups):
    prefix = sudo_prefix()
    run(*prefix, "apt-get", "update")
    run(*prefix, "apt-get", "install", "-y", "--no-install-recommends",
        *native_packages(*groups))


def native_deps_install():
    # The hypervisor's build and release dependencies, without test-only tools.
    apt_install("virgl", "hypervisor", "release")


def test_native_deps_install():
    apt_install("virgl", "hypervisor", "test")
    prefix = sudo_prefix()
    run(*prefix, "install", "-d", "/usr/share/qemu")
    for name in ("vgabios-stdvga.bin", "vgabios-virtio.bin"):
        target = Path("/usr/share/qemu") / name
        if not target.exists():
            run(*prefix, "ln", "-s", Path("/usr/share/seabios") / name, target)
    if not shutil.which("magick"):
        run(*prefix, "ln", "-s", "/usr/bin/convert", "/usr/local/bin/magick")


def fresh_virgl():
    """A dist build (build/dist.sh sets VMLAB_HV_VIRGL=fresh) packages a
    virglrenderer that it built itself, and reads none from the cache."""
    return os.environ.get("VMLAB_HV_VIRGL") == "fresh"


def tree_manifest(prefix):
    """Every file and link under `prefix` with its content, `.complete` apart."""
    lines = []
    for path in sorted(prefix.rglob("*")):
        name = path.relative_to(prefix).as_posix()
        if name == ".complete":
            continue
        if path.is_symlink():
            lines.append(f"link {os.readlink(path)}  {name}\n")
        elif path.is_file():
            lines.append(f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {name}\n")
        elif not path.is_dir():
            lines.append(f"other  {name}\n")
    return "".join(lines)


def virgl_build(prefix=None):
    """Build the pinned, patched virglrenderer into `prefix`, by default the
    cache directory named by its key.

    The cache directory is reused only while it is the tree that a build here
    left: `.complete` holds the manifest of that tree, written after the
    install, and a tree that differs from it in one file is removed and built
    again. This finds a tree that was cut short, damaged, or put there with no
    manifest. It is no proof against code that can write the cache, which can
    write a manifest too: a dist build does not come here (`package_virgl`)."""
    if prefix is not None:
        CACHE.mkdir(parents=True, exist_ok=True)
        virgl_install(Path(prefix))
        return
    if fresh_virgl():
        print("virgl-build: nothing to do, the package step builds its own virglrenderer", flush=True)
        return
    complete = VIRGL / ".complete"
    if complete.is_file() and not complete.is_symlink():
        recorded = complete.read_text()
        if recorded and recorded == tree_manifest(VIRGL):
            run("pkg-config", f"--with-path={VIRGL}/lib/pkgconfig",
                f"--exact-version={VIRGL_VERSION}", "virglrenderer")
            if not (VIRGL / "libexec/virgl_render_server").is_file():
                raise RuntimeError("cached virglrenderer has no render server")
            return
        print(f"virgl-build: {VIRGL} is not the tree that its build left: building it again", flush=True)
    CACHE.mkdir(parents=True, exist_ok=True)
    remove_tree(VIRGL, "virglrenderer-")
    virgl_install(VIRGL)
    complete.write_text(tree_manifest(VIRGL))


def remove_tree(path, name):
    """Remove the directory `path`, which must be named `name` and not be a link."""
    if path.is_symlink():
        path.unlink()
    elif path.exists():
        if not path.name.startswith(name) or path.parent in (path, Path("/")):
            raise RuntimeError(f"refusing to remove {path}")
        shutil.rmtree(path)


@contextlib.contextmanager
def package_virgl():
    """The virglrenderer that `package` builds against and copies from: the
    cache directory, or for a dist build one that is built now in
    `DIST_VIRGL`, which is emptied first and removed afterwards. That path is
    fixed and inside the checkout because the library keeps the prefix as its
    last-resort render server path: a path under /tmp there could be made by
    any user of the host that runs the bundle, and a random one would change
    the bundle from build to build."""
    if not fresh_virgl():
        yield VIRGL
        return
    remove_tree(DIST_VIRGL, "hv-virgl")
    DIST_VIRGL.mkdir(parents=True)
    try:
        prefix = DIST_VIRGL / "prefix"
        virgl_install(prefix, scratch=DIST_VIRGL)
        yield prefix
    finally:
        remove_tree(DIST_VIRGL, "hv-virgl")


def virgl_install(prefix, scratch=None):
    with tempfile.TemporaryDirectory(prefix="virgl-", dir=scratch or CACHE) as temporary:
        work = Path(temporary)
        archive = work / "virgl.tar.gz"
        run("curl", "-fsSL", "--retry", "3", "-o", archive, VIRGL_URL)
        if hashlib.sha256(archive.read_bytes()).hexdigest() != VIRGL_SHA256:
            raise RuntimeError("virglrenderer tarball checksum mismatch")
        with tarfile.open(archive) as tar:
            tar.extractall(work, filter="data")
        source = work / f"virglrenderer-{VIRGL_VERSION}"
        for patch in PATCHES:
            print(f"Applying {patch.name}", flush=True)
            run("patch", "--batch", "--forward", "-p1", "-i", patch, cwd=source)
        run("meson", "setup", work / "build", source, f"--prefix={prefix}",
            *VIRGL_MESON_OPTIONS)
        run("ninja", "-C", work / "build", "install")
        shutil.copy2(source / "COPYING", prefix / "COPYING")
        (prefix / "BUILDINFO").write_text(
            f"virglrenderer: {VIRGL_VERSION}\nsource-sha256: {VIRGL_SHA256}\n"
            f"patch-sha256: {PATCH_SHA256}\n" + "".join(
                f"patch: {patch.name}\n" for patch in PATCHES)
            + f"server-sha256: {hashlib.sha256((prefix / 'libexec/virgl_render_server').read_bytes()).hexdigest()}\n")



def check_elf(path, rpath):
    run("patchelf", "--set-rpath", rpath, path)
    dynamic = subprocess.check_output(["readelf", "-d", str(path)], text=True)
    print(dynamic, flush=True)
    if f"[{rpath}]" not in dynamic:
        raise RuntimeError(f"incorrect relative RUNPATH on {path}")
    env = os.environ.copy()
    env.pop("LD_LIBRARY_PATH", None)
    linked = subprocess.check_output(["ldd", str(path)], env=env, text=True)
    print(f"ldd {path}:\n{linked}", flush=True)
    if "not found" in linked:
        raise RuntimeError(f"unresolved library in {path}")
    if any("(NEEDED)" in line and "libvirglrenderer.so" in line for line in dynamic.splitlines()):
        bundled = path.parent.parent / "lib"
        if not any("libvirglrenderer.so" in line and "=>" in line
                   and Path(line.split("=>", 1)[1].split()[0]).resolve().parent == bundled
                   for line in linked.splitlines()):
            raise RuntimeError(f"{path} did not resolve bundled virglrenderer")


def stage_viewer_desktop(stage):
    app = "dev.vmlab.viewer"
    applications = stage / "share/applications"
    applications.mkdir(parents=True)
    shutil.copy2(ROOT / "build" / f"{app}.desktop",
                 applications / f"{app}.desktop")
    for size in (16, 24, 32, 48, 64, 128, 256, "scalable"):
        directory = str(size) if size == "scalable" else f"{size}x{size}"
        icons = stage / "share/icons/hicolor" / directory / "apps"
        icons.mkdir(parents=True)
        original = "icon.svg" if size == "scalable" else f"icon-{size}.png"
        suffix = Path(original).suffix
        shutil.copy2(ROOT / "guest/logo" / original, icons / f"{app}{suffix}")
    installer = stage / "install-viewer-icons.sh"
    shutil.copy2(ROOT / "build/install-viewer-icons.sh", installer)
    installer.chmod(0o755)


def package(version):
    if os.uname().machine != "x86_64":
        raise RuntimeError("the hypervisor release bundle requires Linux x86_64")
    version = version.removeprefix("v")
    if not version:
        version = tomllib.loads((ROOT / "Cargo.toml").read_text())["package"]["version"]
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:[-+][A-Za-z0-9.+-]+)?", version):
        raise ValueError(f"invalid release version: {version}")
    with package_virgl() as virgl:
        package_from(virgl, version)


def package_from(virgl, version):
    commit = revision()
    target = ROOT / "target/hv"
    env = os.environ.copy()
    env["PKG_CONFIG_PATH"] = f"{virgl}/lib/pkgconfig"
    env["CARGO_TARGET_DIR"] = str(target)
    run("cargo", "build", "--release", "--locked", "-p", "vmlab-hv", "-p", "vmlab-hv-viewer", "--bin", "vmlab-hv",
        "--bin", "vmlab-hv-viewer", cwd=ROOT, env=env)
    out = ROOT / "target/dist"
    out.mkdir(parents=True, exist_ok=True)
    name = f"vmlab-hv-{version}-linux-x86_64"
    with tempfile.TemporaryDirectory(prefix="hv-package-") as temporary:
        stage = Path(temporary) / name
        for directory in ("bin", "lib", "libexec", "licenses"):
            (stage / directory).mkdir(parents=True)
        stage_viewer_desktop(stage)
        shutil.copy2(target / "release/vmlab-hv", stage / "libexec/vmlab-hv")
        shutil.copy2(target / "release/vmlab-hv-viewer", stage / "bin/vmlab-hv-viewer")
        shutil.copy2(virgl / "libexec/virgl_render_server", stage / "libexec/virgl_render_server")
        shutil.copy2(virgl / "BUILDINFO", stage / "virglrenderer-BUILDINFO")
        for library in (virgl / "lib").glob("libvirglrenderer.so*"):
            shutil.copy2(library, stage / "lib" / library.name, follow_symlinks=False)
        launcher = stage / "bin/vmlab-hv"
        launcher.write_text('''#!/bin/sh
set -eu
root=$(dirname "$(dirname "$(readlink -f "$0")")")
export RENDER_SERVER_EXEC_PATH="${RENDER_SERVER_EXEC_PATH:-$root/libexec/virgl_render_server}"
exec "$root/libexec/vmlab-hv" "$@"
''')
        launcher.chmod(0o755)
        shutil.copy2(virgl / "COPYING", stage / "licenses/virglrenderer-COPYING")
        for notice in ("LICENSE", "LICENSE.crosvm", "THIRD_PARTY_NOTICES"):
            origin = ROOT / notice if notice != "LICENSE.crosvm" else ROOT / "hv" / notice
            shutil.copy2(origin, stage / "licenses" / notice)
        (stage / "README").write_text(f'''vmlab-hv {version} (Linux x86_64)
vmlab commit: {commit}
Source: {REPOSITORY}
virglrenderer: {VIRGL_VERSION}
Source SHA256: {VIRGL_SHA256}
Patch set SHA256: {PATCH_SHA256}
Applied patches: {', '.join(patch.name for patch in PATCHES)}

Keep bin/, lib/ and libexec/ together. Add this bundle's bin/ to PATH,
or set host {{ vmlab_hv = "/absolute/path/to/bin/vmlab-hv" }} in
~/.config/vmlab/config.wcl, following import <vmlab-host.wcl>.
The viewer is beside that launcher and is discovered automatically.

Run ./install-viewer-icons.sh once as your desktop user to install the viewer's
hidden desktop entry and hicolor icons. It uses $XDG_DATA_HOME, defaulting to
~/.local/share: applications/dev.vmlab.viewer.desktop and
icons/hicolor/<size>x<size>/apps/dev.vmlab.viewer.png (16, 24, 32, 48, 64, 128, 256),
plus icons/hicolor/scalable/apps/dev.vmlab.viewer.svg.
Wayland matches the viewer's dev.vmlab.viewer app ID to this entry; X11 also
receives an embedded icon. No menu entry is added. Keep share/ with the bundle
until you have run the helper. Run it again after updating the bundle's icons.

bin/vmlab-hv is a launcher for libexec/vmlab-hv. It sets the supported
RENDER_SERVER_EXEC_PATH override to this bundle's libexec/virgl_render_server
unless you already set it. Venus needs this because virglrenderer otherwise
uses its build-time absolute server path. All ELF files use relative RUNPATHs;
no LD_LIBRARY_PATH is needed. Do not move individual binaries out of the tree.

Host requirements: Linux x86_64, glibc >= 2.39, libgcc_s, libstdc++, /dev/kvm,
libdrm, EGL, GBM, libepoxy, Vulkan loader and the appropriate Mesa or vendor
GPU drivers. Audio uses PipeWire and PulseAudio client libraries. The viewer
also needs Wayland or X11, libxkbcommon and their client libraries. liblzma
and audio client dependencies remain host-provided. On Ubuntu 24.04 install:
  libegl1 libgbm1 libepoxy0 libdrm2 libvulkan1 libpipewire-0.3-0 libpulse0
  libwayland-client0 libxkbcommon0 libxkbcommon-x11-0 libx11-6 libxcursor1
  libxrandr2 libxi6 libxfixes3 libxtst6 liblzma5 libstdc++6
The GPU driver and a desktop/audio session are still needed for those features.
glibc, Mesa and GPU drivers are not bundled. See ldd on the ELF files for
the complete host library dependency list.

Firmware is not looked up relative to this bundle. vmlab writes firmware paths
into the machine description, preferring host.firmware_dir and its installed
guest assets before distro paths. Install the matching vmlab-guest bundle.
No host PCI or USB device is required except explicitly configured passthrough.
''')
        for elf in (stage / "libexec").iterdir():
            check_elf(elf, "$ORIGIN/../lib")
        check_elf(stage / "bin/vmlab-hv-viewer", "$ORIGIN/../lib")
        for elf in (stage / "lib").iterdir():
            if not elf.is_symlink():
                check_elf(elf, "$ORIGIN")
        with (stage / "virglrenderer-BUILDINFO").open("a") as info:
            digest = hashlib.sha256((stage / "libexec/virgl_render_server").read_bytes()).hexdigest()
            info.write(f"bundled-server-sha256: {digest}\n")
        epoch = int(os.environ.get("SOURCE_DATE_EPOCH", subprocess.check_output(
            ["git", "-C", str(ROOT), "show", "-s", "--format=%ct", "HEAD"], text=True).strip()))

        def normalize(info):
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mtime = epoch
            return info

        archive = out / f"{name}.tar.gz"
        with archive.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as zipped:
            with tarfile.open(fileobj=zipped, mode="w") as tar:
                tar.add(stage, arcname=name, filter=normalize)
        checksum = hashlib.sha256(archive.read_bytes()).hexdigest()
        archive.with_suffix(".gz.sha256").write_text(f"{checksum}  {archive.name}\n")
        print(f"Built {archive}\nvmlab commit: {commit}\nSHA256: {checksum}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("revision", "cache-key", "native-deps-install", "test-native-deps-install", "virgl-deps-install", "virgl-build", "package"))
    parser.add_argument("version", nargs="?", default="")
    parser.add_argument("--prefix", help="virgl-build: install here instead of the cache")
    args = parser.parse_args()
    if args.command == "revision":
        print(revision())
    elif args.command == "cache-key":
        print(f"key={CACHE_KEY}")
    elif args.command == "native-deps-install":
        native_deps_install()
    elif args.command == "test-native-deps-install":
        test_native_deps_install()
    elif args.command == "virgl-deps-install":
        apt_install("virgl")
    elif args.command == "virgl-build":
        virgl_build(args.prefix)
    else:
        package(args.version)


if __name__ == "__main__":
    main()
