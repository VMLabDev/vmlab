#!/usr/bin/env python3
"""Classify the leaf instruction pointers of a vmlab-hv run (section 2 of
the phase 3e design).

Input: files of lines "<dso index> <offset in hex>", one per sample, as the
variant of bench/ipsample.c writes that this directory describes in the
design ("How to measure again"), and the binary with line tables. Each
offset of the binary is resolved with addr2line to its chain of inlined
functions, and the innermost function decides the class.

usage: ipclass.py <binary> <raw file>... [--top N]
"""
import collections
import re
import subprocess
import sys

CLASSES = (
    # (class, pattern on "function file" of the innermost frame, with the
    # generic arguments of the function taken away). The first match wins.
    ("RAM behind a TLB hit: the bounds, the region and the copy of GuestBus",
     r"with_roms|GuestBus as |GuestBus>::|mark_dirty|vm_memory|load_reserved|store_conditional"),
    ("softmmu: the TLB lookup and the load and store paths",
     r"Side>::|tlb\.rs|Mmu>::hit|exec::load|exec::store|^load |^store |^lookup |Compiled>::access|^needed "),
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
    binary, files = args[0], args[1:]
    counts = collections.Counter()
    other = 0
    for name in files:
        for line in open(name):
            dso, offset = line.split()
            if dso == "0":
                counts[offset] += 1
            else:
                other += 1
    offsets = sorted(counts)
    out = subprocess.run(["addr2line", "-f", "-i", "-C", "-a", "-e", binary, *("0x" + o for o in offsets)],
                         capture_output=True, text=True, check=True).stdout.splitlines()
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
    total = sum(counts.values()) + other
    classes = collections.Counter()
    leaves = collections.Counter()
    for offset, count in counts.items():
        chain = chains.get(offset.lstrip("0") or "0", [("??", "??")])
        function, where = chain[0]
        function = strip_generics(function)
        where = re.sub(r"^.*/(crates|src|registry/src/[^/]+)/", "", where)
        text = function + " " + where
        for name, pattern in CLASSES:
            if re.search(pattern, text):
                classes[name] += count
                break
        else:
            classes["other in the binary"] += count
        leaves[function[:70] + "  " + re.sub(r":\d+.*$", "", where)] += count
    classes["libraries (libc and others)"] = other
    print(f"{total} samples")
    for name, count in classes.most_common():
        print(f"{100 * count / total:6.1f} %  {name}")
    if top:
        print()
        for name, count in leaves.most_common(top):
            print(f"{100 * count / total:6.1f} %  {name}")


if __name__ == "__main__":
    main()
