"""Core: validation, the lab lifecycle, per-VM power, declared hardware and
attached media, each proved from inside the guest where the guest can see it
and from QEMU's own argv where it cannot. Firmware, the TPM and secure boot
are the `firmware` scenario's, which proves them on either hypervisor."""

import json
import pathlib
import re
from datetime import datetime, timezone

from harness import E2E, ScenarioFailed

LAB = "e2e-core"
OWNED = [
    "lab.validate", "lab.validate.reject", "lab.up", "lab.up.partial", "lab.depends_on",
    "lab.depends_on_provision", "lab.provision.parallel",
    "lab.status", "lab.down", "lab.up.reload", "lab.reload.stopped", "lab.reload.network",
    "lab.up.template-changed", "lab.destroy",
    "vm.start", "vm.stop", "vm.restart", "vm.destroy", "vm.ip", "vm.hw.cpus-memory",
    "vm.hw.disk", "vm.hw.disk-from", "vm.hw.cdrom", "vm.hw.floppy",
    "vm.hw.qemu_args", "vm.hw.nested", "vm.scratch", "vm.media.iso", "vm.media.floppy",
    "share.unmountable",
]


# -- local helpers -------------------------------------------------------------


def qemu_argv(vm: str) -> list[str]:
    """The running QEMU's argv for `vm` of this lab, read from /proc (QEMU is
    a child of the lab daemon in this same container)."""
    return qemu_argv_of(LAB, vm)


# Where the image installs vmlab's own UEFI firmware (e2e/Dockerfile).
BUNDLED_FIRMWARE = "/usr/share/vmlab/guest/firmware"


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


def unmountable(h):
    """`up` waits for the share mounts and names the one that cannot mount,
    with the guest's own error, before `is up` — and still exits 0."""
    with h.lab("core-unmountable") as lab:
        up = h.vmlab("up", cwd=lab, timeout=300, check=False)
        lines = up.out.splitlines()
        warn = next((i for i, l in enumerate(lines) if l.startswith('WARNING: "um": share "blocked"')), None)
        is_up = next((i for i, l in enumerate(lines) if 'lab "e2e-core-unmountable" is up' in l), None)
        good = sh(h, lab, "um", "cat /mnt/good/host.txt", check=False).out.strip()
        h.ok(
            "share.unmountable",
            up.code == 0 and warn is not None and is_up is not None and warn < is_up
            and "Not a directory" in lines[warn] and 'share "good"' not in up.out and good == "from-host",
            f"exit {up.code}; warning before `is up`: {lines[warn] if warn is not None else None!r}; "
            f"the mounted share read {good!r} and was not named",
        )


def edit_lab(lab: pathlib.Path, old: str, new: str) -> None:
    """Edit the lab's vmlab.wcl in place, the way a user would between runs."""
    path = lab / "vmlab.wcl"
    text = path.read_text()
    if old not in text:
        raise ScenarioFailed(f"vmlab.wcl has no {old!r} to edit")
    path.write_text(text.replace(old, new, 1))


def qemu_pid(vm: str) -> str:
    """The running QEMU's pid for `vm` of this lab, or "" when none runs."""
    want = f"vmlab:{LAB}/{vm}"
    for p in pathlib.Path("/proc").iterdir():
        if not p.name.isdigit():
            continue
        try:
            args = [a.decode(errors="replace") for a in (p / "cmdline").read_bytes().split(b"\0") if a]
        except OSError:
            continue
        if args and "qemu-system" in args[0] and want in args:
            return p.name
    return ""


def memory_of(argv: list[str]) -> str:
    """The `-m` value in a QEMU argv."""
    i = argv.index("-m") if "-m" in argv else -1
    return argv[i + 1] if 0 <= i < len(argv) - 1 else "?"


