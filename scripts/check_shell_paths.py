#!/usr/bin/env python3
"""Fail when a shell script or just recipe can turn an empty path into /.

An empty variable turns `"$d"/.` into `/.` and `"$d"/*` into `/*`. A tracked
script that copies, syncs, moves or removes such a path copies or deletes the
whole root filesystem (see VML-81 and VML-88).

Rules, for every tracked *.sh file, justfile and *.just file:

S1  A shell script turns on errexit (`set -e`) before its first command.
    Without it a failed `ls` or `realpath` lets the next line run anyway.
    Files with no shebang are sourced libraries and are not checked.

P1  An argument of cp, rsync, mv, rm, install, chown or chmod that starts with
    a variable or a command substitution, and is followed by `/` or ends
    there, needs that variable to be safe at that point. The file is read top
    to bottom, and a variable becomes safe by:
      - a test on the whole variable inside `[ ]`, `[[ ]]` or `test`: -n, -d,
        -e, -f, -s or -z, or a comparison with `/` alone (`[ "$d" != / ]`);
      - a `${d:?}` expansion;
      - an assignment whose value cannot be empty or `/`: it has a literal
        character other than `/` and `.` (`d=/opt/x`, `d=$work/src`).
    Any other assignment (`d=$(ls ...)`, `d=$1`, `local d`, `read d`,
    `for d in "$@"`) makes it unsafe again, also after a guard.
    A command result (`$(...)` or backticks) followed by `/` always fails.
    The command may follow sudo, env, command, exec, nice, ionice, nohup,
    time or `VAR=value`, and `find -exec` or xargs.

This is a text check, not a shell parser. Known limits:
  - It reads the file in order and ignores control flow: a guard in another
    function, or in another branch of an `if`, counts for every later line.
  - A test counts as a guard also when its result is not acted on
    (`[ -z "$d" ] && echo empty` passes); write it with `|| exit 1`.
  - Guards in another file, and variables set by `eval` or `printf -v`, are
    not seen.
  - `$((...))`, `$'...'` and nested quotes inside `$(...)` can confuse the
    tokenizer; a line it cannot split is skipped.
Put each guard next to the command it protects.

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
CONTROL = {";", "&&", "||", "|", "(", ")", "&", "{", "}", "!", ";;", "|&"}
KEYWORDS = {"then", "do", "else", "if", "while", "until", "elif"}
# Prefix commands: the command they run comes after their options. The value
# lists name the options that take an argument.
PREFIXES = {
    "sudo": {"-u", "-g", "-C", "-D", "-h", "-p", "-r", "-t", "-U", "-T", "-R"},
    "doas": {"-u", "-C"},
    "env": {"-u", "-C", "-S"},
    "nice": {"-n"},
    "ionice": {"-c", "-n", "-p"},
    "command": set(), "exec": {"-a"}, "nohup": set(), "time": set(),
}
# `find ... -exec cp ...` and `xargs cp ...` start a new command too.
COMMAND_LEADS = {"-exec", "-execdir", "xargs"}
DECLARE = {"local", "export", "readonly", "declare", "typeset"}
TESTS = {"[": "]", "[[": "]]", "test": None}
GUARD_FLAGS = {"-n", "-d", "-e", "-f", "-s", "-z"}
COMPARE = {"!=", "=", "=="}
# A variable name, with an optional array subscript: `srcdir[$name]` is its own name.
NAME = r"\w+(?:\[[^\]]*\])?"
WHOLE_VAR = re.compile(r"^\$(?:\{(" + NAME + r")\}|(\w+))$")
VAR_BASE = re.compile(r"^\$(?:\{(" + NAME + r")[^}]*\}|(\w+))(/|$)")
GUARD_EXPAND = re.compile(r"\$\{(" + NAME + r"):\?")
ASSIGN = re.compile(r"^(" + NAME + r")\+?=(.*)$", re.DOTALL)
EXPANSION = re.compile(r"\$\{[^}]*\}|\$\w+|\$[#?@*!$-]")
# `${X:-word}` and `${X:=word}` are never emptier than word.
DEFAULT = re.compile(r"\$\{\w+:[-=]([^}]*)\}")
# Command substitutions are replaced by this name before the line is split.
CMD = "__CMD__"
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


def replace_substitutions(line: str) -> str:
    """Replace each `$(...)` and `...` (backticks) with `$__CMD__`, so the split
    keeps it in one word, quoted or not. `$((...))` arithmetic is left alone."""
    out: list[str] = []
    i = 0
    while i < len(line):
        if line.startswith("$(", i) and not line.startswith("$((", i):
            depth, j = 0, i + 1
            while j < len(line):
                if line[j] == "(":
                    depth += 1
                elif line[j] == ")":
                    depth -= 1
                    if depth == 0:
                        break
                j += 1
            out.append("$" + CMD)
            i = j + 1
        elif line[i] == "`":
            j = line.find("`", i + 1)
            if j < 0:
                out.append(line[i:])
                break
            out.append("$" + CMD)
            i = j + 1
        else:
            out.append(line[i])
            i += 1
    return "".join(out)


def tokens(line: str) -> list[str] | None:
    lexer = shlex.shlex(replace_substitutions(line), posix=True, punctuation_chars=True)
    lexer.whitespace_split = True
    try:
        return list(lexer)
    except ValueError:
        return None


def whole_var(token: str) -> str | None:
    m = WHOLE_VAR.match(token)
    return (m.group(1) or m.group(2)) if m else None


def safe_value(value: str) -> bool:
    """True when the value of an assignment can be neither empty nor `/`."""
    literal = EXPANSION.sub("", DEFAULT.sub(r"\1", value))
    return any(c not in "/." for c in literal)


def walk(text: str, safe: dict[str, bool]):
    """Read one logical line in order. Update `safe` for each guard and assignment,
    and yield (argument, safe-at-that-point) for each argument of a path command."""
    toks = tokens(text)
    if toks is None:
        return
    at_start = True
    prefix: str | None = None
    skip_next = False
    declare = False
    test_end: str | None = None  # the closing word of the test we are in, or "" for `test`
    loop_var: str | None = None
    loop_safe = True
    in_loop_list = False
    current: list[str] | None = None

    def finish_command():
        nonlocal current
        if current is not None:
            for arg in current:
                yield arg
        current = None

    for i, tok in enumerate(toks):
        for m in GUARD_EXPAND.finditer(tok):
            safe[m.group(1)] = True
        if in_loop_list:
            if tok in CONTROL or tok == "do":
                safe[loop_var] = loop_safe
                in_loop_list = False
            else:
                loop_safe = loop_safe and safe_value(tok)
                continue
        if test_end is not None:
            if tok == test_end or (test_end in ("", "]") and tok in CONTROL - {"!", "(", ")"}):
                test_end = None
                if tok in CONTROL:
                    at_start = True
                continue
            if tok in GUARD_FLAGS and i + 1 < len(toks):
                name = whole_var(toks[i + 1])
                if name:
                    safe[name] = True
            if tok in COMPARE and 0 < i < len(toks) - 1:
                left, right = toks[i - 1], toks[i + 1]
                name = whole_var(left) if right == "/" else whole_var(right) if left == "/" else None
                if name:
                    safe[name] = True
            continue
        if current is not None:
            if tok in CONTROL:
                for arg in current:
                    yield arg, safe.get(var_name(arg) or "", False)
                current = None
                at_start = True
            else:
                current.append(tok)
            continue
        if tok in CONTROL or tok in COMMAND_LEADS:
            at_start, prefix, declare = True, None, False
            continue
        if not at_start:
            if declare:
                m = ASSIGN.match(tok)
                if m:
                    safe[m.group(1)] = safe_value(m.group(2))
                elif re.fullmatch(NAME, tok):
                    safe[tok] = False
            elif prefix == "read" and re.fullmatch(r"\w+", tok):
                safe[tok] = False
            continue
        if skip_next:
            skip_next = False
            continue
        if prefix in PREFIXES and tok.startswith("-"):
            skip_next = tok in PREFIXES[prefix]
            continue
        m = ASSIGN.match(tok)
        if m:
            # `d=value` alone sets d; before a command it sets it for that command only.
            safe[m.group(1)] = safe_value(m.group(2))
            continue
        if tok in KEYWORDS:
            continue
        if tok in PREFIXES:
            prefix = tok
            continue
        if tok in TESTS:
            test_end = TESTS[tok] or ""
            at_start = False
            continue
        if tok == "for" and i + 2 < len(toks) and toks[i + 2] == "in":
            loop_var, loop_safe, in_loop_list = toks[i + 1], True, False
            continue
        if tok == "in" and loop_var is not None and i >= 2 and toks[i - 2] == "for":
            in_loop_list = True
            continue
        if loop_var is not None and i >= 1 and toks[i - 1] == "for" and tok == loop_var:
            continue
        at_start = False
        if tok in PATH_COMMANDS:
            current = []
        elif tok in DECLARE:
            declare = True
        elif tok == "read":
            prefix = "read"
    if in_loop_list:
        safe[loop_var] = loop_safe
    if current is not None:
        for arg in current:
            yield arg, safe.get(var_name(arg) or "", False)


def var_name(arg: str) -> str | None:
    m = VAR_BASE.match(arg)
    return (m.group(1) or m.group(2)) if m else None


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
    safe: dict[str, bool] = {}
    for number, text_line in lines:
        for arg, ok in walk(text_line, safe):
            if arg.startswith("-"):
                continue
            m = VAR_BASE.match(arg)
            if not m:
                continue
            name = m.group(1) or m.group(2)
            if name == CMD:
                # A command result is a path base only when a `/` follows it: an empty
                # result then becomes `/`. An empty result alone is a cp error, not a root copy.
                if m.group(3) == "/":
                    problems.append(f"{path}:{number}: P1: `{arg}` builds a path from a "
                                    "command result; assign it to a variable and guard it")
                continue
            if not ok:
                problems.append(f"{path}:{number}: P1: `{arg}` builds a path from "
                                f"`${name}` with no guard since its last assignment; add "
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
