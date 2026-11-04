#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Helpers for scripts/release.sh.

usage: assemble.py fill TEMPLATE OUT [--crlf] KEY=VALUE...
           Replace @KEY@ in TEMPLATE; fail if any @KEY@ is left.
       assemble.py zip SRC_DIR OUT.zip EPOCH
           Zip the contents of SRC_DIR with sorted entries and every
           timestamp set to EPOCH, so the same files give the same zip.
       assemble.py guest PACKAGE DEST VERSION COMMIT CERTIFICATE
           Verify existing signed output, add licences and retain build provenance.
       assemble.py notes OUT.md VERSION COMMIT EPOCH CERT_INFO ASSET...
           Write RELEASE-NOTES.md.
"""
import hashlib
import json
import shutil
import os
import posixpath
import re
import subprocess
import sys
import time
import zipfile
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
REPO_URL = "https://github.com/VMLabDev/vmlab"


def fill(template, out, *args):
    crlf = "--crlf" in args
    text = Path(template).read_text()
    for kv in (a for a in args if a != "--crlf"):
        key, _, value = kv.partition("=")
        text = text.replace(f"@{key}@", value)
    left = sorted(set(re.findall(r"@[A-Z0-9_]+@", text)))
    if left:
        sys.exit(f"{template}: unfilled {', '.join(left)}")
    if crlf:
        text = text.replace("\r\n", "\n").replace("\n", "\r\n")
    Path(out).write_bytes(text.encode())


def make_zip(src, out, epoch):
    stamp = time.gmtime(int(epoch))[:6]
    src = Path(src)
    paths = sorted(p for p in src.rglob("*") if p.is_file())
    with zipfile.ZipFile(out, "w", zipfile.ZIP_DEFLATED, compresslevel=9) as z:
        for p in paths:
            info = zipfile.ZipInfo(p.relative_to(src).as_posix(), stamp)
            info.compress_type = zipfile.ZIP_DEFLATED
            info.external_attr = 0o644 << 16
            z.writestr(info, p.read_bytes(), compresslevel=9)


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def relink(text, base):
    """Point relative markdown links (relative to docs/ at `base`) at the repo at the commit."""
    def sub(m):
        target = m.group(2)
        if re.match(r"^(https?:|mailto:|#)", target):
            return m.group(0)
        path, _, frag = target.partition("#")
        url = posixpath.normpath(posixpath.join("docs", path)) if path else "docs/known-limits.md"
        return f"{m.group(1)}({base}/{url}{'#' + frag if frag else ''})"
    return re.sub(r"(\[[^\]]*\])\(([^)\s]+)\)", sub, text)


def section(md, heading):
    """The body of the markdown section titled `heading`, up to the next heading of any level."""
    lines = md.splitlines()
    start = next(i for i, l in enumerate(lines) if l.strip() == heading) + 1
    end = next((i for i in range(start, len(lines)) if lines[i].startswith("#")), len(lines))
    return "\n".join(lines[start:end]).strip()


def guest(package, destination, version, commit, certificate):
    package, destination = Path(package), Path(destination)
    manifest = json.loads((package / "manifest.json").read_text())
    if manifest["version"] != version:
        raise ValueError("prebuilt package version does not match installer/VERSION")
    if (package / "vmlab-win3d-test.cer").read_bytes() != Path(certificate).read_bytes():
        raise ValueError("prebuilt package uses a different signing certificate")
    for entry in manifest["files"]:
        path = package / entry["path"]
        if not path.resolve().is_relative_to(package.resolve()):
            raise ValueError("manifest path escapes package")
        if path.stat().st_size != entry["size"] or sha256(path) != entry["sha256"]:
            raise ValueError(f"prebuilt package hash mismatch: {entry['path']}")
    shutil.copytree(package, destination)
    shutil.copytree(REPO / "licenses", destination / "licenses", dirs_exist_ok=True)
    shutil.copy2(REPO.parents[1] / "THIRD_PARTY_NOTICES", destination / "THIRD_PARTY_NOTICES")
    subprocess.run([sys.executable, str(REPO / "build/package/manifest.py"),
                    str(destination), version, commit, str(destination.parent / "bundle.zip")], check=True)
    (destination.parent / "bundle.zip").unlink()
    packaged = json.loads((destination / "manifest.json").read_text())
    packaged["commit"] = manifest["commit"]
    packaged["release_commit"] = commit
    (destination / "manifest.json").write_text(json.dumps(packaged, indent=2) + "\n")


def notes(out, version, commit, epoch, cert_info, *assets):
    cert = dict((key.strip(), value.strip()) for key, _, value in
                (line.partition(":") for line in Path(cert_info).read_text().splitlines()) if value)
    rows = "\n".join(f"| `{Path(asset).name}` | `{sha256(asset)}` |" for asset in assets)
    Path(out).write_text(f"""# win3d {version}

Tag: win3d-v{version}
Guest release-packaging commit: {commit}
Source: {REPO_URL}/tree/{commit}/guest/win3d
Commit date: {time.strftime('%Y-%m-%d', time.gmtime(int(epoch)))}

Alpha, for test VMs with Secure Boot off. Extract the zip and run Install.cmd
as Administrator inside the vmlab-win3d folder. The package enables test signing
and trusts its certificate machine-wide.

Certificate subject: {cert['subject']}
SHA-1 thumbprint: {cert['sha1 thumbprint']}

The zip includes licenses/ and THIRD_PARTY_NOTICES. Host binaries are released
with vmlab. Its hypervisor bundle carries the patched Venus render server.
No QEMU or standalone render-server asset is part of this release.
The bundle manifest retains the signed input's original build commit.
Known limits: {REPO_URL}/blob/{commit}/guest/win3d/docs/known-limits.md

| Asset | SHA256 |
| --- | --- |
{rows}
""")


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    cmd, args = sys.argv[1], sys.argv[2:]
    {"fill": fill, "zip": make_zip, "notes": notes, "guest": guest}[cmd](*args)


if __name__ == "__main__":
    main()
