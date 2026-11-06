"""Phase-4 gate guests, one running at a time, using sealed local templates."""

import json
import os
import pathlib

from harness import RESULTS, ScenarioFailed

GUESTS = [
    ("freedos", "vm.legacy.freedos", "x86/freedos-1.3/1.3.0"),
    ("win98", "vm.legacy.windows98", "x86/windows-98-se/4.10.2222.5"),
    ("win7", "vm.legacy.windows7", "x86_64/windows-7/7600.0.1"),
    ("temple", "vm.legacy.templeos", "x86_64/templeos/5.03.2"),
]

# Text each guest shows on its usual screen once it has finished starting, as
# OCR reads it (lower case, any one of them). A screenshot alone proves only
# that a frame was captured: in the VML-50 run Windows 7 answered exec while
# its setup still showed "Setup is installing devices".
SCREEN_MARKERS = {
    "freedos": ("welcome to the freedos", "agent-legacy"),
    "win98": ("windows password", "identify yourself"),
    "win7": ("recycle bin",),
    # OCR reads TempleOS's blue title "TempleOS V5.03" as "empled US".
    "temple": ("public domain operating system",),
}
# Text that means the guest has not reached its usual screen yet.
SCREEN_NOT_YET = {
    "win7": ("setup is", "preparing your computer", "starting windows"),
}
SCREEN_TIMEOUT = {"win7": 900}


def screen_marker(machine: str, text: str) -> str | None:
    """The marker of `machine` that the OCR `text` holds, or None."""
    lowered = " ".join(text.lower().split())
    if any(word in lowered for word in SCREEN_NOT_YET.get(machine, ())):
        return None
    return next((m for m in SCREEN_MARKERS[machine] if m in lowered), None)


def wait_for_screen(h, lab, machine) -> str:
    """Wait until the guest's screen shows its marker, and return the marker."""
    seen = {"text": ""}

    def look():
        seen["text"] = h.vmlab("vm", "ocr", machine, cwd=lab, check=False).out
        return screen_marker(machine, seen["text"])

    try:
        return h.wait_until(look, timeout=SCREEN_TIMEOUT.get(machine, 180), interval=5,
                            what=f"{machine}'s screen to show one of {SCREEN_MARKERS[machine]}")
    except ScenarioFailed as error:
        last = " ".join(seen["text"].split())[:200]
        raise ScenarioFailed(f"{machine}: {error}; last OCR: {last!r}") from None


def project_vms(proc_root=pathlib.Path("/proc")) -> list[int]:
    """Count emulator executables, excluding the owner's Android emulator.

    A container must use --pid=host for this to include other sessions.
    """
    found = []
    for proc in proc_root.glob("[0-9]*"):
        try:
            argv = (proc / "cmdline").read_bytes().split(b"\0")
            executable = pathlib.Path(os.fsdecode(argv[0])).name
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
        if b"-avd" in argv:
            continue
        if executable.startswith(("qemu-system-", "vmlab-hv")):
            found.append(int(proc.name))
            continue
        # Acceptance tests can own a KVM VM inside a Rust test executable.
        for descriptor in (proc / "fd").glob("*"):
            try:
                if os.readlink(descriptor) == "anon_inode:kvm-vm":
                    found.append(int(proc.name))
                    break
            except (FileNotFoundError, PermissionError, ProcessLookupError):
                continue
    return found


def lab_events(out: str) -> list[dict]:
    """The JSON events in `vmlab logs -o jsonl` output, skipping other lines."""
    events = []
    for line in out.splitlines():
        try:
            event = json.loads(line)
        except ValueError:
            continue
        if isinstance(event, dict):
            events.append(event)
    return events


