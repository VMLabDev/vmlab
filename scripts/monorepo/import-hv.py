#!/usr/bin/env python3
"""Import hypervisor main into a clean branch, then replay rewiring commits."""

import argparse
from pathlib import Path
import subprocess
import tempfile


SOURCE = "https://github.com/VMLabDev/vmlab-hypervisor.git"


def git(*args, cwd=None):
    return subprocess.check_output(["git", *args], cwd=cwd, text=True).strip()


def run(*args, cwd=None):
    subprocess.run(args, cwd=cwd, check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--replay", help="old-import..old-tip range of rewiring commits")
    args = parser.parse_args()
    root = Path(git("rev-parse", "--show-toplevel"))
    branch = git("symbolic-ref", "--short", "HEAD", cwd=root)
    if branch in ("main", "master"):
        parser.error("check out a new import branch first")
    if git("status", "--porcelain", cwd=root):
        parser.error("the import branch must have a clean worktree")
    if (root / "hv").exists():
        parser.error("hv/ already exists; use a new branch from vmlab main")
    commits = []
    if args.replay:
        if ".." not in args.replay or "..." in args.replay:
            parser.error("--replay requires old-import..old-tip")
        if git("rev-list", "--merges", args.replay, cwd=root):
            parser.error("the rewiring range must contain only ordinary commits")
        commits = git("rev-list", "--reverse", "--topo-order", args.replay, cwd=root).splitlines()
        if not commits:
            parser.error("the rewiring range is empty")
    with tempfile.TemporaryDirectory(prefix="vmlab-hv-import-") as temporary:
        source = Path(temporary) / "source"
        run("git", "clone", "--no-tags", "--single-branch", "--branch", "main", SOURCE, str(source))
        original = git("rev-parse", "HEAD", cwd=source)
        count = git("rev-list", "--count", "HEAD", cwd=source)
        run("git", "filter-repo", "--to-subdirectory-filter", "hv",
            "--prune-empty", "never", "--prune-degenerate", "never", cwd=source)
        imported = git("rev-list", "--count", "HEAD", cwd=source)
        if imported != count:
            raise RuntimeError(f"history count changed: {count} -> {imported}")
        run("git", "fetch", "--no-tags", str(source), "main", cwd=root)
        message = ("chore(hv): import hypervisor history under hv/\n\n"
                   f"Source: {SOURCE}\nSource-main: {original}\n"
                   f"Source-commits: {count}\nImported-commits: {imported}")
        run("git", "merge", "--no-ff", "--allow-unrelated-histories", "FETCH_HEAD", "-m", message, cwd=root)
    merge = git("rev-parse", "HEAD", cwd=root)
    for commit in commits:
        run("git", "cherry-pick", commit, cwd=root)
    print(f"Import merge: {merge}\nSource main: {original}\nCommits: {count} -> {imported}")


if __name__ == "__main__":
    main()
