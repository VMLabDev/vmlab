#!/usr/bin/env python3
"""card-1032: what QEMU's gdbstub gives a lockstep harness, and how fast.
usage: gdbstep.py <unix-socket> <steps>"""
import socket, sys, time, re
s = socket.socket(socket.AF_UNIX); s.connect(sys.argv[1]); s.settimeout(30)
buf = b""
def send(p):
    d = p.encode() if isinstance(p, str) else p
    s.sendall(b"$" + d + b"#" + (b"%02x" % (sum(d) & 255)))
def recv():
    global buf
    while True:
        m = re.search(rb"\$([^#]*)#[0-9a-fA-F]{2}", buf, re.S)
        if m:
            buf = buf[m.end():]
            s.sendall(b"+")
            return m.group(1)
        buf += s.recv(65536)
def cmd(p):
    send(p); return recv()
print("qSupported:", cmd("qSupported:multiprocess+;xmlRegisters=aarch64;qXfer:features:read+").decode()[:400])
def xfer(annex):
    out = b""; off = 0
    while True:
        r = cmd(f"qXfer:features:read:{annex}:{off:x},ffb")
        out += r[1:]; off += len(r) - 1
        if r[:1] == b"l": return out.decode(errors="replace")
t = xfer("target.xml")
incs = re.findall(r'href="([^"]+)"', t)
print("target.xml includes:", incs)
nreg = 0
for inc in incs:
    x = xfer(inc)
    names = re.findall(r'<reg name="([^"]+)"', x)
    nreg += len(names)
    print(f"  {inc}: {len(names)} registers; first: {names[:6]}")
    if "sys" in inc or "system" in inc:
        sysnames = names; sysxml = x
print("total registers:", nreg)
print("qqemu.sstepbits:", cmd("qqemu.sstepbits").decode(), "| qqemu.sstep:", cmd("qqemu.sstep").decode())
print("stop reply:", cmd("?").decode())
g = cmd("g"); print("g packet bytes:", len(g) // 2)
pc = lambda g: int.from_bytes(bytes.fromhex(g[32*16:33*16].decode()), "little")
print("pc at reset:", hex(pc(g)), "cpsr:", hex(int.from_bytes(bytes.fromhex(g[33*16:33*16+8].decode()), "little")))
# write x5 then read back; write a system register (first one listed) and see the answer
print("P x5:", cmd("P5=" + (0x1122334455667788).to_bytes(8, "little").hex()).decode(), "readback:", cmd("p5").decode())
try:
    base = nreg - len(sysnames)
    i = sysnames.index("SCTLR_EL1") if "SCTLR_EL1" in sysnames else 0
    # regnum of a system register = position in the concatenated feature list
    before = cmd(f"p{base+i:x}").decode()
    w = cmd(f"P{base+i:x}=" + (0x30d00800).to_bytes(8, "little").hex()).decode()
    after = cmd(f"p{base+i:x}").decode()
    print(f"system register {sysnames[i]} (regnum {base+i}): read {before}, write answers '{w}', read again {after}")
    print("system registers exposed include:", [n for n in sysnames if n in ("SCTLR_EL1","TTBR0_EL1","TCR_EL1","VBAR_EL1","ESR_EL1","FAR_EL1","ELR_EL1","SPSR_EL1","CNTVCT_EL0","CurrentEL","DAIF","NZCV","SP_EL0","SP_EL1")])
except Exception as e:
    print("sysreg probe failed:", e)
# memory write + read
print("M write:", cmd("M40000000,4:d503201f").decode(), "m read:", cmd("m40000000,4").decode())
n = int(sys.argv[2])
t0 = time.monotonic()
for _ in range(n):
    r = cmd("s")
t1 = time.monotonic()
print(f"single step only: {n} steps {t1-t0:.2f}s = {n/(t1-t0):.0f} steps/s; last stop reply {r.decode()[:40]}")
t0 = time.monotonic()
for _ in range(n):
    cmd("s"); g = cmd("g")
t1 = time.monotonic()
print(f"step + g (all core registers): {n} steps {t1-t0:.2f}s = {n/(t1-t0):.0f} steps/s; pc now {hex(pc(g))}")
send("k")
