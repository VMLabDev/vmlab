#!/usr/bin/env python3
"""kvm-unit-tests for arm64 on QEMU and on vmlab-hv, with the same pass list.

Phase 3d, DESIGN.md O5: the tests of what the lockstep cannot see, the GIC,
the timer and PSCI with more than one CPU. kvm-unit-tests is GPL-2.0. It is
fetched and built here at test time, runs as a guest image, and nothing of
it is committed or read to write the emulator.

Each test of `arm/unittests.cfg` whose name is selected runs twice with the
arguments of that file: on `qemu-system-aarch64 -machine virt -cpu cortex-a53
-accel tcg`, and on `vmlab-hv` with a machine description of a kernel boot.
The serial output of both is reduced to its result lines (`PASS:`, `FAIL:`,
`SKIP:`, `XFAIL:`, `XPASS:`, `SUMMARY:`, `EXIT: STATUS=`). A test passes here
when both lists are the same. The lists themselves are printed, so a subtest
that fails on both sides shows as what it is: QEMU's answer.

    python3 scripts/a64-kvm-unit-tests.py --work target/kvm-unit-tests \\
        --vmlab-hv ../target/release/vmlab-hv

The build needs `aarch64-linux-gnu-gcc`, or clang with ld.lld and the LLVM
binutils, which are then wrapped under the names of the GNU cross tools.
"""

from __future__ import annotations

import argparse
import configparser
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import time

REPOSITORY = "https://gitlab.com/kvm-unit-tests/kvm-unit-tests.git"
# The commit that the results of card C10 were taken at.
REVISION = "master"
# The tests of card C10: selftest, gic (the GICv2 of `virt`), timer, psci.
SELECTED = re.compile(r"^(selftest-.*|gicv2-.*|timer|psci)$")
MAX_SMP = 4
RESULT = re.compile(r"^(PASS|FAIL|SKIP|XFAIL|XPASS|SUMMARY|EXIT: STATUS)\b")
# The tools of the GNU cross toolchain, and what LLVM calls them.
LLVM_TOOLS = {
    "ld": "ld.lld",
    "objcopy": "llvm-objcopy",
    "objdump": "llvm-objdump",
    "ar": "llvm-ar",
    "nm": "llvm-nm",
    "readelf": "llvm-readelf",
    "strip": "llvm-strip",
}
PREFIX = "aarch64-linux-gnu-"


def run(argv: list[str], **kwargs) -> subprocess.CompletedProcess:
    print("+", " ".join(shlex.quote(str(arg)) for arg in argv), flush=True)
    return subprocess.run(argv, check=True, **kwargs)


def checked_dir(path: Path) -> Path:
    """Refuse the paths that a recursive delete must never get."""
    resolved = path.resolve()
    if str(resolved) in ("", "/") or resolved == Path.home().resolve():
        raise SystemExit(f"refusing to work in {resolved}")
    return resolved


def toolchain(work: Path) -> tuple[str, dict[str, str]]:
    """The cross prefix for `configure`, and the environment of the build."""
    if shutil.which(PREFIX + "gcc"):
        return PREFIX, dict(os.environ)
    missing = [tool for tool in ["clang", *LLVM_TOOLS.values()] if not shutil.which(tool)]
    if missing:
        raise SystemExit(f"no {PREFIX}gcc, and no {', '.join(missing)} to stand in for it")
    tools = work / "cross"
    tools.mkdir(parents=True, exist_ok=True)
    wrappers = {name: f'exec {tool} "$@"' for name, tool in LLVM_TOOLS.items()}
    # The Makefiles ask the compiler for its library with GCC's option.
    wrappers["gcc"] = (
        'for arg in "$@"; do\n'
        '  if [ "$arg" = -print-libgcc-file-name ]; then exit 0; fi\n'
        "done\n"
        "exec clang --target=aarch64-linux-gnu -Wno-unknown-warning-option "
        '-Wno-unused-command-line-argument "$@"'
    )
    for name, body in wrappers.items():
        path = tools / (PREFIX + name)
        path.write_text(f"#!/bin/sh\nset -eu\n{body}\n")
        path.chmod(0o755)
    env = dict(os.environ)
    env["PATH"] = f"{tools}{os.pathsep}{env['PATH']}"
    return PREFIX, env


