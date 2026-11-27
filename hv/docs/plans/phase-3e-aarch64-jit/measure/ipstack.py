#!/usr/bin/env python3
"""The stack profile of a vmlab-hv run (card C13b, #1144).

Input: the files that ipstack.c writes (OUT.stacks, OUT.maps and
OUT.threads), one prefix per sampling window, a binary with line tables
and frame pointers, and the perf map of the run (`VMLAB_HV_JIT_PERF_MAP=1`
writes `/tmp/perf-<pid>.map`). Every frame of a sample is resolved with
addr2line to its chain of inlined functions. A frame in compiled code is
named "compiled code". Compiled code keeps no frame pointer, so the chain
of a sample ends at the first frame that is neither in the binary, in a
library nor in the perf map.

Output, over the threads whose name holds --thread (default: every thread):
- the class of the leaf, by the classes of ipclass.py;
- the leaf function, with its first caller outside it;
- the inclusive share of each function: the samples with it anywhere in
  their chain.

usage: ipstack.py <binary> <prefix>... [--perf-map FILE] [--thread TEXT] [--top N]
"""
import bisect
import collections
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ipclass import CLASSES, strip_generics  # noqa: E402

JIT = "compiled code"


def segments(path):
    """The LOAD segments of an ELF file: (offset, vaddr, size)."""
    out = subprocess.run(["readelf", "-lW", path], capture_output=True, text=True).stdout
    found = []
    for line in out.splitlines():
        parts = line.split()
        if parts and parts[0] == "LOAD":
            found.append((int(parts[1], 16), int(parts[2], 16), int(parts[4], 16)))
    return found


class Maps:
    def __init__(self, path):
        self.ranges = []
        for line in open(path):
            parts = line.split()
            if len(parts) < 6 or "x" not in parts[1] or not parts[5].startswith("/"):
                continue
            start, end = (int(x, 16) for x in parts[0].split("-"))
            self.ranges.append((start, end, int(parts[2], 16), parts[5]))
        self.ranges.sort()
        self.starts = [r[0] for r in self.ranges]
        self.loads = {}

    def resolve(self, ip):
        """(file, address in the file) or None."""
        at = bisect.bisect_right(self.starts, ip) - 1
        if at < 0:
            return None
        start, end, offset, path = self.ranges[at]
        if ip >= end:
            return None
        file_offset = ip - start + offset
        if path not in self.loads:
            self.loads[path] = segments(path)
        for seg_offset, vaddr, size in self.loads[path]:
            if seg_offset <= file_offset < seg_offset + size:
                return path, file_offset - seg_offset + vaddr
        return path, file_offset


def perf_map(path):
    ranges = []
    if path:
        for line in open(path):
            parts = line.split(maxsplit=2)
            if len(parts) >= 2:
                ranges.append((int(parts[0], 16), int(parts[1], 16)))
    ranges.sort()
    return ranges


def in_jit(ranges, starts, ip):
    at = bisect.bisect_right(starts, ip) - 1
    return at >= 0 and ip < ranges[at][0] + ranges[at][1]


def symbolise(path, addresses):
    """address -> [(function, file:line)], innermost first."""
    result = {}
    addresses = sorted(addresses)
    for chunk in range(0, len(addresses), 20000):
        part = addresses[chunk:chunk + 20000]
        out = subprocess.run(["addr2line", "-f", "-i", "-C", "-a", "-e", path, *(hex(a) for a in part)],
                             capture_output=True, text=True).stdout.splitlines()
        current, index = None, 0
        while index < len(out):
            line = out[index]
            if re.fullmatch(r"0x[0-9a-f]+", line):
                current = result.setdefault(int(line, 16), [])
                index += 1
            else:
                where = out[index + 1].split(" ")[0] if index + 1 < len(out) else "??"
                current.append((strip_generics(line), re.sub(r"^.*/(crates|src|registry/src/[^/]+)/", "", where)))
                index += 2
    return result


def classify(function, where):
    text = function + " " + where
    for name, pattern in CLASSES:
        if re.search(pattern, text):
            return name
    return "other in the binary"


def main():
    args = sys.argv[1:]
    options = {"--perf-map": None, "--thread": "", "--top": "25"}
    for key in list(options):
        if key in args:
            at = args.index(key)
            options[key] = args[at + 1]
            del args[at:at + 2]
    binary, prefixes = os.path.realpath(args[0]), args[1:]
    top = int(options["--top"])
    jit = perf_map(options["--perf-map"])
    jit_starts = [r[0] for r in jit]

    # Each sample as a list of frames: ("jit",), (file, address) or None.
    samples = []
    wanted = collections.defaultdict(set)
    for prefix in prefixes:
        maps = Maps(prefix + ".maps")
        names = {}
        if os.path.exists(prefix + ".threads"):
            for line in open(prefix + ".threads"):
                tid, _, name = line.rstrip("\n").partition(" ")
                names[tid] = name
        for line in open(prefix + ".stacks"):
            parts = line.split()
            if not parts or options["--thread"] not in names.get(parts[0], ""):
                continue
            frames = []
            for index, text in enumerate(parts[1:]):
                ip = int(text, 16)
                if in_jit(jit, jit_starts, ip):
                    frames.append((JIT,))
                    continue
                place = maps.resolve(ip)
                if place is None:
                    break
                # A return address points after the call: look at the call.
                address = place[1] if index == 0 else place[1] - 1
                frames.append((place[0], address))
                wanted[place[0]].add(address)
            samples.append(frames)
    names = {}
    for path, addresses in wanted.items():
        if os.path.realpath(path) == binary:
            for address, chain in symbolise(path, addresses).items():
                names[(path, address)] = chain
        else:
            short = os.path.basename(path)
            for address in addresses:
                names[(path, address)] = [(short, short)]

    def chain_of(frame):
        if frame[0] == JIT:
            return [(JIT, JIT)]
        return names.get(frame, [("??", "??")]) or [("??", "??")]

    total = len(samples)
    classes, leaves, inclusive = collections.Counter(), collections.Counter(), collections.Counter()
    for frames in samples:
        if not frames:
            classes["no frame resolved"] += 1
            continue
        leaf = chain_of(frames[0])
        function, where = leaf[0]
        if function == JIT:
            classes["compiled code"] += 1
        elif os.path.realpath(frames[0][0]) != binary:
            classes["libraries (libc and others)"] += 1
        else:
            classes[classify(function, where)] += 1
        # The first caller that is another function than the leaf.
        caller = "-"
        for frame in frames[1:]:
            for name, _ in chain_of(frame):
                if name != function:
                    caller = name
                    break
            if caller != "-":
                break
        leaves[f"{function[:80]}  <-  {caller[:70]}"] += 1
        seen = set()
        for frame in frames:
            for name, _ in chain_of(frame):
                seen.add(name)
        for name in seen:
            inclusive[name] += 1
    print(f"{total} samples, threads matching {options['--thread']!r}")
    print("\nThe class of the leaf:")
    for name, count in classes.most_common():
        print(f"{100 * count / max(total, 1):6.1f} %  {name}")
    print(f"\nThe leaf and its caller, top {top}:")
    for name, count in leaves.most_common(top):
        print(f"{100 * count / max(total, 1):6.1f} %  {name}")
    print(f"\nInclusive, top {top}:")
    for name, count in inclusive.most_common(top):
        print(f"{100 * count / max(total, 1):6.1f} %  {name[:110]}")


if __name__ == "__main__":
    main()
