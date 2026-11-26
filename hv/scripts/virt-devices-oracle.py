#!/usr/bin/env python3
"""Observe the devices of QEMU's aarch64 `virt` machine through qtest.

The devices are the GICv2, the GICv2m frame, the PL011, the PL031 and the
ACPI generic event device (GED). QEMU runs with accel=qtest, so no firmware
and no guest executes: every register access below is the script's own,
starting from QEMU's power-on state. Each access and its result is recorded
as a step, so a device model can replay the same sequence and compare every
read, every interrupt level and every byte of serial output. This is
black-box observation only (DESIGN.md of phase 3d, S6): no QEMU source is
read. The machine has 64 MiB and runs for a few seconds.
"""

import argparse
import datetime
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import tempfile
import time

GICD = 0x0800_0000
GICC = 0x0801_0000
V2M = 0x0802_0000
PL011 = 0x0900_0000
PL031 = 0x0901_0000
GED = 0x0908_0000

# The GIC of `virt` with one CPU has 288 inputs: 256 SPIs, then the 32
# private interrupts of CPU 0, of which 16 to 31 are the PPIs. Its outputs
# are the lines IRQ (0), FIQ (1), and two that only a CPU with EL2 uses.
GIC_PATH = "/machine/unattached/device[1]"
PPI_BASE = 256


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


class Qmp:
    def __init__(self, stream):
        self.stream = stream
        self.reader = stream.makefile("rb")
        self.events = []
        json.loads(self.reader.readline())
        self.command("qmp_capabilities")

    def command(self, name, **arguments):
        message = {"execute": name}
        if arguments:
            message["arguments"] = arguments
        self.stream.sendall(json.dumps(message).encode() + b"\n")
        while True:
            reply = json.loads(self.reader.readline())
            if "event" in reply:
                self.events.append(reply["event"])
                continue
            if "error" in reply:
                raise RuntimeError(f"QMP {name}: {reply['error']}")
            return reply["return"]

    def take_events(self, wait=0.3):
        """The events that arrive within `wait` seconds, with those kept."""
        deadline = time.monotonic() + wait
        while True:
            left = deadline - time.monotonic()
            if left <= 0:
                break
            ready, _, _ = select.select([self.stream], [], [], left)
            if not ready:
                break
            reply = json.loads(self.reader.readline())
            if "event" in reply:
                self.events.append(reply["event"])
        events, self.events = self.events, []
        return events


class Recorder:
    """A qtest connection that records every access as a replayable step."""

    def __init__(self, stream, serial, qmp):
        self.stream = stream
        self.stream.settimeout(10)
        self.reader = stream.makefile("rb")
        self.serial = serial
        self.qmp = qmp
        self.irq = {}
        self.edges = []
        self.steps = []

    def raw(self, command):
        self.stream.sendall(command.encode("ascii") + b"\n")
        while True:
            line = self.reader.readline()
            if not line:
                raise RuntimeError("qtest connection closed")
            response = line.decode("ascii").strip().split()
            if response and response[0] == "IRQ":
                high = response[1] == "raise"
                self.irq[int(response[2])] = high
                self.edges.append([int(response[2]), high])
                continue
            if not response or response[0] != "OK":
                raise RuntimeError(f"qtest {command!r}: {line!r}")
            return response[1:]

    def record(self, step):
        # A level that an access changes is reported before its answer. A
        # no-op read flushes what a clock step or a QMP command left behind.
        step["irq"] = sorted(line for line, high in self.irq.items() if high)
        if self.edges:
            step["edges"] = self.edges
            self.edges = []
        self.steps.append(step)
        return step

    def note(self, text):
        self.steps.append({"op": "note", "text": text})

    def write(self, address, value, width=4):
        suffix = {1: "b", 2: "w", 4: "l"}[width]
        self.raw(f"write{suffix} {address:#x} {value:#x}")
        step = {"op": "write", "address": address, "width": width, "value": value}
        tx = self.take_serial()
        if tx:
            step["tx"] = tx.hex()
        self.record(step)

    def read(self, address, width=4):
        suffix = {1: "b", 2: "w", 4: "l"}[width]
        value = int(self.raw(f"read{suffix} {address:#x}")[0], 0)
        self.record({"op": "read", "address": address, "width": width, "value": value})
        return value

    def peek(self, address):
        """A read that is not a step: the script waits on it."""
        return int(self.raw(f"readl {address:#x}")[0], 0)

    def line(self, number, high):
        """Drive input `number` of the GIC."""
        self.raw(f"set_irq_in {GIC_PATH} unnamed-gpio-in {number} {int(high)}")
        self.record({"op": "line", "input": number, "high": high})

    def step(self, ns):
        self.raw(f"clock_step {ns}")
        self.record({"op": "step", "ns": ns})

    def take_serial(self, wait=0.05):
        if self.serial is None:
            return b""
        data = b""
        while True:
            ready, _, _ = select.select([self.serial], [], [], wait)
            if not ready:
                return data
            data += self.serial.recv(4096)

    def receive(self, data, settled):
        """Send `data` to the UART and wait until `settled()` holds."""
        self.serial.sendall(data)
        deadline = time.monotonic() + 5
        while not settled():
            if time.monotonic() >= deadline:
                raise TimeoutError("the UART did not take the bytes")
            time.sleep(0.01)
        time.sleep(0.1)
        self.record({"op": "rx", "hex": data.hex()})

    def powerdown(self):
        self.qmp.command("system_powerdown")
        self.peek(GICD)
        self.record({"op": "powerdown"})

    def events(self):
        self.record({"op": "events", "names": self.qmp.take_events()})


