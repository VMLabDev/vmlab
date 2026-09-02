#!/usr/bin/env python3
"""List vmlab's public surface, one JSON object per line, for the manual's
coverage check (`/technical-book audit`).

Three kinds come out:

- `command`: every top-level `vmlab` verb, as `vmlab <verb>`. The manual
  carries one Commands chapter per verb, with subcommands as sections.
- `symbol`: every block the lab file, host config and profile schemas
  declare, as `<kind> {}`; and every wscript host method, as
  `<Type>.<method>`, plus every free function in the `vmlab` module as
  `vmlab::<fn>`. Each is an `h2` in a Reference chapter.

The CLI tree comes from the built binary's `--help`, so build first
(`cargo build`). The wscript surface comes from the interface file the same
binary writes (`vmlab wscripti`).
"""

import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BIN = ROOT / "target" / "debug" / "vmlab"

SCHEMAS = [
    ROOT / "src" / "config" / "schema.wcl",
    ROOT / "src" / "config" / "host_schema.wcl",
    ROOT / "src" / "profiles" / "profile_schema.wcl",
]

HIDDEN_VERBS = {"help", "wscripti", "__supervisord", "__labd", "__vncbridge", "ssh-proxy"}


def emit(path: str, kind: str) -> None:
    print(json.dumps({"path": path, "kind": kind}))


def verbs() -> list[str]:
    if not BIN.exists():
        sys.exit(f"{BIN} is missing — run `cargo build` first")
    out = subprocess.run([str(BIN), "--help"], capture_output=True, text=True, check=True).stdout
    found = []
    in_commands = False
    for line in out.splitlines():
        if line.startswith("Commands:"):
            in_commands = True
            continue
        if line.startswith("Options:"):
            break
        if in_commands and line.startswith("  ") and not line.startswith("   "):
            verb = line.split()[0]
            if verb not in HIDDEN_VERBS:
                found.append(verb)
    return found


def schema_blocks() -> list[str]:
    found = []
    for schema in SCHEMAS:
        for match in re.finditer(r'^@block\("([a-z_]+)"\)', schema.read_text(), re.MULTILINE):
            found.append(match.group(1))
    return found


def wscript_surface() -> list[str]:
    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "vmlab.wscripti"
        subprocess.run([str(BIN), "wscripti", str(path)], capture_output=True, text=True, check=True)
        text = path.read_text()
    found = []
    owner = None
    for line in text.splitlines():
        stripped = line.strip()
        if m := re.match(r"^impl (\w+) \{", stripped):
            owner = m.group(1)
        elif m := re.match(r"^mod (\w+) \{", stripped):
            owner = f"{m.group(1)}::"
        elif stripped == "}":
            owner = None
        elif owner and (m := re.match(r"^fn (\w+)\(", stripped)):
            sep = "" if owner.endswith("::") else "."
            found.append(f"{owner}{sep}{m.group(1)}")
        elif m := re.match(r"^struct (\w+) \{$", stripped):
            # A plain data struct (not `#[opaque]`, which closes on `{}`).
            found.append(m.group(1))
    return found


def main() -> None:
    for verb in verbs():
        emit(f"vmlab {verb}", "command")
    for block in schema_blocks():
        emit(f"{block} {{}}", "symbol")
    for symbol in wscript_surface():
        emit(symbol, "symbol")


if __name__ == "__main__":
    main()
