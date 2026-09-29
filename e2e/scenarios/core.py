"""Core: validation, the lab lifecycle, per-VM power, declared hardware and
attached media, each proved from inside the guest where the guest can see it
and from QEMU's own argv where it cannot."""

import json
import pathlib
import re
from datetime import datetime, timezone

from harness import E2E, ScenarioFailed

LAB = "e2e-core"
OWNED = [
    "lab.validate", "lab.validate.reject", "lab.up", "lab.up.partial", "lab.depends_on",
    "lab.status", "lab.down", "lab.destroy", "vm.start", "vm.stop", "vm.restart",
    "vm.destroy", "vm.ip", "vm.hw.cpus-memory", "vm.hw.disk", "vm.hw.disk-from",
    "vm.hw.cdrom", "vm.hw.floppy", "vm.hw.tpm", "vm.hw.firmware", "vm.hw.secure_boot",
    "vm.hw.qemu_args", "vm.hw.nested", "vm.scratch", "vm.media.iso", "vm.media.floppy",
]


# -- local helpers -------------------------------------------------------------


def qemu_argv(vm: str) -> list[str]:
    """The running QEMU's argv for `vm` of this lab, read from /proc (QEMU is
    a child of the lab daemon in this same container)."""
    return qemu_argv_of(LAB, vm)


def qemu_argv_of(lab: str, vm: str) -> list[str]:
    want = f"vmlab:{lab}/{vm}"
    for p in pathlib.Path("/proc").iterdir():
        if not p.name.isdigit():
            continue
        try:
            argv = (p / "cmdline").read_bytes().split(b"\0")
        except OSError:
            continue
        args = [a.decode(errors="replace") for a in argv if a]
        if args and "qemu-system" in args[0] and want in args:
            return args
    return []


def cpu_model(argv: list[str]) -> str:
    """The `-cpu` value in a QEMU argv."""
    i = argv.index("-cpu") if "-cpu" in argv else -1
    return argv[i + 1] if 0 <= i < len(argv) - 1 else "?"


def process_running(needle: str) -> bool:
    for p in pathlib.Path("/proc").iterdir():
        if not p.name.isdigit():
            continue
        try:
            text = (p / "cmdline").read_bytes().replace(b"\0", b" ").decode(errors="replace")
            state = (p / "stat").read_text().split(") ", 1)[1][0]
        except (OSError, IndexError):
            continue
        if needle in text and state != "Z":
            return True
    return False


def detail_line(h, lab, vm: str) -> str:
    return h.machine_line(lab, vm)


def state_of(h, lab, vm: str) -> str:
    m = re.search(r"state=(\S+)", detail_line(h, lab, vm))
    return m.group(1) if m else ""


def sh(h, lab, vm: str, script: str, **kw):
    """Run a shell snippet in the guest as the agent identity."""
    return h.vmlab("exec", "--user", "root", vm, "--", "sh", "-c", script, cwd=lab, **kw)


def events_since(h, lab, since: datetime) -> list[dict]:
    out = h.vmlab("logs", "-o", "jsonl", "-n", "1000", cwd=lab, check=False).out
    evs = []
    for line in out.splitlines():
        try:
            ev = json.loads(line)
        except ValueError:
            continue
        ts = ev.get("ts", "")
        # Nanosecond timestamps; the first 26 characters are microseconds.
        try:
            when = datetime.fromisoformat(ts[:26])
        except ValueError:
            continue
        if when >= since:
            ev["_when"] = when
            evs.append(ev)
    return evs


def first(evs, name: str, vm: str):
    for ev in evs:
        if ev.get("event") == name and (ev.get("data") or {}).get("vm") == vm:
            return ev["_when"]
    return None


def kv(text: str) -> dict[str, str]:
    """`key=value` lines from a guest probe."""
    out = {}
    for line in text.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            out[k.strip()] = v.strip()
    return out


