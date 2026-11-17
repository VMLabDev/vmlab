#!/usr/bin/env python3
"""Verify a signed package against its build inputs and stage it in the guest bundle."""

import argparse
import hashlib
import json
from pathlib import Path
import shutil

SOURCE = Path(__file__).resolve().parent
REQUIRED = ["vmlab-display.inf", "vmlab-display.cat", "vmlab-display.dll", "vmlab-display.cer",
            "install.exe", "install.ps1", "BUILDINFO.json", "ivshmem/ivshmem.inf",
            "ivshmem/ivshmem.cat", "ivshmem/ivshmem.sys"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--unsigned-dry-run", action="store_true",
                        help="stage compiled files only, never a signed installable package")
    arguments = parser.parse_args()
    package = arguments.package.resolve()
    build = json.loads((package / "BUILDINFO.json").read_text(encoding="utf-8-sig"))
    if not arguments.unsigned_dry_run:
        manifest = json.loads((package / "PACKAGE.json").read_text(encoding="utf-8-sig"))
        for name in REQUIRED:
            expected = manifest["files"][name]
            if hashlib.sha256((package / name).read_bytes()).hexdigest() != expected:
                raise ValueError(f"Signed package member changed: {name}")
    for name, expected in build["sources"].items():
        path = (SOURCE / name).resolve()
        if not path.is_relative_to(SOURCE):
            raise ValueError(f"Invalid build input: {name}")
        if hashlib.sha256(path.read_bytes()).hexdigest() != expected:
            raise ValueError(f"Source changed since signing: {name}; rebuild and sign again")
    required = [name for name in REQUIRED
                if not arguments.unsigned_dry_run or name not in
                {"vmlab-display.cat", "vmlab-display.cer"}]
    extra = [] if arguments.unsigned_dry_run else ["PACKAGE.json"]
    for name in required + extra + ["LICENSE-Microsoft.txt", "LICENSE-IVSHMEM.txt"]:
        destination = arguments.output / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(package / name, destination)
    kind = "UNSIGNED dry-run" if arguments.unsigned_dry_run else "Verified signed"
    print(f"{kind} display package: {arguments.output}")


if __name__ == "__main__":
    main()
