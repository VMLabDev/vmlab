#!/usr/bin/env python3
"""Check Bash syntax and ShellCheck errors without executing guest or host actions."""

from pathlib import Path
import subprocess


ROOT = Path(__file__).resolve().parents[1]
scripts = sorted(path for path in ROOT.rglob("*.sh")
                 if not any(part in (".scratch", "out", "target")
                            for part in path.relative_to(ROOT).parts))
for script in scripts:
    subprocess.run(["bash", "-n", str(script)], check=True)
subprocess.run(["shellcheck", "--shell=bash", "--severity=error", *map(str, scripts)], check=True)
