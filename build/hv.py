#!/usr/bin/env python3
"""Build the in-tree hypervisor and its relocatable release bundle."""

import argparse
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
VIRGL_VERSION = "1.3.0"
VIRGL_SHA256 = "065bc56e89e6f631f96101cd62eba0748e48eb888b434edc86e89d05395e76f3"
PATCHES = sorted((ROOT / "patches/virglrenderer").glob("*.patch"))
PATCH_SHA256 = hashlib.sha256(b"".join(
    patch.name.encode() + b"\0" + patch.read_bytes() for patch in PATCHES)).hexdigest()
CACHE_KEY = f"virglrenderer-{VIRGL_VERSION}-{VIRGL_SHA256}-{PATCH_SHA256}"
CACHE = ROOT / ".ci-cache"
VIRGL = CACHE / CACHE_KEY


def run(*args, **kwargs):
    return subprocess.run([str(arg) for arg in args], check=True, **kwargs)


def revision():
    return subprocess.check_output(
        ["git", "-C", str(ROOT), "rev-parse", "HEAD"], text=True).strip()


def native_deps_install():
    # The hypervisor's ci::native-deps-install, without its test-only tools.
    prefix = [] if os.geteuid() == 0 else ["sudo"]
    run(*prefix, "apt-get", "update")
    run(*prefix, "apt-get", "install", "-y", "--no-install-recommends",
        "build-essential", "curl", "ca-certificates", "git", "pkg-config",
        "meson", "ninja-build", "patch", "python3-yaml", "libepoxy-dev", "libdrm-dev",
        "libgbm-dev", "libegl-dev", "libvulkan-dev", "glslang-tools",
        "libpipewire-0.3-dev", "libpulse-dev", "clang", "libclang-dev",
        "liblzma-dev", "libwayland-dev", "libxkbcommon-dev", "libxkbcommon-x11-0",
        "libx11-6", "libxcursor1", "libxrandr2", "libxi6", "libxfixes3",
        "libxtst6", "patchelf", "binutils")


def test_native_deps_install():
    # Keep the imported test dependency list separate until D12's follow-up.
    prefix = [] if os.geteuid() == 0 else ["sudo"]
    run(*prefix, "apt-get", "update")
    run(*prefix, "apt-get", "install", "-y", "--no-install-recommends",
        "build-essential", "gcc-multilib", "liblzma-dev", "curl", "pkg-config",
        "meson", "ninja-build", "patch", "python3-yaml", "libepoxy-dev", "libdrm-dev",
        "libgbm-dev", "libegl-dev", "libvulkan-dev", "glslang-tools",
        "libpipewire-0.3-dev", "libpulse-dev", "clang", "libclang-dev",
        "qemu-utils", "nasm", "acpica-tools", "imagemagick", "cpio", "xvfb",
        "libvulkan1", "mesa-vulkan-drivers", "seabios", "xorriso", "edid-decode",
        "pipewire-bin", "libxkbcommon-x11-0", "libxkbcommon0",
        "libwayland-client0", "libx11-6", "libxcursor1", "libxrandr2",
        "libxi6", "libxfixes3", "libxtst6")
    run(*prefix, "install", "-d", "/usr/share/qemu")
    for name in ("vgabios-stdvga.bin", "vgabios-virtio.bin"):
        target = Path("/usr/share/qemu") / name
        if not target.exists():
            run(*prefix, "ln", "-s", Path("/usr/share/seabios") / name, target)
    if not shutil.which("magick"):
        run(*prefix, "ln", "-s", "/usr/bin/convert", "/usr/local/bin/magick")


def virgl_build():
    if (VIRGL / ".complete").is_file():
        run("pkg-config", f"--with-path={VIRGL}/lib/pkgconfig",
            f"--exact-version={VIRGL_VERSION}", "virglrenderer")
        if not (VIRGL / "libexec/virgl_render_server").is_file():
            raise RuntimeError("cached virglrenderer has no render server")
        return
    CACHE.mkdir(exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="virgl-", dir=CACHE) as temporary:
        work = Path(temporary)
        archive = work / "virgl.tar.gz"
        run("curl", "-fsSL", "--retry", "3", "-o", archive,
            f"https://gitlab.freedesktop.org/virgl/virglrenderer/-/archive/"
            f"{VIRGL_VERSION}/virglrenderer-{VIRGL_VERSION}.tar.gz")
        if hashlib.sha256(archive.read_bytes()).hexdigest() != VIRGL_SHA256:
            raise RuntimeError("virglrenderer tarball checksum mismatch")
        with tarfile.open(archive) as tar:
            tar.extractall(work, filter="data")
        source = work / f"virglrenderer-{VIRGL_VERSION}"
        for patch in PATCHES:
            print(f"Applying {patch.name}", flush=True)
            run("patch", "--batch", "--forward", "-p1", "-i", patch, cwd=source)
        run("meson", "setup", work / "build", source, f"--prefix={VIRGL}",
            "--libdir=lib", "--buildtype=release", "-Dvenus=true", "-Dtests=false")
        run("ninja", "-C", work / "build", "install")
        shutil.copy2(source / "COPYING", VIRGL / "COPYING")
        (VIRGL / "BUILDINFO").write_text(
            f"virglrenderer: {VIRGL_VERSION}\nsource-sha256: {VIRGL_SHA256}\n"
            f"patch-sha256: {PATCH_SHA256}\n" + "".join(
                f"patch: {patch.name}\n" for patch in PATCHES)
            + f"server-sha256: {hashlib.sha256((VIRGL / 'libexec/virgl_render_server').read_bytes()).hexdigest()}\n")
        (VIRGL / ".complete").touch()



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
    commit = revision()
    target = ROOT / "target/hv"
    env = os.environ.copy()
    env["PKG_CONFIG_PATH"] = f"{VIRGL}/lib/pkgconfig"
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
        shutil.copy2(VIRGL / "libexec/virgl_render_server", stage / "libexec/virgl_render_server")
        shutil.copy2(VIRGL / "BUILDINFO", stage / "virglrenderer-BUILDINFO")
        for library in (VIRGL / "lib").glob("libvirglrenderer.so*"):
            shutil.copy2(library, stage / "lib" / library.name, follow_symlinks=False)
        launcher = stage / "bin/vmlab-hv"
        launcher.write_text('''#!/bin/sh
set -eu
root=$(dirname "$(dirname "$(readlink -f "$0")")")
export RENDER_SERVER_EXEC_PATH="${RENDER_SERVER_EXEC_PATH:-$root/libexec/virgl_render_server}"
exec "$root/libexec/vmlab-hv" "$@"
''')
        launcher.chmod(0o755)
        shutil.copy2(VIRGL / "COPYING", stage / "licenses/virglrenderer-COPYING")
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
    parser.add_argument("command", choices=("revision", "cache-key", "native-deps-install", "test-native-deps-install", "virgl-build", "package"))
    parser.add_argument("version", nargs="?", default="")
    args = parser.parse_args()
    if args.command == "revision":
        print(revision())
    elif args.command == "cache-key":
        print(f"key={CACHE_KEY}")
    elif args.command == "native-deps-install":
        native_deps_install()
    elif args.command == "test-native-deps-install":
        test_native_deps_install()
    elif args.command == "virgl-build":
        virgl_build()
    else:
        package(args.version)


if __name__ == "__main__":
    main()
