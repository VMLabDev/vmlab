#!/usr/bin/env python3
"""Read a flattened device tree and list what a machine model must answer.

Usage:
  dtnodes.py nodes  FILE.dtb            every enabled node with a compatible:
                                        path, compatible, CPU address, interrupts
  dtnodes.py needs  FILE.dtb PATH...    the nodes that the named nodes need,
                                        through clocks, resets, power domains,
                                        regulators, pin control, PHYs and
                                        interrupt parents (the closure)
  dtnodes.py counts FILE.dtb            the number of nodes, enabled nodes and
                                        distinct compatibles

Only the Python standard library. The file is data: nothing in it is run.
The device tree blobs of a Raspberry Pi are in the `boot` directory of
github.com/raspberrypi/firmware. DESIGN.md names the commit that was read.
"""

import struct
import sys

BEGIN_NODE, END_NODE, PROP, NOP, END = 1, 2, 3, 4, 9

# Properties that name the nodes which the owner needs. Each entry of such a
# property is a phandle, then as many cells as the named node asks for in the
# property given here (None: no cells follow).
NEEDS = {
    "clocks": "#clock-cells",
    "resets": "#reset-cells",
    "power-domains": "#power-domain-cells",
    "phys": "#phy-cells",
    "dmas": "#dma-cells",
    "mboxes": "#mbox-cells",
    "interrupts-extended": "#interrupt-cells",
    "nvmem-cells": "#nvmem-cell-cells",
    "phy-handle": None,
    "interrupt-parent": None,
    "msi-parent": None,
    "pinctrl-0": None,
    "vmmc-supply": None,
    "vqmmc-supply": None,
    "vbus-supply": None,
    "firmware": None,
    "syscon": None,
}


