#!/usr/bin/env python3
"""Fail when a shell script or just recipe can turn an empty path into /.

An empty variable turns `"$d"/.` into `/.` and `"$d"/*` into `/*`. A tracked
script that copies, syncs, moves or removes such a path copies or deletes the
whole root filesystem (see VML-81 and VML-88).

Rules, for every tracked *.sh file, justfile and *.just file:

S1  A shell script turns on errexit (`set -e`) before its first command.
    Without it a failed `ls` or `realpath` lets the next line run anyway.
    Files with no shebang are sourced libraries and are not checked.

P1  An argument of cp, rsync, mv, rm or install that starts with a variable
    or a command substitution, and is followed by `/` or ends there, needs a
    guard for that variable earlier in the same file. A guard is a test such
    as `[ -n "$d" ]`, `[ -d "$d" ]`, or a `${d:?}` expansion, or a comparison
    with `/`. The guard must come on the same line or above the command.

This is a text check, not a shell parser. It does not follow functions or
guards written in another file. Put each guard next to the command it protects.

Usage: check_shell_paths.py [REPO [FILE...]]
Without FILE, checks the git-tracked files of REPO (default: the current directory).
With FILE, checks only those paths, relative to REPO; the tests use this.
"""

import re
import shlex
import subprocess
import sys
from pathlib import Path

PATH_COMMANDS = {"cp", "rsync", "mv", "rm", "install", "chown", "chmod"}
# Words that start a new command after a control operator or a keyword.
CONTROL = {";", "&&", "||", "|", "(", ")", "&", "{", "}", "!"}
KEYWORDS = {"then", "do", "else", "if", "while", "until", "elif", "sudo", "exec"}
# `find ... -exec cp ...` and `xargs cp ...` start a new command too.
COMMAND_LEADS = {"-exec", "-execdir", "xargs"}
VAR_BASE = re.compile(r"^(?:\$\{(\w+)[^}]*\}|\$(\w+)|\$\(([^)]*)\))(/|$)")
# Guard forms: a test with -n/-d/-e/-f/-s/-z on a variable; ${VAR:?}; a root check.
# The operand must be the whole variable: `-f "$d/x"` says nothing about $d.
GUARD_TEST = re.compile(r'(?:^|\s)-[ndefsz]\s+"?\$\{?(\w+)\}?"?(?=\s|\]|$)')
GUARD_EXPAND = re.compile(r"\$\{(\w+):\?")
ROOT_CHECK = re.compile(r'!=\s*"?/"?|==\s*"?/"?')
# A script that must keep running after a failed command says why, on its own line.
OPT_OUT = re.compile(r"^#\s*check-shell-paths:\s*no-errexit\s+\S", re.MULTILINE)
JUST_VAR = re.compile(r"\{\{\s*([A-Za-z_]\w*)\s*\}\}")
JUST_EXPR = re.compile(r"\{\{(?!\s*[A-Za-z_]\w*\s*\}\})(.*?)\}\}")


def is_just(path: Path) -> bool:
    return path.name == "justfile" or path.suffix == ".just"


def logical_lines(text: str, just: bool):
    """Yield (line_number, text) with continuations joined and comments dropped."""
    start = None
    parts: list[str] = []
    for number, raw in enumerate(text.splitlines(), 1):
        stripped = raw.strip()
        if start is None:
            if not stripped or stripped.startswith("#"):
                continue
            if just and not raw[:1].isspace():
                continue
            start = number
        if stripped.endswith("\\"):
            parts.append(stripped[:-1])
            continue
        parts.append(stripped)
        yield start, " ".join(parts)
        start, parts = None, []
    if parts:
        yield start, " ".join(parts)


def normalize_just(line: str) -> str:
    """Turn `{{name}}` into `$name`, and each other `{{expr}}` into `$JUST_<expr>`.

    The name is built from the expression text, so a guard counts only for the
    same expression.
    """
    line = JUST_VAR.sub(lambda m: "$" + m.group(1), line)
    return JUST_EXPR.sub(lambda m: "$JUST_" + re.sub(r"\W+", "_", m.group(1).strip()), line)


