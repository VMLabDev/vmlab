#!/usr/bin/env python3
"""Build the shared vmlab-hypervisor iPXE pin as standalone PCI ROMs."""

import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile


HERE = Path(__file__).resolve().parent
GUEST = HERE.parent
OUTPUT = GUEST / "dist/firmware/x86_64"


def run(*arguments, **kwargs):
    return subprocess.run(arguments, check=True, **kwargs)


def main():
    pin = json.loads((HERE / "upstream.json").read_text())
    source = GUEST / ".cache/ipxe/upstream"
    source.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.mkdir(parents=True, exist_ok=True)
    if not source.exists():
        run("git", "clone", "--no-checkout", pin["url"], str(source))
        run("git", "-C", str(source), "checkout", "--detach", pin["commit"])
    actual = subprocess.check_output(
        ["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    if actual != pin["commit"]:
        raise SystemExit(f"{source}: expected {pin['commit']}, got {actual}")
    if subprocess.check_output(["git", "-C", str(source), "diff", "HEAD", "--"]):
        raise SystemExit(f"{source}: tracked changes; refusing an unpinned build")
    if subprocess.check_output(
        ["git", "-C", str(source), "ls-files", "--others", "--exclude-standard"], text=True).strip():
        raise SystemExit(f"{source}: untracked source files; refusing an unpinned build")
    src = source / "src"
    local = src / "config/local"
    local.mkdir(exist_ok=True)
    for config in sorted((HERE / "config").glob("*.h")):
        shutil.copyfile(config, local / config.name)
    env = dict(os.environ, SOURCE_DATE_EPOCH=pin["source_date_epoch"], LC_ALL="C", TZ="UTC")
    names = []
    for model, pci in (("virtio-net", "1af41000"), ("e1000", "8086100e"),
                       ("rtl8139", "10ec8139"), ("pcnet", "10222000")):
        legacy, efi = f"bin/{pci}.rom", f"bin-x86_64-efi/{pci}.efirom"
        run("make", "-C", str(src), f"-j{min(os.cpu_count() or 1, 8)}",
            "BUILD_SERIAL=0", "NO_WERROR=1", legacy, efi, env=env)
        name = f"{model}.rom"
        with (OUTPUT / name).open("wb") as output:
            run("perl", str(src / "util/catrom.pl"), str(src / legacy),
                str(src / efi), stdout=output, env=env)
        names.append(name)
        if model == "virtio-net":
            for target in (legacy, efi):
                name = Path(target).name
                shutil.copyfile(src / target, OUTPUT / name)
                names.append(name)
    # Complete corresponding source, including the exact local configuration.
    with (OUTPUT / "ipxe-source.tar").open("wb") as output:
        run("git", "-C", str(source), "archive", "--format=tar", "--prefix=ipxe/",
            pin["commit"], stdout=output)
    with tarfile.open(OUTPUT / "ipxe-source.tar", "a") as archive:
        for path in (HERE / "upstream.json", HERE / "build.py", *sorted((HERE / "config").glob("*.h"))):
            info = archive.gettarinfo(str(path), "ipxe/vmlab-build/" + path.relative_to(HERE).as_posix())
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mtime = int(pin["source_date_epoch"])
            info.mode = 0o644
            with path.open("rb") as data:
                archive.addfile(info, data)
    names.append("ipxe-source.tar")
    (OUTPUT / "IPXE-SHA256SUMS").write_text("".join(
        f"{hashlib.sha256((OUTPUT / name).read_bytes()).hexdigest()}  {name}\n" for name in names))
    print((OUTPUT / "IPXE-SHA256SUMS").read_text(), end="")


if __name__ == "__main__":
    main()
