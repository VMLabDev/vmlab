#!/usr/bin/env python3
"""Observe QEMU's pc floppy controller, ISA DMA and PIIX3 UHCI through qtest.

QEMU runs with accel=qtest, so no firmware or guest executes: every register
access below is the script's own, starting from QEMU's power-on state. Each
access and its result is recorded as a step, so a device model can replay the
same sequence and compare every read, guest-memory read and IRQ level. This
is black-box observation only; no QEMU source is read (phase-4 SPEC).
"""

import argparse
import contextlib
import ctypes
import datetime
import hashlib
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import time


def project_vms():
    processes = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            arguments = (entry / "cmdline").read_bytes().split(b"\0")
        except (FileNotFoundError, ProcessLookupError):
            continue
        if not arguments or not arguments[0]:
            continue
        name = os.path.basename(arguments[0])
        if (name.startswith(b"qemu-system-") and b"-avd" not in arguments) or (
            name == b"vmlab-hv" or name.startswith(b"vmlab-hv-test-")
        ):
            processes.append(int(entry.name))
    return processes


def require_scope():
    entries = Path("/proc/self/cgroup").read_text().splitlines()
    relative = next((line.split(":", 2)[2] for line in entries if line.startswith("0::")), None)
    if relative is None:
        raise RuntimeError("the oracle requires cgroup v2 memory limits")
    directory = Path("/sys/fs/cgroup") / relative.lstrip("/")
    memory = (directory / "memory.max").read_text().strip()
    swap = (directory / "memory.swap.max").read_text().strip()
    if memory == "max" or int(memory) > 24 * 1024**3 or swap != "0":
        raise RuntimeError("run under systemd-run --user --scope -q -p MemoryMax=24G -p MemorySwapMax=0")


def connect(path, process, timeout=10):
    deadline = time.monotonic() + timeout
    while True:
        stream = socket.socket(socket.AF_UNIX)
        try:
            stream.connect(str(path))
            return stream
        except (FileNotFoundError, ConnectionRefusedError):
            stream.close()
        if process.poll() is not None:
            raise RuntimeError(f"QEMU exited before binding {path}")
        if time.monotonic() >= deadline:
            raise TimeoutError(f"waiting for {path}")
        time.sleep(0.01)


def floppy_byte(offset):
    """The test image: each sector starts with its LBA, then a pattern."""
    lba, index = divmod(offset, 512)
    if index < 2:
        return (lba >> (8 * index)) & 0xFF
    return (index * 7 + lba * 13) & 0xFF


def floppy_image(sectors=2880):
    return bytes(floppy_byte(offset) for offset in range(sectors * 512))


class Recorder:
    """A qtest connection that records every access as a replayable step."""

    def __init__(self, stream):
        self.stream = stream
        self.stream.settimeout(10)
        self.reader = stream.makefile("rb")
        self.irq = {}
        self.steps = []

    def raw(self, command):
        self.stream.sendall(command.encode("ascii") + b"\n")
        while True:
            line = self.reader.readline()
            if not line:
                raise RuntimeError("qtest connection closed")
            response = line.decode("ascii").strip().split()
            if response and response[0] == "IRQ":
                self.irq[int(response[2])] = response[1] == "raise"
                continue
            if not response or response[0] != "OK":
                raise RuntimeError(f"qtest {command!r}: {line!r}")
            return response[1:]

    def record(self, step):
        step["irq"] = sorted(line for line, high in self.irq.items() if high)
        self.steps.append(step)
        return step

    def note(self, text):
        self.steps.append({"op": "note", "text": text})

    def out(self, port, value, width=1):
        suffix = {1: "b", 2: "w", 4: "l"}[width]
        self.raw(f"out{suffix} {port:#x} {value:#x}")
        self.record({"op": "out", "port": port, "width": width, "value": value})

    def inp(self, port, width=1):
        suffix = {1: "b", 2: "w", 4: "l"}[width]
        value = int(self.raw(f"in{suffix} {port:#x}")[0], 0)
        self.record({"op": "in", "port": port, "width": width, "value": value})
        return value

    def write(self, address, data):
        self.raw(f"write {address:#x} {len(data):#x} 0x{data.hex()}")
        self.record({"op": "write", "address": address, "hex": data.hex()})

    def read(self, address, length):
        data = bytes.fromhex(self.raw(f"read {address:#x} {length:#x}")[0][2:])
        self.record({"op": "read", "address": address, "hex": data.hex()})
        return data

    def step(self, ns):
        now = int(self.raw(f"clock_step {ns}")[0], 0)
        self.record({"op": "step", "ns": ns})
        return now

    # PCI configuration through 0xcf8/0xcfc, recorded as plain port accesses.
    def config_read(self, bdf, offset, width=4):
        self.out(0xCF8, 0x80000000 | bdf << 8 | (offset & ~3), 4)
        return self.inp(0xCFC + (offset & 3), width)

    def config_write(self, bdf, offset, value, width=4):
        self.out(0xCF8, 0x80000000 | bdf << 8 | (offset & ~3), 4)
        self.out(0xCFC + (offset & 3), value, width)