def build_images(h, lab: pathlib.Path) -> None:
    """`cd.iso` and `fd.img` for the `cdrom`/`floppy` attachments, built from
    `cd/` so no binary image is checked in."""
    h.run(["xorriso", "-as", "mkisofs", "-V", "E2ECDROM", "-o", str(lab / "cd.iso"), str(lab / "cd")])
    h.run(["mkfs.fat", "-C", "-n", "E2EFD", str(lab / "fd.img"), "1440"])
    h.run(["mcopy", "-i", str(lab / "fd.img"), str(lab / "cd" / "cdrom.txt"), "::/FD.TXT"])


PROBE_VM01 = r"""
echo nproc=$(nproc)
echo memkb=$(awk '/MemTotal/ {print $2}' /proc/meminfo)
echo virt=$(grep -cwE 'vmx|svm' /proc/cpuinfo)
echo efi=$([ -d /sys/firmware/efi ] && echo yes || echo no)
echo tpm2_acpi=$([ -e /sys/firmware/acpi/tables/TPM2 ] && echo yes || echo no)
echo tpm_log=$(dmesg | grep -c 'TPMEventLog')
echo serial=$(cat /sys/class/dmi/id/product_serial)
for d in /sys/block/vd*; do echo "size_$(basename $d)=$(cat $d/size)"; done
mkdir -p /mnt/c /mnt/m /mnt/p
mount -o ro /dev/disk/by-label/E2ECDROM /mnt/c 2>/dev/null
echo cdrom=$(cat /mnt/c/cdrom.txt 2>/dev/null)
mount -o ro /dev/disk/by-label/E2EMEDIA /mnt/m 2>/dev/null
echo media_iso=$(cat /mnt/m/media-iso.txt /mnt/m/MEDIA-ISO.TXT 2>/dev/null)
for d in /dev/vd[b-z]; do
  if mount -o ro -t vfat $d /mnt/p 2>/dev/null; then
    [ -e /mnt/p/payload.txt ] && echo payload=$(cat /mnt/p/payload.txt) && echo payload_dev=$d
    umount /mnt/p
  fi
done
true
"""

PROBE_FLOPPY = r"""
mkdir -p /mnt/f
mount -o ro -t vfat /dev/fd0 /mnt/f 2>/dev/null
echo label=$(blkid -s LABEL -o value /dev/fd0)
echo files=$(ls /mnt/f | tr '\n' ' ')
echo fd=$(cat /mnt/f/FD.TXT 2>/dev/null)
echo mfloppy=$(cat /mnt/f/MFLOPPY.TXT 2>/dev/null)
true
"""


# -- the scenario ----------------------------------------------------------------


def run(h):
    try:
        _run(h)
    finally:
        for f in OWNED:
            if f not in h.results:
                h.ok(f, False, "not reached: the scenario stopped before this step")


def secure_boot(h):
    """`secure_boot = true` enforces: QEMU runs the secboot build with secure
    pflash, the VM's VARS carry enrolled keys (PK, KEK, db), and the firmware
    refuses the Alpine image's unsigned bootloader — which boots in the core
    lab without secure boot. Enforced, it never reaches a guest to ask."""
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
        argv = qemu_argv_of("e2e-core-secureboot", "sb")
        code = next((a for a in argv if "OVMF_CODE" in a), "")
        vars_file = lab / ".vmlab" / "vms" / "sb" / "OVMF_VARS.fd"
        blob = vars_file.read_bytes() if vars_file.exists() else b""
        keys = [k for k in ("PK", "KEK", "db") if (k + "\0").encode("utf-16-le") in blob]
        h.ok(
            "vm.hw.secure_boot",
            up.code == 0 and "secboot" in code and "driver=cfi.pflash01,property=secure,value=on" in argv
            and keys == ["PK", "KEK", "db"] and bool(refused),
            f"qemu runs {code.rsplit('/', 1)[-1]} with secure pflash; the VM's VARS enrol {keys}; "
            f"serial: {refused or 'no refusal seen'!r}",
        )