def guards(lines) -> dict[str, int]:
    """Map each guarded variable name to the first line that guards it."""
    found: dict[str, int] = {}

    def add(name: str, number: int) -> None:
        found.setdefault(name, number)

    for number, text in lines:
        for m in GUARD_TEST.finditer(text):
            add(m.group(1), number)
        for m in GUARD_EXPAND.finditer(text):
            add(m.group(1), number)
        if ROOT_CHECK.search(text):
            for m in re.finditer(r"\$\{?(\w+)", text):
                add(m.group(1), number)
    return found


def command_args(text: str):
    """Yield the argument lists of path commands in one logical line."""
    lexer = shlex.shlex(text, posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    try:
        tokens = list(lexer)
    except ValueError:
        return
    at_start = True
    current: list[str] | None = None
    for token in tokens:
        if current is not None:
            if token in CONTROL:
                yield current
                current = None
                at_start = True
            else:
                current.append(token)
            continue
        if token in CONTROL or token in COMMAND_LEADS:
            at_start = True
            continue
        if at_start and token in KEYWORDS:
            continue
        if at_start and token in PATH_COMMANDS:
            current = []
        at_start = False
    if current is not None:
        yield current


def shell_problems(path: Path, text: str, just: bool) -> list[str]:
    problems: list[str] = []
    lines = list(logical_lines(text, just))
    if not just and not OPT_OUT.search(text):
        first = lines[0] if lines else None
        if first is None or not re.match(r"^set\s+(-\S*e\S*|-o\s+errexit)\b", first[1]):
            problems.append(f"{path}:{first[0] if first else 1}: S1: "
                            "the first command must be `set -e` (or -eu, -euo pipefail), "
                            "or a `# check-shell-paths: no-errexit <reason>` line")
    if just:
        # Just bodies run only where recipe lines are indented: normalize and keep them.
        lines = [(n, normalize_just(t)) for n, t in lines]
    guarded = guards(lines)
    for number, text_line in lines:
        for args in command_args(text_line):
            for arg in args:
                if arg.startswith("-"):
                    continue
                m = VAR_BASE.match(arg)
                if not m:
                    continue
                name = m.group(1) or m.group(2)
                if name is None:
                    # A command result is a path base only when a `/` follows it: an empty
                    # result then becomes `/`. An empty result alone is a cp error, not a root copy.
                    if m.group(4) == "/":
                        problems.append(f"{path}:{number}: P1: `{arg}` builds a path from a "
                                        "command result; assign it to a variable and guard it")
                    continue
                if guarded.get(name, 10**9) > number:
                    problems.append(f"{path}:{number}: P1: `{arg}` builds a path from "
                                    f"`${name}` with no earlier guard; add "
                                    f'`[ -n "${name}" ] && [ -d "${name}" ] || exit 1` '
                                    "or drop the trailing `/`")
    return problems


def check_file(path: Path) -> list[str]:
    text = path.read_text(encoding="utf-8", errors="replace")
    if is_just(path):
        return shell_problems(path, text, just=True)
    if text.startswith("#!"):
        return shell_problems(path, text, just=False)
    # No shebang: a sourced library. errexit here would change the caller's shell,
    # so only the path rule applies.
    return [p for p in shell_problems(path, text, just=False) if ": P1: " in p]


def tracked(repo: str) -> list[Path]:
    out = subprocess.run(["git", "-C", repo, "ls-files", "-z"],
                         check=True, capture_output=True).stdout
    names = [n for n in out.decode("utf-8", "surrogateescape").split("\0") if n]
    keep = [n for n in names if n.endswith(".sh") or n == "justfile"
            or n.endswith("/justfile") or n.endswith(".just")]
    return [Path(repo, n) for n in keep]


def main(argv: list[str]) -> int:
    repo = argv[1] if len(argv) > 1 else "."
    names = argv[2:]
    files = [Path(repo, n) for n in names] if names else tracked(repo)
    found: list[str] = []
    for path in files:
        found.extend(check_file(path))
    for line in found:
        print(f"check_shell_paths: {line}", file=sys.stderr)
    if found:
        print(f"check_shell_paths: {len(found)} problem(s) in {len(files)} file(s)", file=sys.stderr)
        return 1
    print(f"check_shell_paths: {len(files)} file(s) ok")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