def build(work: Path, revision: str) -> tuple[Path, str]:
    """Fetch and build kvm-unit-tests: its directory and its commit."""
    source = work / "kvm-unit-tests"
    if not (source / ".git").is_dir():
        run(["git", "clone", "--quiet", REPOSITORY, str(source)])
    run(["git", "-C", str(source), "fetch", "--quiet", "origin"])
    run(["git", "-C", str(source), "checkout", "--quiet", "--detach", revision])
    commit = subprocess.run(
        ["git", "-C", str(source), "rev-parse", "HEAD"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()
    stamp = source / ".vmlab-built"
    if stamp.is_file() and stamp.read_text() == commit:
        return source, commit
    prefix, env = toolchain(work)
    run(
        ["./configure", "--arch=arm64", f"--cross-prefix={prefix}", "--processor=cortex-a53"],
        cwd=source,
        env=env,
    )
    log = work / "build.log"
    with log.open("w") as output:
        done = subprocess.run(
            ["make", "-j8"], cwd=source, env=env, stdout=output, stderr=subprocess.STDOUT
        )
    if done.returncode != 0:
        errors = [line for line in log.read_text().splitlines() if "rror" in line]
        raise SystemExit(f"the build failed, see {log}:\n" + "\n".join(errors[:12])[:3000])
    stamp.write_text(commit)
    return source, commit


class Test:
    def __init__(self, name: str, section: configparser.SectionProxy):
        self.name = name
        self.file = section["file"]
        smp = section.get("smp", "1").replace("$MAX_SMP", str(MAX_SMP))
        # `$(($MAX_SMP>4?4:$MAX_SMP))` and its kin: the largest count here.
        numbers = [int(number) for number in re.findall(r"\d+", smp)]
        self.smp = min(max(numbers or [1]), MAX_SMP)
        extra = shlex.split(section.get("extra_params", "").replace("$MAX_SMP", str(MAX_SMP)))
        self.memory = 256
        self.append = ""
        self.machine = ""
        self.cpu = ""
        at = 0
        while at < len(extra):
            option, value = extra[at], extra[at + 1] if at + 1 < len(extra) else ""
            if option == "-m":
                self.memory = int(value.rstrip("M"))
            elif option == "-append":
                self.append = value
            elif option == "-machine":
                self.machine = value
            elif option == "-smp":
                self.smp = min(int(value), MAX_SMP)
            elif option == "-cpu":
                self.cpu = value
            else:
                raise SystemExit(f"{name}: no way to give `{option}` to vmlab-hv")
            at += 2
        self.timeout = int(re.sub(r"\D", "", section.get("timeout", "90")) or 90)


def tests(source: Path, only: list[str]) -> list[Test]:
    parser = configparser.ConfigParser(interpolation=None, strict=False)
    parser.read(source / "arm" / "unittests.cfg")
    found = []
    for name in parser.sections():
        section = parser[name]
        if section.get("arch", "arm64") != "arm64":
            continue
        if (name in only) if only else SELECTED.match(name):
            found.append(Test(name, section))
    return found


def results(output: str) -> list[str]:
    lines = []
    for line in output.replace("\r", "").splitlines():
        line = line.strip()
        if RESULT.match(line):
            lines.append(line)
    return lines


def on_qemu(qemu: str, source: Path, test: Test, timeout: int) -> str:
    machine = "virt" + (f",{test.machine}" if test.machine else "")
    argv = [
        qemu, "-nodefaults", "-machine", machine, "-cpu", "cortex-a53", "-accel", "tcg",
        "-display", "none", "-monitor", "none", "-serial", "stdio",
        "-smp", str(test.smp), "-m", str(test.memory),
        "-kernel", str(source / "arm" / test.file), "-append", test.append,
    ]  # fmt: skip
    try:
        done = subprocess.run(argv, capture_output=True, timeout=timeout, stdin=subprocess.DEVNULL)
        return done.stdout.decode("latin1")
    except subprocess.TimeoutExpired as expired:
        return (expired.stdout or b"").decode("latin1") + "\nEXIT: STATUS=timeout\n"


def on_vmlab_hv(binary: Path, source: Path, test: Test, timeout: int, scratch: Path) -> str:
    scratch = checked_dir(scratch)
    if scratch.exists():
        shutil.rmtree(scratch)
    scratch.mkdir(parents=True)
    serial = scratch / "serial.log"
    description = {
        "name": test.name, "arch": "aarch64", "accel": "tcg", "machine": "virt",
        "vcpus": test.smp, "memory_mib": test.memory,
        "boot": {
            "kind": "kernel", "kernel": str(source / "arm" / test.file),
            "initrd": None, "cmdline": test.append,
        },
        "disks": [], "nics": [], "ports": [], "serial_log": str(serial),
    }  # fmt: skip
    machine = scratch / "machine.json"
    machine.write_text(json.dumps(description))
    argv = [str(binary), "--machine", str(machine), "--control", str(scratch / "c.sock")]
    process = subprocess.Popen(
        argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE
    )
    timed_out = False
    try:
        _, errors = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        process.terminate()
        try:
            _, errors = process.communicate(timeout=10)
        except subprocess.TimeoutExpired:
            process.kill()
            _, errors = process.communicate()
    output = serial.read_bytes().decode("latin1") if serial.is_file() else ""
    if timed_out:
        output += "\nEXIT: STATUS=timeout\n"
    elif process.returncode != 0:
        tail = errors.decode("latin1")[-600:]
        output += f"\nEXIT: STATUS=vmlab-hv exit {process.returncode}\n"
        print(f"  vmlab-hv said: {tail}")
    return output


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--work", type=Path, required=True, help="where the tests are built")
    parser.add_argument("--vmlab-hv", type=Path, required=True, help="the vmlab-hv binary")
    parser.add_argument("--qemu", default="qemu-system-aarch64")
    parser.add_argument("--revision", default=REVISION)
    parser.add_argument("--timeout", type=int, default=120, help="seconds for one run")
    parser.add_argument("only", nargs="*", help="the tests to run (default: those of card C10)")
    arguments = parser.parse_args()
    work = checked_dir(arguments.work)
    work.mkdir(parents=True, exist_ok=True)
    source, commit = build(work, arguments.revision)
    print(f"kvm-unit-tests at {commit}")
    selected = tests(source, arguments.only)
    if not selected:
        raise SystemExit("no test selected")
    different = 0
    for test in selected:
        timeout = max(arguments.timeout, test.timeout)
        started = time.time()
        qemu = results(on_qemu(arguments.qemu, source, test, timeout))
        between = time.time()
        ours_output = on_vmlab_hv(arguments.vmlab_hv, source, test, timeout, work / "run")
        ours = results(ours_output)
        same = qemu == ours and bool(qemu)
        different += not same
        print(
            f"{'same' if same else 'DIFFERENT'}: {test.name} (smp {test.smp}, {test.memory} MiB, "
            f"`{test.append}`): {len(qemu)} result lines on QEMU in {between - started:.1f} s, "
            f"{len(ours)} on vmlab-hv in {time.time() - between:.1f} s",
            flush=True,
        )
        if same:
            for line in qemu:
                print(f"    {line}")
        else:
            for line in sorted(set(qemu) - set(ours)):
                print(f"    QEMU only:     {line}")
            for line in sorted(set(ours) - set(qemu)):
                print(f"    vmlab-hv only: {line}")
            if not ours:
                print("    vmlab-hv wrote: " + ours_output[-400:].replace("\n", " | "))
    print(f"{len(selected) - different} of {len(selected)} tests give the same results as on QEMU")
    return 1 if different else 0


if __name__ == "__main__":
    sys.exit(main())
