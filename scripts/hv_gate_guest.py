"""Prepare the pinned x86_64 guest fixtures for the hypervisor gate."""

import fcntl
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


INPUTS = (
    "scripts/hv_gate_guest.py",
    "rust-toolchain.toml",
    "guest/build-asset.sh",
    "guest/build-agent.sh",
    "guest/cinit",
    "guest/cinit-proto",
    "guest/agent",
    "guest/agent-proto",
    "hv/justfile",
)
BOOT_FILES = ("vmlinuz", "initramfs.img", "VERSION")
GUEST_DIST = Path("guest/dist/x86_64")


def cache_key():
    tracked = subprocess.check_output(["git", "ls-files", "-z", "--", *INPUTS])
    digest = hashlib.sha256()
    for name in tracked.split(b"\0")[:-1]:
        digest.update(name + b"\0")
        digest.update(hashlib.sha256(Path(os.fsdecode(name)).read_bytes()).digest())
    return digest.hexdigest()


def ready(path):
    return path.is_file() and path.stat().st_size > 0


def stage_boot_files(source):
    GUEST_DIST.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=GUEST_DIST) as temporary:
        for name in BOOT_FILES:
            staged = Path(temporary) / name
            shutil.copyfile(source / name, staged)
            staged.replace(GUEST_DIST / name)


def prepare(cache):
    key = cache_key()
    entry = cache / key
    cache.mkdir(parents=True, exist_ok=True)
    with (cache / f"{key}.lock").open("a") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        if all(ready(entry / name) for name in (*BOOT_FILES, "rootfs.sqfs")):
            print(f"hv-gate: reusing guest fixtures {key}", file=sys.stderr, flush=True)
            stage_boot_files(entry)
            return entry / "rootfs.sqfs"

        print(f"hv-gate: building guest fixtures {key}", file=sys.stderr, flush=True)
        env = os.environ.copy()
        env.pop("CARGO_TARGET_DIR", None)
        env.update(
            VMLAB_SKIP_FIRMWARE="1",
            VMLAB_REQUIRE_ALL_TARGETS="1",
            VMLAB_AGENT_STAMP=f"agent=hv-gate-{key[:12]}",
        )
        subprocess.run(
            ["just", "guest-build", "x86_64"], check=True, env=env, stdout=sys.stderr
        )
        subprocess.run(
            ["just", "--justfile", "hv/justfile", "s1-rootfs-build"],
            check=True,
            stdout=sys.stderr,
        )

        entry.mkdir(exist_ok=True)
        sources = [GUEST_DIST / name for name in BOOT_FILES]
        sources.append(Path("hv/target/s1/rootfs.sqfs"))
        with tempfile.TemporaryDirectory(dir=cache) as temporary:
            for source in sources:
                if not ready(source):
                    raise ValueError(f"missing or empty guest fixture: {source}")
                shutil.copyfile(source, Path(temporary) / source.name)
            for source in sources:
                (Path(temporary) / source.name).replace(entry / source.name)
        return entry / "rootfs.sqfs"


if __name__ == "__main__":
    try:
        print(prepare(Path(sys.argv[1])).resolve())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        sys.exit(f"hv-gate: cannot prepare required guest fixtures: {error}")
