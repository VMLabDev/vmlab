#!/usr/bin/env python3
"""Build the pinned upstream iPXE as standalone PCI ROMs, for vmlab's guest
firmware (`--profile guest`, the default) or vmlab-hv's NIC tests (`hv`)."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]

# What each profile builds and where. Both share the pin, the console
# configuration and this procedure; they differ in `config/<profile>/` and in
# which ROMs they make (README.md says why).
PROFILES = {
    "guest": {
        "source": ROOT / "guest/.cache/ipxe/upstream",
        "output": ROOT / "guest/dist/firmware/x86_64",
        # Combined legacy and EFI ROMs, with the virtio-net halves on their own.
        "models": (("virtio-net", "1af41000"), ("e1000", "8086100e"),
                   ("rtl8139", "10ec8139"), ("pcnet", "10222000")),
        "singles": "virtio-net",
        "sums": "IPXE-SHA256SUMS",
        "order": None,
    },
    "hv": {
        "source": ROOT / "hv/target/ipxe/upstream",
        "output": ROOT / "hv/target/ipxe",
        "models": (("virtio-net", "1af41000"),),
        "singles": "virtio-net",
        "sums": "SHA256SUMS",
        # The order hv's recorded SHA256SUMS.hv lists them in.
        "order": ("1af41000.rom", "1af41000.efirom", "virtio-net.rom", "ipxe-source.tar"),
    },
}


def run(*arguments, **kwargs):
    return subprocess.run(arguments, check=True, **kwargs)


def configs(profile):
    """The local configuration headers: shared first, then the profile's."""
    return [*sorted((HERE / "config").glob("*.h")),
            *sorted((HERE / "config" / profile).glob("*.h"))]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=sorted(PROFILES), default="guest")
    profile = parser.parse_args().profile
    plan = PROFILES[profile]
    pin = json.loads((HERE / "upstream.json").read_text())
    source, output = plan["source"], plan["output"]
    source.parent.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
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
    for config in configs(profile):
        shutil.copyfile(config, local / config.name)
    env = dict(os.environ, SOURCE_DATE_EPOCH=pin["source_date_epoch"], LC_ALL="C", TZ="UTC")
    names = []
    for model, pci in plan["models"]:
        legacy, efi = f"bin/{pci}.rom", f"bin-x86_64-efi/{pci}.efirom"
        run("make", "-C", str(src), f"-j{min(os.cpu_count() or 1, 8)}",
            "BUILD_SERIAL=0", "NO_WERROR=1", legacy, efi, env=env)
        name = f"{model}.rom"
        with (output / name).open("wb") as combined:
            run("perl", str(src / "util/catrom.pl"), str(src / legacy),
                str(src / efi), stdout=combined, env=env)
        names.append(name)
        if model == plan["singles"]:
            for target in (legacy, efi):
                name = Path(target).name
                shutil.copyfile(src / target, output / name)
                names.append(name)
    # Complete corresponding source, including the exact local configuration.
    with (output / "ipxe-source.tar").open("wb") as tar:
        run("git", "-C", str(source), "archive", "--format=tar", "--prefix=ipxe/",
            pin["commit"], stdout=tar)
    with tarfile.open(output / "ipxe-source.tar", "a") as archive:
        for path in (HERE / "upstream.json", HERE / "build.py", *configs(profile)):
            info = archive.gettarinfo(str(path), "ipxe/vmlab-build/" + path.relative_to(HERE).as_posix())
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mtime = int(pin["source_date_epoch"])
            info.mode = 0o644
            with path.open("rb") as data:
                archive.addfile(info, data)
    names.append("ipxe-source.tar")
    order = plan["order"] or names
    (output / plan["sums"]).write_text("".join(
        f"{hashlib.sha256((output / name).read_bytes()).hexdigest()}  {name}\n" for name in order))
    print((output / plan["sums"]).read_text(), end="")


if __name__ == "__main__":
    main()
