#!/usr/bin/env python3
"""hwref for aarch64 `virt`: what QEMU hands the firmware through fw_cfg.

QEMU runs with accel=qtest, so no firmware and no guest executes. The script
reads the fw_cfg device at 0x09020000 through qtest: the file directory, every
named file, and the fixed numeric items. The result is the reference side of
the normalised diff of D9 (phase 3, `notes/12-aarch64-acpi.md`): the ACPI
tables as QEMU builds them before a firmware relocates them
(`etc/acpi/tables`, `etc/acpi/rsdp`, `etc/table-loader`), and SMBIOS
(`etc/smbios/smbios-tables`, `etc/smbios/smbios-anchor`).

This is black-box observation only (DESIGN.md of phase 3d, S6). No QEMU source
is read. The fw_cfg register layout is the one of QEMU's published interface
document `docs/specs/fw_cfg.rst` (S7).

    python3 scripts/hwref/virt_fwcfg.py --output docs/hwref/virt-fwcfg
    python3 scripts/hwref/virt_fwcfg.py --list
    python3 scripts/hwref/virt_fwcfg.py --output /tmp/one smp2-2g

Each configuration takes about two seconds and 64 MiB to 8 GiB of address
space that is never touched.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time

FW_CFG = 0x0902_0000
DATA = FW_CFG
SELECTOR = FW_CFG + 8
FILE_DIR = 0x19

# The numeric items that a firmware reads, with the length that is recorded.
NUMERIC = {
    "signature": (0x00, 4),
    "id": (0x01, 4),
    "uuid": (0x02, 16),
    "ram_size": (0x03, 8),
    "nographic": (0x04, 2),
    "nb_cpus": (0x05, 2),
    "boot_menu": (0x0E, 2),
    "max_cpus": (0x0F, 2),
}

FIRMWARE_DIRS = (
    Path.home() / ".local/share/vmlab/guest/firmware/aarch64",
    Path("/usr/share/AAVMF"),
)

# vmlab's argv for a `linux-modern` aarch64 VM, as `bench/backends.py` records
# it, without the sockets. The disk is a null device: no table depends on it.
DEVICES = [
    "-device", "virtio-serial-pci",
    "-blockdev", "driver=null-co,node-name=disk0,size=1073741824",
    "-device", "virtio-blk-pci,drive=disk0",
    "-device", "virtio-gpu-pci",
    "-nic", "none",
    "-device", "qemu-xhci",
    "-device", "usb-tablet",
]

# name: (cpu, smp, memory in MiB, and optionally more arguments).
# `max` is what vmlab's QEMU path presents today. `cortex-a53,pmu=off` is the
# QEMU answer for a CPU without a PMU: it pins the difference V5 exactly.
CONFIGS = {
    "smp1-2g": ("max", 1, 2048),
    "smp2-2g": ("max", 2, 2048),
    "smp4-2g": ("max", 4, 2048),
    "smp2-1g": ("max", 2, 1024),
    "smp2-4g": ("max", 2, 4096),
    "smp8-8g": ("max", 8, 8192),
    "a53-nopmu-smp1-2g": ("cortex-a53,pmu=off", 1, 2048),
    "a53-nopmu-smp2-2g": ("cortex-a53,pmu=off", 2, 2048),
    "a53-nopmu-smp4-2g": ("cortex-a53,pmu=off", 4, 2048),
    "a53-smp2-2g": ("cortex-a53", 2, 2048),
    # Memory sizes that change the encoding of an SMBIOS field, and a UUID.
    "smp2-512m": ("max", 2, 512),
    "smp2-1536m": ("max", 2, 1536),
    "smp2-16g": ("max", 2, 16384),
    "smp2-32g": ("max", 2, 32768),
    "smp2-64g": ("max", 2, 65536),
    "smp2-2g-uuid": ("max", 2, 2048, ["-uuid", "00112233-4455-6677-8899-aabbccddeeff"]),
}


def firmware():
    for directory in FIRMWARE_DIRS:
        code, variables = directory / "AAVMF_CODE.fd", directory / "AAVMF_VARS.fd"
        if code.is_file() and variables.is_file():
            return code, variables
    raise SystemExit("no AAVMF_CODE.fd and AAVMF_VARS.fd: run guest/fetch-firmware.sh aarch64")


def pad_flash(source: Path, target: Path) -> None:
    """A pflash of `virt` is 64 MiB. A distribution file can be shorter."""
    data = source.read_bytes()
    if len(data) > 64 << 20:
        raise SystemExit(f"{source} is larger than 64 MiB")
    with target.open("wb") as stream:
        stream.write(data)
        stream.truncate(64 << 20)


class Qtest:
    def __init__(self, path: Path, process: subprocess.Popen):
        deadline = time.monotonic() + 20
        while True:
            self.stream = socket.socket(socket.AF_UNIX)
            try:
                self.stream.connect(str(path))
                break
            except (FileNotFoundError, ConnectionRefusedError):
                self.stream.close()
            if process.poll() is not None:
                raise RuntimeError(f"QEMU exited {process.returncode} before qtest")
            if time.monotonic() >= deadline:
                raise TimeoutError("waiting for the qtest socket")
            time.sleep(0.02)
        self.stream.settimeout(30)
        self.reader = self.stream.makefile("rb")

    def answers(self, commands: list[str]) -> list[list[str]]:
        self.stream.sendall("".join(command + "\n" for command in commands).encode("ascii"))
        result = []
        while len(result) < len(commands):
            line = self.reader.readline()
            if not line:
                raise RuntimeError("qtest connection closed")
            words = line.decode("ascii").split()
            if words and words[0] == "IRQ":
                continue
            if not words or words[0] != "OK":
                raise RuntimeError(f"qtest answered {line!r}")
            result.append(words[1:])
        return result

    def select(self, key: int) -> None:
        # The selector is a big-endian register. qtest writes in guest order.
        swapped = ((key & 0xFF) << 8) | (key >> 8)
        self.answers([f"writew {SELECTOR:#x} {swapped:#x}"])

    def item(self, key: int, length: int) -> bytes:
        self.select(key)
        data = bytearray()
        while len(data) < length:
            count = min(256, length - len(data))
            data.extend(int(answer[0], 0) for answer in self.answers([f"readb {DATA:#x}"] * count))
        return bytes(data)


def capture(qemu: str, name: str, output: Path) -> dict:
    cpu, smp, memory, *more = CONFIGS[name]
    more = more[0] if more else []
    code, variables = firmware()
    target = output / name
    if target.exists():
        if target.name != name or not (target / "capture.json").is_file():
            raise SystemExit(f"{target} exists and is not a capture: not replaced")
        shutil.rmtree(target)
    blobs = target / "fwcfg"
    blobs.mkdir(parents=True)
    with tempfile.TemporaryDirectory(prefix="virt-fwcfg-") as tmp:
        run = Path(tmp)
        pad_flash(code, run / "code.fd")
        pad_flash(variables, run / "vars.fd")
        logical = [
            "qemu-system-aarch64", "-machine", "virt", "-cpu", cpu, "-smp", str(smp),
            "-m", f"{memory}M", "-display", "none", "-monitor", "none", "-serial", "none",
            "-drive", "if=pflash,format=raw,readonly=on,file=AAVMF_CODE.fd",
            "-drive", "if=pflash,format=raw,file=AAVMF_VARS.fd",
            *DEVICES, *more,
        ]
        argv = [qemu if word == "qemu-system-aarch64" else word for word in logical]
        argv = [word.replace("file=AAVMF_CODE.fd", f"file={run}/code.fd")
                .replace("file=AAVMF_VARS.fd", f"file={run}/vars.fd") for word in argv]
        argv += ["-accel", "qtest", "-qtest", f"unix:{run}/qtest.sock,server=on,wait=off",
                 "-qtest-log", "/dev/null"]
        # QEMU writes to a file: a pipe that nobody reads would stop it.
        with (run / "qemu.log").open("wb") as log:
            process = subprocess.Popen(argv, stdin=subprocess.DEVNULL, stdout=log,
                                       stderr=subprocess.STDOUT)
        try:
            qtest = Qtest(run / "qtest.sock", process)
            numeric = {key: qtest.item(number, length).hex()
                       for key, (number, length) in NUMERIC.items()}
            count = int.from_bytes(qtest.item(FILE_DIR, 4), "big")
            directory = qtest.item(FILE_DIR, 4 + 64 * count)[4:]
            files = []
            for index in range(count):
                entry = directory[index * 64:(index + 1) * 64]
                size = int.from_bytes(entry[:4], "big")
                select = int.from_bytes(entry[4:6], "big")
                file_name = entry[8:].split(b"\0", 1)[0].decode("ascii")
                data = qtest.item(select, size)
                path = blobs / file_name
                if not path.resolve().is_relative_to(blobs.resolve()):
                    raise RuntimeError(f"fw_cfg file name leaves the capture: {file_name!r}")
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_bytes(data)
                files.append({"name": file_name, "select": select, "size": size,
                              "sha256": hashlib.sha256(data).hexdigest()})
        finally:
            process.kill()
            process.wait()
            said = (run / "qemu.log").read_text(errors="replace").strip()
            if sys.exc_info()[0] is not None and said:
                print(f"QEMU said: {said}", file=sys.stderr)
    tables = (blobs / "etc/acpi/tables").read_bytes()
    split = target / "acpi"
    split.mkdir()
    offset = 0
    acpi = []
    while tables[offset:offset + 4].strip(b"\0"):
        signature = tables[offset:offset + 4].decode("ascii")
        length = int.from_bytes(tables[offset + 4:offset + 8], "little")
        if not signature.isalnum() or length < 36 or offset + length > len(tables):
            raise RuntimeError(f"{name}: no ACPI table at offset {offset:#x}")
        (split / signature).write_bytes(tables[offset:offset + length])
        acpi.append({"signature": signature, "offset": offset, "length": length})
        offset += length
    if any(tables[offset:]):
        raise RuntimeError(f"{name}: bytes after the last ACPI table")
    record = {
        "configuration": name,
        "cpu": cpu,
        "smp": smp,
        "memory_mib": memory,
        "qemu_version": subprocess.check_output([qemu, "--version"], text=True).splitlines()[0],
        "qemu_argv": logical + ["-accel", "qtest"],
        "firmware_sha256": {path.name: hashlib.sha256(path.read_bytes()).hexdigest()
                            for path in (code, variables)},
        "numeric": numeric,
        "files": files,
        "acpi_tables": acpi,
    }
    if said:
        record["qemu_said"] = said
    (target / "capture.json").write_text(json.dumps(record, indent=2) + "\n")
    return record


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--output", type=Path)
    parser.add_argument("--qemu", default="qemu-system-aarch64")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("only", nargs="*", help="the configurations to capture (default: all)")
    args = parser.parse_args()
    if args.list:
        for name, (cpu, smp, memory, *more) in CONFIGS.items():
            print(" ".join([f"{name}: -cpu {cpu} -smp {smp} -m {memory}M", *(more[0] if more else [])]))
        return 0
    if args.output is None or str(args.output) in ("", "/"):
        parser.error("--output names the directory of the captures")
    unknown = [name for name in args.only if name not in CONFIGS]
    if unknown:
        parser.error(f"unknown configurations: {unknown}")
    os.makedirs(args.output, exist_ok=True)
    for name in args.only or CONFIGS:
        record = capture(args.qemu, name, args.output)
        sizes = ", ".join(f"{entry['name']}={entry['size']}" for entry in record["files"])
        print(f"OK {name}: {sizes}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
