#!/usr/bin/env python3
"""Build the pinned upstream iPXE as standalone PCI ROMs, for vmlab's guest
firmware (`--profile guest`, the default) or vmlab-hv's NIC tests (`hv`)."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile


HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
HV_OUTPUT = Path(os.environ["VMLAB_TARGET_ROOT"]) / "ipxe" if os.environ.get("VMLAB_TARGET_ROOT") else ROOT / "hv/target/ipxe"

# What each profile builds and where. Both share the pin, the console
# configuration and this procedure; they differ in `config/<profile>/` and in
# which ROMs they make (README.md says why).
PROFILES = {
    "guest": {
        "source": ROOT / "guest/.cache/ipxe/upstream",
        "output": ROOT / "guest/dist/firmware/x86_64",
        # Combined legacy and EFI ROMs, with the virtio-net halves on their own.
        "models": (("virtio-net", "1af41000"), ("e1000", "8086100e"),
                   ("rtl8139", "10ec8139"), ("pcnet", "10222000")),
        "singles": "virtio-net",
        "sums": "IPXE-SHA256SUMS",
        "order": None,
        # Built in the build container; the repository holds no record of it.
        "record": None,
    },
    "hv": {
        "source": HV_OUTPUT / "upstream",
        "output": HV_OUTPUT,
        "models": (("virtio-net", "1af41000"),),
        "singles": "virtio-net",
        "sums": "SHA256SUMS",
        # The order hv's recorded SHA256SUMS.hv lists them in.
        "order": ("1af41000.rom", "1af41000.efirom", "virtio-net.rom", "ipxe-source.tar"),
        # Every build of this profile is compared with it (`verify`).
        "record": HERE / "SHA256SUMS.hv",
    },
}


def run(*arguments, **kwargs):
    return subprocess.run(arguments, check=True, **kwargs)


def configs(profile):
    """The local configuration headers: shared first, then the profile's."""
    return [*sorted((HERE / "config").glob("*.h")),
            *sorted((HERE / "config" / profile).glob("*.h"))]


# The outputs whose bytes no compiler or linker decides. A record is compared
# on these under every toolchain.
TOOLCHAIN_FREE = ("ipxe-source.tar",)

# The object directories of a checkout, removed when `object_inputs` changes.
OBJECTS = ("bin", "bin-x86_64-efi")


def toolchain():
    """The compiler and the linker of this host, one line each."""
    return [subprocess.check_output([tool, "--version"], text=True).splitlines()[0].strip()
            for tool in ("gcc", "ld")]


def make_variables(src):
    """What every make call passes. iPXE's build id is a checksum of its
    objects, and their debug information names the build directory, so without
    the prefix map the ROM bytes differ from one checkout path to the next."""
    return ["BUILD_SERIAL=0", "NO_WERROR=1", f"EXTRA_CFLAGS=-ffile-prefix-map={src}=."]


def object_inputs(tools):
    """What the objects of a cached checkout depend on and make does not
    track: the make variables, with the checkout path left out, and the
    toolchain."""
    return "\n".join([*make_variables("<src>"), *tools]) + "\n"


def restore_objects(source):
    """Put back the tracked files that are missing from the object directories
    of a checkout (upstream tracks `src/bin/.gitignore`). Removing an object
    directory removes them too, and the checkout must stay equal to the pin."""
    tracked = subprocess.check_output(
        ["git", "-C", str(source), "ls-files", "--deleted", "--",
         *(f"src/{name}" for name in OBJECTS)], text=True).split("\n")
    missing = [path for path in tracked if path]
    if missing:
        run("git", "-C", str(source), "checkout", "HEAD", "--", *missing)


def clear_stale_objects(source, tools):
    """Remove the objects of a checkout when `object_inputs` differs from the
    stamp beside it, and write the stamp. The checkout stays equal to the pin."""
    stamp = source.parent / "object-inputs"
    if not stamp.exists() or stamp.read_text() != object_inputs(tools):
        for name in OBJECTS:
            shutil.rmtree(source / "src" / name, ignore_errors=True)
        restore_objects(source)
        stamp.write_text(object_inputs(tools))


def install_configs(source, headers):
    """Make `src/config/local` of a checkout hold the given headers and
    nothing else that changes a build. Returns the names it removed.

    Upstream ignores that directory in git, so the untracked-files check of
    `main` does not see it, and a header that an earlier revision of
    `config/` held would stay in a cached checkout and be compiled in. make
    writes an empty header for each local one the build does not give. Those
    stay: they change nothing, and writing them again would rebuild every
    object. make rebuilds what included a removed header, because it writes
    the empty one anew."""
    local = source / "src/config/local"
    local.mkdir(exist_ok=True)
    tracked = set(subprocess.check_output(
        ["git", "-C", str(source), "ls-files", "-z", "--", "src/config/local"],
        text=True).split("\0"))
    given = {header.name for header in headers}
    removed = []
    for path in sorted(local.iterdir()):
        inside = f"src/config/local/{path.name}"
        if path.name in given or any(name == inside or name.startswith(inside + "/") for name in tracked):
            continue
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        elif path.is_file() and not path.is_symlink() and path.stat().st_size == 0:
            continue
        else:
            path.unlink()
        removed.append(path.name)
    for header in headers:
        shutil.copyfile(header, local / header.name)
    return removed


def parse_record(text):
    """A record as (toolchain lines, {name: sha256}). The toolchain is the
    `# ` lines at its head, which `sha256sum -c` skips."""
    tools, sums = [], {}
    for line in text.splitlines():
        if line.startswith("#"):
            tools.append(line[1:].strip())
        elif line.strip():
            digest, name = line.split(maxsplit=1)
            sums[name.strip()] = digest
    return tools, sums


