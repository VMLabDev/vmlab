#!/usr/bin/env python3
"""The costs P1 to P5 of a vmlab-hv run (card #1212).

Input: a directory of the 5 s windows that `ipstack` writes with
IPSTACK_WORDS set (`w-<unix s>.stacks`, `.maps`, `.threads`, `.clock`: each
sample "<tid> <ip> <callers>... | <stack words>... @<monotonic ns>"), the binary with
line tables and frame pointers, the perf map of the run, and the names of
the IFUNC picks of glibc that `libcsyms` writes.

Each sample is classed by its leaf as `ipclass.py` does, with two
additions:
- a leaf in libc is named by the function of `.eh_frame` that holds it
  (glibc's internal functions have no symbol, but each has an FDE), with
  the name that `libcsyms` gives the start of that function, and it is
  charged to its caller: the first word at the stack pointer that is a
  return address into the binary or into compiled code (a leaf memmove
  keeps no frame, so its return address is there and not in the
  frame-pointer chain);
- the samples of the threads that are not vCPUs are counted apart, by
  thread name.

Then one section for each cost:
- P1: the leaves of "RAM behind a TLB hit", by function and line, and the
  inclusive share of the locks (`parking_lot`, `lock_api`, a futex);
- P2: the leaves of "devices and the VMM" and of "system", by function,
  and the threads that are not vCPUs;
- P3: the libc leaves, by function and by the class of the caller;
- P4 and P5 read the general table and the leaves: compiled blocks by name
  (guest PC, level, size), the exclusives and the waits of the vCPUs.

`--callers REGEX` adds the callers of the functions whose bare name
matches: for each vCPU sample with one in its chain, the first outer
function of the chain with another name, two levels.

usage: ipcost.py <binary> <dir> --perf-map FILE [--libc-names FILE]
                 [--window FROM TO] [--top N] [--callers REGEX]
FROM and TO are unix seconds, fractions allowed: the samples whose time is
inside are kept.
"""
import bisect
import collections
import glob
import os
import re
import subprocess
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from ipclass import bare, classify  # noqa: E402
from ipstack import Maps, perf_map, symbolise  # noqa: E402

# On the function name without its generic arguments (`bare`).
LOCK = re.compile(r"^<?(parking_lot|lock_api)::|raw_mutex|RawMutex>::|RawReentrantMutex|futex|lock_slow|unlock_slow")
EXCLUSIVE = re.compile(r"load_reserved|store_conditional|compare_and_store|compare_exchange|cas16|"
                       r"exclusive|Exclusive|transaction")
WAIT = re.compile(r"broadcast|Broadcast|Shootdown|wait|Wait|park|Park|code_mail|take_mail|post|yield")


def fde_ranges(path):
    """The (start, end) of every function of `.eh_frame` of an ELF file."""
    out = subprocess.run(["readelf", "--debug-dump=frames", path], capture_output=True, text=True).stdout
    found = []
    for match in re.finditer(r"FDE cie=\S+ pc=([0-9a-f]+)\.\.([0-9a-f]+)", out):
        found.append((int(match.group(1), 16), int(match.group(2), 16)))
    found.sort()
    return found


def dynamic(path):
    out = subprocess.run(["nm", "-D", "-n", "--defined-only", path], capture_output=True, text=True).stdout
    names = {}
    for line in out.splitlines():
        parts = line.split()
        if len(parts) == 3 and parts[1] in "tTwWiI":
            names.setdefault(int(parts[0], 16), parts[2])
    return names


class Library:
    """Names a library address by its FDE and the names known for its start."""

    def __init__(self, path, extra):
        self.short = os.path.basename(path)
        self.fdes = fde_ranges(path)
        self.starts = [s for s, _ in self.fdes]
        self.names = dynamic(path)
        self.names.update(extra)

    def name(self, address):
        at = bisect.bisect_right(self.starts, address) - 1
        if at >= 0 and address < self.fdes[at][1]:
            start = self.fdes[at][0]
            return f"{self.short}:{self.names.get(start, hex(start))}"
        return f"{self.short}:?"


