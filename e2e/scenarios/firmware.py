"""Firmware: OVMF and SeaBIOS, a TPM and enforcing secure boot, each proved
from inside the guest where it can see it and from the launch where it
cannot — QEMU's argv, or the machine description vmlab-hv was started with —
so the same checks hold on either hypervisor."""

import json
import os
import pathlib

from harness import ScenarioFailed
from scenarios.core import BUNDLED_FIRMWARE, kv, process_running, qemu_argv_of, sh

LAB = "e2e-firmware"
OWNED = ["vm.hw.firmware", "vm.hw.tpm", "vm.hw.secure_boot"]

PROBE = r"""
echo efi=$([ -d /sys/firmware/efi ] && echo yes || echo no)
echo tpm2_acpi=$([ -e /sys/firmware/acpi/tables/TPM2 ] && echo yes || echo no)
true
"""


def launch(lab: str, vm: str) -> dict:
    """What the running machine was launched with: its firmware CODE image
    (empty under SeaBIOS), whether secure boot is armed, and whether it has a
    TPM. Read from QEMU's argv in /proc, else from the machine description
    vmlab-hv reads, which sits in the machine's runtime directory."""
    argv = qemu_argv_of(lab, vm)
    if argv:
        pflash = next((a for a in argv if "OVMF_CODE" in a), "")
        return {
            "via": "qemu argv",
            "code": pflash.rsplit("file=", 1)[-1] if pflash else "",
            "secure": "driver=cfi.pflash01,property=secure,value=on" in argv,
            "tpm": "tpm-tis,tpmdev=tpm0" in argv,
        }
    run = pathlib.Path(os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}")
    path = run / "vmlab" / "labs" / lab / "vms" / vm / "machine.json"
    try:
        desc = json.loads(path.read_text())
    except (OSError, ValueError):
        return {"via": "nothing running", "code": "", "secure": False, "tpm": False}
    boot = desc.get("boot", {})
    ovmf = boot.get("firmware") == "ovmf"
    return {
        "via": "vmlab-hv machine description",
        "code": boot.get("code", "") if ovmf else "",
        "secure": bool(boot.get("secure_boot")),
        "tpm": "tpm" in desc,
    }


def run(h):
    try:
        with h.lab("firmware-lab") as lab:
            h.vmlab("up", cwd=lab, timeout=600)
            h.wait_ready(lab, "uefi")
            p = kv(sh(h, lab, "uefi", PROBE).out)
            uefi = launch(LAB, "uefi")
            swtpm = process_running(f"--tpmstate dir={lab}/.vmlab/vms/uefi/tpm-state")
            h.ok(
                "vm.hw.tpm",
                swtpm and uefi["tpm"] and p.get("tpm2_acpi") == "yes",
                f"swtpm running={swtpm}; TPM in the launch ({uefi['via']})={uefi['tpm']}; guest ACPI "
                f"TPM2 table={p.get('tpm2_acpi')} (the Alpine virt kernel ships no tpm_tis driver)",
            )
            blank_screen = h.wait_until(
                lambda: (lambda t: t if "bootable" in t else None)(
                    h.vmlab("vm", "ocr", "bios", cwd=lab, check=False).out),
                timeout=90, interval=3, what="the SeaBIOS VM's boot failure screen",
            )
            bios = launch(LAB, "bios")
            h.ok(
                "vm.hw.firmware",
                p.get("efi") == "yes" and uefi["code"].startswith(f"{BUNDLED_FIRMWARE}/x86_64/OVMF_CODE")
                and "SeaBIOS" in blank_screen and bios["code"] == "",
                f"uefi (firmware=ovmf) has /sys/firmware/efi and the bundled {uefi['code'].rsplit('/', 1)[-1]} "
                f"({uefi['via']}); bios (seabios) shows the SeaBIOS banner and no OVMF",
            )
        secure_boot(h)
    finally:
        for f in OWNED:
            if f not in h.results:
                h.ok(f, False, "not reached: the scenario stopped before this step")


def secure_boot(h):
    """`secure_boot = true` enforces: the secboot build with secure boot
    armed, the VM's VARS carry enrolled keys (PK, KEK, db), and the firmware
    refuses the Alpine image's unsigned bootloader — which boots in the lab
    above without secure boot. Enforced, it never reaches a guest to ask."""
    with h.lab("core-secureboot") as lab:
        up = h.vmlab("up", cwd=lab, timeout=300, check=False)
        serial = pathlib.Path.home() / ".local/state/vmlab/labs/e2e-core-secureboot/vms/sb/serial.log"
        refused = ""
        try:
            refused = h.wait_until(
                lambda: next(
                    (l.strip() for l in (serial.read_text(errors="replace") if serial.exists() else "").splitlines()
                     if "failed to load" in l and "Access Denied" in l),
                    None,
                ),
                timeout=120, interval=2, what="the firmware to refuse the unsigned bootloader",
            )
        except ScenarioFailed:
            pass
        sb = launch("e2e-core-secureboot", "sb")
        vars_file = lab / ".vmlab" / "vms" / "sb" / "OVMF_VARS.fd"
        blob = vars_file.read_bytes() if vars_file.exists() else b""
        keys = [k for k in ("PK", "KEK", "db") if (k + "\0").encode("utf-16-le") in blob]
        # vmlab's own firmware, not the host's: the image carries no OVMF
        # package, and the VM's VARS started as the bundled enrolled template.
        bundled = sb["code"].startswith(f"{BUNDLED_FIRMWARE}/x86_64/")
        template = pathlib.Path(BUNDLED_FIRMWARE, "x86_64", "OVMF_VARS_4M.ms.fd")
        same_size = template.exists() and len(blob) == template.stat().st_size
        h.ok(
            "vm.hw.secure_boot",
            up.code == 0 and "secboot" in sb["code"] and bundled and same_size and sb["secure"]
            and keys == ["PK", "KEK", "db"] and bool(refused),
            f"{sb['via']}: the bundled {sb['code'].rsplit('/', 1)[-1]} with secure boot armed={sb['secure']}; "
            f"the VM's VARS enrol {keys}; serial: {refused or 'no refusal seen'!r}",
        )