class Node:
    def __init__(self, name, parent):
        self.name = name
        self.parent = parent
        self.props = {}
        self.children = []

    @property
    def path(self):
        if self.parent is None:
            return "/"
        head = self.parent.path
        return head + ("" if head.endswith("/") else "/") + self.name

    def cells(self, name):
        raw = self.props.get(name, b"")
        return list(struct.unpack(">%dI" % (len(raw) // 4), raw[: len(raw) // 4 * 4]))

    def strings(self, name):
        raw = self.props.get(name, b"")
        return [s.decode("ascii", "replace") for s in raw.split(b"\0") if s]

    def enabled(self):
        node = self
        while node is not None:
            status = node.strings("status")
            if status and status[0] not in ("okay", "ok"):
                return False
            node = node.parent
        return True

    def walk(self):
        yield self
        for child in self.children:
            yield from child.walk()


def parse(blob):
    magic, _total, off_struct, off_strings = struct.unpack(">4I", blob[:16])
    if magic != 0xD00DFEED:
        raise SystemExit("not a flattened device tree")

    def string_at(offset):
        end = blob.index(b"\0", off_strings + offset)
        return blob[off_strings + offset : end].decode("ascii")

    at = off_struct
    root = None
    node = None
    while True:
        (token,) = struct.unpack(">I", blob[at : at + 4])
        at += 4
        if token == BEGIN_NODE:
            end = blob.index(b"\0", at)
            child = Node(blob[at:end].decode("ascii"), node)
            at = (end + 4) & ~3
            if node is None:
                root = child
            else:
                node.children.append(child)
            node = child
        elif token == END_NODE:
            node = node.parent
        elif token == PROP:
            length, name_offset = struct.unpack(">2I", blob[at : at + 8])
            at += 8
            node.props[string_at(name_offset)] = blob[at : at + length]
            at = (at + length + 3) & ~3
        elif token == NOP:
            continue
        elif token == END:
            return root
        else:
            raise SystemExit("bad token %#x at %#x" % (token, at - 4))


def number(cells):
    value = 0
    for cell in cells:
        value = (value << 32) | cell
    return value


def address_cells(node):
    cells = node.cells("#address-cells")
    return cells[0] if cells else 2


def size_cells(node):
    cells = node.cells("#size-cells")
    return cells[0] if cells else 1


def cpu_address(node):
    """The first `reg` address of a node, carried up through every `ranges`.

    None when the node has no `reg`, or a parent gives no translation (an
    I2C or SPI child, a PCI function).
    """
    parent = node.parent
    reg = node.cells("reg")
    if parent is None or not reg:
        return None
    address = number(reg[: address_cells(parent)])
    child, bus = node, parent
    while bus.parent is not None:
        if "ranges" not in bus.props:
            return None
        ranges = bus.cells("ranges")
        ca, pa, sz = address_cells(bus), address_cells(bus.parent), size_cells(bus)
        if bus.strings("device_type") == ["pci"]:
            return None
        step = ca + pa + sz
        hit = not ranges
        for i in range(0, len(ranges) - step + 1, step):
            base = number(ranges[i : i + ca])
            size = number(ranges[i + ca + pa : i + step])
            if base <= address < base + size:
                address = address - base + number(ranges[i + ca : i + ca + pa])
                hit = True
                break
        if not hit:
            return None
        child, bus = bus, bus.parent
    del child
    return address


def by_phandle(root):
    table = {}
    for node in root.walk():
        cells = node.cells("phandle")
        if cells:
            table[cells[0]] = node
    return table


def interrupt_parent(node, phandles):
    while node is not None:
        cells = node.cells("interrupt-parent")
        if cells:
            return phandles.get(cells[0])
        node = node.parent
    return None


def interrupts(node, phandles):
    raw = node.cells("interrupts")
    parent = interrupt_parent(node, phandles)
    if not raw or parent is None:
        return ""
    width = (parent.cells("#interrupt-cells") or [1])[0]
    groups = [raw[i : i + width] for i in range(0, len(raw), width)]
    text = " ".join("<" + " ".join(str(c) for c in g) + ">" for g in groups)
    return "%s %s" % (parent.name, text)


def needs(node, phandles):
    found = []
    for name, width_name in NEEDS.items():
        cells = node.cells(name)
        at = 0
        while at < len(cells):
            other = phandles.get(cells[at])
            at += 1
            if other is None:
                continue
            if width_name is not None:
                at += (other.cells(width_name) or [0])[0]
            if other is not node and other not in found:
                found.append(other)
    return found


def print_node(node, phandles):
    address = cpu_address(node)
    print(
        "%-58s %-44s %-14s %s"
        % (
            node.path,
            ",".join(node.strings("compatible")[:2]),
            "-" if address is None else "%#x" % address,
            interrupts(node, phandles),
        )
    )


def main(argv):
    if len(argv) < 3 or argv[1] not in ("nodes", "needs", "counts"):
        raise SystemExit(__doc__)
    with open(argv[2], "rb") as handle:
        root = parse(handle.read())
    phandles = by_phandle(root)
    everything = list(root.walk())
    if argv[1] == "counts":
        enabled = [n for n in everything if n.enabled() and "compatible" in n.props]
        names = {n.strings("compatible")[0] for n in enabled}
        print(
            "nodes %d, enabled with a compatible %d, distinct first compatibles %d"
            % (len(everything), len(enabled), len(names))
        )
    elif argv[1] == "nodes":
        for node in everything:
            if node.enabled() and "compatible" in node.props:
                print_node(node, phandles)
    else:
        paths = {n.path: n for n in everything}
        aliases = paths.get("/aliases")
        queue = []
        for want in argv[3:]:
            if not want.startswith("/") and aliases is not None:
                want = (aliases.strings(want) or [want])[0]
            if want not in paths:
                raise SystemExit("no node %s" % want)
            queue.append(paths[want])
        seen = []
        while queue:
            node = queue.pop(0)
            if node in seen:
                continue
            seen.append(node)
            queue.extend(needs(node, phandles))
            # A pin control state is a child of its controller.
            if node.parent is not None and "compatible" not in node.props:
                queue.append(node.parent)
        for node in seen:
            if "compatible" in node.props:
                print_node(node, phandles)


if __name__ == "__main__":
    main(sys.argv)