def reload_after_down(h, lab: pathlib.Path) -> None:
    """The lab daemon outlives `down`. `up` must still run the lab file as it
    is on disk now, not the one the daemon loaded (PRD §7.1): replacing the
    daemon when nothing runs, else reloading the file into it machine by
    machine — never quietly running the old one."""
    # An edited qemu_args takes effect on the next `up`.
    edit_lab(lab, "serial=E2E-QEMU-ARGS\"", "serial=E2E-QEMU-ARGS-2\"")
    up = h.vmlab("up", "vm01", cwd=lab, timeout=600, check=False)
    if up.code == 0:
        h.wait_ready(lab, "vm01")
    serial = sh(h, lab, "vm01", "cat /sys/class/dmi/id/product_serial", check=False).out.strip()
    argv = qemu_argv("vm01")
    applied = up.code == 0 and serial == "E2E-QEMU-ARGS-2" and "type=1,serial=E2E-QEMU-ARGS-2" in argv

    # Edited again while vm01 and vm03 run (the `up vm01` above pulled vm03
    # in through vm01's depends_on_provision): the daemon reloads the file,
    # `up` succeeds, and vm01 is named pending and keeps what it booted with
    # until a restart rebuilds it.
    edit_lab(lab, "serial=E2E-QEMU-ARGS-2\"", "serial=E2E-QEMU-ARGS-3\"")
    busy = h.vmlab("up", "vm01", cwd=lab, timeout=300, check=False)
    pending = (
        busy.code == 0 and "reloading it into the running lab" in busy.text
        and "vm01: pending" in busy.text
        and "type=1,serial=E2E-QEMU-ARGS-2" in qemu_argv("vm01")
    )
    rs = h.vmlab("vm", "restart", "vm01", cwd=lab, timeout=600, check=False)
    if rs.code == 0:
        h.wait_ready(lab, "vm01")
    restarted = (
        rs.code == 0 and "vm01: rebuilt" in rs.text
        and "type=1,serial=E2E-QEMU-ARGS-3" in qemu_argv("vm01")
        and "pending" not in h.vmlab("status", cwd=lab, check=False).out
    )

    # A stopped machine edited while others run is rebuilt by the reload and
    # boots on the new declaration, and nothing running is stopped for it.
    vm01_pid = qemu_pid("vm01")
    edit_lab(lab, "memory     = 512MiB\n    floppy", "memory     = 640MiB\n    floppy")
    stopped = h.vmlab("up", "vm02", cwd=lab, timeout=600, check=False)
    vm02_memory = memory_of(qemu_argv("vm02"))
    h.ok(
        "lab.reload.stopped",
        stopped.code == 0 and "vm02: rebuilt from the new declaration" in stopped.text
        and vm02_memory == "640M" and vm01_pid != "" and qemu_pid("vm01") == vm01_pid,
        f"exit {stopped.code}; vm02 boots -m {vm02_memory}; vm01 kept pid {vm01_pid}: "
        f"{qemu_pid('vm01') == vm01_pid}; "
        + next((l.strip() for l in stopped.text.splitlines() if l.startswith("vm02:")), stopped.text.strip()[-200:]),
    )

    # A network change cannot apply while the lab runs: refused, naming the
    # segment and `vmlab down`, with nothing reloaded.
    edit_lab(lab, 'segment "lan" { subnet = "10.80.0.0/24" }', 'segment "lan" { subnet = "10.80.0.0/24" mtu = 1400 }')
    net = h.vmlab("up", cwd=lab, timeout=120, check=False)
    edit_lab(lab, 'segment "lan" { subnet = "10.80.0.0/24" mtu = 1400 }', 'segment "lan" { subnet = "10.80.0.0/24" }')
    h.ok(
        "lab.reload.network",
        net.code != 0 and 'segment "lan" changed (mtu)' in net.text and "vmlab down" in net.text
        and qemu_pid("vm01") == vm01_pid,
        f"exit {net.code}: "
        + next((l.strip() for l in net.text.splitlines() if "network" in l), net.text.strip()[-200:]),
    )

    # `vm stop` leaves the daemon up with nothing running, still holding the
    # file it loaded: `vm start` replaces it and boots the edit.
    edit_lab(lab, "serial=E2E-QEMU-ARGS-3\"", "serial=E2E-QEMU-ARGS-4\"")
    for vm in ("vm02", "vm01", "vm03"):
        h.vmlab("vm", "stop", vm, cwd=lab, check=False, timeout=180)
    sv = h.vmlab("vm", "start", "vm01", cwd=lab, timeout=600, check=False)
    if sv.code == 0:
        h.wait_ready(lab, "vm01")
    replaced = (
        sv.code == 0 and "restarting the lab daemon" in sv.text
        and "type=1,serial=E2E-QEMU-ARGS-4" in qemu_argv("vm01")
    )
    h.ok(
        "lab.up.reload",
        applied and pending and restarted and replaced,
        f"after down + edit: up exit {up.code}, guest serial {serial!r}; "
        f"edit over running vm01: up exit {busy.code}, pending and on its old argv={pending}, "
        f"vm restart exit {rs.code} on the new argv={restarted}; "
        f"vm stop + vm start: exit {sv.code}, replaced daemon and booted serial 4={replaced}",
    )

    # A clone cannot move to another template: `up` refuses before starting
    # anything and keeps the clone, rather than booting the old template.
    h.vmlab("down", cwd=lab, check=False, timeout=300)
    edit_lab(
        lab,
        'template = "x86_64/e2e-alpine"',
        'template = "scratch"\n    arch     = "x86_64"\n    profile  = "linux-generic"\n    disk     = 1GiB',
    )
    moved = h.vmlab("up", "vm03", cwd=lab, timeout=300, check=False)
    h.ok(
        "lab.up.template-changed",
        moved.code != 0 and "vmlab vm destroy vm03" in moved.text
        and state_of(h, lab, "vm03") == "stopped"
        and (lab / ".vmlab" / "vms" / "vm03" / "disk0.qcow2").exists(),
        f"exit {moved.code}: "
        + next((l.strip() for l in moved.text.splitlines() if "clone" in l), moved.text.strip()[-200:]),
    )


