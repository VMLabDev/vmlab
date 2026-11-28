#!/usr/bin/env python3
"""card-1235: what an instruction set adds to the 3d set, read from
llvm-tblgen's JSON dump of AArch64.td with the selection rule of the
generator (a64gen, G1c of the 3d design): a record is selected when each of
its predicates holds with the selected features on and every other feature
off. The format class of a record is its last superclass.

usage: sets.py aarch64.json                  every feature, alone on top of 3d
       sets.py aarch64.json FEATURE ...      these features together on top of 3d,
                                             then each alone and what only the
                                             combination selects
"""
import collections
import json
import sys

BASE = ["FPARMv8", "NEON", "AES", "SHA2", "CRC"]
COMMON = {"InstructionEncoding", "Instruction", "AArch64Inst", "EncodedI", "I", "Sched", "NormalizedBadReg"}

tables = json.load(open(sys.argv[1]))


def holds(dag, selected, mentioned):
    if dag["kind"] == "def":
        feature = dag["def"].removeprefix("Feature")
        if feature == "All":
            return False
        mentioned.add(feature)
        return feature in selected
    results = [holds(argument[0], selected, mentioned) for argument in dag["args"]]
    return all(results) if dag["operator"]["def"] == "all_of" else any(results)


records = []
for name in tables["!instanceof"]["EncodedI"]:
    record = tables[name]
    if record["isPseudo"] or record["isCodeGenOnly"]:
        continue
    mask = 0
    for position, bit in enumerate(record["Inst"]):
        if isinstance(bit, int) and not record["SoftFail"][position]:
            mask |= 1 << position
    if mask == 0:
        continue
    if record["DecoderNamespace"] not in ("", "Fallback"):
        continue
    dags = [tables[item["def"]]["AssemblerCondDag"] for item in record["Predicates"]]
    form = next((c for c in reversed(record["!superclasses"]) if c not in COMMON), name)
    records.append((name, dags, form))


def select(features):
    chosen = {}
    for name, dags, form in records:
        mentioned = set()
        if all([holds(dag, features, mentioned) for dag in dags]):
            chosen[name] = form
    return chosen


base = select(set(BASE))
base_forms = set(base.values())
print(f"3d set: {len(base)} records in {len(base_forms)} format classes")


def report(label, features, show):
    chosen = select(set(BASE) | set(features))
    added = {name: form for name, form in chosen.items() if name not in base}
    forms = collections.Counter(added.values())
    new = sorted(form for form in forms if form not in base_forms)
    print(f"{label}: +{len(added)} records, {len(forms)} format classes, {len(new)} of them new")
    if show:
        for form, count in sorted(forms.items()):
            mark = "new" if form not in base_forms else "3d "
            names = sorted(name for name, f in added.items() if f == form)
            print(f"    {mark} {form} ({count}): {' '.join(names[:6])}{' ...' if count > 6 else ''}")
    return added


if len(sys.argv) == 2:
    every = set()
    for _, dags, _ in records:
        for dag in dags:
            holds(dag, set(), every)
    for feature in sorted(every - set(BASE)):
        report(feature, [feature], False)
else:
    wanted = sys.argv[2:]
    together = report("together", wanted, False)
    alone = set()
    for feature in wanted:
        alone |= set(report(feature, [feature], True))
    both = {name: form for name, form in together.items() if name not in alone}
    print(f"selected only by a combination: {len(both)}")
    for form, count in sorted(collections.Counter(both.values()).items()):
        names = sorted(name for name, f in both.items() if f == form)
        mark = "new" if form not in base_forms else "3d "
        print(f"    {mark} {form} ({count}): {' '.join(names[:6])}{' ...' if count > 6 else ''}")
