#!/usr/bin/env python3
"""Classify the leaf instruction pointers of a vmlab-hv run (section 2 of
the phase 3e design).

Input: files of lines "<dso index> <offset in hex>", one per sample, as the
variant of bench/ipsample.c writes that this directory describes in the
design ("How to measure again"), and the binary with line tables. Each
offset of the binary is resolved with addr2line to its chain of inlined
functions, and the innermost function decides the class.

A line "-1 <address in hex>" is a sample in no mapped object: compiled code
of the JIT (card J9). `--perf-map` names the perf map of the run
(`VMLAB_HV_JIT_PERF_MAP=1`, `/tmp/perf-<pid>.map`); such a sample is then
"compiled code" and is named by the block that held the address. After a
reset of the code region an address can carry two names: the later line of
the map wins. A raw file given as `<raw>@<map>` uses its own map: each
process has its own addresses.

`--window <from> <to>` keeps the samples of those unix seconds, from the
third field that `measure/ipraw.c` writes.

usage: ipclass.py <binary> <raw file>[@<perf map>]... [--top N] [--perf-map FILE]
       [--window FROM TO]
"""
import bisect
import collections
import re
import subprocess
import sys

CLASSES = (
    # (class, pattern on "function file" of the innermost frame, with the
    # generic arguments of the function taken away). The first match wins.
    # The first three are the Rust side of the JIT (card J9).
    ("compiled code: entering and leaving it in the Rust loop",
     r"Seat>::|seat::|run_chained|exec::execute|Jit>::current|Jit>::link|make_link|Jit>::unlink"),
    ("compiled code: the thunk and the pure calls", r"exec::thunk|pure_call|Compiled>::call"),
    ("JIT compile: emit, analyse, the assembler",
     r"vmlab_hv_tcg_jit::emit|vmlab_hv_tcg_jit::asm|Asm>::|Jit>::compile|Jit>::name"),
    ("RAM behind a TLB hit: the bounds, the region and the copy of GuestBus",
     r"with_roms|GuestBus as |GuestBus>::|mark_dirty|vm_memory|load_reserved|store_conditional"),
    ("softmmu: the TLB lookup and the load and store paths",
     r"Side>::|tlb\.rs|FastTlb|Mmu>::hit|hit_host|exec::load|exec::store|^load[ <]|^store[ <]|^lookup |"
     r"Compiled>::access|^needed |^\{closure#0\}<vmlab_hv_vmm::vcpu::guest_bus::GuestBus, \d+>"),
    ("page walk and TLB fill", r"mmu::|Mmu>::|mmu\.rs"),
    ("register copies at the entry and the exit of a block",
     r"^(read|write) .*block\.rs|Place|next<\(|eq<\("),
    ("effect dispatch: guards, the call of a load, a store or a helper",
     r"^effect |Compiled>::(effect|guard|leave|call)"),
    ("operation loop: fetch, dispatch and the pure operations",
     r"step_unchecked|^step |Generic>::run|Compiled>::run|next<|eq<|block\.rs|uint_macros|int_macros"),
    ("block lookup and the checks of the loop",
     r"run_inner|usable|as_mut|Cache>::|hashbrown|^index |Cpu>::|Cpu<|state\.rs|option\.rs|run_block|take_mail"),
    ("translation: lift, graph, lowering",
     r"lift|Graph|Lowering|compile|native|tcg_ir|decode|generated|fasthash|alloc|vec|Vec|realloc|malloc|memmove|memcpy|memset|hash"),
    ("helpers: FP, vector, crypto", r"helpers|softfloat|f\d+_"),
    ("system: exceptions, system registers, timer, interrupts", r"exception|sysreg|timer|Clock|gic|Gic|psci"),
    ("devices and the VMM", r"vmlab_hv_vmm|vmlab_hv_devices|virtio|vmlab_disk"),
)


# Classes before CLASSES (card #1212): the serving of the thunk (T3-D2b,
# T3-D2c), and the exclusive paths of exec.rs.
EXTRA = (
    ("compiled code: the thunk and the pure calls",
     r"^(serve_jump|serve_terminal|serve_next|terminal_run|resolve) .*exec\.rs"),
    ("exclusives: the paths of exec.rs", r"^(store_exclusive|store_exclusive_locked|load_exclusive) .*exec\.rs"),
)
SOFTMMU_CLOSURE = re.compile(r"^\{closure#0\}<vmlab_hv_vmm::vcpu::guest_bus::GuestBus, \d+>")


def bare(name):
    """The name without the generic arguments of each path segment:
    `serve_jump<vmlab_hv_vmm::vcpu::guest_bus::GuestBus>` is `serve_jump`,
    `<a::Cpu<B>>::run` is `<a::Cpu>::run`. Before card #1212 the classes
    matched the whole name, so a generic argument from `vmlab_hv_vmm` put a
    function of the JIT (`effect`, `serve_jump`, `store_pair` over
    `GuestBus`) into "devices and the VMM"."""
    previous = None
    while previous != name:
        previous = name
        name = re.sub(r"(?<=[A-Za-z0-9_}])<[^<>]*>", "", name)
    return name


def classify(function, where):
    """The class of a frame: `function` with its generic arguments,
    `where` the file of its line, with the crate path taken away."""
    if SOFTMMU_CLOSURE.search(function):
        return "softmmu: the TLB lookup and the load and store paths"
    text = bare(function) + " " + where
    for name, pattern in EXTRA + CLASSES:
        if re.search(pattern, text):
            return name
    return "other in the binary"


