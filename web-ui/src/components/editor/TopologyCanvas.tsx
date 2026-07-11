// The SVG topology canvas: segments as bus bars, VMs as boxes, NICs as
// edges dropping onto the bars. One world-transform group gives pan/zoom;
// positions are cosmetic and live in localStorage (editor/layout.ts).
//
// Interactions: click = select (background = lab), drag node = move,
// drag background = pan, wheel = zoom-to-cursor, Delete = remove selection.
// Cabling: a NIC's port dot on the VM and its lit socket on the bar are two
// ends of the same cable — drag either onto a bar to re-home the NIC, or
// onto empty space to unplug it (the NIC stays on the VM as a loose port).
// Drag a bar's free socket onto a NIC dot to swap that NIC over, or onto
// the VM body to add a NIC there. Bars also wear interconnect ports on
// their short sides: drag one onto another switch to route the segments
// together (routes_to), or onto the WAN object to give the whole segment
// internet egress (segment nat) — grab an interconnect cable to re-home or
// remove it the same way.

import { For, Index, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import { Button } from "@forge/ui";
import {
  Container,
  Expand,
  FlaskConical,
  FilePenLine,
  LayoutGrid,
  Monitor,
  Play,
  RotateCw,
  Square,
  Waypoints,
} from "lucide-solid";
import type { Layout, NodePos } from "../../editor/layout";
import {
  MIN_FREE_PORTS,
  PORT_SIZE,
  PORT_SPACING,
  PORT_X0,
  SEG_H,
  VM_H,
  VM_W,
  autoLayout,
  loadLayout,
  renameInLayout,
  saveLayout,
  segWidthFor,
} from "../../editor/layout";
import type { ContainerModel, LabModel, NicModel, VmModel } from "../../editor/model";
import type { MachineKind } from "../../editor/store";
import {
  addContainer,
  addMachineNic,
  addSegment,
  addSegmentRoute,
  addVm,
  disconnectMachineNic,
  editor,
  removeContainer,
  removeSegment,
  removeSegmentRoute,
  removeVm,
  select,
  setMachineNicTarget,
  setSegmentNat,
  storeTemplateFor,
} from "../../editor/store";
import { formatMemory } from "../../editor/bytesize";
import {
  anyVmRunning,
  containerIsUp,
  containerLook,
  containerRestart,
  containerStart,
  containerStop,
  look,
  showVm,
  state,
  vmIsUp,
  vmRestart,
  vmStart,
  vmStop,
} from "../../store";
import { confirmDialog } from "../dialogs";
import OsIcon from "./OsIcon";
import { registerFxNode } from "../../fx";

interface Drag {
  kind: "vm" | "container" | "segment" | "nat" | "lab";
  name: string;
  dx: number;
  dy: number;
  moved: boolean;
}

/** Re-homing an existing NIC, grabbed by its machine port dot or lit socket. */
interface ConnDrag {
  kind: MachineKind;
  index: number;
  nicIndex: number;
  moved: boolean;
  x: number;
  y: number;
}

/** Cabling out from a bar's free socket (or a NAT-cloud port) toward a VM. */
interface SocketDrag {
  barKey: string;
  socketIdx: number;
  /** Rubber-band anchor: the grabbed socket face / port. */
  ax: number;
  ay: number;
  moved: boolean;
  x: number;
  y: number;
}

/** An interconnect in flight: cabling out from a bar's side port, or
 *  re-homing an existing segment↔segment / segment↔WAN link. */
interface LinkDrag {
  /** Source bar: a segment name, or NAT_KEY when cabling out of the WAN. */
  from: string;
  existing: { kind: "route"; to: string } | { kind: "wan" } | null;
  moved: boolean;
  x: number;
  y: number;
}

export default function TopologyCanvas(props: { onEditConfig: () => void }) {
  let svg: SVGSVGElement | undefined;
  const model = () => editor.draft!;
  const lab = () => editor.lab!;

  const [layout, setLayout] = createSignal<Layout>({ vms: {}, containers: {}, segments: {} });
  const [view, setView] = createSignal({ tx: 0, ty: 0, k: 1 });
  const [drag, setDrag] = createSignal<Drag | null>(null);
  const [pan, setPan] = createSignal<{ sx: number; sy: number; tx: number; ty: number } | null>(
    null,
  );
  const [connDrag, setConnDrag] = createSignal<ConnDrag | null>(null);
  const [socketDrag, setSocketDrag] = createSignal<SocketDrag | null>(null);
  const [linkDrag, setLinkDrag] = createSignal<LinkDrag | null>(null);

  // (Re)seed layout whenever the lab or the set of node names changes.
  createEffect(() => {
    const l = lab();
    const names = [
      ...model().vms.map((v) => v.name),
      ...model().containers.map((c) => c.name),
      ...model().segments.map((s) => s.name),
    ];
    void names.length;
    const stored = loadLayout(l);
    const full = autoLayout(model(), stored);
    setLayout(full);
    if (stored.view) setView(stored.view);
  });

  // Track renames: reconcile stored positions to current names by dropping
  // stale entries (autoLayout refills). Rename migration happens on the
  // rename path in the store via renameInLayout when wired; cheap fallback.
  const persist = () => {
    const l = { ...layout(), view: view() };
    setLayout(l);
    saveLayout(lab(), l);
  };

  const world = (e: PointerEvent | WheelEvent) => {
    const rect = svg!.getBoundingClientRect();
    const { tx, ty, k } = view();
    return { x: (e.clientX - rect.left - tx) / k, y: (e.clientY - rect.top - ty) / k };
  };

  const vmPos = (name: string): NodePos => layout().vms[name] ?? { x: 40, y: 40 };
  const ctrPos = (name: string): NodePos => layout().containers[name] ?? { x: 40, y: 40 };
  /** VMs and containers share the same node geometry on the canvas. */
  const machinePos = (kind: MachineKind, name: string): NodePos =>
    kind === "vm" ? vmPos(name) : ctrPos(name);
  const machinesOf = (kind: MachineKind): (VmModel | ContainerModel)[] =>
    kind === "vm" ? model().vms : model().containers;
  const segPos = (name: string): NodePos => layout().segments[name] ?? { x: 60, y: 200 };
  const natPos = (): NodePos =>
    layout().nat ?? { x: 60, y: 200 + model().segments.length * 170 };
  const labPos = (): NodePos => layout().lab ?? { x: 60, y: 30 };

  /** The lab block's size (name-dependent width). */
  const LAB_H = 64;
  const labW = () => Math.max(220, 58 + model().name.length * 8);

  // --- port assignment --------------------------------------------------------
  // Each bar (segment or the NAT bus) owns a bank of sockets; every attached
  // NIC claims the next one in declaration order (vms, then containers, then
  // nic order), so edges spread left-to-right like a patch panel. Machine
  // names are unique across VMs and containers (one namespace), so
  // `${name}:${nicIndex}` addresses a NIC unambiguously.

  const NAT_KEY = "__nat__";

  const ports = createMemo(() => {
    const byNic = new Map<string, number>(); // `${machine}:${nicIndex}` → socket index
    const used = new Map<string, number>(); // bar key → sockets in use
    // `${bar}:${socketIdx}` → occupying NIC (the reverse map for socket grabs)
    const bySocket = new Map<string, { kind: MachineKind; index: number; nicIndex: number }>();
    for (const kind of ["vm", "container"] as const) {
      machinesOf(kind).forEach((m, index) => {
        m.nics.forEach((nic, i) => {
          // NAT NICs all share the WAN's single port — no socket to claim.
          const key = nic.nat ? null : nic.segment;
          if (!key) return;
          const n = used.get(key) ?? 0;
          byNic.set(`${m.name}:${i}`, n);
          bySocket.set(`${key}:${n}`, { kind, index, nicIndex: i });
          used.set(key, n + 1);
        });
      });
    }
    return { byNic, used, bySocket };
  });


  /** The WAN cloud is a small fixed-size node with a single port. */
  const WAN_W = 150;

  const barUsed = (key: string) => ports().used.get(key) ?? 0;
  const barWidth = (key: string) => (key === NAT_KEY ? WAN_W : segWidthFor(barUsed(key)));
  // The socket bank is exactly the connections plus a couple of spares.
  const barCapacity = (key: string) => barUsed(key) + MIN_FREE_PORTS;
  const socketX = (barX: number, idx: number) => barX + PORT_X0 + idx * PORT_SPACING;
  const vmPortX = (vp: NodePos, nicIndex: number) => vp.x + 18 + nicIndex * 16;
  const barPos = (key: string): NodePos => (key === NAT_KEY ? natPos() : segPos(key));

  // Interconnects: declared segment↔segment routes plus segment↔WAN (nat).
  const links = createMemo(() => {
    const names = new Set(model().segments.map((s) => s.name));
    const out: { from: string; kind: "route" | "wan"; to: string }[] = [];
    for (const s of model().segments) {
      for (const t of s.routes_to) {
        if (names.has(t)) out.push({ from: s.name, kind: "route", to: t });
      }
      if (s.nat) out.push({ from: s.name, kind: "wan", to: NAT_KEY });
    }
    return out;
  });

  // The NAT cloud has one port per side; every connection lands on the
  // port closest to its far end.
  type WanSide = "top" | "bottom" | "left" | "right";
  const WAN_SIDES: WanSide[] = ["top", "bottom", "left", "right"];

  const wanPorts = (): Record<WanSide, NodePos> => {
    const p = natPos();
    return {
      top: { x: p.x + WAN_W / 2, y: p.y },
      bottom: { x: p.x + WAN_W / 2, y: p.y + SEG_H },
      left: { x: p.x, y: p.y + SEG_H / 2 },
      right: { x: p.x + WAN_W, y: p.y + SEG_H / 2 },
    };
  };

  function closestWanSide(x: number, y: number, sides: WanSide[] = WAN_SIDES): WanSide {
    const ps = wanPorts();
    let best = sides[0];
    let bd = Infinity;
    for (const s of sides) {
      const d = (ps[s].x - x) ** 2 + (ps[s].y - y) ** 2;
      if (d < bd) {
        bd = d;
        best = s;
      }
    }
    return best;
  }

  /** The closest pair of side ports between two bars — each interconnect
   *  leaves and lands wherever is nearest. */
  function linkEnds(fromKey: string, toKey: string) {
    const ends = (key: string) => {
      if (key === NAT_KEY) {
        const ps = wanPorts();
        return { left: ps.left, right: ps.right };
      }
      const p = barPos(key);
      const y = p.y + SEG_H / 2;
      return { left: { x: p.x, y }, right: { x: p.x + barWidth(key), y } };
    };
    const A = ends(fromKey);
    const B = ends(toKey);
    let best = { aSide: "left" as "left" | "right", a: A.left, bSide: "left" as "left" | "right", b: B.left };
    let bd = Infinity;
    for (const sa of ["left", "right"] as const) {
      for (const sb of ["left", "right"] as const) {
        const d = (A[sa].x - B[sb].x) ** 2 + (A[sa].y - B[sb].y) ** 2;
        if (d < bd) {
          bd = d;
          best = { aSide: sa, a: A[sa], bSide: sb, b: B[sb] };
        }
      }
    }
    return best;
  }

  /** Which NAT-cloud ports have something plugged in (they light up). */
  const wanSideUsed = createMemo(() => {
    const used: Record<WanSide, boolean> = {
      top: false,
      bottom: false,
      left: false,
      right: false,
    };
    for (const kind of ["vm", "container"] as const) {
      machinesOf(kind).forEach((m) => {
        m.nics.forEach((nic, i) => {
          if (!nic.nat) return;
          const mp = machinePos(kind, m.name);
          used[closestWanSide(vmPortX(mp, i), mp.y + VM_H)] = true;
        });
      });
    }
    for (const l of links()) {
      if (l.kind !== "wan") continue;
      used[linkEnds(l.from, NAT_KEY).bSide] = true;
    }
    return used;
  });

  // --- interactions ---------------------------------------------------------

  function nodeDown(e: PointerEvent, kind: Drag["kind"], name: string) {
    e.stopPropagation();
    const w = world(e);
    const pos =
      kind === "vm"
        ? vmPos(name)
        : kind === "container"
          ? ctrPos(name)
          : kind === "segment"
            ? segPos(name)
            : kind === "lab"
              ? labPos()
              : natPos();
    setDrag({ kind, name, dx: w.x - pos.x, dy: w.y - pos.y, moved: false });
  }

  /** Grab a NIC's port dot on its machine: re-home (or unplug) that NIC. */
  function dotDown(e: PointerEvent, kind: MachineKind, index: number, nicIndex: number) {
    e.stopPropagation();
    if (anyVmRunning()) {
      select({ kind, index });
      return;
    }
    const w = world(e);
    setConnDrag({ kind, index, nicIndex, moved: false, x: w.x, y: w.y });
  }

  /** Grab a bar socket: lit = re-home its NIC, free = cable out to a VM. */
  function socketDown(e: PointerEvent, barKey: string, socketIdx: number, ax: number, ay: number) {
    e.stopPropagation();
    if (anyVmRunning()) return;
    const w = world(e);
    const occupant = ports().bySocket.get(`${barKey}:${socketIdx}`);
    if (occupant) {
      setConnDrag({ ...occupant, moved: false, x: w.x, y: w.y });
    } else {
      setSocketDrag({ barKey, socketIdx, ax, ay, moved: false, x: w.x, y: w.y });
    }
  }

  /** Grab a bar's side port: cable a new interconnect out. */
  function linkDown(e: PointerEvent, from: string) {
    e.stopPropagation();
    if (anyVmRunning()) return;
    const w = world(e);
    setLinkDrag({ from, existing: null, moved: false, x: w.x, y: w.y });
  }

  /** Grab an existing interconnect cable to re-home or remove it. */
  function linkGrab(e: PointerEvent, link: { from: string; kind: "route" | "wan"; to: string }) {
    e.stopPropagation();
    if (anyVmRunning()) return;
    const w = world(e);
    setLinkDrag({
      from: link.from,
      existing: link.kind === "route" ? { kind: "route", to: link.to } : { kind: "wan" },
      moved: false,
      x: w.x,
      y: w.y,
    });
  }

  /** The bar (segment or the WAN) under a world point, with a little slack. */
  function barAt(x: number, y: number): string | null {
    for (const s of model().segments) {
      const p = segPos(s.name);
      if (x >= p.x && x <= p.x + barWidth(s.name) && y >= p.y - 8 && y <= p.y + SEG_H + 8) {
        return s.name;
      }
    }
    const p = natPos();
    if (x >= p.x && x <= p.x + barWidth(NAT_KEY) && y >= p.y - 8 && y <= p.y + SEG_H + 8) {
      return NAT_KEY;
    }
    return null;
  }

  /** The machine connection point under a world point: an existing NIC dot
   *  (swap target) or the machine box itself (add target). */
  function machineTargetAt(
    x: number,
    y: number,
  ): { kind: MachineKind; index: number; nicIndex: number | null } | null {
    for (const kind of ["vm", "container"] as const) {
      const machines = machinesOf(kind);
      for (let mi = 0; mi < machines.length; mi++) {
        const mp = machinePos(kind, machines[mi].name);
        const py = mp.y + VM_H;
        for (let i = 0; i < machines[mi].nics.length; i++) {
          const dx = x - vmPortX(mp, i);
          const dy = y - py;
          if (dx * dx + dy * dy <= 100) return { kind, index: mi, nicIndex: i };
        }
        if (x >= mp.x && x <= mp.x + VM_W && y >= mp.y && y <= mp.y + VM_H + 8) {
          return { kind, index: mi, nicIndex: null };
        }
      }
    }
    return null;
  }

  function backgroundDown(e: PointerEvent) {
    setPan({ sx: e.clientX, sy: e.clientY, tx: view().tx, ty: view().ty });
  }

  function move(e: PointerEvent) {
    const d = drag();
    if (d) {
      const w = world(e);
      const pos = { x: w.x - d.dx, y: w.y - d.dy };
      setDrag({ ...d, moved: true });
      setLayout((l) =>
        d.kind === "vm"
          ? { ...l, vms: { ...l.vms, [d.name]: pos } }
          : d.kind === "container"
            ? { ...l, containers: { ...l.containers, [d.name]: pos } }
            : d.kind === "segment"
              ? { ...l, segments: { ...l.segments, [d.name]: pos } }
              : d.kind === "lab"
                ? { ...l, lab: pos }
                : { ...l, nat: pos },
      );
      return;
    }
    const cd = connDrag();
    if (cd) {
      const w = world(e);
      setConnDrag({ ...cd, moved: true, x: w.x, y: w.y });
      return;
    }
    const sd = socketDrag();
    if (sd) {
      const w = world(e);
      setSocketDrag({ ...sd, moved: true, x: w.x, y: w.y });
      return;
    }
    const ld = linkDrag();
    if (ld) {
      const w = world(e);
      setLinkDrag({ ...ld, moved: true, x: w.x, y: w.y });
      return;
    }
    const p = pan();
    if (p) {
      setView((v) => ({ ...v, tx: p.tx + (e.clientX - p.sx), ty: p.ty + (e.clientY - p.sy) }));
    }
  }

  function up(e: PointerEvent) {
    const d = drag();
    if (d) {
      if (!d.moved) {
        // A click: select the node.
        if (d.kind === "vm") {
          const i = model().vms.findIndex((v) => v.name === d.name);
          if (i >= 0) select({ kind: "vm", index: i });
        } else if (d.kind === "container") {
          const i = model().containers.findIndex((c) => c.name === d.name);
          if (i >= 0) select({ kind: "container", index: i });
        } else if (d.kind === "segment") {
          const i = model().segments.findIndex((s) => s.name === d.name);
          if (i >= 0) select({ kind: "segment", index: i });
        } else if (d.kind === "lab") {
          select({ kind: "lab" });
        } else {
          select({ kind: "nat" });
        }
      } else {
        persist();
      }
      setDrag(null);
      return;
    }
    const cd = connDrag();
    if (cd) {
      setConnDrag(null);
      if (!cd.moved) {
        // A click on the port: select its machine.
        select({ kind: cd.kind, index: cd.index });
        return;
      }
      const w = world(e);
      // Drop on a bar → re-home the NIC; anywhere else → unplug it (the
      // NIC stays on the machine as a loose port, ready to cable again).
      const bar = barAt(w.x, w.y);
      if (bar) setMachineNicTarget(cd.kind, cd.index, cd.nicIndex, bar === NAT_KEY ? null : bar);
      else disconnectMachineNic(cd.kind, cd.index, cd.nicIndex);
      return;
    }
    const sd = socketDrag();
    if (sd) {
      setSocketDrag(null);
      if (!sd.moved) return;
      const w = world(e);
      const seg = sd.barKey === NAT_KEY ? null : sd.barKey;
      const target = machineTargetAt(w.x, w.y);
      if (target) {
        // A NIC dot swaps that NIC here (one connection per port); the
        // machine body adds a fresh NIC.
        if (target.nicIndex !== null) {
          setMachineNicTarget(target.kind, target.index, target.nicIndex, seg);
        } else {
          addMachineNic(target.kind, target.index, seg);
        }
      } else if (sd.barKey === NAT_KEY) {
        // The WAN's single port also cables onto switches: segment egress.
        const drop = barAt(w.x, w.y);
        if (drop && drop !== NAT_KEY) {
          setSegmentNat(
            model().segments.findIndex((s) => s.name === drop),
            true,
          );
        }
      }
      return;
    }
    const ld = linkDrag();
    if (ld) {
      setLinkDrag(null);
      if (!ld.moved) return;
      const w = world(e);
      const drop = barAt(w.x, w.y);
      if (drop === ld.from) return; // dropped back home = cancel
      const segIdx = (name: string) => model().segments.findIndex((s) => s.name === name);
      const from = segIdx(ld.from);
      if (from < 0) return;
      // Detach the grabbed end, then attach wherever it landed (nowhere =
      // the interconnect is removed).
      if (ld.existing?.kind === "route" && drop !== ld.existing.to) {
        removeSegmentRoute(from, ld.existing.to);
      }
      if (ld.existing?.kind === "wan" && drop !== NAT_KEY) setSegmentNat(from, false);
      if (drop === NAT_KEY) setSegmentNat(from, true);
      else if (drop) addSegmentRoute(from, drop);
      return;
    }
    if (pan()) {
      const p = pan()!;
      const clicked = Math.abs(e.clientX - p.sx) < 4 && Math.abs(e.clientY - p.sy) < 4;
      setPan(null);
      if (clicked) select({ kind: "lab" });
      else persist();
    }
  }

  function wheel(e: WheelEvent) {
    e.preventDefault();
    const rect = svg!.getBoundingClientRect();
    const sx = e.clientX - rect.left;
    const sy = e.clientY - rect.top;
    setView((v) => {
      const k = Math.min(2, Math.max(0.25, v.k * (e.deltaY < 0 ? 1.1 : 0.9)));
      // Keep the point under the cursor fixed while zooming.
      const wx = (sx - v.tx) / v.k;
      const wy = (sy - v.ty) / v.k;
      return { k, tx: sx - wx * k, ty: sy - wy * k };
    });
  }

  async function onKey(e: KeyboardEvent) {
    if (e.key !== "Delete" && e.key !== "Backspace") return;
    const sel = editor.selection;
    if (sel.kind === "vm") {
      const vm = model().vms[sel.index];
      if (vm && vmIsUp(vm.name)) return;
      if (
        vm &&
        (await confirmDialog({ title: `Delete VM "${vm.name}"?`, danger: true }))
      ) {
        removeVm(sel.index);
      }
    } else if (sel.kind === "container") {
      const c = model().containers[sel.index];
      if (c && containerIsUp(c.name)) return;
      if (
        c &&
        (await confirmDialog({ title: `Delete container "${c.name}"?`, danger: true }))
      ) {
        removeContainer(sel.index);
      }
    } else if (sel.kind === "segment") {
      const seg = model().segments[sel.index];
      if (anyVmRunning()) return;
      if (
        seg &&
        (await confirmDialog({ title: `Delete segment "${seg.name}"?`, danger: true }))
      ) {
        removeSegment(sel.index);
      }
    }
  }

  function arrange() {
    const fresh = autoLayout(model(), { vms: {}, containers: {}, segments: {} });
    setLayout(fresh);
    saveLayout(lab(), { ...fresh, view: view() });
  }

  function zoomFit() {
    const xs: number[] = [];
    const ys: number[] = [];
    for (const v of model().vms) {
      const p = vmPos(v.name);
      xs.push(p.x, p.x + VM_W);
      ys.push(p.y, p.y + VM_H);
    }
    for (const c of model().containers) {
      const p = ctrPos(c.name);
      xs.push(p.x, p.x + VM_W);
      ys.push(p.y, p.y + VM_H);
    }
    for (const s of model().segments) {
      const p = segPos(s.name);
      xs.push(p.x, p.x + barWidth(s.name));
      ys.push(p.y, p.y + SEG_H);
    }
    {
      const p = natPos();
      xs.push(p.x, p.x + barWidth(NAT_KEY));
      ys.push(p.y, p.y + SEG_H);
    }
    {
      const p = labPos();
      xs.push(p.x, p.x + labW());
      ys.push(p.y, p.y + LAB_H);
    }
    if (!xs.length || !svg) return;
    const rect = svg.getBoundingClientRect();
    const minX = Math.min(...xs) - 40;
    const minY = Math.min(...ys) - 40;
    const w = Math.max(...xs) - minX + 40;
    const h = Math.max(...ys) - minY + 40;
    const k = Math.min(2, Math.max(0.25, Math.min(rect.width / w, rect.height / h)));
    setView({ k, tx: -minX * k, ty: -minY * k });
    persist();
  }

  // Rename migration: when a selected node's name changes, keep its position.
  // (The store rewrites references; the canvas keys positions by name.)
  let lastNames: string[] = [];
  createEffect(() => {
    const names = [
      ...model().vms.map((v) => v.name),
      ...model().containers.map((c) => c.name),
      ...model().segments.map((s) => s.name),
    ];
    if (lastNames.length === names.length) {
      const changed = names.findIndex((n, i) => n !== lastNames[i]);
      if (changed >= 0 && names.filter((n, i) => n !== lastNames[i]).length === 1) {
        const kind =
          changed < model().vms.length
            ? "vms"
            : changed < model().vms.length + model().containers.length
              ? "containers"
              : "segments";
        setLayout((l) => renameInLayout(l, kind, lastNames[changed], names[changed]));
      }
    }
    lastNames = names;
  });

  // Drags are tracked at the window level so a release outside the canvas
  // (over the inspector, toolbar, …) still completes/cancels cleanly —
  // per-element pointer capture proved unreliable for that. A pointercancel
  // (browser took the gesture over) just abandons whatever was in flight.
  const cancelDrags = () => {
    setDrag(null);
    setConnDrag(null);
    setSocketDrag(null);
    setLinkDrag(null);
    setPan(null);
  };
  window.addEventListener("pointermove", move);
  window.addEventListener("pointerup", up);
  window.addEventListener("pointercancel", cancelDrags);
  onCleanup(() => {
    window.removeEventListener("pointermove", move);
    window.removeEventListener("pointerup", up);
    window.removeEventListener("pointercancel", cancelDrags);
    persist();
  });

  // --- geometry helpers -------------------------------------------------------

  const selectedVm = () =>
    editor.selection.kind === "vm" ? model().vms[editor.selection.index]?.name : null;
  const selectedCtr = () =>
    editor.selection.kind === "container"
      ? model().containers[editor.selection.index]?.name
      : null;
  const selectedSeg = () =>
    editor.selection.kind === "segment" ? model().segments[editor.selection.index]?.name : null;

  /** The router-style socket bank straddling a bar's top and bottom edges.
   *  A used socket lights only the face its cable actually enters through
   *  (the one nearest its VM). Grabbing a lit face re-homes that NIC; a
   *  free one cables out. */
  function Sockets(props: { barKey: string; pos: NodePos }) {
    return (
      <Index each={Array.from({ length: barCapacity(props.barKey) })}>
        {(_, i) => {
          const cx = () => socketX(props.pos.x, i);
          // Which face the occupying NIC's cable enters through, if any.
          const cableAbove = () => {
            const o = ports().bySocket.get(`${props.barKey}:${i}`);
            if (!o) return null;
            const m = machinesOf(o.kind)[o.index];
            if (!m) return null;
            return machinePos(o.kind, m.name).y + VM_H <= props.pos.y + SEG_H / 2;
          };
          const down = (e: PointerEvent, faceY: number) =>
            socketDown(e, props.barKey, i, cx(), faceY);
          const hint = () =>
            anyVmRunning()
              ? "Networking is read-only while a machine is up"
              : cableAbove() !== null
              ? "Drag to another bar to move (empty space unplugs)"
              : "Drag onto a VM to connect";
          return (
            <>
              <rect
                x={cx() - PORT_SIZE / 2}
                y={props.pos.y - PORT_SIZE / 2}
                width={PORT_SIZE}
                height={PORT_SIZE}
                rx="1.5"
                class="topo-socket"
                classList={{ lit: cableAbove() === true, locked: anyVmRunning() }}
                onPointerDown={(e: PointerEvent) => down(e, props.pos.y)}
              >
                <title>{hint()}</title>
              </rect>
              <rect
                x={cx() - PORT_SIZE / 2}
                y={props.pos.y + SEG_H - PORT_SIZE / 2}
                width={PORT_SIZE}
                height={PORT_SIZE}
                rx="1.5"
                class="topo-socket"
                classList={{ lit: cableAbove() === false, locked: anyVmRunning() }}
                onPointerDown={(e: PointerEvent) => down(e, props.pos.y + SEG_H)}
              >
                <title>{hint()}</title>
              </rect>
            </>
          );
        }}
      </Index>
    );
  }

  /** Interconnect ports on a bar's short sides: drag one onto another
   *  switch to route the segments together, or onto the WAN for egress.
   *  Only the side an interconnect actually leaves/lands through lights. */
  function SidePorts(props: { barKey: string }) {
    const pos = () => barPos(props.barKey);
    const w = () => barWidth(props.barKey);
    const down = (e: PointerEvent) => linkDown(e, props.barKey);
    const sideLit = (side: "left" | "right") =>
      links().some((l) => {
        const ends = linkEnds(l.from, l.to);
        return (
          (l.from === props.barKey && ends.aSide === side) ||
          (l.to === props.barKey && ends.bSide === side)
        );
      });
    const hint = () =>
      anyVmRunning()
        ? "Networking is read-only while a machine is up"
        : "Drag onto another switch to route to it, or onto the NAT cloud for internet egress";
    return (
      <>
        <rect
          x={pos().x - PORT_SIZE / 2}
          y={pos().y + SEG_H / 2 - PORT_SIZE / 2}
          width={PORT_SIZE}
          height={PORT_SIZE}
          rx="1.5"
          class="topo-linkport"
          classList={{ lit: sideLit("left"), locked: anyVmRunning() }}
          onPointerDown={down}
        >
          <title>{hint()}</title>
        </rect>
        <rect
          x={pos().x + w() - PORT_SIZE / 2}
          y={pos().y + SEG_H / 2 - PORT_SIZE / 2}
          width={PORT_SIZE}
          height={PORT_SIZE}
          rx="1.5"
          class="topo-linkport"
          classList={{ lit: sideLit("right"), locked: anyVmRunning() }}
          onPointerDown={down}
        >
          <title>{hint()}</title>
        </rect>
      </>
    );
  }

  /** Interconnect path between the closest side ports of two bars: a
   *  bracket outside both when they use the same side, or a Z through the
   *  gap when opposite sides face each other. Per-link lanes keep parallel
   *  interconnects from overlapping. */
  function linkPath(fromKey: string, toKey: string, lane: number): string {
    const { aSide, a, bSide, b } = linkEnds(fromKey, toKey);
    if (aSide !== bSide) {
      const mx = (a.x + b.x) / 2 + lane * 8;
      return `M ${a.x} ${a.y} L ${mx} ${a.y} L ${mx} ${b.y} L ${b.x} ${b.y}`;
    }
    const off = 18 + lane * 10;
    const x = aSide === "left" ? Math.min(a.x, b.x) - off : Math.max(a.x, b.x) + off;
    return `M ${a.x} ${a.y} L ${x} ${a.y} L ${x} ${b.y} L ${b.x} ${b.y}`;
  }

  /** The strings the OS icon classifies a VM by. */
  const osStringFor = (vm: VmModel) =>
    `${vm.template} ${vm.profile ?? ""} ${storeTemplateFor(vm.template)?.profile ?? ""}`;

  // Live power state per VM / container (the daemon's view; absent = no daemon).
  const runtimeOf = (name: string) => state.status?.vms.find((v) => v.name === name);
  const vmRunning = (name: string) => {
    const rv = runtimeOf(name);
    return !!rv && rv.state !== "stopped";
  };
  const ledTone = (name: string) => {
    const rv = runtimeOf(name);
    return rv ? look(rv).tone : "neutral";
  };
  const ledLabel = (name: string) => {
    const rv = runtimeOf(name);
    return rv ? look(rv).label : "no daemon";
  };
  const ctrRuntimeOf = (name: string) =>
    (state.status?.containers ?? []).find((c) => c.name === name);
  const ctrRunning = (name: string) => {
    const rc = ctrRuntimeOf(name);
    return !!rc && rc.state !== "stopped";
  };
  const ctrLedTone = (name: string) => {
    const rc = ctrRuntimeOf(name);
    return rc ? containerLook(rc).tone : "neutral";
  };
  const ctrLedLabel = (name: string) => {
    const rc = ctrRuntimeOf(name);
    return rc ? containerLook(rc).label : "no daemon";
  };
  const machineRunning = (kind: MachineKind, name: string) =>
    kind === "vm" ? vmRunning(name) : ctrRunning(name);

  /** A small in-canvas icon button on a VM box. */
  function VmBtn(props: {
    x: number;
    y: number;
    act: string;
    title: string;
    onClick: () => void;
    children: any;
  }) {
    return (
      <g
        class={`topo-console act-${props.act}`}
        transform={`translate(${props.x} ${props.y})`}
        onPointerDown={(e: PointerEvent) => e.stopPropagation()}
        onClick={props.onClick}
      >
        <rect width="18" height="16" rx="4" />
        <g transform="translate(3.5 2.5)">{props.children}</g>
        <title>{props.title}</title>
      </g>
    );
  }

  /** Edge from a machine's port dot to its assigned socket on the target bar
   *  (NAT NICs land on the cloud port closest to the machine). */
  function edgePath(
    kind: MachineKind,
    name: string,
    nicIndex: number,
    nic: NicModel,
  ): string | null {
    const key = nic.nat ? NAT_KEY : nic.segment;
    if (!key) return null;
    const vp = machinePos(kind, name);
    const px = vmPortX(vp, nicIndex);
    const py = vp.y + VM_H;
    if (nic.nat) {
      const side = closestWanSide(px, py);
      const pt = wanPorts()[side];
      if (side === "left" || side === "right") {
        // Drop to port height, then straight into the side port's face.
        const sx = side === "left" ? pt.x - PORT_SIZE / 2 : pt.x + PORT_SIZE / 2;
        return `M ${px} ${py} L ${px} ${pt.y} L ${sx} ${pt.y}`;
      }
      const sy = side === "top" ? pt.y - PORT_SIZE / 2 : pt.y + PORT_SIZE / 2;
      if (px === pt.x) return `M ${px} ${py} L ${px} ${sy}`;
      const elbow = side === "top" ? sy - 14 : sy + 14;
      return `M ${px} ${py} L ${px} ${elbow} L ${pt.x} ${elbow} L ${pt.x} ${sy}`;
    }
    const bar = segPos(key);
    const socket = ports().byNic.get(`${name}:${nicIndex}`);
    if (socket === undefined) return null;
    const sx = socketX(bar.x, socket);
    // Enter through the socket face nearest the VM (sockets straddle both
    // edges, like through-ports on a patch panel).
    const fromAbove = py <= bar.y + SEG_H / 2;
    const sy = fromAbove ? bar.y - PORT_SIZE / 2 : bar.y + SEG_H + PORT_SIZE / 2;
    if (px === sx) return `M ${px} ${py} L ${px} ${sy}`;
    const elbow = fromAbove ? sy - 14 : sy + 14;
    return `M ${px} ${py} L ${px} ${elbow} L ${sx} ${elbow} L ${sx} ${sy}`;
  }

  return (
    <div class="topo-wrap">
      <div class="topo-toolbar">
        <Button size="sm" icon={Monitor} onClick={() => addVm()}>
          Add VM
        </Button>
        <Button size="sm" icon={Container} onClick={() => addContainer()}>
          Add container
        </Button>
        <Button
          size="sm"
          icon={Waypoints}
          onClick={() => addSegment()}
          disabled={anyVmRunning()}
          title={anyVmRunning() ? "Networking is read-only while a machine is up" : undefined}
        >
          Add segment
        </Button>
        <Button size="sm" variant="ghost" icon={LayoutGrid} onClick={arrange} title="Auto-arrange">
          Arrange
        </Button>
        <Button size="sm" variant="ghost" icon={Expand} onClick={zoomFit} title="Zoom to fit">
          Fit
        </Button>
      </div>
      <svg
        ref={svg}
        class="topo"
        tabindex="0"
        onPointerDown={backgroundDown}
        onWheel={wheel}
        onKeyDown={onKey}
      >
        <g transform={`translate(${view().tx} ${view().ty}) scale(${view().k})`}>
          {/* NIC edges under the nodes (VMs and containers cable alike) */}
          <For each={["vm", "container"] as MachineKind[]}>
            {(mkind) => (
              <For each={machinesOf(mkind)}>
                {(m) => (
                  <Index each={m.nics}>
                    {(nic, i) => {
                      const path = () => edgePath(mkind, m.name, i, nic());
                      // Hide the cable being re-homed; the rubber band replaces it.
                      const dragged = () => {
                        const cd = connDrag();
                        return (
                          cd !== null &&
                          cd.kind === mkind &&
                          machinesOf(mkind)[cd.index]?.name === m.name &&
                          cd.nicIndex === i
                        );
                      };
                      return (
                        <Show when={path() && !dragged()}>
                          <g
                            class="topo-edge"
                            classList={{ live: machineRunning(mkind, m.name) }}
                            onPointerDown={(e: PointerEvent) => {
                              e.stopPropagation();
                              const mi = machinesOf(mkind).findIndex((x) => x.name === m.name);
                              if (mi >= 0) select({ kind: mkind, index: mi });
                            }}
                          >
                            <path d={path()!} class="topo-edge-hit" />
                            <path d={path()!} class="topo-edge-line" />
                            <Show when={nic().ip}>
                              <text
                                x={machinePos(mkind, m.name).x + 22 + i * 16}
                                y={machinePos(mkind, m.name).y + VM_H + 16}
                                class="topo-edge-label"
                              >
                                {nic().ip}
                              </text>
                            </Show>
                          </g>
                        </Show>
                      );
                    }}
                  </Index>
                )}
              </For>
            )}
          </For>

          {/* switch↔switch / switch↔WAN interconnects */}
          <For each={links()}>
            {(link, li) => {
              // Hide the cable being re-homed; the rubber band replaces it.
              const grabbed = () => {
                const ld = linkDrag();
                if (!ld || ld.from !== link.from || !ld.existing) return false;
                return ld.existing.kind === "wan"
                  ? link.kind === "wan"
                  : link.kind === "route" && ld.existing.to === link.to;
              };
              return (
                <Show when={!grabbed()}>
                  <g
                    class="topo-link"
                    classList={{ live: link.kind === "wan", locked: anyVmRunning() }}
                    onPointerDown={(e: PointerEvent) => linkGrab(e, link)}
                  >
                    <path d={linkPath(link.from, link.to, li())} class="topo-link-hit">
                      <title>
                        {anyVmRunning()
                          ? "Networking is read-only while a machine is up"
                          : link.kind === "wan"
                          ? `${link.from} ⇄ WAN — drag off to disconnect`
                          : `${link.from} routes to ${link.to} — drag off to disconnect`}
                      </title>
                    </path>
                    <path d={linkPath(link.from, link.to, li())} class="topo-link-line" />
                  </g>
                </Show>
              );
            }}
          </For>

          {/* re-home rubber band: anchored at the NIC's port dot */}
          <Show when={connDrag()}>
            {(cd) => {
              const m = () => machinesOf(cd().kind)[cd().index];
              const mp = () => machinePos(cd().kind, m().name);
              return (
                <Show when={m()}>
                  <path
                    class="topo-edge-draft"
                    d={`M ${vmPortX(mp(), cd().nicIndex)} ${mp().y + VM_H} L ${cd().x} ${cd().y}`}
                  />
                </Show>
              );
            }}
          </Show>

          {/* cable-out rubber band: anchored at the grabbed socket/port */}
          <Show when={socketDrag()}>
            {(sd) => (
              <path
                class="topo-edge-draft"
                d={`M ${sd().ax} ${sd().ay} L ${sd().x} ${sd().y}`}
              />
            )}
          </Show>

          {/* interconnect rubber band: anchored at whichever of the source's
              side ports is nearer the pointer */}
          <Show when={linkDrag()}>
            {(ld) => {
              const bar = () => barPos(ld().from);
              const ax = () =>
                ld().x > bar().x + barWidth(ld().from) / 2
                  ? bar().x + barWidth(ld().from)
                  : bar().x;
              return (
                <path
                  class="topo-edge-draft"
                  d={`M ${ax()} ${bar().y + SEG_H / 2} L ${ld().x} ${ld().y}`}
                />
              );
            }}
          </Show>

          {/* the lab block: click to edit lab-wide properties */}
          <g
            class="topo-lab"
            classList={{ selected: editor.selection.kind === "lab" }}
            onPointerDown={(e: PointerEvent) => nodeDown(e, "lab", "__lab__")}
          >
            <rect x={labPos().x} y={labPos().y} width={labW()} height={LAB_H} rx="10" />
            <g
              class="topo-lab-glyph"
              transform={`translate(${labPos().x + 10} ${labPos().y + 11})`}
            >
              <FlaskConical size={17} />
            </g>
            <text class="topo-lab-kind" x={labPos().x + 34} y={labPos().y + 16}>
              LAB
            </text>
            <text class="topo-lab-name" x={labPos().x + 34} y={labPos().y + 31}>
              {model().name}
            </text>
            <g
              class="topo-lab-edit"
              transform={`translate(${labPos().x + 10} ${labPos().y + 41})`}
              onPointerDown={(e: PointerEvent) => e.stopPropagation()}
              onClick={(e: MouseEvent) => {
                e.stopPropagation();
                props.onEditConfig();
              }}
            >
              <rect width="118" height="17" rx="4" />
              <g transform="translate(5 3)">
                <FilePenLine size={11} />
              </g>
              <text x="21" y="12">Edit vmlab.wcl</text>
              <title>Open vmlab.wcl in the config editor</title>
            </g>
            <title>Lab-wide properties: provisions, event handlers, DNS</title>
          </g>

          {/* segment bars */}
          <For each={model().segments}>
            {(seg) => {
              const p = () => segPos(seg.name);
              const w = () => barWidth(seg.name);
              return (
                <g
                  class="topo-seg"
                  classList={{ selected: selectedSeg() === seg.name }}
                  onPointerDown={(e: PointerEvent) => nodeDown(e, "segment", seg.name)}
                >
                  <rect x={p().x} y={p().y} width={w()} height={SEG_H} rx="8" />
                  <Sockets barKey={seg.name} pos={p()} />
                  <SidePorts barKey={seg.name} />
                  <text x={p().x + 12} y={p().y + SEG_H / 2 + 4} class="topo-seg-name">
                    {seg.name}
                  </text>
                  <text x={p().x + w() - 12} y={p().y + SEG_H / 2 + 4} class="topo-seg-meta">
                    {[
                      seg.subnet ?? "auto subnet",
                      seg.nat ? "nat" : null,
                      seg.global ? "global" : null,
                      seg.dhcp ? null : "dhcp off",
                    ]
                      .filter(Boolean)
                      .join(" · ")}
                  </text>
                </g>
              );
            }}
          </For>

          {/* the WAN object: an internet cloud — plug VM NICs in for per-NIC
              NAT, or cable a switch's side port to it for segment egress.
              The interactive body keeps bar geometry (sockets, side ports,
              drop bounds); the cloud is fill-only dressing around it. */}
          <g
            class="topo-seg topo-nat"
            classList={{ selected: editor.selection.kind === "nat" }}
            onPointerDown={(e: PointerEvent) => nodeDown(e, "nat", "__nat__")}
          >
            {(() => {
              const p = () => natPos();
              const w = () => barWidth(NAT_KEY); // fixed WAN_W
              // Three cloud bumps along the top edge.
              const bumps = () => {
                const step = (w() - 48) / 2;
                return Array.from({ length: 3 }, (_, i) => ({
                  cx: p().x + 24 + i * step,
                  cy: p().y + 4,
                  r: i % 2 === 0 ? 14 : 10,
                }));
              };
              const gx = () => p().x + WAN_W / 2 - 20; // globe glyph centre
              const gy = () => p().y + SEG_H / 2;
              return (
                <>
                  <Show when={editor.selection.kind === "nat"}>
                    <rect
                      x={p().x - 6}
                      y={p().y - 18}
                      width={w() + 12}
                      height={SEG_H + 26}
                      rx="16"
                      class="topo-wan-outline"
                    />
                  </Show>
                  <For each={bumps()}>
                    {(b) => <circle cx={b.cx} cy={b.cy} r={b.r} class="topo-wan-cloud" />}
                  </For>
                  <rect
                    x={p().x}
                    y={p().y}
                    width={w()}
                    height={SEG_H}
                    rx="14"
                    class="topo-wan-cloud"
                  />
                  {/* one port per side; cables land on the closest one */}
                  <For each={WAN_SIDES}>
                    {(side) => {
                      const pt = () => wanPorts()[side];
                      return (
                        <rect
                          x={pt().x - (PORT_SIZE + 2) / 2}
                          y={pt().y - (PORT_SIZE + 2) / 2}
                          width={PORT_SIZE + 2}
                          height={PORT_SIZE + 2}
                          rx="2"
                          class="topo-socket"
                          classList={{ lit: wanSideUsed()[side], locked: anyVmRunning() }}
                          onPointerDown={(e: PointerEvent) =>
                            socketDown(e, NAT_KEY, 0, pt().x, pt().y)
                          }
                        >
                          <title>
                            {anyVmRunning()
                              ? "Networking is read-only while a machine is up"
                              : "Drag onto a VM for a NAT NIC, or onto a switch for segment egress"}
                          </title>
                        </rect>
                      );
                    }}
                  </For>
                  {/* a small globe: circle + meridian + equator */}
                  <g class="topo-wan-globe">
                    <circle cx={gx()} cy={gy()} r="7" />
                    <ellipse cx={gx()} cy={gy()} rx="3" ry="7" />
                    <line x1={gx() - 7} y1={gy()} x2={gx() + 7} y2={gy()} />
                  </g>
                  <text x={gx() + 13} y={gy() + 4} class="topo-wan-name">
                    NAT
                  </text>
                </>
              );
            })()}
          </g>

          {/* VM nodes */}
          <For each={model().vms}>
            {(vm, vi) => {
              const p = () => vmPos(vm.name);
              const hw = () => {
                const tpl = storeTemplateFor(vm.template);
                const cpus = vm.cpus ?? tpl?.cpus ?? null;
                const mem = vm.memory ?? tpl?.memory ?? null;
                return `${cpus ?? "?"} vCPU · ${mem != null ? formatMemory(mem) : "?"}`;
              };
              const badgeW = () => hw().length * 5.6 + 12;
              let gEl!: SVGGElement;
              // Post-mount so gEl is set; re-runs on rename, unregisters on
              // row disposal (destroy fx looks nodes up by machine name).
              createEffect(() => onCleanup(registerFxNode(`vm:${vm.name}`, gEl)));
              return (
                <g
                  ref={gEl}
                  class="topo-vm"
                  classList={{ selected: selectedVm() === vm.name, locked: vmIsUp(vm.name) }}
                  onPointerDown={(e: PointerEvent) => nodeDown(e, "vm", vm.name)}
                >
                  <rect x={p().x} y={p().y} width={VM_W} height={VM_H} rx="10" />
                  <OsIcon os={osStringFor(vm)} x={p().x + 10} y={p().y + 9} />
                  <text x={p().x + 34} y={p().y + 21} class="topo-vm-name">
                    {vm.name}
                  </text>
                  <text x={p().x + 34} y={p().y + 35} class="topo-vm-meta">
                    {vm.template === "scratch"
                      ? "scratch"
                      : (vm.template.split("/").pop() ?? vm.template).split("@")[0] ||
                        "(no template)"}
                  </text>
                  {/* hardware badge */}
                  <g class="topo-vm-badge">
                    <rect
                      x={p().x + 10}
                      y={p().y + VM_H - 22}
                      width={badgeW()}
                      height="14"
                      rx="7"
                    />
                    <text x={p().x + 16} y={p().y + VM_H - 11.5}>
                      {hw()}
                    </text>
                  </g>
                  {/* power LED (live daemon state) */}
                  <circle
                    cx={p().x + VM_W - 12}
                    cy={p().y + 12}
                    r="4"
                    class={`topo-led ${ledTone(vm.name)}`}
                  >
                    <title>{ledLabel(vm.name)}</title>
                  </circle>
                  {/* power / restart / console buttons */}
                  <VmBtn
                    x={p().x + VM_W - 70}
                    y={p().y + VM_H - 24}
                    act="power"
                    title={vmRunning(vm.name) ? "Stop" : "Start"}
                    onClick={() => (vmRunning(vm.name) ? vmStop(vm.name) : vmStart(vm.name))}
                  >
                    <Show when={vmRunning(vm.name)} fallback={<Play size={11} />}>
                      <Square size={11} />
                    </Show>
                  </VmBtn>
                  <VmBtn
                    x={p().x + VM_W - 49}
                    y={p().y + VM_H - 24}
                    act="restart"
                    title="Restart"
                    onClick={() => vmRestart(vm.name)}
                  >
                    <RotateCw size={11} />
                  </VmBtn>
                  <VmBtn
                    x={p().x + VM_W - 28}
                    y={p().y + VM_H - 24}
                    act="console"
                    title="Open the console"
                    onClick={() => showVm(vm.name)}
                  >
                    <Monitor size={11} />
                  </VmBtn>
                  {/* NIC port dots: cable ends — drag to re-home/unplug,
                      and drop targets while cabling out from a bar. A NIC
                      with no segment/NAT shows as a hollow loose port. */}
                  <Index each={vm.nics}>
                    {(nic, i) => (
                      <circle
                        cx={p().x + 18 + i * 16}
                        cy={p().y + VM_H}
                        r={socketDrag() ? 7 : 4}
                        class="topo-port"
                        classList={{
                          target: socketDrag() !== null,
                          loose: !nic().segment && !nic().nat,
                          locked: anyVmRunning(),
                        }}
                        onPointerDown={(e: PointerEvent) => dotDown(e, "vm", vi(), i)}
                      >
                        <title>
                          {anyVmRunning()
                            ? "Networking is read-only while a machine is up"
                            : !nic().segment && !nic().nat
                            ? "Unplugged NIC — drag onto a switch or the WAN to connect"
                            : "Drag to another bar to move (empty space unplugs)"}
                        </title>
                      </circle>
                    )}
                  </Index>
                </g>
              );
            }}
          </For>

          {/* container nodes: same footprint as VMs, visually distinct —
              container glyph, dashed outline, image reference as the meta
              line, no console button (containers have no display) */}
          <For each={model().containers}>
            {(ctr, ci) => {
              const p = () => ctrPos(ctr.name);
              const hw = () => {
                // Schema defaults: 1 vCPU, 256MiB micro-VM.
                const cpus = ctr.cpus ?? 1;
                const mem = ctr.memory ?? 256 * 1024 * 1024;
                return `${cpus} vCPU · ${formatMemory(mem)}`;
              };
              const badgeW = () => hw().length * 5.6 + 12;
              let gEl!: SVGGElement;
              createEffect(() => onCleanup(registerFxNode(`container:${ctr.name}`, gEl)));
              return (
                <g
                  ref={gEl}
                  class="topo-vm topo-ctr"
                  classList={{ selected: selectedCtr() === ctr.name, locked: containerIsUp(ctr.name) }}
                  onPointerDown={(e: PointerEvent) => nodeDown(e, "container", ctr.name)}
                >
                  <rect x={p().x} y={p().y} width={VM_W} height={VM_H} rx="10" />
                  <g class="topo-ctr-glyph" transform={`translate(${p().x + 10} ${p().y + 9})`}>
                    <Container size={18} />
                  </g>
                  <text x={p().x + 34} y={p().y + 21} class="topo-vm-name">
                    {ctr.name}
                  </text>
                  <text x={p().x + 34} y={p().y + 35} class="topo-vm-meta">
                    {ctr.image || "(no image)"}
                  </text>
                  {/* hardware badge */}
                  <g class="topo-vm-badge">
                    <rect
                      x={p().x + 10}
                      y={p().y + VM_H - 22}
                      width={badgeW()}
                      height="14"
                      rx="7"
                    />
                    <text x={p().x + 16} y={p().y + VM_H - 11.5}>
                      {hw()}
                    </text>
                  </g>
                  {/* power LED (live daemon state, incl. health) */}
                  <circle
                    cx={p().x + VM_W - 12}
                    cy={p().y + 12}
                    r="4"
                    class={`topo-led ${ctrLedTone(ctr.name)}`}
                  >
                    <title>{ctrLedLabel(ctr.name)}</title>
                  </circle>
                  {/* power / restart buttons — no console for containers */}
                  <VmBtn
                    x={p().x + VM_W - 49}
                    y={p().y + VM_H - 24}
                    act="power"
                    title={ctrRunning(ctr.name) ? "Stop" : "Start"}
                    onClick={() =>
                      ctrRunning(ctr.name) ? containerStop(ctr.name) : containerStart(ctr.name)
                    }
                  >
                    <Show when={ctrRunning(ctr.name)} fallback={<Play size={11} />}>
                      <Square size={11} />
                    </Show>
                  </VmBtn>
                  <VmBtn
                    x={p().x + VM_W - 28}
                    y={p().y + VM_H - 24}
                    act="restart"
                    title="Restart"
                    onClick={() => containerRestart(ctr.name)}
                  >
                    <RotateCw size={11} />
                  </VmBtn>
                  {/* NIC port dots (no NICs = air-gapped) */}
                  <Index each={ctr.nics}>
                    {(nic, i) => (
                      <circle
                        cx={p().x + 18 + i * 16}
                        cy={p().y + VM_H}
                        r={socketDrag() ? 7 : 4}
                        class="topo-port"
                        classList={{
                          target: socketDrag() !== null,
                          loose: !nic().segment && !nic().nat,
                          locked: anyVmRunning(),
                        }}
                        onPointerDown={(e: PointerEvent) => dotDown(e, "container", ci(), i)}
                      >
                        <title>
                          {anyVmRunning()
                            ? "Networking is read-only while a machine is up"
                            : !nic().segment && !nic().nat
                            ? "Unplugged NIC — drag onto a switch or the WAN to connect"
                            : "Drag to another bar to move (empty space unplugs)"}
                        </title>
                      </circle>
                    )}
                  </Index>
                </g>
              );
            }}
          </For>
        </g>
      </svg>
    </div>
  );
}
