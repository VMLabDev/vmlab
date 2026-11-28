#!/usr/bin/env python3
"""card-1242: read the dumps of rv64trace.so and print what the boots executed.
usage: summary.py <trace.txt>...
Per boot: the instruction count, the share of 16-bit instructions, the
discontinuities, and the count per group. Then the union over the boots: the
mnemonics, the CSRs by number with their access kinds, and the other SYSTEM
instructions."""
import sys

GROUPS = [
    ("A (lr, sc, amo)", lambda m: m.startswith(("lr.", "sc.", "amo"))),
    ("F and D", lambda m: m.startswith("f") and not m.startswith("fence")),
    ("M (mul, div, rem)", lambda m: m.startswith(("mul", "div", "rem"))),
    ("Zba, Zbb, Zbs", lambda m: m.startswith((
        "sh1add", "sh2add", "sh3add", "add.uw", "slli.uw", "andn", "orn", "xnor", "clz", "ctz",
        "cpop", "max", "min", "sext.", "zext.", "rol", "ror", "orc.b", "rev8", "bclr", "bext",
        "binv", "bset"))),
    ("V", lambda m: m.startswith("v")),
]
CSR_OPS = {1: "csrrw", 2: "csrrs", 3: "csrrc", 5: "csrrwi", 6: "csrrsi", 7: "csrrci"}


def read(path):
    counts, system, info = {}, {}, {}
    for line in open(path):
        part = line.split()
        if part[0] == "count":
            if part[1].startswith("sys:"):
                system[int(part[1][4:], 16)] = int(part[2])
            else:
                counts[part[1]] = counts.get(part[1], 0) + int(part[2])
        elif part[0] == "total":
            info["total"], info["len16"] = int(part[1]), int(part[7])
        elif part[0] == "discon":
            info["discon"] = " ".join(part[1:])
        elif part[0] == "enc":
            info["encodings"] = info.get("encodings", 0) + 1
    return counts, system, info


def main():
    names, csrs, others = set(), {}, {}
    for path in sys.argv[1:]:
        counts, system, info = read(path)
        total = info["total"]
        print(f"== {path}")
        print(f"instructions {total:.4e}  16-bit {100 * info['len16'] / total:.1f} %  "
              f"distinct encodings {info.get('encodings', 0)}  mnemonics {len(counts)}")
        print(f"discon {info.get('discon')}")
        for name, test in GROUPS:
            n = sum(c for m, c in counts.items() if test(m))
            print(f"  {name}: {n} ({100 * n / total:.3f} %)")
        n = sum(system.values())
        print(f"  SYSTEM: {n} ({100 * n / total:.3f} %)")
        names |= {m for m, c in counts.items() if c}
        for word, count in system.items():
            if not count:
                continue
            funct3 = (word >> 12) & 7
            if funct3:
                entry = csrs.setdefault(word >> 20, {})
                entry[CSR_OPS.get(funct3, "?")] = entry.get(CSR_OPS.get(funct3, "?"), 0) + count
            else:
                key = (word >> 25, (word >> 20) & 31)
                # sfence.vma (funct7 9) and the like keep rs2 in the key: fold it
                key = key if key[0] in (0, 8, 0x18) else (key[0], None)
                others[key] = others.get(key, 0) + count
    print("== union")
    print(f"mnemonics {len(names)}: {' '.join(sorted(names))}")
    print(f"CSRs {len(csrs)}:")
    for number in sorted(csrs):
        print(f"  {number:#05x} " + " ".join(f"{k}={v}" for k, v in sorted(csrs[number].items())))
    print("other SYSTEM (funct7, rs2 field): " + " ".join(
        f"({k[0]:#x},{k[1]})={v}" for k, v in sorted(others.items(), key=str)))


main()
