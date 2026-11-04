#!/usr/bin/env python3
"""Import component main into a clean branch, then replay rewiring commits."""

import argparse
import os
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
    parser.add_argument("--component", choices=("hv", "win3d"), default="hv")
    parser.add_argument("--replay", help="old-import..old-tip range of rewiring commits")
    args = parser.parse_args()
    root = Path(git("rev-parse", "--show-toplevel"))
    branch = git("symbolic-ref", "--short", "HEAD", cwd=root)
    if branch in ("main", "master"):
        parser.error("check out a new import branch first")
    if git("status", "--porcelain", cwd=root):
        parser.error("the import branch must have a clean worktree")
    destination = "hv" if args.component == "hv" else "guest/win3d"
    source_url = SOURCE if args.component == "hv" else "https://github.com/VMLabDev/vmlab-win3d.git"
    if (root / destination).exists():
        parser.error(f"{destination}/ already exists; use a new branch from vmlab main")
    commits = []
    if args.replay:
        if ".." not in args.replay or "..." in args.replay:
            parser.error("--replay requires old-import..old-tip")
        if git("rev-list", "--merges", args.replay, cwd=root):
            parser.error("the rewiring range must contain only ordinary commits")
        commits = git("rev-list", "--reverse", "--topo-order", args.replay, cwd=root).splitlines()
        if not commits:
            parser.error("the rewiring range is empty")
    cache = Path.home() / ".cache/vmlab-import"
    cache.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=f"{args.component}-", dir=cache) as temporary:
        source = Path(temporary) / "source"
        run("git", "clone", "--no-tags", "--single-branch", "--branch", "main", source_url, str(source))
        original = git("rev-parse", "HEAD", cwd=source)
        count = git("rev-list", "--count", "HEAD", cwd=source)
        before = git("count-objects", "-v", cwd=source)
        filters = ["--prune-empty", "never", "--prune-degenerate", "never"]
        if args.component == "win3d":
            filters = ["--invert-paths", "--path", "docs/evidence/",
                       "--path-glob", "*.pyc"]
            retained = git("rev-list", "HEAD", "--", ".", ":(exclude)docs/evidence/**",
                           ":(exclude)*.pyc", cwd=source).splitlines()
            expected = len(retained)
        else:
            expected = int(count)
        run("git", "filter-repo", *filters, "--to-subdirectory-filter", destination, cwd=source)
        imported = git("rev-list", "--count", "HEAD", cwd=source)
        if int(imported) != expected:
            raise RuntimeError(f"unexpected history count: {count} -> {imported}, expected {expected}")
        after = git("count-objects", "-v", cwd=source)
        print(f"Pack before:\n{before}\nPack after:\n{after}", flush=True)
        run("git", "fetch", "--no-tags", str(source), "main", cwd=root)
        message = (f"chore({args.component}): import history under {destination}/\n\n"
                   f"Source: {source_url}\nSource-main: {original}\n"
                   f"Source-commits: {count}\nImported-commits: {imported}\n"
                   f"Stripped-commits: {int(count) - int(imported)}")
        run("git", "merge", "--no-ff", "--allow-unrelated-histories", "FETCH_HEAD", "-m", message, cwd=root)
    merge = git("rev-parse", "HEAD", cwd=root)
    for commit in commits:
        run("git", "cherry-pick", commit, cwd=root)
    print(f"Import merge: {merge}\nSource main: {original}\nCommits: {count} -> {imported}")


if __name__ == "__main__":
    main()