def _run(h):
    # A lab validate must refuse, naming what is wrong.
    bad = h.vmlab("validate", cwd=E2E / "labs" / "core-bad", check=False)
    h.ok(
        "lab.validate.reject",
        bad.code != 0 and "cpus" in bad.text and "at least 1" in bad.text,
        f"exit {bad.code}: " + next((l.strip(" ×") for l in bad.text.splitlines() if "at least" in l), bad.text.strip()[-120:]),
    )

    secure_boot(h)

    with h.lab("core-lab") as lab:
        build_images(h, lab)

        v = h.vmlab("validate", cwd=lab, check=False)
        h.ok("lab.validate", v.code == 0 and f'lab "{LAB}"' in v.out and "4 vm(s)" in v.out, v.out.strip())
        if v.code != 0:
            raise ScenarioFailed(f"the core lab does not validate: {v.text.strip()[-400:]}")

        # `up vm02` drags vm01 in through depends_on and starts nothing else.
        t0 = datetime.now(timezone.utc).replace(tzinfo=None)
        part = h.vmlab("up", "vm02", cwd=lab, timeout=600, check=False)
        if part.code != 0:
            raise ScenarioFailed(f"`vmlab up vm02` failed: {part.text.strip()[-400:]}")
        h.wait_ready(lab, "vm01", "vm02")
        others = {m: state_of(h, lab, m) for m in ("vm03", "blank")}
        h.ok(
            "lab.up.partial",
            all(s == "stopped" for s in others.values()) and state_of(h, lab, "vm02") == "running",
            f"vm01+vm02 running, others {others}",
        )
        evs = events_since(h, lab, t0)
        ready1, start2 = first(evs, "vm.ready", "vm01"), first(evs, "vm.starting", "vm02")
        h.ok(
            "lab.depends_on",
            "vm01: pulled in" in part.text and ready1 is not None and start2 is not None and start2 >= ready1,
            f"vm01 ready {ready1:%H:%M:%S.%f}, vm02 starting {start2:%H:%M:%S.%f}"
            if ready1 and start2 else f"events missing: {[e.get('event') for e in evs]}",
        )

        # The rest of the lab.
        full = h.vmlab("up", cwd=lab, timeout=600, check=False)
        h.wait_ready(lab, "vm03")
        states = {m: state_of(h, lab, m) for m in ("vm01", "vm02", "vm03", "blank")}
        h.ok(
            "lab.up",
            full.code == 0 and f'lab "{LAB}" is up' in full.out and set(states.values()) == {"running"},
            f"states {states}",
        )

        st = h.vmlab("status", cwd=lab).out
        stv = h.vmlab("status", "-v", cwd=lab).out
        h.ok(
            "lab.status",
            all(re.search(rf"^\s+{m}\s+vm\s+running", st, re.M) for m in ("vm01", "vm02", "vm03"))
            and "10.80.0.10" in st and "lan" in st and stv.count("ready=yes") == 3
            and "state=running" in stv,
            "status rows running with vm01 at 10.80.0.10; -v shows state=/ready= per machine",
        )

        ip = h.vmlab("vm", "ip", "vm01", cwd=lab, check=False).out.strip()
        ip_nic = h.vmlab("vm", "ip", "--nic", "0", "vm02", cwd=lab, check=False).out.strip()
        h.ok("vm.ip", ip == "10.80.0.10" and ip_nic.startswith("10.80.0."), f"vm01 {ip!r}, vm02 --nic 0 {ip_nic!r}")

        # Hardware, from inside vm01.
        p = kv(sh(h, lab, "vm01", PROBE_VM01).out)
        memkb = int(p.get("memkb", "0") or 0)
        h.ok(
            "vm.hw.cpus-memory",
            p.get("nproc") == "2" and 600_000 < memkb <= 768 * 1024,
            f"nproc={p.get('nproc')} MemTotal={memkb}kB (declared 2 cpus, 768MiB)",
        )
        sizes = {k[5:]: int(v) for k, v in p.items() if k.startswith("size_")}
        h.ok(
            "vm.hw.disk",
            2 * 1024 * 1024 in sizes.values(),
            f"guest block devices (512-byte sectors): {sizes}; data = 1GiB = 2097152",
        )
        h.ok(
            "vm.hw.disk-from",
            p.get("payload") == "disk-from payload e2e",
            f"relative `from = \"./payload/\"`: the FAT disk {p.get('payload_dev')} carries "
            f"payload.txt={p.get('payload')!r}",
        )
        h.ok("vm.hw.cdrom", p.get("cdrom") == "cdrom attachment e2e", f"E2ECDROM mounted: cdrom.txt={p.get('cdrom')!r}")
        h.ok("vm.media.iso", p.get("media_iso") == "media iso e2e", f"E2EMEDIA mounted: {p.get('media_iso')!r}")
        swtpm = process_running(f"--tpmstate dir={lab}/.vmlab/vms/vm01/tpm-state")
        argv1 = qemu_argv("vm01")
        h.ok(
            "vm.hw.tpm",
            swtpm and "tpm-tis,tpmdev=tpm0" in argv1 and p.get("tpm2_acpi") == "yes",
            f"swtpm running={swtpm}; qemu has tpm-tis; guest ACPI TPM2 table={p.get('tpm2_acpi')} "
            "(the Alpine virt kernel ships no tpm_tis driver, so no /dev/tpm0)",
        )
        h.ok("vm.hw.qemu_args", p.get("serial") == "E2E-QEMU-ARGS", f"guest DMI product_serial={p.get('serial')!r}")
        # vm01 declares `nested = true`, vm02 does not: the switch decides
        # what the guest CPU carries, whatever the host allows.
        virt2 = sh(h, lab, "vm02", "grep -cwE 'vmx|svm' /proc/cpuinfo; true", check=False).out.strip()
        cpu1, cpu2 = cpu_model(argv1), cpu_model(qemu_argv("vm02"))
        h.ok(
            "vm.hw.nested",
            int(p.get("virt", "0") or 0) > 0 and virt2 == "0",
            f"nested vm01 (-cpu {cpu1}): vmx|svm on {p.get('virt')} cpu(s); "
            f"vm02 without it (-cpu {cpu2}): on {virt2 or '?'}",
        )

        # SeaBIOS on the scratch VM (profile linux-generic): its screen says so.
        blank_screen = h.wait_until(
            lambda: (lambda t: t if "bootable" in t else None)(h.vmlab("vm", "ocr", "blank", cwd=lab, check=False).out),
            timeout=90, interval=3, what="the scratch VM's boot failure screen",
        )
        code1 = next((a for a in argv1 if "OVMF_CODE" in a), "")
        h.ok(
            "vm.hw.firmware",
            p.get("efi") == "yes" and "OVMF_CODE" in code1 and "SeaBIOS" in blank_screen
            and not any("OVMF" in a for a in qemu_argv("blank")),
            f"vm01 (firmware=ovmf) has /sys/firmware/efi and pflash {code1.rsplit('/', 1)[-1]}; "
            "blank (seabios) shows the SeaBIOS banner",
        )
        disk = lab / ".vmlab" / "vms" / "blank" / "disk0.qcow2"
        info = h.run(["qemu-img", "info", "-U", "--output=json", str(disk)], check=False)
        vsize = json.loads(info.out).get("virtual-size") if info.code == 0 else None
        h.ok(
            "vm.scratch",
            state_of(h, lab, "blank") == "running" and vsize == 1 << 30 and "No bootable device" in blank_screen,
            f"running with a blank {vsize}-byte disk and no backing template; SeaBIOS: 'No bootable device'",
        )

        # vm02: the `floppy` attachment.
        f2 = kv(sh(h, lab, "vm02", PROBE_FLOPPY).out)
        h.ok("vm.hw.floppy", f2.get("fd") == "cdrom attachment e2e", f"/dev/fd0 label={f2.get('label')} FD.TXT={f2.get('fd')!r}")

        # vm03: a floppy built from a folder.
        f3 = kv(sh(h, lab, "vm03", PROBE_FLOPPY).out)
        h.ok(
            "vm.media.floppy",
            f3.get("label") == "E2EFLOP" and f3.get("mfloppy") == "media floppy e2e",
            f"/dev/fd0 label={f3.get('label')} MFLOPPY.TXT={f3.get('mfloppy')!r}",
        )

        # Restart keeps the disk and gives a new boot.
        before = sh(h, lab, "vm03", "cat /proc/sys/kernel/random/boot_id; touch /root/e2e-marker; sync").out.strip()
        rs = h.vmlab("vm", "restart", "vm03", cwd=lab, check=False)
        h.wait_ready(lab, "vm03")
        after = sh(h, lab, "vm03", "cat /proc/sys/kernel/random/boot_id; ls /root/e2e-marker", check=False).out
        h.ok(
            "vm.restart",
            rs.code == 0 and before and before not in after and "/root/e2e-marker" in after,
            "new boot_id, and a file written before the restart survived",
        )

        # Destroy drops the clone; start re-creates it from the template.
        clone = lab / ".vmlab" / "vms" / "vm03"
        ds = h.vmlab("vm", "destroy", "vm03", cwd=lab, check=False)
        h.ok(
            "vm.destroy",
            ds.code == 0 and 'vm "vm03" destroyed' in ds.out and not clone.exists()
            and state_of(h, lab, "vm03") == "stopped" and state_of(h, lab, "vm01") == "running",
            "clone directory removed, vm03 stopped, the rest of the lab still running",
        )
        sv = h.vmlab("vm", "start", "vm03", cwd=lab, check=False, timeout=300)
        h.wait_ready(lab, "vm03")
        fresh = sh(h, lab, "vm03", "ls /root/e2e-marker", check=False)
        h.ok(
            "vm.start",
            sv.code == 0 and clone.exists() and fresh.code != 0 and "No such file" in fresh.text,
            "vm03 booted from a fresh clone: the pre-destroy marker is gone",
        )

        # Stop: graceful for vm02, forced for the scratch VM with no agent.
        sp = h.vmlab("vm", "stop", "vm02", cwd=lab, check=False, timeout=180)
        sf = h.vmlab("vm", "stop", "--force", "blank", cwd=lab, check=False, timeout=60)
        h.ok(
            "vm.stop",
            sp.code == 0 and sf.code == 0 and state_of(h, lab, "vm02") == "stopped"
            and state_of(h, lab, "blank") == "stopped" and not qemu_argv("vm02") and not qemu_argv("blank")
            and state_of(h, lab, "vm01") == "running",
            "vm02 (graceful) and blank (--force) stopped with their QEMU gone; vm01 untouched",
        )

        dn = h.vmlab("down", cwd=lab, check=False, timeout=300)
        states = {m: state_of(h, lab, m) for m in ("vm01", "vm02", "vm03", "blank")}
        h.ok(
            "lab.down",
            dn.code == 0 and set(states.values()) == {"stopped"} and not qemu_argv("vm01")
            and (lab / ".vmlab" / "vms" / "vm01").exists(),
            f"states {states}, clones kept",
        )

        de = h.vmlab("destroy", cwd=lab, check=False, timeout=300)
        # Checked at once: destroy returns only once the lab daemon is gone.
        after = h.vmlab("status", cwd=lab, check=False).out
        h.ok(
            "lab.destroy",
            de.code == 0 and f'lab "{LAB}" destroyed' in de.out and not (lab / ".vmlab").exists()
            and "not running" in after,
            f".vmlab removed; status: {after.strip()}",
        )