# ---- ISA DMA ---------------------------------------------------------------

def dma2(q, mode, address, count, page=0, unmask=True):
    """Program 8237 channel 2 the way a floppy driver does."""
    q.out(0x0A, 0x06)
    q.out(0x0C, 0)
    q.out(0x0B, mode)
    q.out(0x04, address & 0xFF)
    q.out(0x04, address >> 8)
    q.out(0x81, page)
    q.out(0x0C, 0)
    q.out(0x05, count & 0xFF)
    q.out(0x05, count >> 8)
    if unmask:
        q.out(0x0A, 0x02)


def dma2_state(q):
    q.inp(0x08)
    q.out(0x0C, 0)
    for port in (0x04, 0x04, 0x05, 0x05):
        q.inp(port)


# ---- floppy ----------------------------------------------------------------

def msr(q):
    return q.inp(0x3F4)


def command(q, *data):
    for byte in data:
        msr(q)
        q.out(0x3F5, byte)
    return msr(q)


def results(q, limit=16):
    values = []
    for _ in range(limit):
        status = msr(q)
        if status & 0xC0 != 0xC0:
            break
        values.append(q.inp(0x3F5))
    return values


def sense_interrupt(q):
    command(q, 0x08)
    return results(q)


def settle(q, ns=2_000_000):
    q.step(ns)
    return msr(q)


