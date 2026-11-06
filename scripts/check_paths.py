#!/usr/bin/env python3
"""Fail when a tracked path cannot be checked out on Windows.

Windows refuses the characters : * ? " < > | \\ in a file name, and drops a
trailing dot or space from each path component. A tracked path with either
breaks actions/checkout on a Windows runner (git exits 128), so the merge bar
refuses it here, on Linux, before it reaches main.

Usage: check_paths.py [REPO]   (default: the current directory)
"""

import subprocess
import sys

FORBIDDEN = set(':*?"<>|\\')


def problem(path: str) -> str | None:
    """Return why `path` cannot be checked out on Windows, or None."""
    bad = sorted(FORBIDDEN.intersection(path))
    if bad:
        return "contains " + " ".join(repr(c) for c in bad)
    for part in path.split("/"):
        if part.endswith((".", " ")):
            return f"component {part!r} ends with a dot or a space"
    return None


def tracked_paths(repo: str) -> list[str]:
    out = subprocess.run(["git", "-C", repo, "ls-files", "-z"],
                         check=True, capture_output=True).stdout
    return [p for p in out.decode("utf-8", "surrogateescape").split("\0") if p]


def main(argv: list[str]) -> int:
    repo = argv[1] if len(argv) > 1 else "."
    found = [(p, why) for p in tracked_paths(repo) if (why := problem(p))]
    for path, why in found:
        print(f"check_paths: {path}: {why}", file=sys.stderr)
    if found:
        print(f"check_paths: {len(found)} tracked path(s) cannot be checked "
              "out on Windows", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