# ---- PL011 -----------------------------------------------------------------

def observe_pl011(q):
    reg = lambda offset: PL011 + offset
    registers = (0x04, 0x18, 0x20, 0x24, 0x28, 0x2C, 0x30, 0x34, 0x38, 0x3C, 0x40, 0x48)
    q.note("reset values")
    for offset in registers:
        q.read(reg(offset))
    for offset in range(0xFE0, 0x1000, 4):
        q.read(reg(offset))
    q.read(reg(0x00))
    q.read(reg(0x44))
    q.read(reg(0x4C))
    q.read(reg(0x100))

    q.note("write masks, with the loopback bit of CR left clear")
    for offset, value in ((0x24, 0xFFFFFFFF), (0x28, 0xFFFFFFFF), (0x34, 0xFFFFFFFF),
                          (0x48, 0xFFFFFFFF), (0x2C, 0xFFFFFFFF), (0x30, 0xFF7F),
                          (0x38, 0xFFFFFFFF), (0x04, 0xFFFFFFFF)):
        q.write(reg(offset), value)
        q.read(reg(offset))
    q.read(reg(0x3C))
    q.read(reg(0x40))
    q.write(reg(0x44), 0xFFFFFFFF)
    q.read(reg(0x3C))
    for offset, value in ((0x38, 0), (0x2C, 0), (0x34, 0x12), (0x48, 0), (0x30, 0x300),
                          (0x24, 0), (0x28, 0)):
        q.write(reg(offset), value)

    q.note("transmit with the UART disabled, then enabled")
    q.write(reg(0x00), 0x41)
    q.read(reg(0x3C))
    q.read(reg(0x18))
    q.write(reg(0x30), 0x301)
    q.write(reg(0x44), 0x7FF)
    q.read(reg(0x3C))
    q.write(reg(0x00), 0x42)
    q.read(reg(0x3C))
    q.read(reg(0x18))
    q.write(reg(0x38), 0x20)
    q.read(reg(0x40))
    q.write(reg(0x44), 0x20)
    q.read(reg(0x3C))
    q.read(reg(0x40))
    q.write(reg(0x00), 0x143)
    q.read(reg(0x3C))
    q.write(reg(0x38), 0)
    q.write(reg(0x44), 0x7FF)
    q.write(reg(0x00), 0x44, 1)
    q.read(reg(0x00), 1)
    q.read(reg(0x18), 1)
    q.read(reg(0x19), 1)
    q.read(reg(0x18), 2)

    q.note("receive without the FIFO")
    empty = lambda: q.peek(reg(0x18)) & 0x10 == 0
    q.receive(b"a", empty)
    q.read(reg(0x18))
    q.read(reg(0x3C))
    q.write(reg(0x38), 0x10)
    q.read(reg(0x40))
    q.read(reg(0x00))
    q.read(reg(0x18))
    q.read(reg(0x3C))
    q.read(reg(0x00))
    q.receive(b"bc", empty)
    q.read(reg(0x18))
    q.read(reg(0x00))
    time.sleep(0.2)
    q.read(reg(0x18))
    q.read(reg(0x00))
    time.sleep(0.2)
    q.read(reg(0x18))
    q.read(reg(0x3C))
    q.write(reg(0x38), 0)

    q.note("receive with the FIFO, one byte at a time up to full")
    q.write(reg(0x2C), 0x70)
    q.read(reg(0x18))
    for count in range(1, 36):
        byte = bytes([0x60 + count])
        q.serial.sendall(byte)
        time.sleep(0.12)
        q.record({"op": "rx", "hex": byte.hex()})
        q.read(reg(0x18))
        q.read(reg(0x3C))
    q.write(reg(0x38), 0x50)
    q.read(reg(0x40))
    q.write(reg(0x44), 0x50)
    q.read(reg(0x3C))
    for _ in range(40):
        q.read(reg(0x00))
        time.sleep(0.02)
        q.read(reg(0x18))
        q.read(reg(0x3C))
    q.write(reg(0x38), 0)

    q.note("the interrupt levels of IFLS")
    for level in (0x00, 0x12, 0x2D):
        q.write(reg(0x34), level)
        q.read(reg(0x34))
        q.write(reg(0x44), 0x7FF)
        for count in range(1, 19):
            byte = bytes([0x30 + count])
            q.serial.sendall(byte)
            time.sleep(0.05)
            q.record({"op": "rx", "hex": byte.hex()})
            q.read(reg(0x3C))
        while q.peek(reg(0x18)) & 0x10 == 0:
            q.read(reg(0x00))
        time.sleep(0.2)
        while q.peek(reg(0x18)) & 0x10 == 0:
            q.read(reg(0x00))
        q.read(reg(0x3C))
    q.write(reg(0x34), 0x12)

    q.note("a write of LCR_H with bytes in the FIFO")
    q.serial.sendall(b"xyz")
    time.sleep(0.3)
    q.record({"op": "rx", "hex": b"xyz".hex()})
    q.read(reg(0x18))
    q.read(reg(0x3C))
    q.write(reg(0x2C), 0x70)
    q.read(reg(0x18))
    q.read(reg(0x3C))
    q.read(reg(0x00))
    q.read(reg(0x18))
    q.write(reg(0x2C), 0x60)
    q.read(reg(0x18))
    q.read(reg(0x3C))
    q.read(reg(0x00))
    q.read(reg(0x18))
    q.read(reg(0x3C))
    q.write(reg(0x2C), 0x70)
    q.read(reg(0x18))
    q.read(reg(0x00))
    q.read(reg(0x04))
    q.write(reg(0x20), 0xFFFFFFFF)
    q.read(reg(0x20))

    q.note("the enable bits of CR: UARTEN, TXE and RXE clear")
    q.write(reg(0x44), 0x7FF)
    q.write(reg(0x30), 0)
    q.write(reg(0x00), 0x45)
    q.read(reg(0x3C))
    q.serial.sendall(b"q")
    time.sleep(0.3)
    q.record({"op": "rx", "hex": b"q".hex()})
    q.read(reg(0x18))
    q.read(reg(0x3C))
    q.read(reg(0x00))
    q.read(reg(0x18))
    q.write(reg(0x30), 0x301)
    q.read(reg(0x18))


