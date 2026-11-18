"""Compare the same pinned template on both hypervisors, one VM at a time.

Run with an isolated XDG environment and branch-built binaries on PATH.
Pass installed x86_64 template references (without the architecture prefix).
Each invocation needs a new output directory. The report and command logs
stay there after the test labs are destroyed.
"""

import argparse
import hashlib
from pathlib import Path
import re
import subprocess
import time


STEPS = (
    "up", "agent-ready", "exec", "cp to guest", "cp from guest", "shared folder",
    "screenshot", "online snapshot", "online restore", "restart", "stop",
    "offline snapshot", "offline restore", "down", "cleanup",
)


def test(root, ref, backend):
    name = hashlib.sha256(f"{root}/{ref}/{backend}".encode()).hexdigest()[:12]
    lab = root / name
    lab.mkdir()
    results = {}
    ready_seconds = None
    windows = ref.startswith("windows-")
    mount = "Z:" if windows else "/mnt/soak"
    shared_file = r"Z:\probe.txt" if windows else "/mnt/soak/probe.txt"
    marker = r"C:\Windows\Temp\vmlab-soak.txt" if windows else "/var/tmp/vmlab-soak.txt"
    share = lab / "share"
    share.mkdir()
    probe = share / "probe.txt"
    probe.write_bytes(b"soak-probe")
    (lab / "vmlab.wcl").write_text(f"""
import <vmlab.wcl>

lab "soak-{name}" {{
  segment "lan" {{ subnet = "10.182.31.0/24" }}
  vm "guest" {{
    template = "x86_64/{ref}"
    hypervisor = "{backend}"
    machine = "q35"
    agent_update = false
    nic {{ segment = "lan" }}
    share {{
      name = "probe"
      host = "./share"
      guest = "{mount}"
      transport = "smb"
    }}
  }}
}}
""")

    with (root / f"{ref}-{backend}.log").open("w", buffering=1) as log:
        def run(*args, timeout=180, check=True):
            print(repr(args), file=log)
            completed = subprocess.run(
                ["vmlab", *args], cwd=lab, timeout=timeout,
                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True,
            )
            print(completed.stdout, file=log)
            if check and completed.returncode:
                raise RuntimeError(f"{' '.join(args[:2])} exited {completed.returncode}")
            return completed.stdout

        def require(ok, message):
            if not ok:
                raise RuntimeError(message)

        def state():
            lines = run("status", "-v", timeout=15, check=False).splitlines()
            return next((detail for row, detail in zip(lines, lines[1:])
                         if row.split()[:1] == ["guest"]), "")

        def wait_until(condition, seconds, message):
            deadline = time.monotonic() + seconds
            while not condition():
                require(time.monotonic() < deadline, message)
                time.sleep(2)

        def ready():
            wait_until(lambda: "ready=yes" in state(), 900, "agent readiness timed out")
            require(f"hypervisor={backend}" in state().split(), "unexpected hypervisor in status")

        def guest(command):
            shell = ("powershell.exe", "-NoProfile", "-NonInteractive", "-Command") if windows else ("sh", "-c")
            return run("exec", "guest", "--", *shell, command, timeout=40).strip()

        def read(path):
            return guest(f"$ErrorActionPreference='Stop'; Get-Content -Raw -LiteralPath '{path}'"
                         if windows else f"cat '{path}'")

        def write(value):
            guest(f"[IO.File]::WriteAllText('{marker}', '{value}')"
                  if windows else f"printf '%s' '{value}' > '{marker}' && sync")

        def verify_marker(value):
            require(read(marker) == value, f"guest marker differs from {value}")

        def step(label, action):
            try:
                action()
            except (OSError, RuntimeError, subprocess.TimeoutExpired) as exc:
                results[label] = f"Fail ({exc})"
            else:
                results[label] = "Pass"
            print(f"{ref} {backend} {label}: {results[label]}", flush=True)
            return results[label] == "Pass"

        def pull():
            dest = lab / "copied.txt"
            run("cp", f"guest:{marker}", str(dest))
            require(dest.read_bytes() == probe.read_bytes(), "copied bytes differ")

        def shared():
            deadline = time.monotonic() + 310
            while True:
                try:
                    require(read(shared_file) == "soak-probe", "share bytes differ")
                    return
                except RuntimeError:
                    if time.monotonic() >= deadline:
                        raise
                    time.sleep(5)

        def screenshot():
            path = lab / "screen.png"
            run("vm", "screenshot", "guest", str(path))
            require(path.read_bytes().startswith(b"\x89PNG\r\n\x1a\n"), "missing PNG signature")

        def capture(label):
            run("snapshot", "create", "--vm", "guest", label, timeout=420)
            listing = run("snapshot", "list", "guest")
            require(any(line.split()[:2] == [label, label]
                        for line in listing.splitlines()), f"snapshot missing or wrong mode: {label}")

        def online():
            write("online-before")
            capture("online")

        def restore_online():
            write("online-after")
            run("snapshot", "restore", "--vm", "guest", "online", timeout=420)
            ready()
            verify_marker("online-before")

        def start():
            run("vm", "start", "guest")
            ready()

        def restart():
            run("vm", "restart", "guest")
            ready()
            require(guest("echo restarted") == "restarted", "guest exec failed after restart")

        def stop():
            run("vm", "stop", "guest")
            wait_until(lambda: "state=stopped" in state(), 120, "VM did not stop")

        def down():
            if "state=stopped" in state():
                start()
            run("down")
            wait_until(lambda: "state=stopped" in state(), 120, "VM did not stop after down")

        def restore_offline():
            start()
            verify_marker("offline-before")
            write("offline-after")
            stop()
            run("snapshot", "restore", "--vm", "guest", "offline", timeout=420)
            require("state=stopped" in state(), "offline restore did not leave VM stopped")
            start()
            verify_marker("offline-before")

        try:
            started = time.monotonic()
            if step("up", lambda: run("up", timeout=900)) and step("agent-ready", ready):
                ready_seconds = time.monotonic() - started
                step("exec", lambda: require(guest("echo soak-exec") == "soak-exec", "unexpected output"))
                if step("cp to guest", lambda: run("cp", str(probe), f"guest:{marker}")):
                    step("cp from guest", pull)
                step("shared folder", shared)
                step("screenshot", screenshot)
                if step("online snapshot", online):
                    step("online restore", restore_online)
                step("restart", restart)
                step("stop", lambda: stop_after_marker(write, stop, log))
                if results["stop"] == "Pass" and step("offline snapshot", lambda: capture("offline")):
                    step("offline restore", restore_offline)
            step("down", down)
        finally:
            step("cleanup", lambda: run("destroy", timeout=300))
    return ready_seconds, results


