#!/usr/bin/env python3
"""Boot an aarch64 template on QEMU with the firmware inputs that vmlab-hv
generates, and read from inside the guest how it booted.

`-cpu cortex-a53,pmu=off` is the QEMU machine whose fw_cfg files
`vmlab_hv_firmware::virt` reproduces byte for byte (the captures
`docs/hwref/virt-fwcfg/a53-nopmu-*`, held by `tests/virt_hwref.rs`). So a
kernel that boots here boots with the ACPI tables and the SMBIOS of vmlab-hv.
The script waits for the vmlab agent of the template, which is a user process,
then asks it for the kernel's ACPI lines, the installed tables and the DMI
identity, and writes them to `<output>/result.json`.

QEMU is the test oracle only (phase 3d, P2). The run is one QEMU TCG process
under the memory cap of `bench/lib.py`, pinned, for one to three minutes.

    python3 scripts/hwref/virt_boot.py --output /tmp/virt-boot
    python3 scripts/hwref/virt_boot.py --output /tmp/virt-boot --cpu max --smp 2
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE.parent.parent / "bench"))

import backends  # noqa: E402
import lib  # noqa: E402
from virt_fwcfg import firmware, pad_flash  # noqa: E402

# What the guest is asked, as (key, argv). Each answer is kept as text.
QUESTIONS = [
    ("kernel", ["uname", "-srm"]),
    ("acpi_log", ["sh", "-c", "dmesg | grep -E 'ACPI|psci|PSCI|GIC|SPCR|arch_timer|DMI:|pmu|PMU'"]),
    ("acpi_tables", ["sh", "-c", "ls /sys/firmware/acpi/tables"]),
    ("device_tree", ["sh", "-c", "ls /proc/device-tree 2>&1 | head -n 3; true"]),
    ("madt_hex", ["sh", "-c", "od -An -v -tx1 /sys/firmware/acpi/tables/APIC | tr -d ' \\n'"]),
    ("interrupts", ["sh", "-c", "grep -E 'arch_timer|GICv2m|uart-pl011|ACPI:Ged|pmu' /proc/interrupts"]),
    ("dmi", ["sh", "-c", "cd /sys/class/dmi/id && grep -H . sys_vendor product_name "
             "product_version chassis_vendor bios_vendor 2>/dev/null"]),
    ("cpu_features", ["sh", "-c", "grep -m1 -E '^Features' /proc/cpuinfo"]),
    ("first_processes", ["sh", "-c", "ps -o pid,comm -p 1; cat /proc/uptime"]),
]


def log(message: str) -> None:
    print(time.strftime("[%H:%M:%S]"), message, flush=True)


def quit_qemu(path: str) -> None:
    with socket.socket(socket.AF_UNIX) as stream:
        stream.settimeout(30)
        stream.connect(path)
        reader = stream.makefile("r")
        reader.readline()
        for command in ("qmp_capabilities", "quit"):
            stream.sendall(json.dumps({"execute": command}).encode() + b"\n")
            reader.readline()


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--template", default="debian-13")
    parser.add_argument("--cpu", default="cortex-a53,pmu=off")
    parser.add_argument("--smp", type=int, default=1)
    parser.add_argument("--deadline", type=float, default=900)
    args = parser.parse_args()
    if str(args.output) in ("", "/"):
        parser.error("--output names a directory for the result")
    args.output.mkdir(parents=True, exist_ok=True)

    code, variables = firmware()
    version = lib.latest_template("aarch64", args.template)
    result = {"template": args.template, "template_version": version, "cpu": args.cpu,
              "smp": args.smp}
    # A short run directory: the sockets must fit in a sun_path.
    run = tempfile.mkdtemp(prefix="vb-")
    session = None
    try:
        overlay = os.path.join(run, "overlay.qcow2")
        subprocess.run(["qemu-img", "create", "-q", "-f", "qcow2", "-F", "qcow2", "-b",
                        lib.template_disk("aarch64", args.template, version), overlay],
                       check=True)
        pad_flash(code, Path(run) / "code.fd")
        pad_flash(variables, Path(run) / "vars.fd")
        real_argv = backends.qemu_vm_argv

        def argv(*positional, **named):
            words = real_argv(*positional, **named)
            words[words.index("-cpu") + 1] = args.cpu
            return words

        backends.qemu_vm_argv = argv
        backend = backends.QemuBackend(run, "aarch64", args.smp, lib.pin_cpus(args.smp),
                                       overlay, os.path.join(run, "code.fd"),
                                       os.path.join(run, "vars.fd"), log)
        result["qemu_version"] = backend.backend_version()
        session = backend.launch()
        backend.release_cpus()
        agent, hello = lib.wait_for_agent(backend.agent_socket(), args.deadline,
                                          vm_exited=session.exit_code)
        result["boot_to_agent_ready_s"] = round(session.elapsed(), 2)
        result["agent"] = {key: hello.get(key) for key in ("agent_version", "os")}
        log(f"agent ready after {result['boot_to_agent_ready_s']} s")
        for key, command in QUESTIONS:
            answer = agent.exec(command, 60)
            result[key] = answer["data"].decode(errors="replace").strip().splitlines()
            if answer["code"]:
                result[key + "_exit"] = answer["code"]
        agent.close()
    except Exception as error:  # the result file says what failed
        result["error"] = repr(error)
    finally:
        if session is not None:
            try:
                quit_qemu(backend.qmp_sock)
                session.wait_exit(60)
            except Exception as error:
                result["quit_error"] = repr(error)
            session.kill()
            serial = os.path.join(run, "serial.log")
            if os.path.isfile(serial):
                shutil.copyfile(serial, args.output / "serial.log")
        if os.path.basename(run).startswith("vb-"):
            shutil.rmtree(run, ignore_errors=True)
    (args.output / "result.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result, indent=2))
    return 1 if "error" in result else 0


if __name__ == "__main__":
    sys.exit(main())