# ---- PL031 -----------------------------------------------------------------

def observe_pl031(q):
    reg = lambda offset: PL031 + offset
    second = 1_000_000_000
    q.note("reset values: the clock starts at 2020-01-01T00:00:00")
    for offset in (0x00, 0x04, 0x08, 0x0C, 0x10, 0x14, 0x18, 0x1C):
        q.read(reg(offset))
    for offset in range(0xFE0, 0x1000, 4):
        q.read(reg(offset))
    q.read(reg(0x20))
    q.note("the counter")
    q.step(second // 2)
    q.read(reg(0x00))
    q.step(second // 2)
    q.read(reg(0x00))
    q.step(3 * second)
    q.read(reg(0x00))
    q.note("write masks")
    for offset, value in ((0x0C, 0), (0x0C, 0xFFFFFFFF), (0x10, 0xFFFFFFFF), (0x10, 0),
                          (0x04, 0xFFFFFFFF), (0x00, 0x1234)):
        q.write(reg(offset), value)
        q.read(reg(offset))
    q.read(reg(0x14))
    q.note("the alarm")
    now = q.read(reg(0x00))
    q.write(reg(0x04), now + 2)
    q.read(reg(0x04))
    q.write(reg(0x10), 1)
    q.step(second)
    q.read(reg(0x14))
    q.read(reg(0x18))
    q.step(second)
    q.read(reg(0x14))
    q.read(reg(0x18))
    q.read(reg(0x00))
    q.step(second)
    q.read(reg(0x14))
    q.write(reg(0x1C), 0)
    q.read(reg(0x14))
    q.write(reg(0x1C), 1)
    q.read(reg(0x14))
    q.read(reg(0x18))
    q.step(5 * second)
    q.read(reg(0x14))
    q.note("the alarm with the interrupt masked")
    q.write(reg(0x10), 0)
    now = q.read(reg(0x00))
    q.write(reg(0x04), now + 1)
    q.step(second)
    q.read(reg(0x14))
    q.read(reg(0x18))
    q.write(reg(0x10), 1)
    q.read(reg(0x18))
    q.write(reg(0x10), 0)
    q.write(reg(0x1C), 1)
    q.note("an alarm in the past, and an alarm at now")
    now = q.read(reg(0x00))
    q.write(reg(0x04), now - 10)
    q.read(reg(0x14))
    q.step(second)
    q.read(reg(0x14))
    q.write(reg(0x1C), 1)
    now = q.read(reg(0x00))
    q.write(reg(0x04), now)
    q.read(reg(0x14))
    q.step(second)
    q.read(reg(0x14))
    q.write(reg(0x1C), 1)
    q.note("a load of the counter")
    q.write(reg(0x08), 1000)
    q.read(reg(0x00))
    q.read(reg(0x08))
    q.step(3 * second)
    q.read(reg(0x00))
    q.write(reg(0x04), 1005)
    q.write(reg(0x10), 1)
    q.step(second)
    q.read(reg(0x14))
    q.step(second)
    q.read(reg(0x14))
    q.write(reg(0x08), 2000)
    q.read(reg(0x14))
    q.write(reg(0x1C), 1)
    q.write(reg(0x04), 2002)
    q.write(reg(0x08), 3000)
    q.step(4 * second)
    q.read(reg(0x14))
    q.write(reg(0x08), 0xFFFFFFFE)
    q.step(3 * second)
    q.read(reg(0x00))
    q.note("narrow accesses")
    q.read(reg(0x00), 1)
    q.read(reg(0x01), 1)
    q.read(reg(0x00), 2)
    q.write(reg(0x04), 0x55, 1)
    q.read(reg(0x04))


# ---- GED -------------------------------------------------------------------

def observe_ged(q):
    q.note("reset values")
    q.read(GED)
    for offset in (0, 1, 2, 3):
        q.read(GED + offset, 1)
    for offset in (0x200, 0x201, 0x202, 0x203):
        q.read(GED + offset, 1)
    q.note("the power button")
    q.powerdown()
    q.read(GED)
    q.read(GED)
    q.powerdown()
    q.powerdown()
    q.read(GED, 1)
    q.read(GED)
    q.powerdown()
    q.write(GED, 0xFFFFFFFF)
    q.read(GED)
    q.read(GED)
    q.note("nothing answers where a sleep and reset block would be")
    for offset in (0x200, 0x201, 0x202):
        q.write(GED + offset, 0xFF, 1)
        q.read(GED + offset, 1)
    q.events()


# ---- GICv2 -----------------------------------------------------------------

def gic_state(q):
    """What the CPU interface and the distributor show of one moment."""
    q.read(GICC + 0x18)
    q.read(GICC + 0x28)
    q.read(GICC + 0x14)
    for offset in (0x200, 0x204, 0x300, 0x304):
        q.read(GICD + offset)


def observe_gic(q):
    q.note("reset values")
    for offset in (0x000, 0x004, 0x008, 0x080, 0x084, 0x100, 0x104, 0x180, 0x184, 0x200,
                   0x204, 0x280, 0x284, 0x300, 0x304, 0x380, 0x384, 0x400, 0x404, 0x420,
                   0x800, 0x804, 0x808, 0x80C, 0x810, 0x814, 0x818, 0x81C, 0x820, 0x824,
                   0xC00, 0xC04, 0xC08, 0xC0C, 0xD00, 0xE00, 0xF00, 0xF10, 0xF20):
        q.read(GICD + offset)
    for offset in range(0xFD0, 0x1000, 4):
        q.read(GICD + offset)
    for offset in (0x00, 0x04, 0x08, 0x0C, 0x14, 0x18, 0x1C, 0x20, 0x28, 0xD0, 0xE0, 0xFC):
        q.read(GICC + offset)
    q.read(GICD + 0x400, 1)
    q.read(GICD + 0x800, 1)
    q.read(GICD + 0x801, 1)

    q.note("write masks")
    for offset in (0x100, 0x104, 0x124):
        q.write(GICD + offset, 0xFFFFFFFF)
        q.read(GICD + offset)
        q.read(GICD + offset + 0x80)
    for offset in (0x180, 0x184, 0x1A4):
        q.write(GICD + offset, 0xFFFFFFFF)
        q.read(GICD + offset)
    for offset in (0x400, 0x41C, 0x420, 0x800, 0x81C, 0x820, 0xC00, 0xC04, 0xC08, 0x080,
                   0x084, 0x004, 0x008):
        q.write(GICD + offset, 0xFFFFFFFF)
        q.read(GICD + offset)
    for offset in (0x400, 0x41C, 0x420, 0x820, 0xC00, 0xC04, 0xC08, 0x080, 0x084):
        q.write(GICD + offset, 0)
        q.read(GICD + offset)
    q.write(GICD + 0x421, 0x5A, 1)
    q.read(GICD + 0x420)
    q.write(GICD + 0x822, 0xFF, 1)
    q.read(GICD + 0x820)
    q.write(GICD + 0x420, 0)
    q.write(GICD + 0x820, 0)
    q.write(GICD + 0x000, 0xFFFFFFFF)
    q.read(GICD + 0x000)
    q.write(GICD + 0x000, 0)
    for offset, values in ((0x00, (0xFFFFFFFF, 0)), (0x04, (0xFFFFFFFF, 0x5A, 0)),
                           (0x08, (0xFFFFFFFF, 0, 5, 3)), (0x1C, (0xFFFFFFFF, 0, 3)),
                           (0xD0, (0xFFFFFFFF, 0)), (0xE0, (0xFFFFFFFF, 0))):
        for value in values:
            q.write(GICC + offset, value)
            q.read(GICC + offset)

    q.note("a level interrupt, group 0: SPI 5 at priority 0xa0")
    q.write(GICD + 0x000, 1)
    q.write(GICC + 0x00, 1)
    q.write(GICC + 0x04, 0xFF)
    q.write(GICD + 0x424, 0xA0A0A0A0)
    q.line(5, True)
    gic_state(q)
    q.write(GICD + 0x104, 1 << 5)
    gic_state(q)
    q.read(GICC + 0x0C)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.line(5, False)
    gic_state(q)
    q.write(GICC + 0x10, 37)
    gic_state(q)

    q.note("the line stays high over the end of the interrupt")
    q.line(5, True)
    q.read(GICC + 0x0C)
    q.write(GICC + 0x10, 37)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.line(5, False)
    q.write(GICC + 0x10, 37)
    gic_state(q)

    q.note("the line falls before the acknowledge")
    q.line(5, True)
    q.line(5, False)
    gic_state(q)
    q.read(GICC + 0x0C)

    q.note("an edge interrupt: SPI 6")
    q.write(GICD + 0xC08, 2 << 12)
    q.read(GICD + 0xC08)
    q.write(GICD + 0x104, 1 << 6)
    q.line(6, True)
    q.line(6, False)
    gic_state(q)
    q.read(GICC + 0x0C)
    gic_state(q)
    q.line(6, True)
    gic_state(q)
    q.write(GICC + 0x10, 38)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.line(6, False)
    q.line(6, True)
    q.line(6, False)
    q.write(GICC + 0x10, 38)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.write(GICC + 0x10, 38)
    gic_state(q)
    q.line(6, True)
    q.read(GICC + 0x0C)
    q.write(GICC + 0x10, 38)
    gic_state(q)
    q.line(6, False)
    gic_state(q)

    q.note("preemption: SPI 7 at 0x40 over SPI 5 at 0xa0, SPI 8 at 0xa0 does not")
    q.write(GICD + 0x104, 1 << 7 | 1 << 8)
    q.write(GICD + 0x424, 0x40A0A0A0)
    q.write(GICD + 0x428, 0xA0)
    q.line(5, True)
    q.read(GICC + 0x0C)
    q.line(8, True)
    gic_state(q)
    q.line(7, True)
    gic_state(q)
    q.read(GICC + 0x0C)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.line(7, False)
    q.write(GICC + 0x10, 39)
    gic_state(q)
    q.line(5, False)
    q.write(GICC + 0x10, 37)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.line(8, False)
    q.write(GICC + 0x10, 40)
    gic_state(q)

    q.note("the ends of two interrupts out of order")
    q.line(5, True)
    q.read(GICC + 0x0C)
    q.line(7, True)
    q.read(GICC + 0x0C)
    q.line(5, False)
    q.line(7, False)
    q.write(GICC + 0x10, 37)
    gic_state(q)
    q.write(GICC + 0x10, 39)
    gic_state(q)

    q.note("the end of an interrupt that is not active, and with nothing active")
    q.write(GICC + 0x10, 37)
    q.write(GICC + 0x10, 1023)
    gic_state(q)
    q.line(5, True)
    q.read(GICC + 0x0C)
    q.line(5, False)
    q.write(GICC + 0x10, 50)
    gic_state(q)
    q.write(GICC + 0x10, 37)
    gic_state(q)
    q.write(GICD + 0x384, 1 << 5)
    gic_state(q)
    q.line(5, True)
    q.read(GICC + 0x0C)
    q.line(5, False)
    q.write(GICC + 0x10, 1023)
    gic_state(q)
    q.write(GICC + 0x10, 37)
    q.write(GICD + 0x384, 1 << 5)
    gic_state(q)

    q.note("the priority mask")
    q.write(GICC + 0x04, 0xA0)
    q.line(5, True)
    gic_state(q)
    q.write(GICC + 0x04, 0xA8)
    gic_state(q)
    q.write(GICC + 0x04, 0xFF)
    q.read(GICC + 0x0C)
    q.line(5, False)
    q.write(GICC + 0x10, 37)
    gic_state(q)

    q.note("the binary point: SPI 8 at 0x90 over SPI 5 at 0xa0")
    q.write(GICD + 0x428, 0x90)
    q.write(GICC + 0x08, 4)
    q.line(5, True)
    q.read(GICC + 0x0C)
    q.line(8, True)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.write(GICC + 0x08, 3)
    gic_state(q)
    q.read(GICC + 0x0C)
    gic_state(q)
    for offset in range(0xD0, 0xF0, 4):
        q.read(GICC + offset)
    q.line(5, False)
    q.line(8, False)
    q.write(GICC + 0x10, 40)
    gic_state(q)
    q.write(GICC + 0x10, 37)
    gic_state(q)
    q.write(GICC + 0x08, 7)
    q.line(8, True)
    q.read(GICC + 0x0C)
    q.write(GICD + 0x424, 0x00A0A0A0)
    q.line(7, True)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.line(7, False)
    q.line(8, False)
    q.write(GICC + 0x10, 40)
    gic_state(q)
    q.write(GICD + 0x424, 0x40A0A0A0)
    q.write(GICD + 0x428, 0xA0)
    q.write(GICC + 0x08, 0)

    q.note("pending by register")
    q.write(GICD + 0x204, 1 << 5)
    gic_state(q)
    q.write(GICD + 0x284, 1 << 5)
    gic_state(q)
    q.write(GICD + 0x204, 1 << 5)
    q.read(GICC + 0x0C)
    gic_state(q)
    q.write(GICC + 0x10, 37)
    gic_state(q)
    q.line(5, True)
    q.write(GICD + 0x284, 1 << 5)
    gic_state(q)
    q.line(5, False)
    gic_state(q)
    q.write(GICD + 0x204, 1 << 6)
    gic_state(q)
    q.write(GICD + 0x284, 1 << 6)
    gic_state(q)
    q.write(GICD + 0x204, 1 << 6)
    q.read(GICC + 0x0C)
    q.write(GICD + 0x204, 1 << 6)
    gic_state(q)
    q.write(GICC + 0x10, 38)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.write(GICC + 0x10, 38)
    gic_state(q)

    q.note("active by register")
    q.write(GICD + 0x304, 1 << 5)
    gic_state(q)
    q.line(5, True)
    gic_state(q)
    q.line(7, True)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.line(7, False)
    q.write(GICC + 0x10, 39)
    gic_state(q)
    q.write(GICD + 0x384, 1 << 5)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.line(5, False)
    q.write(GICC + 0x10, 37)
    gic_state(q)

    q.note("a disabled interrupt, and the enables of the distributor and the interface")
    q.write(GICD + 0x184, 1 << 5)
    q.line(5, True)
    gic_state(q)
    q.write(GICD + 0x104, 1 << 5)
    gic_state(q)
    q.write(GICD + 0x000, 0)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.write(GICD + 0x000, 1)
    q.write(GICC + 0x00, 0)
    gic_state(q)
    q.read(GICC + 0x0C)
    gic_state(q)
    q.write(GICC + 0x00, 1)
    q.line(5, False)
    q.write(GICC + 0x10, 37)
    q.write(GICD + 0x384, 1 << 5)
    gic_state(q)

    q.note("software generated interrupts")
    q.write(GICD + 0xF00, 0x00010003)
    gic_state(q)
    q.read(GICD + 0xF10)
    q.read(GICD + 0xF20)
    q.read(GICC + 0x0C)
    q.read(GICD + 0xF20)
    gic_state(q)
    q.write(GICC + 0x10, 3)
    gic_state(q)
    q.write(GICD + 0xF00, 0x02000004)
    q.read(GICC + 0x18)
    q.read(GICC + 0x0C)
    q.write(GICC + 0x10, 4)
    q.write(GICD + 0xF00, 0x01000005)
    q.read(GICC + 0x18)
    q.write(GICD + 0xF00, 0x00020006)
    q.read(GICC + 0x18)
    q.write(GICD + 0xF00, 0x00010007)
    q.write(GICD + 0xF00, 0x00010007)
    q.read(GICC + 0x0C)
    q.write(GICC + 0x10, 7)
    q.read(GICC + 0x0C)
    q.write(GICD + 0x180, 0xFFFF)
    q.read(GICD + 0x100)
    q.write(GICD + 0x200, 1 << 2)
    q.read(GICD + 0x200)
    q.write(GICD + 0x400, 0x00100000)
    q.write(GICD + 0xF00, 0x00010002)
    q.write(GICD + 0xF00, 0x00010003)
    q.read(GICD + 0x200)
    q.read(GICC + 0x0C)
    q.read(GICC + 0x0C)
    q.write(GICC + 0x10, 3)
    q.read(GICC + 0x0C)
    q.write(GICC + 0x10, 2)
    q.write(GICD + 0x400, 0)
    gic_state(q)

    q.note("a private interrupt: PPI 27, the virtual timer")
    q.read(GICD + 0xC04)
    q.write(GICD + 0x100, 1 << 27)
    q.write(GICD + 0x418, 0x80000000)
    q.line(PPI_BASE + 27, True)
    gic_state(q)
    q.read(GICC + 0x0C)
    gic_state(q)
    q.line(PPI_BASE + 27, False)
    q.write(GICC + 0x10, 27)
    gic_state(q)
    q.write(GICD + 0x180, 1 << 27)
    q.read(GICD + 0x100)

    q.note("group 1: SPI 9")
    q.write(GICD + 0x084, 1 << 9)
    q.write(GICD + 0x104, 1 << 9)
    q.write(GICD + 0x428, 0xA0A0)
    q.line(9, True)
    gic_state(q)
    q.write(GICD + 0x000, 3)
    gic_state(q)
    q.write(GICC + 0x00, 3)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.read(GICC + 0x20)
    gic_state(q)
    q.write(GICC + 0x24, 41)
    gic_state(q)
    q.write(GICC + 0x00, 2)
    gic_state(q)
    q.note("group 1 with AckCtl")
    q.write(GICC + 0x00, 7)
    gic_state(q)
    q.read(GICC + 0x0C)
    gic_state(q)
    q.line(9, False)
    q.write(GICC + 0x10, 41)
    gic_state(q)
    q.note("FIQEn: group 0 on FIQ, group 1 on IRQ")
    q.write(GICC + 0x00, 0xF)
    q.line(5, True)
    gic_state(q)
    q.line(9, True)
    gic_state(q)
    q.line(5, False)
    gic_state(q)
    q.line(5, True)
    q.read(GICC + 0x0C)
    gic_state(q)
    q.read(GICC + 0x0C)
    gic_state(q)
    q.line(5, False)
    q.line(9, False)
    q.write(GICC + 0x10, 37)
    gic_state(q)
    q.write(GICC + 0x10, 41)
    gic_state(q)
    q.write(GICC + 0x00, 9)
    q.line(5, True)
    gic_state(q)
    q.read(GICC + 0x0C)
    q.line(5, False)
    q.write(GICC + 0x10, 37)
    gic_state(q)
    q.write(GICC + 0x00, 1)
    q.write(GICD + 0x000, 2)
    q.line(9, True)
    q.line(5, True)
    gic_state(q)
    q.line(9, False)
    q.line(5, False)
    q.write(GICD + 0x000, 1)
    q.write(GICD + 0x084, 0)
    gic_state(q)

    q.note("the end of an interrupt in two parts: EOImode and DIR")
    q.write(GICC + 0x00, 0x201)
    q.read(GICC + 0x00)
    q.line(5, True)
    q.read(GICC + 0x0C)
    q.line(5, False)
    q.write(GICC + 0x10, 37)
    gic_state(q)
    q.line(5, True)
    gic_state(q)
    q.line(5, False)
    q.write(GICC + 0x1000, 37)
    gic_state(q)
    q.write(GICC + 0x00, 1)
    q.line(5, True)
    q.read(GICC + 0x0C)
    q.line(5, False)
    q.write(GICC + 0x1000, 37)
    gic_state(q)
    q.write(GICC + 0x10, 37)
    gic_state(q)


# ---- GICv2m ----------------------------------------------------------------

def observe_gicv2m(q):
    pending = (0x208, 0x20C, 0x210)

    def clear():
        for offset in pending:
            q.write(GICD + offset + 0x80, 0xFFFFFFFF)

    q.note("the frame: 64 SPIs from INTID 80")
    for offset in (0x000, 0x008, 0x040, 0x044, 0xFCC):
        q.read(V2M + offset)
    for offset in range(0xFD0, 0x1000, 4):
        q.read(V2M + offset)
    q.write(V2M + 0x008, 0xFFFFFFFF)
    q.read(V2M + 0x008)
    q.write(GICD + 0x000, 1)
    q.write(GICC + 0x00, 1)
    q.write(GICC + 0x04, 0xFF)
    for offset in (0xC14, 0xC18, 0xC1C, 0xC20):
        q.read(GICD + offset)
    q.note("a message to a level interrupt")
    q.write(V2M + 0x040, 80)
    q.read(GICD + 0x208)
    q.note("a message to an edge interrupt: below the range, the range, above")
    for offset in (0xC10, 0xC14, 0xC18, 0xC1C, 0xC20, 0xC24):
        q.write(GICD + offset, 0xAAAAAAAA)
    for intid in (79, 80, 81, 143, 144, 0, 1023, 80 | 0x10000, 80 | 0x400):
        q.write(V2M + 0x040, intid)
        for offset in pending:
            q.read(GICD + offset)
        clear()
    q.write(V2M + 0x040, 90, 2)
    q.read(GICD + 0x208)
    clear()
    q.write(V2M + 0x040, 91, 1)
    q.read(GICD + 0x208)
    clear()
    q.write(V2M + 0x044, 92)
    q.write(V2M + 0x000, 92)
    q.read(GICD + 0x208)
    clear()
    q.note("delivery")
    q.write(GICD + 0x10C, 1 << (115 - 96))
    q.write(V2M + 0x040, 115)
    q.read(GICC + 0x18)
    q.read(GICC + 0x0C)
    q.read(GICD + 0x20C)
    q.write(V2M + 0x040, 115)
    q.read(GICD + 0x20C)
    q.write(GICC + 0x10, 115)
    q.read(GICC + 0x0C)
    q.write(GICC + 0x10, 115)
    q.read(GICC + 0x18)
    q.read(GICD + 0x30C)


RUNS = (
    ("pl011", observe_pl011, "in"),
    ("pl031", observe_pl031, "in"),
    ("ged", observe_ged, "in"),
    ("gic", observe_gic, "out"),
    ("gicv2m", observe_gicv2m, "out"),
)


def run(args, name, observe, intercept):
    with tempfile.TemporaryDirectory(prefix="virt-oracle-") as directory:
        directory = Path(directory)
        flash = directory / "flash"
        with open(flash, "wb") as file:
            file.truncate(64 << 20)
        argv = [
            args.qemu, "-machine", "virt,accel=qtest", "-cpu", "cortex-a53", "-smp", "1",
            "-m", "64", "-display", "none", "-nodefaults", "-no-shutdown",
            # The GED is there when the machine has firmware.
            "-drive", f"if=pflash,format=raw,file={flash},readonly=on",
            "-rtc", "base=2020-01-01T00:00:00,clock=vm",
            "-qtest", f"unix:{directory / 'qtest'},server=on,wait=off",
            "-qtest-log", "/dev/null",
            "-qmp", f"unix:{directory / 'qmp'},server=on,wait=off",
            "-chardev", f"socket,id=serial,path={directory / 'serial'},server=on,wait=off",
            "-serial", "chardev:serial",
        ]
        process = subprocess.Popen(argv, stderr=subprocess.PIPE)
        try:
            qmp = Qmp(connect(directory / "qmp", process))
            serial = connect(directory / "serial", process)
            recorder = Recorder(connect(directory / "qtest", process), serial, qmp)
            if intercept == "in":
                recorder.raw(f"irq_intercept_in {GIC_PATH}")
            else:
                recorder.raw(f"irq_intercept_out {GIC_PATH} sysbus-irq")
            tree = qmp.command("human-monitor-command", **{"command-line": "info mtree -f"})
            regions = [line.strip() for line in tree.splitlines()
                       if any(key in line for key in ("gic", "pl011", "pl031", "ged"))]
            result = {"configuration": name, "intercept": intercept, "regions": regions,
                      "argv": [str(part).replace(str(directory), "$DIR") for part in argv[1:]],
                      "steps": recorder.steps}
            try:
                observe(recorder)
            except Exception as exception:
                # Keep the steps up to the failure: they say where it was.
                result["error"] = f"{type(exception).__name__}: {exception}"
            return result
        finally:
            process.kill()
            stderr = process.communicate()[1].decode(errors="replace").strip()
            if stderr:
                print(f"{name}: QEMU said: {stderr}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--qemu", default="qemu-system-aarch64")
    parser.add_argument("only", nargs="*", help="the runs to make (default: all)")
    args = parser.parse_args()

    def interrupted(number, _frame):
        raise RuntimeError(f"oracle interrupted by signal {number}")

    signal.signal(signal.SIGTERM, interrupted)
    result = {"captured_utc": datetime.datetime.now(datetime.timezone.utc).isoformat()}
    error = None
    try:
        result["qemu_version"] = subprocess.check_output(
            [args.qemu, "--version"], text=True).strip()
        result["runs"] = [run(args, name, observe, intercept)
                          for name, observe, intercept in RUNS
                          if not args.only or name in args.only]
        failed = [run["configuration"] for run in result["runs"] if "error" in run]
        if failed:
            raise RuntimeError(f"the runs {failed} did not finish")
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