def format_record(tools, sums, order):
    return "".join([*(f"# {tool}\n" for tool in tools),
                    *(f"{sums[name]}  {name}\n" for name in order)])


def verify(record, built, tools):
    """Compare a build with a record. Returns (faults, notes): a fault is an
    output whose bytes are not the recorded ones, or one the two do not share.
    Under a toolchain other than the recorded one the compiled outputs are not
    compared, and a note says so."""
    recorded_tools, recorded = parse_record(record)
    same_tools = recorded_tools == list(tools)
    faults, skipped = [], []
    for name in sorted(set(recorded) | set(built)):
        if name not in built:
            faults.append(f"{name}: recorded, not built")
        elif name not in recorded:
            faults.append(f"{name}: built, not recorded")
        elif not same_tools and name not in TOOLCHAIN_FREE:
            skipped.append(name)
        elif recorded[name] != built[name]:
            faults.append(f"{name}: recorded {recorded[name]}, built {built[name]}")
    notes = []
    if skipped:
        notes.append(
            f"not compared with the record: {', '.join(skipped)}. This host builds with "
            f"{' and '.join(tools)}; the record was made with {' and '.join(recorded_tools) or 'an unnamed toolchain'}.")
    return faults, notes


def finish(plan, output, names, tools, record=False):
    """Write the sums of a finished build, then compare them with the
    profile's record, or with `record` set write that record instead."""
    order = plan["order"] or names
    built = {name: hashlib.sha256((output / name).read_bytes()).hexdigest() for name in order}
    (output / plan["sums"]).write_text(format_record([], built, order))
    print((output / plan["sums"]).read_text(), end="")
    path = plan["record"]
    if record:
        if path is None:
            raise SystemExit("this profile has no record in the repository")
        path.write_text(format_record(tools, built, order))
        print(f"recorded in {path}")
    elif path is not None:
        faults, notes = verify(path.read_text(), built, tools)
        for note in notes:
            print(f"warning: {note}")
        if faults:
            raise SystemExit(
                f"the build differs from {path}:\n" + "".join(f"  {fault}\n" for fault in faults)
                + "If an input changed on purpose (the pin, a configuration header, this "
                "script), build again with --record and commit the record. If none did, "
                "the build is not reproducible: find out why.")
        print(f"matches {path}" + ("" if not notes else " where compared"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--profile", choices=sorted(PROFILES), default="guest")
    parser.add_argument("--record", action="store_true",
                        help="write the profile's record from this build instead of comparing with it")
    arguments = parser.parse_args()
    profile = arguments.profile
    plan = PROFILES[profile]
    if arguments.record and plan["record"] is None:
        parser.error(f"the {profile} profile has no record in the repository")
    pin = json.loads((HERE / "upstream.json").read_text())
    source, output = plan["source"], plan["output"]
    source.parent.mkdir(parents=True, exist_ok=True)
    output.mkdir(parents=True, exist_ok=True)
    if not source.exists():
        run("git", "clone", "--no-checkout", pin["url"], str(source))
        run("git", "-C", str(source), "checkout", "--detach", pin["commit"])
    actual = subprocess.check_output(
        ["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    if actual != pin["commit"]:
        raise SystemExit(f"{source}: expected {pin['commit']}, got {actual}")
    # A checkout whose objects an earlier build.py removed lacks them.
    restore_objects(source)
    if subprocess.check_output(["git", "-C", str(source), "diff", "HEAD", "--"]):
        raise SystemExit(f"{source}: tracked changes; refusing an unpinned build")
    if subprocess.check_output(
        ["git", "-C", str(source), "ls-files", "--others", "--exclude-standard"], text=True).strip():
        raise SystemExit(f"{source}: untracked source files; refusing an unpinned build")
    src = source / "src"
    for name in install_configs(source, configs(profile)):
        print(f"removed the stale {source / 'src/config/local' / name}")
    env = dict(os.environ, SOURCE_DATE_EPOCH=pin["source_date_epoch"], LC_ALL="C", TZ="UTC")
    # make does not rebuild an object when a flag or the compiler changes.
    tools = toolchain()
    clear_stale_objects(source, tools)
    names = []
    for model, pci in plan["models"]:
        legacy, efi = f"bin/{pci}.rom", f"bin-x86_64-efi/{pci}.efirom"
        run("make", "-C", str(src), f"-j{min(os.cpu_count() or 1, 8)}",
            *make_variables(src), legacy, efi, env=env)
        name = f"{model}.rom"
        with (output / name).open("wb") as combined:
            run("perl", str(src / "util/catrom.pl"), str(src / legacy),
                str(src / efi), stdout=combined, env=env)
        names.append(name)
        if model == plan["singles"]:
            for target in (legacy, efi):
                name = Path(target).name
                shutil.copyfile(src / target, output / name)
                names.append(name)
    # Complete corresponding source, including the exact local configuration.
    with (output / "ipxe-source.tar").open("wb") as tar:
        run("git", "-C", str(source), "archive", "--format=tar", "--prefix=ipxe/",
            pin["commit"], stdout=tar)
    with tarfile.open(output / "ipxe-source.tar", "a") as archive:
        for path in (HERE / "upstream.json", HERE / "build.py", *configs(profile)):
            info = archive.gettarinfo(str(path), "ipxe/vmlab-build/" + path.relative_to(HERE).as_posix())
            info.uid = info.gid = 0
            info.uname = info.gname = ""
            info.mtime = int(pin["source_date_epoch"])
            info.mode = 0o644
            with path.open("rb") as data:
                archive.addfile(info, data)
    names.append("ipxe-source.tar")
    finish(plan, output, names, tools, record=arguments.record)


if __name__ == "__main__":
    main()
