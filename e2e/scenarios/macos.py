"""Intel macOS lifecycle on vmlab-hv with an owner-supplied template and OSK."""

import json
import os
import pathlib

from harness import RESULTS, ScenarioFailed
from scenarios.guest import png_read

OWNED = ["macos.ready", "macos.exec", "macos.cp", "macos.screenshot", "macos.snapshot", "macos.stop"]
MARKER = "/Users/Shared/vmlab-e2e-macos.txt"


def run(h):
    if h.hypervisor != "vmlab":
        for feature in OWNED:
            h.skip(feature, "macOS requires vmlab-hv")
        return
    try:
        with h.lab("macos") as lab:
            validation = h.vmlab("validate", cwd=lab, check=False)
            if validation.code:
                missing = ("requires host.apple_smc_osk", "not in the template store")
                if any(message in validation.text for message in missing):
                    for feature in OWNED:
                        h.skip(feature, "macos-tahoe template or host.apple_smc_osk is missing")
                    return
                raise ScenarioFailed(validation.text)
            h.vmlab("up", cwd=lab, timeout=600)
            h.wait_ready(lab, "mac", timeout=600)
            h.ok("macos.ready", True, "agent handshake completed on vmlab-hv")
            result = h.vmlab("exec", "mac", "--", "/usr/bin/uname", "-s", cwd=lab)
            h.ok("macos.exec", result.out.strip() == "Darwin", result.out.strip())

            pushed = lab / "pushed.txt"
            pulled = lab / "pulled.txt"
            pushed.write_text("vmlab macOS copy round-trip\n")
            h.vmlab("cp", str(pushed), f"mac:{MARKER}", cwd=lab)
            read = h.vmlab("exec", "mac", "--", "/bin/cat", MARKER, cwd=lab)
            h.vmlab("cp", f"mac:{MARKER}", str(pulled), cwd=lab)
            h.ok("macos.cp", read.out == pushed.read_text() and pulled.read_bytes() == pushed.read_bytes(),
                 "host-to-guest and guest-to-host bytes match")

            screenshot = RESULTS / "macos.png"
            h.vmlab("vm", "screenshot", "mac", str(screenshot), cwd=lab)
            width, height, _, rows = png_read(screenshot)
            h.ok("macos.screenshot", width > 0 and height > 0 and len(set(b"".join(rows))) > 1,
                 f"native display {width}x{height}")

            pin = lab / ".vmlab" / "vms" / "mac" / "tsc-hz"
            frequency = pin.read_text()
            assert int(frequency) > 0
            guest_frequency = h.vmlab("exec", "mac", "--", "/usr/sbin/sysctl", "-n", "machdep.tsc.frequency", cwd=lab).out.strip()
            runtime = pathlib.Path(os.environ.get("XDG_RUNTIME_DIR") or f"/run/user/{os.getuid()}")
            description = json.loads((runtime / "vmlab/labs/e2e-macos/vms/mac/machine.json").read_text())
            assert description["applesmc"]["osk"] == "[redacted]"
            assert description["tsc_khz"] * 1000 == int(frequency)
            h.vmlab("exec", "mac", "--", "/bin/sync", cwd=lab)
            h.vmlab("snapshot", "create", "--vm", "mac", "macos", cwd=lab, timeout=600)
            h.vmlab("exec", "mac", "--", "/bin/sh", "-c", f"echo changed > {MARKER}; sync", cwd=lab)
            h.vmlab("snapshot", "restore", "--vm", "mac", "macos", cwd=lab, timeout=600)

            def restored():
                result = h.vmlab("exec", "mac", "--", "/bin/cat", MARKER, cwd=lab, check=False, timeout=30)
                return result.code == 0 and result.out == pushed.read_text()

            h.wait_until(restored, timeout=180, interval=2, what="macOS snapshot contents to rewind")
            restored_frequency = h.vmlab("exec", "mac", "--", "/usr/sbin/sysctl", "-n", "machdep.tsc.frequency", cwd=lab).out.strip()
            h.ok("macos.snapshot", pin.read_text() == frequency and restored_frequency == guest_frequency,
                 "online snapshot rewound the file and retained the host pin and guest TSC frequency")
            h.vmlab("snapshot", "delete", "mac", "macos", cwd=lab, timeout=300)
            h.vmlab("vm", "stop", "mac", cwd=lab, timeout=300)
            h.wait_until(lambda: "state=stopped" in h.machine_line(lab, "mac"), timeout=180,
                         what="macOS to stop through its agent")
            h.ok("macos.stop", True, "agent powerdown completed")
    finally:
        for feature in OWNED:
            if feature not in h.results:
                h.ok(feature, False, "not reached: the scenario stopped before this step")
