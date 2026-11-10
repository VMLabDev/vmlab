#!/usr/bin/env python3
"""Run cargo test with raw libtest JSON evidence, preserving its exit status."""

import os
from pathlib import Path
import subprocess
import sys


def main():
    name, *args = sys.argv[1:]
    root = Path(os.environ.get("VMLAB_TARGET_ROOT", "target"))
    if str(root) in ("", "/"):
        raise SystemExit("unsafe VMLAB_TARGET_ROOT")
    result = root.resolve() / "test-results" / f"{name}.json"
    result.parent.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, RUSTC_BOOTSTRAP="1")
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