def main():
    args = sys.argv[1:]
    options = {"--perf-map": None, "--libc-names": None, "--top": "25", "--callers": None}
    window = None
    if "--window" in args:
        at = args.index("--window")
        window = (float(args[at + 1]), float(args[at + 2]))
        del args[at:at + 3]
    for key in list(options):
        if key in args:
            at = args.index(key)
            options[key] = args[at + 1]
            del args[at:at + 2]
    binary, folder = os.path.realpath(args[0]), args[1]
    top = int(options["--top"])
    jit = perf_map(options["--perf-map"])
    jit_starts = [r[0] for r in jit]
    jit_names = []
    if options["--perf-map"]:
        for order, line in enumerate(open(options["--perf-map"])):
            parts = line.rstrip("\n").split(" ", 2)
            if len(parts) == 3:
                jit_names.append((int(parts[0], 16), int(parts[1], 16), order, parts[2]))
    jit_names.sort()
    name_starts = [e[0] for e in jit_names]
    widest = max((e[1] for e in jit_names), default=0)

    def jit_name(ip):
        best = None
        at = bisect.bisect_right(name_starts, ip) - 1
        while at >= 0 and jit_names[at][0] + widest > ip:
            start, size, order, name = jit_names[at]
            if ip < start + size and (best is None or order > best[0]):
                best = (order, name)
            at -= 1
        return best[1] if best else None

    def in_jit(ip):
        at = bisect.bisect_right(jit_starts, ip) - 1
        return at >= 0 and ip < jit[at][0] + jit[at][1]

    libc_extra = {}
    if options["--libc-names"]:
        for line in open(options["--libc-names"]):
            address, name = line.split()
            libc_extra.setdefault(int(address, 16), name)

    # Pass 1: the samples, each (thread, frames, caller frame of a library leaf).
    samples = []
    wanted = collections.defaultdict(set)
    windows = sorted(glob.glob(os.path.join(folder, "w-*.stacks")))
    for stacks in windows:
        prefix = stacks[:-len(".stacks")]
        start = int(prefix.rsplit("-", 1)[1])
        if window and (start + 6 < window[0] or start > window[1]):
            continue
        maps = Maps(prefix + ".maps")
        shift = 0.0
        if os.path.exists(prefix + ".clock"):
            real, mono = (int(x) for x in open(prefix + ".clock").read().split())
            shift = (real - mono) / 1e9
        threads = {}
        if os.path.exists(prefix + ".threads"):
            for line in open(prefix + ".threads"):
                tid, _, name = line.rstrip("\n").partition(" ")
                threads[tid] = name
        for line in open(stacks):
            parts = line.split()
            if len(parts) < 2:
                continue
            words, when = [], None
            if "|" in parts:
                at = parts.index("|")
                tail = parts[at + 1:]
                parts = parts[:at]
                if tail and tail[-1].startswith("@"):
                    when = int(tail[-1][1:]) / 1e9 + shift
                    tail = tail[:-1]
                words = [int(w, 16) for w in tail]
            if window and (when is None or not window[0] <= when < window[1]):
                continue
            thread = threads.get(parts[0], "?")
            frames = []
            for index, text in enumerate(parts[1:]):
                ip = int(text, 16)
                if in_jit(ip):
                    frames.append(("jit", ip))
                    continue
                place = maps.resolve(ip)
                if place is None:
                    break
                address = place[1] if index == 0 else place[1] - 1
                frames.append(place if index == 0 else (place[0], address))
                wanted[place[0]].add(address)
            caller = None
            if frames and frames[0][0] != "jit" and os.path.realpath(frames[0][0]) != binary:
                for word in words:
                    if in_jit(word):
                        caller = ("jit", word)
                        break
                    place = maps.resolve(word)
                    if place and os.path.realpath(place[0]) == binary:
                        caller = (place[0], place[1] - 1)
                        wanted[place[0]].add(place[1] - 1)
                        break
            samples.append((thread, frames, caller))

    names = {}
    for path, addresses in wanted.items():
        if os.path.realpath(path) == binary:
            for address, chain in symbolise(path, addresses).items():
                names[(path, address)] = chain
        else:
            library = Library(path, libc_extra if "libc.so" in path else {})
            for address in addresses:
                names[(path, address)] = [(library.name(address), library.short)]

    def chain_of(frame):
        if frame[0] == "jit":
            return [("compiled code", "jit")]
        return names.get(frame, [("??", "??")]) or [("??", "??")]

    def class_of(frame):
        if frame[0] == "jit":
            name = jit_name(frame[1]) or ""
            return "compiled code: the trampoline" if name.startswith("vmlab-hv-jit") else "compiled code: the blocks"
        function, where = chain_of(frame)[0]
        if os.path.realpath(frame[0]) != binary:
            return "library"
        return classify(function, where)

    vcpu = [s for s in samples if s[0].startswith("vcpu-")]
    other = [s for s in samples if not s[0].startswith("vcpu-")]
    total = len(vcpu)
    print(f"{len(samples)} samples in the window, {total} on vCPU threads, {len(other)} on other threads")

    def pct(count, of=None):
        return f"{100 * count / max(of if of is not None else total, 1):6.1f} %"

    classes = collections.Counter()
    leaves = collections.Counter()
    blocks = collections.Counter()
    libc = collections.Counter()
    libc_by_caller = collections.Counter()
    p1 = collections.Counter()
    p2 = collections.Counter()
    unclassed = collections.Counter()
    inclusive = collections.Counter()
    for thread, frames, caller in vcpu:
        if not frames:
            classes["no frame resolved"] += 1
            continue
        leaf = frames[0]
        kind = class_of(leaf)
        function, where = chain_of(leaf)[0]
        if kind == "library":
            # Charged to the caller at the stack pointer, else to the first
            # frame of the chain in the binary or in compiled code.
            by = caller
            if by is None:
                for frame in frames[1:]:
                    if frame[0] == "jit" or os.path.realpath(frame[0]) == binary:
                        by = frame
                        break
            by_kind = class_of(by) if by else "no caller"
            by_function = (jit_name(by[1]) or "?").split(" pa ")[0] if by and by[0] == "jit" else (
                chain_of(by)[-1][0] if by else "-")
            classes[f"libraries, by the class of the caller: {by_kind}"] += 1
            libc[function] += 1
            libc_by_caller[f"{function[:40]}  <-  {by_kind}: {by_function[:60]}"] += 1
        else:
            classes[kind] += 1
        if leaf[0] == "jit":
            blocks[jit_name(leaf[1]) or "no name"] += 1
        else:
            leaves[f"{function[:80]}  {re.sub(r':(\d+).*$', r':\1', where)}"] += 1
        if kind.startswith("RAM behind"):
            # The innermost frame of the inlined chain that is not in core
            # or alloc: the line of GuestBus that the time is under.
            chain = chain_of(leaf)
            own = next(((f, w) for f, w in chain if not re.match(r"^(core|alloc|std)/|^(sync|ptr|slice|num|intrinsics|vec|iter)/|^[a-z_]+\.rs", w)
                        or "guest_bus" in w), chain[0])
            p1[f"{bare(own[0])[-60:]}  {re.sub(r':(\d+).*$', r':\1', own[1])}  (leaf {re.sub(r':(\d+).*$', r':\1', where)})"] += 1
        if kind == "other in the binary":
            unclassed[f"{bare(function)[:70]}  {re.sub(r':(\d+).*$', r'', where)}"] += 1
        if kind.startswith("devices") or kind.startswith("system"):
            p2[f"{kind[:7]}  {bare(function)[:70]}  {re.sub(r':(\d+).*$', r'', where)}"] += 1
        seen = set()
        for frame in frames:
            for name, _ in chain_of(frame):
                seen.add(bare(name))
        if caller:
            for name, _ in chain_of(caller):
                seen.add(bare(name))
        for name in seen:
            inclusive[name] += 1

    print("\nThe vCPU threads, by the class of the leaf:")
    for name, count in classes.most_common():
        print(f"{pct(count)}  {name}")
    print(f"\nThe leaves, top {top}:")
    for name, count in leaves.most_common(top):
        print(f"{pct(count)}  {name}")
    print(f"\nCompiled blocks, top {top} of {len(blocks)}:")
    for name, count in blocks.most_common(top):
        print(f"{pct(count)}  {name}")
    # Compiled code by where the guest ran: user (el0), the kernel's
    # virtual addresses, and code with the MMU off or identity-mapped (the
    # address is the physical one: the firmware), by thread.
    spaces = collections.Counter()
    for thread, frames, caller in vcpu:
        if frames and frames[0][0] == "jit":
            name = jit_name(frames[0][1]) or ""
            match = re.match(r"a64 (0x[0-9a-f]+) pa (0x[0-9a-f]+) el(\d)", name)
            if not match:
                space = "the trampoline or no name"
            elif match.group(3) == "0":
                space = "el0"
            elif match.group(1) == match.group(2):
                space = f"el{match.group(3)}, virtual = physical (firmware, MMU off)"
            else:
                space = f"el{match.group(3)}, kernel addresses"
            spaces[(thread, space)] += 1
    print("\nCompiled code by thread and where the guest ran:")
    for (thread, space), count in sorted(spaces.items()):
        print(f"{pct(count)}  {thread:8}  {space}")
    print(f"\nP1: the leaves of RAM behind a TLB hit, top {top}:")
    for name, count in p1.most_common(top):
        print(f"{pct(count)}  {name}")
    lock = sum(c for n, c in inclusive.items() if LOCK.search(n))
    print(f"\nInclusive, functions that take a lock (any sample with one in its chain, may overlap): "
          f"{pct(max((c for n, c in inclusive.items() if LOCK.search(n)), default=0))} the largest, {lock} sums")
    for name, count in sorted(((n, c) for n, c in inclusive.items() if LOCK.search(n)), key=lambda x: -x[1])[:10]:
        print(f"{pct(count)}  {name[:110]}")
    print(f"\nP2: the leaves of devices, the VMM and system, top {top}:")
    for name, count in p2.most_common(top):
        print(f"{pct(count)}  {name}")
    print(f"\nOther in the binary, top {top}:")
    for name, count in unclassed.most_common(top):
        print(f"{pct(count)}  {name}")
    by_thread = collections.Counter(re.sub(r"\d+$", "N", s[0]) for s in other)
    print(f"\nThe other threads, samples against the vCPU samples ({len(other)}):")
    for name, count in by_thread.most_common(15):
        print(f"{pct(count)}  {name}")
    other_leaves = collections.Counter()
    for thread, frames, caller in other:
        if frames:
            function, where = chain_of(frames[0])[0]
            other_leaves[f"{function[:80]}  {re.sub(r':(\d+).*$', r'', where)}"] += 1
    for name, count in other_leaves.most_common(15):
        print(f"{pct(count)}  {name}")
    print("\nP3: the library leaves by function:")
    for name, count in libc.most_common(top):
        print(f"{pct(count)}  {name}")
    print(f"\nP3: by function and caller, top {top}:")
    for name, count in libc_by_caller.most_common(top):
        print(f"{pct(count)}  {name}")
    if options["--callers"]:
        wanted_re = re.compile(options["--callers"])
        callers = collections.Counter()
        hit = 0
        for thread, frames, caller in vcpu:
            flat = []
            for frame in frames:
                for name, where in chain_of(frame):
                    flat.append((bare(name), re.sub(r":(\d+).*$", r":\1", where)))
            at = next((i for i, (n, _) in enumerate(flat) if wanted_re.search(n)), None)
            if at is None:
                continue
            hit += 1
            outer = [f"{n[-50:]} {w}" for n, w in flat[at + 1:] if n != flat[at][0]][:2]
            callers[f"{flat[at][0][-50:]}  <-  " + "  <-  ".join(outer)] += 1
        print(f"\nCallers of {options['--callers']!r}: {pct(hit)} of the vCPU samples")
        for name, count in callers.most_common(top):
            print(f"{pct(count)}  {name}")
    print("\nP5: inclusive, the exclusives and the waits, top 20 each:")
    for pattern in (EXCLUSIVE, WAIT):
        for name, count in sorted(((n, c) for n, c in inclusive.items() if pattern.search(n)),
                                  key=lambda x: -x[1])[:20]:
            print(f"{pct(count)}  {name[:110]}")
        print()


if __name__ == "__main__":
    main()
