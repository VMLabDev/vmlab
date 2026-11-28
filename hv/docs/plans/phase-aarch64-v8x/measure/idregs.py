#!/usr/bin/env python3
"""card-1235: the ID registers that a QEMU CPU model presents, read over the
gdbstub of a stopped machine (S6 of the 3d design: QEMU as a running program).

usage: idregs.py <cpu> [machine options, default "virt"] [vCPUs] [name pattern]
example: idregs.py max
         idregs.py max,sve=off,sme=off virt,gic-version=3 16
         idregs.py neoverse-n1 virt 1 'ISAR|PFR'
"""
import os
import re
import socket
import subprocess
import sys
import tempfile
import time

cpu = sys.argv[1]
machine = sys.argv[2] if len(sys.argv) > 2 else "virt"
smp = sys.argv[3] if len(sys.argv) > 3 else "1"
pattern = sys.argv[4] if len(sys.argv) > 4 else r"ID_AA64|MIDR|CTR_EL0|DCZID|GMID"
directory = tempfile.mkdtemp(prefix="idregs.")
path = os.path.join(directory, "gdb.sock")
qemu = subprocess.Popen(
    ["qemu-system-aarch64", "-machine", machine, "-cpu", cpu, "-accel", "tcg", "-smp", smp,
     "-m", "256", "-display", "none", "-S", "-gdb", f"unix:{path},server=on,wait=off"],
    stderr=subprocess.PIPE)
try:
    for _ in range(100):
        if os.path.exists(path) or qemu.poll() is not None:
            break
        time.sleep(0.05)
    if qemu.poll() is not None:
        sys.exit("qemu: " + qemu.stderr.read().decode().strip())
    sock = socket.socket(socket.AF_UNIX)
    sock.connect(path)

    def command(text):
        body = text.encode()
        sock.sendall(b"$" + body + b"#" + f"{sum(body) & 0xff:02x}".encode())
        data = b""
        while True:
            data += sock.recv(65536)
            start = data.find(b"$")
            end = data.find(b"#", start)
            if start >= 0 and end >= 0 and len(data) >= end + 3:
                sock.sendall(b"+")
                return data[start + 1:end].decode()

    command("qSupported:xmlRegisters=aarch64")

    def document(name):
        text, offset = "", 0
        while True:
            answer = command(f"qXfer:features:read:{name}:{offset:x},ffb")
            text += answer[1:]
            offset += len(answer) - 1
            if answer[0] == "l":
                return text

    target = document("target.xml")
    number, found = 0, {}
    for include in re.findall(r'<xi:include href="([^"]+)"', target):
        for reg in re.finditer(r"<reg ([^>]*)>", document(include)):
            attributes = dict(re.findall(r'(\w+)="([^"]*)"', reg.group(1)))
            if "regnum" in attributes:
                number = int(attributes["regnum"])
            found[attributes["name"]] = (number, int(attributes["bitsize"]))
            number += 1
    wanted = sorted(name for name in found
                    if re.search(pattern, name))
    print(f"qemu -machine {machine} -cpu {cpu} -smp {smp}: {len(found)} registers")
    for name in wanted:
        index, bits = found[name]
        raw = command(f"p{index:x}")
        if raw.startswith("E") or len(raw) != bits // 4:
            print(f"{name} unreadable ({raw})")
            continue
        value = int.from_bytes(bytes.fromhex(raw), "little")
        print(f"{name} {value:#018x}")
    icc = sorted(name for name in found if name.startswith(("ICC_", "ICH_", "ICV_")))
    print(f"GIC system registers: {len(icc)}: {' '.join(icc)}")
finally:
    qemu.kill()
    qemu.wait()
    if os.path.exists(path):
        os.unlink(path)
    os.rmdir(directory)
