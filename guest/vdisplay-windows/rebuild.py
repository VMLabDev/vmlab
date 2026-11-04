#!/usr/bin/env python3
"""Build and test-sign the driver using an isolated, memory-capped vmlab build VM."""

import argparse
import importlib.util
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import zipfile

SOURCE = Path(__file__).resolve().parent
ROOT = SOURCE.parent.parent
SOURCES = ["driver.cpp", "protocol.h", "ivshmem.h", "install.cpp", "vmlab-display.inf", "build.py",
           "build.ps1", "sign.ps1", "install.ps1", "dependencies.json", "LICENSE-Microsoft.txt",
           "LICENSE-IVSHMEM.txt", "build-vm/toolchain/toolchain.json"]


def extract_package(archive, directory):
    root = Path(directory).resolve()
    with zipfile.ZipFile(archive) as package:
        for member in package.infolist():
            # Windows PowerShell's Compress-Archive uses backslashes in ZIP member names.
            destination = (root / member.filename.replace("\\", "/")).resolve()
            if not destination.is_relative_to(root):
                raise ValueError(f"Unsafe package path: {member.filename}")
            if member.is_dir():
                destination.mkdir(parents=True, exist_ok=True)
            else:
                destination.parent.mkdir(parents=True, exist_ok=True)
                with package.open(member) as source, destination.open("wb") as output:
                    shutil.copyfileobj(source, output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--route", choices=["container", "windows"], default="container")
    parser.add_argument("--vmlab", type=Path, default=ROOT / "target/debug/vmlab")
    arguments = parser.parse_args()
    executable = str(arguments.vmlab.resolve())
    (SOURCE / "target").mkdir(parents=True, exist_ok=True)
    runtime = Path(tempfile.mkdtemp(prefix="vdb-", dir="/tmp"))
    environment = dict(os.environ, XDG_RUNTIME_DIR=str(runtime / "r"), XDG_STATE_HOME=str(runtime / "s"),
                       XDG_CONFIG_HOME=str(runtime / "c"), VMLAB_SKIP_UPDATES="1")
    environment.pop("DISPLAY", None)
    environment.pop("WAYLAND_DISPLAY", None)
    lab = SOURCE / "build-vm"

    def vmlab(*args, capped=False):
        command = [executable, *map(str, args)]
        if capped:
            command = ["systemd-run", "--user", "--scope", "-q", "-p", "MemoryMax=24G",
                       "-p", "MemorySwapMax=0", "--", "env", "-u", "DISPLAY", "-u", "WAYLAND_DISPLAY",
                       *[f"{name}={environment[name]}" for name in
                         ["XDG_RUNTIME_DIR", "XDG_STATE_HOME", "XDG_CONFIG_HOME", "VMLAB_SKIP_UPDATES"]], *command]
        subprocess.run(command, cwd=lab, env=os.environ if capped else environment, check=True)

    print(f"Build runtime: {runtime}", flush=True)
    spec = importlib.util.spec_from_file_location("vdisplay_build", SOURCE / "build.py")
    build = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(build)
    cache = ROOT / ".ci-cache/vdisplay-windows"
    cache.mkdir(parents=True, exist_ok=True)
    packages = build.fetch(cache)
    if arguments.route == "container":
        subprocess.run([str(ROOT / "build/run.sh"), "python3", str(SOURCE / "build.py"),
                        "--output", str(SOURCE / "target/rebuild-package")], cwd=ROOT, check=True)
    try:
        vmlab("up", capped=True)
        for name, directory in packages.items():
            vmlab("cp", directory.with_suffix(".zip"), f"build:C:/vdisplay-tools/downloads/{name}.zip")
        with tempfile.TemporaryDirectory(prefix="vdisplay-source-", dir=SOURCE / "target") as directory:
            staged = Path(directory)
            for name in SOURCES:
                destination = staged / name
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(SOURCE / name, destination)
            vmlab("cp", staged, "build:C:/vdisplay/src")
        if arguments.route == "container":
            vmlab("cp", SOURCE / "target/rebuild-package", "build:C:/vdisplay/package")
        vmlab("exec", "build", "--user", "SYSTEM", "--timeout", "1800", "--", "powershell.exe",
              "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", "C:/vdisplay/src/build.ps1",
              "-Mode", "sign-only" if arguments.route == "container" else "msvc", "-Package", "C:/vdisplay/package")
        vmlab("exec", "build", "--user", "SYSTEM", "--", "powershell.exe", "-NoProfile", "-NonInteractive",
              "-Command", "Compress-Archive -Path C:/vdisplay/package/* -DestinationPath C:/vdisplay/package.zip -Force")
        archive = SOURCE / "target/signed-package.zip"
        vmlab("cp", "build:C:/vdisplay/package.zip", archive)
        with tempfile.TemporaryDirectory(prefix="vdisplay-signed-", dir=SOURCE / "target") as directory:
            extract_package(archive, directory)
            subprocess.run(["python3", str(SOURCE / "package.py"), directory,
                            str(SOURCE / "target/signed-package")], check=True)
    finally:
        try:
            vmlab("down")
        finally:
            vmlab("daemon", "stop")
    print(f"Signed package: {SOURCE / 'target/signed-package'}", flush=True)


if __name__ == "__main__":
    main()