def stop_after_marker(write, stop, log):
    try:
        write("offline-before")
    except (RuntimeError, subprocess.TimeoutExpired) as exc:
        print(f"Offline marker setup failed: {exc}", file=log)
    stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("output", type=Path)
    parser.add_argument("refs", nargs="+")
    args = parser.parse_args()
    if any(not re.fullmatch(r"[a-z0-9.-]+@[a-zA-Z0-9.-]+(?:#[a-z0-9-]+)?", ref)
           for ref in args.refs):
        parser.error("use pinned name@version[#variant] references")
    root = args.output.resolve()
    root.mkdir()
    failed = False
    with (root / "report.md").open("w", buffering=1) as report:
        revision = subprocess.check_output(["git", "rev-parse", "HEAD"], text=True).strip()
        print(f"# Modern q35 lifecycle soak\n\nCheckout `{revision}`.", file=report)
        print("Readiness time includes `up` and ends at the first subsequent `ready=yes`.", file=report)
        for ref in args.refs:
            hv_time, hv = test(root, ref, "vmlab")
            qemu_time, qemu = test(root, ref, "qemu")
            print(f"\n## {ref}\n\n| Check | vmlab-hv | QEMU |\n| --- | --- | --- |", file=report)
            times = [f"{value:.1f} s" if value is not None else "Unavailable"
                     for value in (hv_time, qemu_time)]
            print(f"| Time to agent-ready | {times[0]} | {times[1]} |", file=report)
            for step in STEPS:
                left, right = (result.get(step, "Blocked by an earlier step") for result in (hv, qemu))
                print(f"| {step} | {left} | {right} |", file=report)
                failed |= left != "Pass" or right != "Pass"
    return failed


if __name__ == "__main__":
    raise SystemExit(main())
