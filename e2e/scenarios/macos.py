"""An Intel macOS guest on the shipped `macos` profile, on the run's hypervisor.

It needs two things the suite cannot make: the operator's Apple SMC key and a
sealed macos-tahoe template. Without either it skips, naming what is missing.
The key comes from `VMLAB_E2E_APPLE_SMC_OSK` and goes only into the run's host
config (mode 600), which is removed again afterwards; it is never logged.
"""

import os
import pathlib

from harness import RESULTS, ScenarioFailed

FEATURE = "vm.macos"
TEMPLATE = "x86_64/macos-tahoe"
OSK_VARIABLE = "VMLAB_E2E_APPLE_SMC_OSK"


def store() -> pathlib.Path:
    data = os.environ.get("XDG_DATA_HOME", pathlib.Path.home() / ".local/share")
    return pathlib.Path(data) / "vmlab/templates"


def installed(root: pathlib.Path) -> bool:
    """Whether `root` holds a macos-tahoe version with its disk."""
    base = root / TEMPLATE
    return base.is_dir() and any((v / "disk.qcow2").is_file() for v in base.iterdir())


def stage_template(h) -> None:
    """Copy the template from `VMLAB_E2E_MACOS_STORE` when the run's store lacks it."""
    if installed(store()):
        return
    source = os.environ.get("VMLAB_E2E_MACOS_STORE")
    if not source or not installed(pathlib.Path(source)):
        raise ScenarioFailed(f"missing template {TEMPLATE}; set VMLAB_E2E_MACOS_STORE to a populated template store")
    target = store() / TEMPLATE
    target.parent.mkdir(parents=True, exist_ok=True)
    h.run(["cp", "-a", "--reflink=auto", "--sparse=always", str(pathlib.Path(source) / TEMPLATE), str(target)])


def with_key(config: str, key: str) -> str:
    """`config` (a host config holding one `host {` block) with `apple_smc_osk`
    set to `key`, escaped as a WCL string."""
    if len(key) != 64 or not all(" " <= c <= "~" for c in key):
        raise ScenarioFailed(f"{OSK_VARIABLE} is not 64 printable ASCII characters")
    quoted = key.replace("\\", "\\\\").replace('"', '\\"')
    if "host {\n" not in config:
        raise ScenarioFailed("the run's host config has no host block")
    return config.replace("host {\n", f'host {{\n  apple_smc_osk = "{quoted}"\n', 1)


def config_path() -> pathlib.Path:
    home = os.environ.get("XDG_CONFIG_HOME", pathlib.Path.home() / ".config")
    return pathlib.Path(home) / "vmlab" / "config.wcl"


def exercise(h, lab) -> bool:
    h.vmlab("validate", cwd=lab)
    h.vmlab("up", cwd=lab, timeout=900)
    try:
        line = h.machine_line(lab, "mac")
        if f"hypervisor={h.hypervisor}" not in line:
            raise ScenarioFailed(f"mac does not run on {h.hypervisor}: {line}")
        # OpenCore, then XNU and launchd, before the agent answers.
        h.wait_ready(lab, "mac", timeout=900)
        version = h.vmlab("exec", "mac", "--", "sw_vers", "-productVersion", cwd=lab).out.strip()
        if not version or not version[0].isdigit():
            raise ScenarioFailed(f"sw_vers gave {version!r}")
        h.log.write(f"[macos] macOS {version} on {h.hypervisor}\n")
        screen = RESULTS / "macos.png"
        h.vmlab("vm", "screenshot", "mac", str(screen), cwd=lab)
        if not screen.read_bytes().startswith(b"\x89PNG\r\n\x1a\n"):
            raise ScenarioFailed("screenshot is not a PNG")
        h.vmlab("vm", "stop", "mac", cwd=lab, timeout=300)
        if "state=stopped" not in h.machine_line(lab, "mac"):
            raise ScenarioFailed("stop did not settle")
        return True
    finally:
        h.vmlab("vm", "stop", "--force", "mac", cwd=lab, check=False, timeout=120)


def run(h):
    key = os.environ.get(OSK_VARIABLE, "")
    if not key:
        h.skip(FEATURE, f"no Apple SMC key; set {OSK_VARIABLE} to your own Mac's OSK")
        return
    try:
        stage_template(h)
    except ScenarioFailed as error:
        h.skip(FEATURE, str(error))
        return
    path = config_path()
    original = path.read_text() if path.is_file() else "import <vmlab-host.wcl>\n\nhost {\n}\n"
    path.parent.mkdir(parents=True, exist_ok=True)
    path.touch(mode=0o600)
    path.chmod(0o600)
    path.write_text(with_key(original, key))
    try:
        with h.lab("macos") as lab:
            h.check(FEATURE, lambda: exercise(h, lab),
                    "validate, the selected hypervisor, agent readiness, exec, PNG capture and stop")
    finally:
        path.write_text(original)