def observe_floppy(q, writable, medium):
    q.note("power-on registers")
    for port in (0x3F0, 0x3F1, 0x3F2, 0x3F3, 0x3F4, 0x3F7):
        q.inp(port)
    q.note("CMOS floppy type and equipment bytes")
    for index in (0x10, 0x14):
        q.out(0x70, index)
        q.inp(0x71)
    q.note("power-on DMA channel 2")
    dma2_state(q)

    q.note("controller reset through DOR, with an MSR read while in reset")
    q.out(0x3F2, 0x08)
    msr(q)
    q.out(0x3F2, 0x0C)
    settle(q)
    q.inp(0x3F2)
    for _ in range(4):
        sense_interrupt(q)
    sense_interrupt(q)

    q.note("controller reset through DOR, back to back")
    q.out(0x3F2, 0x08)
    q.out(0x3F2, 0x0C)
    settle(q)
    sense_interrupt(q)

    q.note("controller reset through DOR, with time in reset")
    q.out(0x3F2, 0x08)
    q.step(1_000_000)
    q.out(0x3F2, 0x0C)
    settle(q)
    for _ in range(4):
        sense_interrupt(q)
    sense_interrupt(q)

    q.note("controller reset through DSR")
    q.out(0x3F4, 0x80)
    msr(q)
    settle(q)
    for _ in range(4):
        sense_interrupt(q)

    q.note("identification and configuration")
    for opcode in (0x10, 0x18, 0x0E):
        command(q, opcode)
        results(q)
    command(q, 0x13, 0x00, 0x57, 0x00)
    results(q)
    command(q, 0x03, 0xAF, 0x02)
    results(q)
    command(q, 0x0E)
    results(q)
    command(q, 0x94)
    results(q)
    command(q, 0x0E)
    results(q)
    command(q, 0x14)
    results(q)
    command(q, 0x12, 0x80)
    results(q)
    command(q, 0x12, 0x00)
    results(q)
    command(q, 0x1F)
    results(q)
    command(q, 0x04, 0x00)
    results(q)
    command(q, 0x04, 0x01)
    results(q)
    command(q, 0x04, 0x04)
    results(q)
    q.inp(0x3F7)

    q.note("data-rate and tape registers")
    q.out(0x3F7, 0x00)
    q.inp(0x3F7)
    q.out(0x3F4, 0x02)
    msr(q)
    q.out(0x3F3, 0x01)
    q.inp(0x3F3)
    q.out(0x3F3, 0x00)
    command(q, 0x4A, 0x00)
    for ns in (1_000_000, 10_000_000, 100_000_000, 1_000_000_000):
        settle(q, ns)
    results(q)
    q.out(0x3F7, 0x00)

    q.note("motor on, recalibrate and seek")
    q.out(0x3F2, 0x1C)
    command(q, 0x07, 0x00)
    settle(q)
    sense_interrupt(q)
    q.inp(0x3F7)
    command(q, 0x0F, 0x00, 0x05)
    settle(q)
    sense_interrupt(q)
    q.inp(0x3F7)
    command(q, 0x0F, 0x04, 0x02)
    settle(q)
    sense_interrupt(q)
    command(q, 0x4A, 0x00)
    for ns in (1_000_000, 10_000_000, 100_000_000, 1_000_000_000):
        settle(q, ns)
    results(q)
    command(q, 0x04, 0x00)
    results(q)
    command(q, 0x0F, 0x00, 0x00)
    settle(q)
    sense_interrupt(q)

    if not medium:
        q.note("read without a medium")
        dma2(q, 0x46, 0x2000, 511)
        command(q, 0xE6, 0x00, 0, 0, 1, 2, 18, 0x1B, 0xFF)
        settle(q)
        results(q)
        dma2_state(q)
        return

    q.note("DMA read of two sectors")
    q.write(0x2000, bytes([0xAA]) * 1024)
    dma2(q, 0x46, 0x2000, 1023)
    command(q, 0xE6, 0x00, 0, 0, 1, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    dma2_state(q)
    q.read(0x2000, 1024)

    q.note("multi-track read crosses to head 1")
    dma2(q, 0x46, 0x3000, 1023)
    command(q, 0xE6, 0x00, 0, 0, 18, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    dma2_state(q)
    q.read(0x3000, 1024)

    q.note("single-track read stops at EOT with DMA count left")
    dma2(q, 0x46, 0x4000, 2047)
    command(q, 0x66, 0x00, 0, 0, 17, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    dma2_state(q)
    q.read(0x4000, 1024)

    q.note("terminal count ends a read before EOT")
    dma2(q, 0x46, 0x5000, 511)
    command(q, 0xE6, 0x00, 0, 0, 1, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    dma2_state(q)

    q.note("terminal count without autoinit: a second read without unmasking")
    q.out(0x0C, 0)
    q.out(0x04, 0x00)
    q.out(0x04, 0x5A)
    q.out(0x0C, 0)
    q.out(0x05, 0xFF)
    q.out(0x05, 0x01)
    command(q, 0xE6, 0x00, 0, 0, 1, 2, 18, 0x1B, 0xFF)
    settle(q)
    q.inp(0x08)
    msr(q)
    q.out(0x0A, 0x02)
    settle(q)
    results(q)
    dma2_state(q)
    q.read(0x5A00, 512)

    q.note("sector not found")
    dma2(q, 0x46, 0x5000, 511)
    command(q, 0x66, 0x00, 0, 0, 25, 2, 18, 0x1B, 0xFF)
    settle(q, 50_000_000)
    results(q)
    dma2_state(q)

    q.note("read on the wrong cylinder")
    dma2(q, 0x46, 0x5000, 511)
    command(q, 0x66, 0x00, 3, 0, 1, 2, 18, 0x1B, 0xFF)
    settle(q, 50_000_000)
    results(q)

    q.note("autoinit read reloads the channel")
    dma2(q, 0x56, 0x6000, 511)
    command(q, 0x66, 0x00, 0, 0, 2, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    dma2_state(q)
    q.read(0x6000, 512)
    q.write(0x6000, bytes([0x99]) * 512)
    command(q, 0x66, 0x00, 0, 0, 3, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    dma2_state(q)
    q.read(0x6000, 512)
    q.read(0x6200, 512)

    q.note("page register and 64 KiB wrap")
    q.write(0x1FF00, bytes([0x55]) * 0x100)
    q.write(0x10000, bytes([0x55]) * 0x100)
    q.write(0x20000, bytes([0x55]) * 0x100)
    dma2(q, 0x46, 0xFF00, 511, page=1)
    command(q, 0x66, 0x00, 0, 0, 3, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    dma2_state(q)
    q.read(0x1FF00, 0x100)
    q.read(0x10000, 0x100)
    q.read(0x20000, 0x100)

    q.note("address-decrement mode")
    q.write(0x7000, bytes([0x33]) * 1024)
    dma2(q, 0x66, 0x73FF, 511)
    command(q, 0x66, 0x00, 0, 0, 4, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    dma2_state(q)
    q.read(0x7000, 1024)

    q.note("verify-mode DMA moves no data")
    q.write(0x8000, bytes([0x77]) * 512)
    dma2(q, 0x42, 0x8000, 511)
    command(q, 0x66, 0x00, 0, 0, 5, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    dma2_state(q)
    q.read(0x8000, 512)

    q.note("a masked channel holds the transfer until unmasked")
    dma2(q, 0x46, 0x9000, 511, unmask=False)
    command(q, 0x66, 0x00, 0, 0, 6, 2, 18, 0x1B, 0xFF)
    settle(q)
    q.inp(0x08)
    q.out(0x0A, 0x02)
    settle(q)
    results(q)
    dma2_state(q)
    q.read(0x9000, 512)

    q.note("PIO (non-DMA) read")
    command(q, 0x03, 0xAF, 0x03)
    results(q)
    command(q, 0x66, 0x00, 0, 0, 7, 2, 7, 0x1B, 0xFF)
    settle(q)
    data = []
    for index in range(512):
        if index < 4 or index > 508:
            msr(q)
        data.append(q.inp(0x3F5))
    msr(q)
    settle(q)
    results(q)
    command(q, 0x03, 0xAF, 0x02)
    results(q)

    q.note("write two sectors on cylinder 1 and read them back")
    command(q, 0x0F, 0x00, 0x01)
    settle(q)
    sense_interrupt(q)
    payload = bytes((index * 3 + 1) & 0xFF for index in range(1024))
    q.write(0xA000, payload)
    dma2(q, 0x4A, 0xA000, 1023)
    command(q, 0xC5, 0x00, 1, 0, 3, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    dma2_state(q)
    dma2(q, 0x46, 0xB000, 1023)
    command(q, 0xE6, 0x00, 1, 0, 3, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    q.read(0xB000, 1024)

    q.note("format track 2 head 1")
    command(q, 0x0F, 0x04, 0x02)
    settle(q)
    sense_interrupt(q)
    ids = b"".join(bytes([2, 1, sector, 2]) for sector in range(1, 19))
    q.write(0xC000, ids)
    dma2(q, 0x4A, 0xC000, len(ids) - 1)
    command(q, 0x4D, 0x04, 2, 18, 0x54, 0xF6)
    settle(q)
    results(q)
    dma2_state(q)
    dma2(q, 0x46, 0xD000, 511)
    command(q, 0xE6, 0x04, 2, 1, 9, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    q.read(0xD000, 512)

    q.note("result phase after the image end, and seek back")
    command(q, 0x0F, 0x00, 79)
    settle(q)
    sense_interrupt(q)
    dma2(q, 0x46, 0xE000, 511)
    command(q, 0xE6, 0x04, 79, 1, 18, 2, 18, 0x1B, 0xFF)
    settle(q)
    results(q)
    q.read(0xE000, 512)
    command(q, 0x0F, 0x00, 85)
    settle(q)
    sense_interrupt(q)
    command(q, 0x4A, 0x00)
    for ns in (1_000_000, 10_000_000, 100_000_000, 1_000_000_000):
        settle(q, ns)
    results(q)

    q.note("motor off and reset preserve the media")
    q.out(0x3F2, 0x0C)
    q.out(0x3F2, 0x08)
    q.out(0x3F2, 0x0C)
    settle(q)
    for _ in range(4):
        sense_interrupt(q)
    q.inp(0x3F7)


# ---- UHCI ------------------------------------------------------------------

UHCI = 0x0A  # 00:01.2
BASE = 0xC000
FRAMES = 0x100000
QH = 0x101000
TD = 0x101100
BUFFER = 0x102000


def uhci_registers(q):
    q.inp(BASE + 0x00, 2)
    q.inp(BASE + 0x02, 2)
    q.inp(BASE + 0x04, 2)
    q.inp(BASE + 0x06, 2)
    q.inp(BASE + 0x08, 4)
    q.inp(BASE + 0x0C, 1)
    q.inp(BASE + 0x10, 2)
    q.inp(BASE + 0x12, 2)


def td(link, control, token, buffer):
    return b"".join(value.to_bytes(4, "little") for value in (link, control, token, buffer))


def token(pid, address, endpoint, toggle, length):
    return ((length - 1) & 0x7FF) << 21 | toggle << 19 | endpoint << 15 | address << 8 | pid


def control_transfer(q, address, setup, data_in=0, data_out=b"", interval_ns=5_000_000):
    """A SETUP, optional DATA stage and STATUS stage on one queue head."""
    active = 1 << 23 | 3 << 27
    tds = []
    q.write(BUFFER + 0x100, bytes(0x100))
    q.write(BUFFER, bytes(setup))
    tds.append((token(0x2D, address, 0, 0, 8), BUFFER))
    toggle = 1
    if data_in:
        offset = 0
        while offset < data_in:
            size = min(64, data_in - offset)
            tds.append((token(0x69, address, 0, toggle, size), BUFFER + 0x100 + offset))
            toggle ^= 1
            offset += size
        status_pid = 0xE1
    else:
        if data_out:
            q.write(BUFFER + 0x100, data_out)
            tds.append((token(0xE1, address, 0, 1, len(data_out)), BUFFER + 0x100))
        status_pid = 0x69
    tds.append((token(status_pid, address, 0, 1, 0x800), 0))
    image = b""
    for index, (value, buffer) in enumerate(tds):
        last = index == len(tds) - 1
        link = 1 if last else (TD + 0x20 * (index + 1)) | 4
        control = active | (1 << 24 if last else 0)
        image += td(link, control, value, buffer) + bytes(16)
    q.write(TD, image)
    q.write(QH, (1).to_bytes(4, "little") + TD.to_bytes(4, "little"))
    q.step(interval_ns)
    q.read(QH, 8)
    q.read(TD, len(image))
    if data_in:
        q.read(BUFFER + 0x100, data_in)
    q.inp(BASE + 0x02, 2)
    q.out(BASE + 0x02, 0x3F, 2)


def observe_uhci(q):
    q.note("UHCI configuration at reset and its write masks")
    for offset in range(0, 256, 4):
        q.config_read(UHCI, offset)
    for value in (0xFFFFFFFF, 0):
        for offset in range(0, 256, 4):
            if offset in (0x04,):
                continue
            q.config_write(UHCI, offset, value)
        for offset in range(0, 256, 4):
            q.config_read(UHCI, offset)
    q.config_write(UHCI, 0xC0, 0x8F00, 2)
    q.config_read(UHCI, 0xC0)
    q.config_write(UHCI, 0xC0, 0x2000, 2)
    q.config_read(UHCI, 0xC0)

    q.note("route PIRQD to IRQ 11, place BAR4 and enable I/O and bus mastering")
    q.config_write(0x08, 0x60, 0x0B808080)
    q.config_write(UHCI, 0x20, BASE | 1)
    q.config_write(UHCI, 0x04, 0x0005, 2)
    q.config_write(UHCI, 0x3C, 0x0B, 1)
    q.config_read(UHCI, 0x04)
    q.config_read(UHCI, 0x20)
    uhci_registers(q)

    q.note("register access widths")
    q.inp(BASE + 0x00, 1)
    q.inp(BASE + 0x01, 1)
    q.inp(BASE + 0x10, 1)
    q.inp(BASE + 0x11, 1)
    q.inp(BASE + 0x00, 4)
    q.inp(BASE + 0x10, 4)
    q.inp(BASE + 0x14, 2)
    q.inp(BASE + 0x18, 2)
    q.inp(BASE + 0x1E, 2)
    q.out(BASE + 0x0C, 0x55, 1)
    q.inp(BASE + 0x0C, 1)
    q.out(BASE + 0x0C, 0x40, 1)
    q.out(BASE + 0x08, 0x12345FFF, 4)
    q.inp(BASE + 0x08, 4)
    q.out(BASE + 0x06, 0xFFFF, 2)
    q.inp(BASE + 0x06, 2)
    q.out(BASE + 0x06, 0, 2)
    q.out(BASE + 0x04, 0xFFFF, 2)
    q.inp(BASE + 0x04, 2)
    q.out(BASE + 0x04, 0, 2)

    q.note("host-controller reset and global reset")
    q.out(BASE + 0x00, 0x0002, 2)
    q.inp(BASE + 0x00, 2)
    q.step(1_000_000)
    uhci_registers(q)
    q.out(BASE + 0x00, 0x0004, 2)
    q.step(10_000_000)
    q.inp(BASE + 0x00, 2)
    q.out(BASE + 0x00, 0x0000, 2)
    uhci_registers(q)
    q.out(BASE + 0x02, 0x3F, 2)
    q.out(BASE + 0x10, 0x000A, 2)
    uhci_registers(q)

    q.note("port reset and enable")
    q.out(BASE + 0x10, 0x0200, 2)
    q.inp(BASE + 0x10, 2)
    q.step(50_000_000)
    q.out(BASE + 0x10, 0x0000, 2)
    q.inp(BASE + 0x10, 2)
    q.out(BASE + 0x10, 0x0004, 2)
    q.inp(BASE + 0x10, 2)
    q.out(BASE + 0x10, 0x000E, 2)
    q.inp(BASE + 0x10, 2)
    q.out(BASE + 0x12, 0x0004, 2)
    q.inp(BASE + 0x12, 2)
    q.out(BASE + 0x12, 0x0200, 2)
    q.inp(BASE + 0x12, 2)
    q.out(BASE + 0x12, 0x0000, 2)

    q.note("start the schedule with an empty frame list")
    q.write(FRAMES, (QH | 2).to_bytes(4, "little") * 1024)
    q.write(QH, (1).to_bytes(4, "little") + (1).to_bytes(4, "little"))
    q.out(BASE + 0x08, FRAMES, 4)
    q.out(BASE + 0x06, 0, 2)
    q.out(BASE + 0x04, 0x000F, 2)
    q.out(BASE + 0x00, 0x00C1, 2)
    uhci_registers(q)
    q.step(1_000_000)
    q.inp(BASE + 0x06, 2)
    q.step(10_000_000)
    q.inp(BASE + 0x06, 2)
    q.inp(BASE + 0x02, 2)

    q.note("GET_DESCRIPTOR(device) at address 0")
    control_transfer(q, 0, [0x80, 6, 0, 1, 0, 0, 18, 0], data_in=18)
    q.note("short GET_DESCRIPTOR(device, 8)")
    control_transfer(q, 0, [0x80, 6, 0, 1, 0, 0, 8, 0], data_in=8)
    q.note("SET_ADDRESS 5")
    control_transfer(q, 0, [0x00, 5, 5, 0, 0, 0, 0, 0])
    q.note("GET_DESCRIPTOR(configuration) at address 5")
    control_transfer(q, 5, [0x80, 6, 0, 2, 0, 0, 34, 0], data_in=34)
    q.note("string descriptors")
    control_transfer(q, 5, [0x80, 6, 0, 3, 0, 0, 255, 0], data_in=4)
    for index in (1, 3, 10):
        control_transfer(q, 5, [0x80, 6, index, 3, 0x09, 0x04, 255, 0], data_in=64)
    q.note("unsupported descriptor stalls")
    control_transfer(q, 5, [0x80, 6, 0, 6, 0, 0, 10, 0], data_in=10)
    q.note("SET_CONFIGURATION 1, SET_IDLE, report descriptor")
    control_transfer(q, 5, [0x00, 9, 1, 0, 0, 0, 0, 0])
    control_transfer(q, 5, [0x21, 10, 0, 0, 0, 0, 0, 0])
    control_transfer(q, 5, [0x81, 6, 0, 0x22, 0, 0, 74, 0], data_in=74)

    q.note("interrupt IN with no input NAKs and stays active")
    q.write(BUFFER + 0x100, bytes(16))
    q.write(TD, td(1, 1 << 23 | 3 << 27 | 1 << 24, token(0x69, 5, 1, 0, 8), BUFFER + 0x100))
    q.write(QH, (1).to_bytes(4, "little") + TD.to_bytes(4, "little"))
    q.step(5_000_000)
    q.read(TD, 16)
    q.read(QH, 8)
    q.inp(BASE + 0x02, 2)

    q.note("request to an absent address times out")
    q.write(TD, td(1, 1 << 23 | 3 << 27 | 1 << 24, token(0x69, 9, 0, 0, 8), BUFFER + 0x100))
    q.write(QH, (1).to_bytes(4, "little") + TD.to_bytes(4, "little"))
    q.step(5_000_000)
    q.read(TD, 16)
    q.inp(BASE + 0x02, 2)
    q.out(BASE + 0x02, 0x3F, 2)

    q.note("stop the schedule")
    q.out(BASE + 0x00, 0x0080, 2)
    q.step(2_000_000)
    uhci_registers(q)
    q.out(BASE + 0x02, 0x3F, 2)
    q.inp(BASE + 0x02, 2)


def run(args, configuration, observe):
    result = {"configuration": configuration}
    with tempfile.TemporaryDirectory(prefix="pc-fdc-uhci-", dir="/var/tmp") as temporary:
        directory = Path(temporary)
        qtest_path = directory / "qtest.sock"
        argv = [args.qemu, "-machine", "pc,accel=qtest", "-nodefaults", "-display", "none", "-monitor", "none",
                "-qtest", f"unix:{qtest_path},server=on,wait=off", "-qtest-log", "/dev/null"]
        if configuration in ("floppy", "readonly"):
            image = directory / "floppy.img"
            image.write_bytes(floppy_image())
            result["image_sha256"] = hashlib.sha256(image.read_bytes()).hexdigest()
            readonly = ",readonly=on" if configuration == "readonly" else ""
            argv += ["-drive", f"if=floppy,index=0,format=raw,file={image}{readonly}"]
        elif configuration == "empty":
            argv += ["-drive", "if=floppy,index=0"]
        else:
            argv += ["-usb", "-device", "usb-tablet"]
        result["argv"] = [str(part) for part in argv]
        result["project_vm_pids_before_launch"] = project_vms()
        if len(result["project_vm_pids_before_launch"]) >= int(os.environ.get("VMLAB_HV_TEST_VM_LIMIT", "4")):
            raise RuntimeError("host VM capacity is exhausted")
        process = None
        with (directory / "qemu.stderr").open("w+b") as stderr:
            try:
                parent_pid = os.getpid()
                library = ctypes.CDLL(None, use_errno=True)

                def parent_death():
                    if library.prctl(1, signal.SIGKILL, 0, 0, 0) != 0 or os.getppid() != parent_pid:
                        os._exit(127)

                process = subprocess.Popen(argv, stdout=subprocess.DEVNULL, stderr=stderr,
                                           start_new_session=True, preexec_fn=parent_death)
                with connect(qtest_path, process) as stream:
                    recorder = Recorder(stream)
                    recorder.raw("irq_intercept_in ioapic")
                    try:
                        observe(recorder)
                    finally:
                        result["steps"] = recorder.steps
                        if configuration == "floppy":
                            final, original = image.read_bytes(), floppy_image()
                            result["image_changes"] = [
                                {"offset": offset, "hex": final[offset:offset + 512].hex()}
                                for offset in range(0, len(final), 512)
                                if final[offset:offset + 512] != original[offset:offset + 512]
                            ]
            finally:
                if process is not None:
                    if process.poll() is None:
                        os.killpg(process.pid, signal.SIGTERM)
                        try:
                            process.wait(timeout=5)
                        except subprocess.TimeoutExpired:
                            os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)
                length = stderr.seek(0, os.SEEK_END)
                stderr.seek(max(0, length - 8192))
                result["qemu_stderr_tail"] = stderr.read().decode("utf-8", errors="replace")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--qemu", default="qemu-system-x86_64")
    args = parser.parse_args()

    def interrupted(number, _frame):
        raise RuntimeError(f"oracle interrupted by signal {number}")

    signal.signal(signal.SIGTERM, interrupted)
    result = {"captured_utc": datetime.datetime.now(datetime.timezone.utc).isoformat()}
    error = None
    try:
        require_scope()
        result["qemu_version"] = subprocess.check_output([args.qemu, "--version"], text=True).strip()
        result["runs"] = [
            run(args, "floppy", lambda q: observe_floppy(q, True, True)),
            run(args, "readonly", lambda q: observe_floppy(q, False, True)),
            run(args, "empty", lambda q: observe_floppy(q, True, False)),
            run(args, "uhci", observe_uhci),
        ]
    except Exception as exception:
        error = exception
        result["error"] = f"{type(exception).__name__}: {exception}"
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(result, separators=(",", ":")) + "\n")
    print(json.dumps({"output": str(args.output), "error": result.get("error"),
                      "steps": [len(run.get("steps", [])) for run in result.get("runs", [])]}))
    if error is not None:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
