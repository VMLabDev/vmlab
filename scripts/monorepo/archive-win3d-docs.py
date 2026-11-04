#!/usr/bin/env python3
"""Print an apply_patch patch that links stripped win3d references to the archive.

Pass the pristine source clone used for the import. This command reads that
clone and the working docs; it never changes the source repository.
"""

import argparse
import json
from pathlib import Path
import posixpath
import re
import subprocess


ROOT = Path(__file__).resolve().parents[2]
GUEST = ROOT / "guest/win3d"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("--document", type=Path, help="limit the patch to one document")
    parser.add_argument("--index", action="store_true", help="print the verified evidence-reference index")
    args = parser.parse_args()

    def git(*arguments):
        return subprocess.check_output(["git", "-C", str(args.source), *arguments], text=True).strip()

    revision = git("rev-parse", "HEAD")
    base = f"https://github.com/VMLabDev/vmlab-win3d/blob/{revision}"
    commits = git("rev-list", "HEAD").splitlines()
    evidence = git("ls-tree", "-r", "--name-only", "HEAD", "docs/evidence/").splitlines()
    archived = git("ls-tree", "-r", "--name-only", "HEAD").splitlines()
    references = set()
    index_path = GUEST / "docs/archived-evidence.json"
    cached = json.loads(index_path.read_text()) if index_path.exists() else {}
    if args.document and cached.get("source_commit") == revision:
        references.update(cached["references"])
    for name in ([] if references else archived):
        if not name.endswith(".md") or name.startswith("docs/evidence/"):
            continue
        original = git("show", f"HEAD:{name}")
        for value in re.findall(r"(?<!\[)`([^`\n]+)`(?!\]\()", original):
            if "/" not in value or " " in value or value.startswith(("docs/evidence/", "patches/host/")):
                continue
            pattern = re.escape(value).replace(r"\*", ".*")
            pattern = re.sub(r"<[^>]+>", ".*", pattern)
            if any(re.search("/" + pattern + "$", path) for path in evidence):
                references.add(value.rstrip("/"))
    if args.index:
        print(json.dumps({"source_commit": revision, "references": sorted(references)}, indent=2))
        return

    def archive_target(path):
        path, separator, anchor = path.partition("#")
        prefix = re.split(r"[<{*]", path, maxsplit=1)[0]
        if prefix != path:
            path = prefix.rsplit("/", 1)[0] + "/"
        if path not in archived and not path.endswith("/"):
            matches = [item for item in archived if item.startswith(path)]
            if len(matches) == 1:
                path = matches[0]
            elif not matches:
                revisions = git("log", "--full-history", "--no-merges", "--format=%H", "--", f":(glob){path}*").splitlines()
                for historical_revision in revisions:
                    historical = git("ls-tree", "-r", "--name-only", historical_revision, "--", path.rsplit("/", 1)[0]).splitlines()
                    matches = [item for item in historical if item.startswith(path)]
                    if len(matches) == 1:
                        return f"https://github.com/VMLabDev/vmlab-win3d/blob/{historical_revision}/{matches[0]}{separator}{anchor}"
        return f"{base}/{path}{separator}{anchor}"
    changes = []
    for doc in sorted(GUEST.rglob("*.md")):
        if args.document and doc != args.document.resolve():
            continue
        if any(part in (".scratch", "out", "target") for part in doc.relative_to(GUEST).parts):
            continue
        old = doc.read_text()
        text = re.sub(r"\[\[([^\]]+)\]\(([^)]+)\)\]\(([^)]+)\)", r"[\1](\3)", old)

        def link(match):
            target = match.group(2)
            if target.startswith(base + "/"):
                label = match.group(1)[1:-1]
                if label in references and target.startswith(base + "/docs/evidence/"):
                    return f"`{label}`"
                return f"{match.group(1)}({archive_target(target[len(base) + 1:])})"
            if "://" in target or target.startswith("#"):
                return match.group(0)
            path = posixpath.normpath(posixpath.join(doc.parent.relative_to(GUEST).as_posix(), target))
            if path == "docs/evidence" or path.startswith(("docs/evidence/", "patches/host/")):
                return f"{match.group(1)}({archive_target(path)})"
            return match.group(0)

        text = re.sub(r"(\[[^\]]*\])\(([^)\s]+)\)", link, text)

        def code(match):
            value = match.group(1)
            if match.start() and match.string[match.start() - 1] == "[":
                return match.group(0)
            if re.fullmatch(r"[0-9a-f]{7,40}", value):
                found = [commit for commit in commits if commit.startswith(value)]
                if len(found) == 1:
                    return f"[{value}](https://github.com/VMLabDev/vmlab-win3d/commit/{found[0]})"
            if value.startswith(("docs/evidence/", "patches/host/")):
                return f"[{value}]({archive_target(value)})"
            return match.group(0)

        # Leave fenced commands intact; old host procedures are removed separately.
        lines, fenced = [], False
        for line in text.splitlines(keepends=True):
            if re.match(r"\s*(```|~~~)", line):
                fenced = not fenced
            lines.append(line if fenced else re.sub(r"`([^`\n]+)`", code, line))
        text = "".join(lines)
        if text != old:
            changes.append(f"*** Update File: {doc}\n@@\n" +
                           "\n".join("-" + line for line in old.rstrip("\n").splitlines()) + "\n" +
                           "\n".join("+" + line for line in text.rstrip("\n").splitlines()))
    print("*** Begin Patch\n" + "\n".join(changes) + "\n*** End Patch")


if __name__ == "__main__":
    main()
