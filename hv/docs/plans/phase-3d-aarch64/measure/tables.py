#!/usr/bin/env python3
"""card-1032: read llvm-tblgen's JSON dump of AArch64.td, build a mask/value
decoder from it, report its shape, and decode the encodings a boot executed.
usage: tables.py aarch64.json [trace.txt ...]"""
import json, re, sys, collections
j = json.load(open(sys.argv[1]))
io = j['!instanceof']

def feats(pred):
    r = j[pred]
    d = r.get('AssemblerCondDag')
    if not d:
        return None
    return tuple(sorted(set(re.findall(r'Feature(\w+)', d['printable'])) - {'All'})), ('any_of' in d['printable'].split('FeatureAll')[-1])

recs = []
skipped = collections.Counter()
for name in io['EncodedI']:
    r = j[name]
    if r['isPseudo'] or r['isCodeGenOnly']:
        skipped['pseudo/codegen-only'] += 1
        continue
    mask = value = 0
    fields = collections.OrderedDict()
    for bit, b in enumerate(r['Inst']):
        if isinstance(b, int):
            mask |= 1 << bit
            value |= b << bit
        elif isinstance(b, dict) and b.get('kind') == 'varbit':
            fields.setdefault(b['var'], []).append((bit, b['index']))
        elif isinstance(b, dict) and b.get('kind') == 'var':
            fields.setdefault(b['var'], []).append((bit, 0))
        else:
            skipped['odd bit'] += 1
    if mask == 0:
        skipped['no fixed bits'] += 1
        continue
    f = []
    anyof = False
    for p in r['Predicates']:
        x = feats(p['def'])
        if x:
            f.append('|'.join(x[0]) if len(x[0]) > 1 else (x[0][0] if x[0] else p['def']))
    src = r['!locs'][0].split(':')[0] if r.get('!locs') else '?'
    recs.append(dict(name=name, mask=mask, value=value, fields=fields, feats=tuple(sorted(f)),
                     ns=r['DecoderNamespace'], dm=r['DecoderMethod'], src=src,
                     asm=r['AsmString'].split('\t')[0], pc=bin(mask).count('1')))
print(f"encoded instruction records: {len(recs)} (skipped {dict(skipped)})")
print("by DecoderNamespace:", dict(collections.Counter(r['ns'] or '(default)' for r in recs)))
bysrc = collections.Counter(r['src'] for r in recs)
print("by .td file:", dict(bysrc.most_common()))
nof = [r for r in recs if not r['feats']]
print("records with no feature predicate:", len(nof))
fc = collections.Counter()
for r in recs:
    for f in r['feats']:
        fc[f] += 1
print("records per feature predicate:", dict(fc.most_common(60)))
dms = collections.Counter(r['dm'] for r in recs if r['dm'])
print(f"records with a C++ DecoderMethod: {sum(dms.values())}, distinct methods: {len(dms)}")
print("  top:", dict(dms.most_common(12)))
ops = collections.Counter()
for name in io['EncodedI']:
    r = j[name]
    if r['isPseudo'] or r['isCodeGenOnly']:
        continue
    for lst in ('InOperandList', 'OutOperandList'):
        for a in r[lst]['args']:
            if a[0] and a[0].get('kind') == 'def':
                ops[a[0]['def']] += 1
print("distinct operand classes:", len(ops))
opdm = {o: j[o].get('DecoderMethod') for o in ops if isinstance(j.get(o), dict)}
print("operand classes with their own DecoderMethod:", sum(1 for v in opdm.values() if v), "distinct methods:", len(set(v for v in opdm.values() if v)))
fieldnames = collections.Counter()
for r in recs:
    for f in r['fields']:
        fieldnames[f] += 1
print("distinct encoding field names:", len(fieldnames))

# decoder: bucket by the fixed bits every record shares? index by bits 28:24
idx = collections.defaultdict(list)
for r in recs:
    for top in range(32):
        e = top << 24
        if (e & r['mask'] & 0x1f000000) == (r['value'] & 0x1f000000):
            idx[top].append(r)
def decode(e, ns_ok=lambda ns: True):
    c = [r for r in idx[(e >> 24) & 0x1f] if (e & r['mask']) == r['value'] and ns_ok(r['ns'])]
    c.sort(key=lambda r: -r['pc'])
    return c

# overlaps between records of the default namespace
def overlap(a, b):
    m = a['mask'] & b['mask']
    return (a['value'] & m) == (b['value'] & m)
