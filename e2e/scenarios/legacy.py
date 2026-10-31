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
        if (executable.startswith("qemu-system-") or executable == "vmlab-hv") and b"-avd" not in argv:
            found.append(int(proc.name))
    return found


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
        screen = RESULTS / f"legacy-{machine}.png"
        h.vmlab("vm", "screenshot", machine, str(screen), cwd=lab)
        if not screen.read_bytes().startswith(b"\x89PNG\r\n\x1a\n"):
            raise ScenarioFailed(f"{machine}: screenshot is not a PNG")
        h.vmlab("vm", "stop", machine, cwd=lab, timeout=300)
        if "state=stopped" not in h.machine_line(lab, machine):
            raise ScenarioFailed(f"{machine}: stop did not settle")
        events = h.vmlab("logs", machine, "--output", "jsonl", "--lines", "1000", cwd=lab)
        stopped = [json.loads(line) for line in events.out.splitlines() if line.strip()]
        stopped = [e for e in stopped if e["event"] == "vm.stopped" and e["data"].get("vm") == machine]
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
                    "agent readiness, exec, disk write/read, PNG capture and stop")