def strip_generics(name):
    """The name without its turbofish arguments: `<a::B>::run::<c::D<E>>`
    is `<a::B>::run`."""
    out, index = [], 0
    while index < len(name):
        if name.startswith("::<", index):
            depth, index = 1, index + 3
            while index < len(name) and depth:
                depth += {"<": 1, ">": -1}.get(name[index], 0)
                index += 1
            continue
        out.append(name[index])
        index += 1
    return "".join(out)


def main():
    args = sys.argv[1:]
    top = 0
    if "--top" in args:
        at = args.index("--top")
        top = int(args[at + 1])
        del args[at:at + 2]
    perf_map = None
    if "--perf-map" in args:
        at = args.index("--perf-map")
        perf_map = args[at + 1]
        del args[at:at + 2]
    window = None
    if "--window" in args:
        at = args.index("--window")
        window = (int(args[at + 1]), int(args[at + 2]))
        del args[at:at + 3]
    binary, files = args[0], args[1:]
    counts = collections.Counter()
    other = 0
    compiled = {}
    # Samples in a library with the first return address in the binary
    # (`measure/ipraw.c`): they are classed by that caller.
    callers = collections.Counter()
    for name in files:
        name, _, own_map = name.partition("@")
        samples = compiled.setdefault(own_map or perf_map, collections.Counter())
        for line in open(name):
            fields = line.split()
            dso, offset = fields[:2]
            if window and len(fields) > 2 and not window[0] <= int(fields[2]) < window[1]:
                continue
            if dso == "0":
                counts[offset] += 1
            elif dso == "-1":
                samples[int(offset, 16)] += 1
            elif len(fields) > 3 and fields[3] != "-":
                callers[fields[3]] += 1
            else:
                other += 1
    offsets = sorted(set(counts) | set(callers))
    out = subprocess.run(["addr2line", "-f", "-i", "-C", "-a", "-e", binary, *("0x" + o for o in offsets)],
                         capture_output=True, text=True, check=True).stdout.splitlines() if offsets else []
    chains, current = {}, None
    index = 0
    while index < len(out):
        line = out[index]
        if re.fullmatch(r"0x[0-9a-f]+", line):
            current = chains.setdefault(line[2:].lstrip("0") or "0", [])
            index += 1
        else:
            current.append((line, out[index + 1].split(" ")[0]))
            index += 2
    total = (sum(counts.values()) + other + sum(callers.values())
             + sum(sum(c.values()) for c in compiled.values()))
    classes = collections.Counter()
    leaves = collections.Counter()
    blocks = collections.Counter()
    named = collections.Counter()
    for map_file, samples in compiled.items():
        named.update(name_compiled(samples, map_file))
    for name, count in named.items():
        if name is None:
            classes["anonymous memory with no name in the perf map"] += count
        elif name.startswith("vmlab-hv-jit"):
            classes["compiled code: the trampoline"] += count
            leaves["[jit] " + name] += count
        else:
            classes["compiled code: the blocks"] += count
            blocks[name] += count
    samples = [(offset, count, "") for offset, count in counts.items()]
    samples += [(offset, count, "libraries, called from: ") for offset, count in callers.items()]
    for offset, count, prefix in samples:
        chain = chains.get(offset.lstrip("0") or "0", [("??", "??")])
        if prefix:
            # The caller of a library: the outermost frame of its chain
            # that is not the library's wrapper is the function that called.
            chain = chain[-1:]
        function, where = chain[0]
        function = strip_generics(function)
        where = re.sub(r"^.*/(crates|src|registry/src/[^/]+)/", "", where)
        classes[prefix + classify(function, where)] += count
        leaves[prefix + function[:70] + "  " + re.sub(r":\d+.*$", "", where)] += count
    if other:
        classes["libraries (libc and others), no caller in the binary"] = other
    print(f"{total} samples")
    for name, count in classes.most_common():
        print(f"{100 * count / total:6.1f} %  {name}")
    if top:
        print()
        for name, count in leaves.most_common(top):
            print(f"{100 * count / total:6.1f} %  {name}")
        if blocks:
            print()
            print(f"{len(blocks)} compiled blocks with a sample")
            for name, count in blocks.most_common(top):
                print(f"{100 * count / total:6.1f} %  {name}")


def name_compiled(compiled, perf_map):
    """The samples of compiled code by the name of the perf map entry that
    holds each address, the latest entry first; None for no entry."""
    out = collections.Counter()
    if not perf_map:
        if compiled:
            out[None] = sum(compiled.values())
        return out
    entries = []
    for order, line in enumerate(open(perf_map)):
        parts = line.rstrip("\n").split(" ", 2)
        if len(parts) == 3:
            entries.append((int(parts[0], 16), int(parts[1], 16), order, parts[2]))
    entries.sort()
    starts = [e[0] for e in entries]
    widest = max((e[1] for e in entries), default=0)
    for address, count in compiled.items():
        best = None
        at = bisect.bisect_right(starts, address) - 1
        while at >= 0 and entries[at][0] + widest > address:
            start, size, order, name = entries[at]
            if address < start + size and (best is None or order > best[0]):
                best = (order, name)
            at -= 1
        out[best[1] if best else None] += count
    return out


if __name__ == "__main__":
    main()