def _run(h):
    # A lab validate must refuse, naming what is wrong.
    bad = h.vmlab("validate", cwd=E2E / "labs" / "core-bad", check=False)
    h.ok(
        "lab.validate.reject",
        bad.code != 0 and "cpus" in bad.text and "at least 1" in bad.text,
        f"exit {bad.code}: " + next((l.strip(" ×") for l in bad.text.splitlines() if "at least" in l), bad.text.strip()[-120:]),
    )

    unmountable(h)

    with h.lab("core-lab") as lab:
        build_images(h, lab)

        v = h.vmlab("validate", cwd=lab, check=False)
        h.ok("lab.validate", v.code == 0 and f'lab "{LAB}"' in v.out and "4 vm(s)" in v.out, v.out.strip())
        if v.code != 0:
            raise ScenarioFailed(f"the core lab does not validate: {v.text.strip()[-400:]}")

        # `up vm02` drags vm01 in through depends_on, and vm03 through vm01's
        # depends_on_provision, saying which key did it; blank stays down.
        t0 = datetime.now(timezone.utc).replace(tzinfo=None)
        part = h.vmlab("up", "vm02", cwd=lab, timeout=600, check=False)
        if part.code != 0:
            raise ScenarioFailed(f"`vmlab up vm02` failed: {part.text.strip()[-400:]}")
        h.wait_ready(lab, "vm01", "vm02", "vm03")
        states = {m: state_of(h, lab, m) for m in ("vm01", "vm02", "vm03", "blank")}
        h.ok(
            "lab.up.partial",
            states == {"vm01": "running", "vm02": "running", "vm03": "running", "blank": "stopped"}
            and 'vm03: pulled in — "vm01" names it in depends_on_provision' in part.text,
            f"states {states}; "
            + next((l.strip() for l in part.text.splitlines() if l.startswith("vm03: pulled in")), "vm03 not announced"),
        )
        evs = events_since(h, lab, t0)
        ready1, start2 = first(evs, "vm.ready", "vm01"), first(evs, "vm.starting", "vm02")
        h.ok(
            "lab.depends_on",
            'vm01: pulled in — "vm02" names it in depends_on' in part.text
            and ready1 is not None and start2 is not None and start2 >= ready1,
            f"vm01 ready {ready1:%H:%M:%S.%f}, vm02 starting {start2:%H:%M:%S.%f}"
            if ready1 and start2 else f"events missing: {[e.get('event') for e in evs]}",
        )

        # The rest of the lab. Every machine but blank is running already, so
        # all three are ready at once: vm02's and vm03's provisions overlap,
        # and vm01's holds until vm03's has finished (§7.2).
        full = h.vmlab("up", cwd=lab, timeout=600, check=False)
        h.wait_ready(lab, "vm03")
        states = {m: state_of(h, lab, m) for m in ("vm01", "vm02", "vm03", "blank")}
        h.ok(
            "lab.up",
            full.code == 0 and f'lab "{LAB}" is up' in full.out and set(states.values()) == {"running"},
            f"states {states}",
        )
        at = {
            f"{m} {edge}": full.text.find(f"{m}: e2e-prov {m} {edge}")
            for m in ("vm01", "vm02", "vm03") for edge in ("begin", "end")
        }
        seen = all(i >= 0 for i in at.values())
        h.ok(
            "lab.depends_on_provision",
            seen and "vm01: waiting for the provisioning of vm03 (depends_on_provision)" in full.text
            and at["vm03 end"] < at["vm01 begin"],
            f"vm03 ended at {at['vm03 end']}, vm01 began at {at['vm01 begin']} (offsets in up's output)"
            if seen else f"provision lines missing: {at}",
        )
        h.ok(
            "lab.provision.parallel",
            seen and at["vm02 begin"] < at["vm03 end"] and at["vm03 begin"] < at["vm02 end"],
            f"vm02 {at['vm02 begin']}..{at['vm02 end']}, vm03 {at['vm03 begin']}..{at['vm03 end']}"
            if seen else f"provision lines missing: {at}",
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
        argv1 = qemu_argv("vm01")
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
        # A full down stops every machine and has the supervisor reap the
        # lab daemon, releasing the name (#142); checked at once, since the
        # release waits for the daemon.
        after_down = h.vmlab("status", cwd=lab, check=False).out
        running = [m for m in ("vm01", "vm02", "vm03", "blank") if qemu_argv(m)]
        h.ok(
            "lab.down",
            dn.code == 0 and not running and "not running" in after_down
            and LAB not in h.vmlab("lab", "list").out
            and (lab / ".vmlab" / "vms" / "vm01").exists(),
            f"no QEMU left ({running or 'none running'}), daemon released "
            f"({after_down.strip()!r}), clones kept",
        )

        reload_after_down(h, lab)

        de = h.vmlab("destroy", cwd=lab, check=False, timeout=300)
        # Checked at once: destroy returns only once the lab daemon is gone.
        after = h.vmlab("status", cwd=lab, check=False).out
        h.ok(
            "lab.destroy",
            de.code == 0 and f'lab "{LAB}" destroyed' in de.out and not (lab / ".vmlab").exists()
            and "not running" in after,
            f".vmlab removed; status: {after.strip()}",
        )
