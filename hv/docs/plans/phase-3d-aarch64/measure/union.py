#!/usr/bin/env python3
"""card-1032: the union of what the four cortex-a53 boots executed, by group.
usage: union.py aarch64.json trace.txt..."""
import json, sys, collections, re
j = json.load(open(sys.argv[1])); io = j['!instanceof']
recs = []
for name in io['EncodedI']:
    r = j[name]
    if r['isPseudo'] or r['isCodeGenOnly']: continue
    mask = value = 0
    for bit, b in enumerate(r['Inst']):
        if isinstance(b, int): mask |= 1 << bit; value |= b << bit
    if not mask: continue
    feats = []
    for p in r['Predicates']:
        d = j[p['def']].get('AssemblerCondDag')
        if d: feats += sorted(set(re.findall(r'Feature(\w+)', d['printable'])) - {'All'})
    sc = [s for s in r['!superclasses'] if s not in ('InstructionEncoding','Instruction','AArch64Inst','EncodedI','I','Sched','NormalizedBadReg')]
    recs.append(dict(name=name, mask=mask, value=value, pc=bin(mask).count('1'), ns=r['DecoderNamespace'], feats=tuple(feats), fmt=sc[-1] if sc else name, asm=r['AsmString'].split('\t')[0].split(' ')[0].split('{')[0]))
idx = collections.defaultdict(list)
for r in recs:
    for top in range(32):
        if ((top << 24) & r['mask'] & 0x1f000000) == (r['value'] & 0x1f000000): idx[top].append(r)
def decode(e):
    c = [r for r in idx[(e >> 24) & 0x1f] if (e & r['mask']) == r['value']]
    c.sort(key=lambda r: (r['ns'] != '', -r['pc']))
    return c[0] if c else None
def group(e):
    op0 = (e >> 25) & 0xf
    if op0 in (8, 9): return 'data processing, immediate'
    if op0 in (10, 11): return 'branches, exceptions, system'
    if op0 in (4, 6, 12, 14): return 'loads and stores'
    if op0 in (5, 13): return 'data processing, register'
    if op0 in (7, 15): return 'FP and AdvSIMD'
    return 'reserved/other'
# mnemonic -> groups, from the LLVM records
mg = collections.defaultdict(set)
for r in recs: mg[r['asm']].add(group(r['value']))
union = {}; tot = 0; fp_exec = 0; perfile = {}
gencs = collections.defaultdict(set)
for path in sys.argv[2:]:
    t = 0; f = 0; hit = set()
    for line in open(path):
        p = line.split()
        if p[0] == 'count':
            c = int(p[2]); t += c
            m = p[1].split('.')[0]
            if mg.get(m) == {'FP and AdvSIMD'}: f += c
        elif p[0] == 'enc':
            e = int(p[1], 16); r = decode(e)
            if r: union[r['name']] = r; hit.add(r['name']); gencs[group(e)].add(e)
    perfile[path.split('/')[-2]] = (t, f, len(hit))
    print(f"{path.split('/')[-2]}: instructions {t/1e9:.2f}e9, executed by mnemonics that are FP/AdvSIMD only {100*f/t:.2f}%, LLVM records {len(hit)}")
print("union of LLVM records:", len(union), "; distinct format classes:", len(set(r['fmt'] for r in union.values())))
g = collections.defaultdict(lambda: [set(), set(), collections.Counter()])
for r in union.values():
    k = group(r['value']); g[k][0].add(r['name']); g[k][1].add(r['fmt'])
    for f in r['feats'] or ('(base)',): g[k][2][f] += 1
allg = collections.Counter(); allf = collections.defaultdict(set)
for r in recs:
    if not r['feats'] or set(r['feats']) <= {'FPARMv8', 'NEON', 'AES', 'SHA2', 'CRC'}:
        allg[group(r['value'])] += 1; allf[group(r['value'])].add(r['fmt'])
for k, (names, fmts, fc) in sorted(g.items()):
    print(f"  {k}: executed records {len(names)}, format classes {len(fmts)}, by feature {dict(fc)}; in the tables for base+FP+NEON+AES+SHA2+CRC: records {allg[k]}, format classes {len(allf[k])}; distinct encodings {len(gencs[k])}")
print("records in the tables per feature (v8.x candidates):", {f: sum(1 for r in recs if f in r['feats']) for f in ('LSE','PAuth','RDM','FullFP16','DotProd','RCPC','JS','ComplxNum','FlagM','SHA3','SM4','MTE','BTI','RandGen','SB','CSSC','MOPS','LSE2','FRInt3264','AltNZCV','PredRes','CacheDeepPersist','SEL2','PAN','LOR','VH','RAS','TLB_RMI','FineGrainedTraps')})
print("branches/system records:", sorted(n for n, r in union.items() if group(r['value']) == 'branches, exceptions, system'))