def stage_templates(h):
    store = pathlib.Path(os.environ.get("XDG_DATA_HOME", pathlib.Path.home() / ".local/share")) / "vmlab/templates"
    source = os.environ.get("VMLAB_E2E_LEGACY_STORE")
    for _, _, relative in GUESTS:
        target = store / relative
        if (target / "template.wcl").is_file() and (target / "disk.qcow2").is_file():
            continue
        if not source or not (pathlib.Path(source) / relative / "disk.qcow2").is_file():
            raise ScenarioFailed(f"missing legacy template {relative}; set VMLAB_E2E_LEGACY_STORE to a populated template store")
        target.parent.mkdir(parents=True, exist_ok=True)
        h.run(["cp", "-a", "--reflink=auto", "--sparse=always", str(pathlib.Path(source) / relative), str(target)])


def exercise(h, lab, machine):
    h.wait_until(lambda: len(project_vms()) < 2, timeout=900, what="a host project VM slot")
    h.log.write(f"[legacy] host VM pids before boot: {project_vms()}\n")
    h.vmlab("up", machine, cwd=lab)
    try:
        h.wait_ready(lab, machine, timeout=300)
        # The agent can answer before the guest has finished starting, so the
        # screen check comes first and the guest operations run on the settled guest.
        marker = wait_for_screen(h, lab, machine)
        h.log.write(f"[legacy] {machine} screen shows {marker!r}\n")
        screen = RESULTS / f"legacy-{machine}.png"
        h.vmlab("vm", "screenshot", machine, str(screen), cwd=lab)
        if not screen.read_bytes().startswith(b"\x89PNG\r\n\x1a\n"):
            raise ScenarioFailed(f"{machine}: screenshot is not a PNG")
        if machine == "temple":
            command = 'FileWrite("~/P4CHECK.TXT","PHASE4_DISK_OK",14); U8 *d=FileRead("~/P4CHECK.TXT"); "%s\\n",d; Free(d); Del("~/P4CHECK.TXT");'
        # DOS COMMAND.COM has no '&' command chaining. Separate execs also
        # verify that the serial agent serves more than one request.
        if machine == "temple":
            result = h.vmlab("exec", machine, "--", command, cwd=lab)
        else:
            shell = {"freedos": "C:\\COMMAND.COM", "win98": "C:\\COMMAND.COM", "win7": "cmd.exe"}[machine]
            h.vmlab("exec", machine, "--", shell, "/c", "echo", "PHASE4_DISK_OK", ">", "C:\\P4CHECK.TXT", cwd=lab)
            result = h.vmlab("exec", machine, "--", shell, "/c", "type", "C:\\P4CHECK.TXT", cwd=lab)
            h.vmlab("exec", machine, "--", shell, "/c", "del", "C:\\P4CHECK.TXT", cwd=lab)
        if "PHASE4_DISK_OK" not in result.out:
            raise ScenarioFailed(f"{machine}: disk read did not return the written marker: {result.text}")
        h.vmlab("vm", "stop", machine, cwd=lab, timeout=300)
        if "state=stopped" not in h.machine_line(lab, machine):
            raise ScenarioFailed(f"{machine}: stop did not settle")
        # The lab's event log: `vmlab logs <vm>` tails that VM's plain-text
        # emulator and serial logs, not events.
        events = h.vmlab("logs", "--output", "jsonl", "--lines", "1000", cwd=lab)
        stopped = [e for e in lab_events(events.out)
                   if e.get("event") == "vm.stopped" and e.get("data", {}).get("vm") == machine]
        if not stopped or stopped[-1]["data"].get("status") != "exit status: 0":
            raise ScenarioFailed(f"{machine}: emulator did not exit cleanly: {stopped[-1:]}")
    finally:
        # Also release the slot when readiness or a guest operation failed.
        h.vmlab("vm", "stop", machine, cwd=lab, check=False, timeout=300)


def run(h):
    try:
        stage_templates(h)
    except ScenarioFailed as error:
        for _, feature, _ in GUESTS:
            h.skip(feature, str(error))
        return
    with h.lab("legacy") as lab:
        for machine, feature, _ in GUESTS:
            h.check(feature, lambda m=machine: exercise(h, lab, m),
                    "agent readiness, the usual screen by OCR, PNG capture, exec, disk write/read and stop")
