#!/usr/bin/env python3
"""Run cargo test with raw libtest JSON evidence, preserving its exit status."""

import os
from pathlib import Path
import subprocess
import sys


def main():
    if sys.argv[1] == "--run":
        os.execvpe(sys.argv[2], sys.argv[2:], dict(os.environ, RUSTC_BOOTSTRAP="1"))
    name, *args = sys.argv[1:]
    root = Path(os.environ.get("VMLAB_TARGET_ROOT", "target"))
    if str(root) in ("", "/"):
        raise SystemExit("unsafe VMLAB_TARGET_ROOT")
    result = root.resolve() / "test-results" / f"{name}.json"
    result.parent.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ)
    # Cargo applies a target runner only to executed binaries, not rustc or
    # build scripts. Refuse an inherited runner instead of silently replacing it.
    host = subprocess.check_output(["rustc", "-vV"], text=True).split("host: ")[1].splitlines()[0]
    runner_key = "CARGO_TARGET_" + host.upper().replace("-", "_") + "_RUNNER"
    if runner_key in env or "RUSTC_BOOTSTRAP" in env:
        raise SystemExit("ci_test requires no inherited runner or RUSTC_BOOTSTRAP")
    import shlex
    env[runner_key] = shlex.join([sys.executable, str(Path(__file__).resolve()), "--run"])
    command = ["cargo", "test", *args, "--", "-Z", "unstable-options",
               "--format", "json", "--report-time"]
    with result.open("w") as output:
        with subprocess.Popen(command, env=env, stdout=subprocess.PIPE,
                              text=True) as process:
            for line in process.stdout:
                output.write(line)
                print(line, end="", flush=True)
            status = process.wait()
    print(f"Test results ({name}, exit {status}): {result}", flush=True)
    return status


if __name__ == "__main__":
    sys.exit(main())