base = [r for r in recs if r['ns'] in ('', 'Fallback')]
amb = strict = 0
for top in range(32):
    rs = [r for r in idx[top] if r['ns'] in ('', 'Fallback')]
    for i, a in enumerate(rs):
        for b in rs[i + 1:]:
            if overlap(a, b):
                if (a['mask'] | b['mask']) == a['mask'] or (a['mask'] | b['mask']) == b['mask']:
                    strict += 1   # one is a refinement of the other
                else:
                    amb += 1
print(f"overlapping pairs in the default namespace (counted per top-5-bit bucket): refinement {strict}, crossing {amb}")

sysreg = {}
for n in io['SysReg']:
    r = j[n]
    enc = 0
    for i, b in enumerate(r['Encoding']):
        enc |= b << i
    sysreg.setdefault(enc, []).append((r['Name'], r.get('Requires', '').strip(' {}')))
print("SysReg records:", len(io['SysReg']), "distinct encodings:", len(sysreg))
for cls in ('SysAlias', 'AT', 'DC', 'IC', 'TLBI', 'PState', 'PStateImm0_15', 'BTI', 'PRFM', 'SVCR'):
    if cls in io:
        print(f"  {cls}: {len(io[cls])}", end='')
print()

for path in sys.argv[2:]:
    print("=" * 20, path)
    counts = {}
    encs = []
    for line in open(path):
        p = line.split()
        if p[0] == 'count':
            counts[p[1]] = int(p[2])
        elif p[0] == 'enc':
            encs.append(int(p[1], 16))
        else:
            print(line.strip())
    total = sum(counts.values())
    nonsys = sorted(((c, k) for k, c in counts.items() if not k.startswith('sys:')), reverse=True)
    print("distinct encodings translated:", len(encs), "distinct mnemonics:", len(nonsys))
    print("top mnemonics by executed count (%):", ' '.join(f"{k}={100*c/total:.2f}" for c, k in nonsys[:45]))
    print("all mnemonics:", ' '.join(sorted(k for c, k in nonsys if c)))
    used = collections.Counter(); usedfeat = collections.Counter(); undec = []; tie = 0
    featrec = collections.defaultdict(set)
    for e in encs:
        c = decode(e, lambda ns: ns in ('', 'Fallback'))
        if not c:
            c = decode(e)
        if not c:
            undec.append(e); continue
        if len(c) > 1 and c[0]['pc'] == c[1]['pc']:
            tie += 1
        r = c[0]
        used[r['name']] += 1
        for f in r['feats'] or ('(none)',):
            usedfeat[f] += 1; featrec[f].add(r['name'])
    print(f"LLVM records hit: {len(used)}; undecodable encodings: {len(undec)} {[hex(x) for x in undec[:12]]}; equal-specificity ties: {tie}")
    print("distinct encodings per feature predicate:", dict(usedfeat.most_common()))
    for f, s in sorted(featrec.items()):
        if f not in ('(none)', 'NEON', 'FPARMv8', 'FPARMv8|NEON'):
            print(f"  {f}: {sorted(s)[:40]}")
    bysrc2 = collections.Counter(next(r['src'] for r in recs if r['name'] == n) for n in used)
    print("records hit by .td file:", dict(bysrc2))
    # system instructions
    sysl = sorted(((c, int(k[4:], 16)) for k, c in counts.items() if k.startswith('sys:')), reverse=True)
    names = []
    for c, e in sysl:
        L = (e >> 21) & 1; op0 = (e >> 19) & 3; op1 = (e >> 16) & 7; crn = (e >> 12) & 15; crm = (e >> 8) & 15; op2 = (e >> 5) & 7
        if op0 >= 2:
            key = (op0 << 14) | (op1 << 11) | (crn << 7) | (crm << 3) | op2
            nm = '/'.join(n for n, _ in sysreg.get(key, [])) or f"S{op0}_{op1}_C{crn}_C{crm}_{op2}"
            names.append((('mrs ' if L else 'msr ') + nm, c))
        else:
            d = decode(e)
            names.append(((d[0]['name'] if d else '?') + f"[op1={op1},CRn={crn},CRm={crm},op2={op2}]", c))
    print(f"system-class encodings executed: {len(names)}, {100*sum(c for _, c in names)/total:.3f}% of instructions")
    regs = sorted(set(n.split(' ', 1)[1] for n, c in names if n[:4] in ('mrs ', 'msr ') and c))
    print("system registers touched:", len(regs)); print("  " + ' '.join(regs))
    print("other system instructions:", ' '.join(f"{n}={c}" for n, c in names if n[:4] not in ('mrs ', 'msr ') and c))
