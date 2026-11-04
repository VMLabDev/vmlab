#!/usr/bin/env python3
"""Test launcher: add H1's real IVSHMEM device to a Windows VM without a host GPU."""

import json
import os
from pathlib import Path
import sys


def pci_slots(value):
    if isinstance(value, dict):
        if set(("bus", "device", "function")).issubset(value):
            yield (value["bus"], value["device"], value["function"])
        for child in value.values():
            yield from pci_slots(child)
    elif isinstance(value, list):
        for child in value:
            yield from pci_slots(child)


def main():
    binary = Path(os.environ["VMLAB_HV_TEST_BINARY"]).resolve(strict=True)
    arguments = sys.argv[1:]
    if "--machine" in arguments:
        path = Path(arguments[arguments.index("--machine") + 1])
        description = json.loads(path.read_text())
        if description.get("vfio"):
            raise ValueError("This test launcher refuses all host PCI passthrough")
        if description.get("virtual_display"):
            raise ValueError("The machine already declares a virtual display")
        used = set(pci_slots(description))
        slot = next(device for device in range(4, 31) if (0, device, 0) not in used)
        description["virtual_display"] = {"pci": {"bus": 0, "device": slot, "function": 0}}
        path.write_text(json.dumps(description))
    os.execv(binary, [str(binary), *arguments])


if __name__ == "__main__":
    main()
