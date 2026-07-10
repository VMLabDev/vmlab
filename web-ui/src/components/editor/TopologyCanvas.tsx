// The SVG topology canvas: segments as bus bars, VMs as boxes, NICs as
// edges dropping onto the bars. One world-transform group gives pan/zoom;
// positions are cosmetic and live in localStorage (editor/layout.ts).
//
// Interactions: click = select (background = lab), drag node = move,
// drag background = pan, wheel = zoom-to-cursor, drag from a VM's port
// strip onto a bar = create a NIC, Delete = remove selection.

import { For, Index, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import { Button } from "@forge/ui";
import { Expand, LayoutGrid, Monitor, Waypoints } from "lucide-solid";
import type { Layout, NodePos } from "../../editor/layout";
import {
  PORT_SIZE,
  PORT_SPACING,
  PORT_X0,
  SEG_H,
  VM_H,
  VM_W,
  autoLayout,
  hasNatNic,
  loadLayout,
  portCapacity,
  renameInLayout,
  saveLayout,
  segWidthFor,
} from "../../editor/layout";
import type { LabModel, NicModel } from "../../editor/model";
import {
  addNic,
  addSegment,
  addVm,
  editor,
  removeSegment,
  removeVm,
  select,
} from "../../editor/store";
import { confirmDialog } from "../dialogs";

interface Drag {
  kind: "vm" | "segment" | "nat";
  name: string;
  dx: number;
  dy: number;
  moved: boolean;
}

interface NicDrag {
  vmIndex: number;
  x: number;
  y: number;
}

export default function TopologyCanvas() {
  let svg: SVGSVGElement | undefined;
  const model = () => editor.draft!;
  const lab = () => editor.lab!;

  const [layout, setLayout] = createSignal<Layout>({ vms: {}, segments: {} });
  const [view, setView] = createSignal({ tx: 0, ty: 0, k: 1 });
  const [drag, setDrag] = createSignal<Drag | null>(null);
  const [pan, setPan] = createSignal<{ sx: number; sy: number; tx: number; ty: number } | null>(
    null,
  );
  const [nicDrag, setNicDrag] = createSignal<NicDrag | null>(null);

  // (Re)seed layout whenever the lab or the set of node names changes.
  createEffect(() => {
    const l = lab();
    const names = [...model().vms.map((v) => v.name), ...model().segments.map((s) => s.name)];
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
  const segPos = (name: string): NodePos => layout().segments[name] ?? { x: 60, y: 200 };
  const natPos = (): NodePos =>
    layout().nat ?? { x: 60, y: 200 + model().segments.length * 170 };

  const showNat = createMemo(() => hasNatNic(model()) || nicDrag() !== null);

  // --- port assignment --------------------------------------------------------
  // Each bar (segment or the NAT bus) owns a bank of sockets; every attached
  // NIC claims the next one in declaration order (vm order, then nic order),
  // so edges spread left-to-right like a patch panel.

  const NAT_KEY = "__nat__";

  const ports = createMemo(() => {
    const byNic = new Map<string, number>(); // `${vm}:${nicIndex}` → socket index
    const used = new Map<string, number>(); // bar key → sockets in use
    for (const vm of model().vms) {
      vm.nics.forEach((nic, i) => {
        const key = nic.nat ? NAT_KEY : nic.segment;
        if (!key) return;
        const n = used.get(key) ?? 0;
        byNic.set(`${vm.name}:${i}`, n);
        used.set(key, n + 1);
      });
    }
    return { byNic, used };
  });

  const barUsed = (key: string) => ports().used.get(key) ?? 0;
  const barWidth = (key: string) => segWidthFor(barUsed(key));
  const barCapacity = (key: string) => portCapacity(barWidth(key));
  const socketX = (barX: number, idx: number) => barX + PORT_X0 + idx * PORT_SPACING;
  const vmPortX = (vp: NodePos, nicIndex: number) => vp.x + 18 + nicIndex * 16;

  // --- interactions ---------------------------------------------------------

  function nodeDown(e: PointerEvent, kind: Drag["kind"], name: string) {
    e.stopPropagation();
    const w = world(e);
    const pos = kind === "vm" ? vmPos(name) : kind === "segment" ? segPos(name) : natPos();
    setDrag({ kind, name, dx: w.x - pos.x, dy: w.y - pos.y, moved: false });
    (e.currentTarget as Element).setPointerCapture?.(e.pointerId);
  }

  function portDown(e: PointerEvent, vmIndex: number) {
    e.stopPropagation();
    const w = world(e);
    setNicDrag({ vmIndex, x: w.x, y: w.y });
    svg?.setPointerCapture(e.pointerId);
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
          : d.kind === "segment"
            ? { ...l, segments: { ...l.segments, [d.name]: pos } }
            : { ...l, nat: pos },
      );
      return;
    }
    const nd = nicDrag();
    if (nd) {
      const w = world(e);
      setNicDrag({ ...nd, x: w.x, y: w.y });
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
        } else if (d.kind === "segment") {
          const i = model().segments.findIndex((s) => s.name === d.name);
          if (i >= 0) select({ kind: "segment", index: i });
        } else {
          select({ kind: "nat" });
        }
      } else {
        persist();
      }
      setDrag(null);
      return;
    }
    const nd = nicDrag();
    if (nd) {
      const w = world(e);
      // Drop on a bar → create the NIC.
      for (const s of model().segments) {
        const p = segPos(s.name);
        if (
          w.x >= p.x &&
          w.x <= p.x + barWidth(s.name) &&
          w.y >= p.y - 8 &&
          w.y <= p.y + SEG_H + 8
        ) {
          addNic(nd.vmIndex, s.name);
          setNicDrag(null);
          return;
        }
      }
      if (showNat()) {
        const p = natPos();
        if (
          w.x >= p.x &&
          w.x <= p.x + barWidth(NAT_KEY) &&
          w.y >= p.y - 8 &&
          w.y <= p.y + SEG_H + 8
        ) {
          addNic(nd.vmIndex, null);
        }
      }
      setNicDrag(null);
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
      if (
        vm &&
        (await confirmDialog({ title: `Delete VM "${vm.name}"?`, danger: true }))
      ) {
        removeVm(sel.index);
      }
    } else if (sel.kind === "segment") {
      const seg = model().segments[sel.index];
      if (
        seg &&
        (await confirmDialog({ title: `Delete segment "${seg.name}"?`, danger: true }))
      ) {
        removeSegment(sel.index);
      }
    }
  }

  function arrange() {
    const fresh = autoLayout(model(), { vms: {}, segments: {} });
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
    for (const s of model().segments) {
      const p = segPos(s.name);
      xs.push(p.x, p.x + barWidth(s.name));
      ys.push(p.y, p.y + SEG_H);
    }
    if (showNat()) {
      const p = natPos();
      xs.push(p.x, p.x + barWidth(NAT_KEY));
      ys.push(p.y, p.y + SEG_H);
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
    const names = [...model().vms.map((v) => v.name), ...model().segments.map((s) => s.name)];
    if (lastNames.length === names.length) {
      const changed = names.findIndex((n, i) => n !== lastNames[i]);
      if (changed >= 0 && names.filter((n, i) => n !== lastNames[i]).length === 1) {
        const kind = changed < model().vms.length ? "vms" : "segments";
        setLayout((l) => renameInLayout(l, kind, lastNames[changed], names[changed]));
      }
    }
    lastNames = names;
  });

  onCleanup(() => persist());

  // --- geometry helpers -------------------------------------------------------

  const selectedVm = () =>
    editor.selection.kind === "vm" ? model().vms[editor.selection.index]?.name : null;
  const selectedSeg = () =>
    editor.selection.kind === "segment" ? model().segments[editor.selection.index]?.name : null;

  /** The router-style socket bank straddling a bar's top and bottom edges.
   *  Sockets left of the used count are lit (an edge terminates there). */
  function Sockets(props: { barKey: string; pos: NodePos }) {
    return (
      <Index each={Array.from({ length: barCapacity(props.barKey) })}>
        {(_, i) => {
          const cx = () => socketX(props.pos.x, i);
          const lit = () => i < barUsed(props.barKey);
          return (
            <>
              <rect
                x={cx() - PORT_SIZE / 2}
                y={props.pos.y - PORT_SIZE / 2}
                width={PORT_SIZE}
                height={PORT_SIZE}
                rx="1.5"
                class="topo-socket"
                classList={{ lit: lit() }}
              />
              <rect
                x={cx() - PORT_SIZE / 2}
                y={props.pos.y + SEG_H - PORT_SIZE / 2}
                width={PORT_SIZE}
                height={PORT_SIZE}
                rx="1.5"
                class="topo-socket"
                classList={{ lit: lit() }}
              />
            </>
          );
        }}
      </Index>
    );
  }

  /** Edge from a VM's port dot to its assigned socket on the target bar. */
  function edgePath(vmName: string, nicIndex: number, nic: NicModel): string | null {
    const key = nic.nat ? NAT_KEY : nic.segment;
    if (!key || (nic.nat && !showNat())) return null;
    const bar = nic.nat ? natPos() : segPos(key);
    const socket = ports().byNic.get(`${vmName}:${nicIndex}`);
    if (socket === undefined) return null;
    const vp = vmPos(vmName);
    const px = vmPortX(vp, nicIndex);
    const py = vp.y + VM_H;
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
        <Button size="sm" icon={Waypoints} onClick={() => addSegment()}>
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
        onPointerMove={move}
        onPointerUp={up}
        onWheel={wheel}
        onKeyDown={onKey}
      >
        <g transform={`translate(${view().tx} ${view().ty}) scale(${view().k})`}>
          {/* NIC edges under the nodes */}
          <For each={model().vms}>
            {(vm) => (
              <Index each={vm.nics}>
                {(nic, i) => {
                  const path = () => edgePath(vm.name, i, nic());
                  return (
                    <Show when={path()}>
                      <g
                        class="topo-edge"
                        onPointerDown={(e: PointerEvent) => {
                          e.stopPropagation();
                          const vi = model().vms.findIndex((v) => v.name === vm.name);
                          if (vi >= 0) select({ kind: "vm", index: vi });
                        }}
                      >
                        <path d={path()!} class="topo-edge-hit" />
                        <path d={path()!} class="topo-edge-line" />
                        <Show when={nic().ip}>
                          <text
                            x={vmPos(vm.name).x + 22 + i * 16}
                            y={vmPos(vm.name).y + VM_H + 16}
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

          {/* live NIC-drag rubber band */}
          <Show when={nicDrag()}>
            {(nd) => {
              const vm = () => model().vms[nd().vmIndex];
              const vp = () => vmPos(vm().name);
              return (
                <path
                  class="topo-edge-draft"
                  d={`M ${vp().x + 18 + vm().nics.length * 16} ${vp().y + VM_H} L ${nd().x} ${nd().y}`}
                />
              );
            }}
          </Show>

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

          {/* built-in NAT bus */}
          <Show when={showNat()}>
            <g
              class="topo-seg topo-nat"
              classList={{ selected: editor.selection.kind === "nat" }}
              onPointerDown={(e: PointerEvent) => nodeDown(e, "nat", "__nat__")}
            >
              <rect
                x={natPos().x}
                y={natPos().y}
                width={barWidth(NAT_KEY)}
                height={SEG_H}
                rx="8"
              />
              <Sockets barKey={NAT_KEY} pos={natPos()} />
              <text x={natPos().x + 12} y={natPos().y + SEG_H / 2 + 4} class="topo-seg-name">
                NAT ⇄ internet
              </text>
              <text
                x={natPos().x + barWidth(NAT_KEY) - 12}
                y={natPos().y + SEG_H / 2 + 4}
                class="topo-seg-meta"
              >
                built-in
              </text>
            </g>
          </Show>

          {/* VM nodes */}
          <For each={model().vms}>
            {(vm, vi) => {
              const p = () => vmPos(vm.name);
              return (
                <g
                  class="topo-vm"
                  classList={{ selected: selectedVm() === vm.name }}
                  onPointerDown={(e: PointerEvent) => nodeDown(e, "vm", vm.name)}
                >
                  <rect x={p().x} y={p().y} width={VM_W} height={VM_H} rx="10" />
                  <text x={p().x + 12} y={p().y + 24} class="topo-vm-name">
                    {vm.name}
                  </text>
                  <text x={p().x + 12} y={p().y + 44} class="topo-vm-meta">
                    {vm.template === "scratch"
                      ? "scratch"
                      : (vm.template.split("/").pop() ?? vm.template).split("@")[0] ||
                        "(no template)"}
                  </text>
                  {/* existing NIC ports */}
                  <Index each={vm.nics}>
                    {(_, i) => (
                      <circle cx={p().x + 18 + i * 16} cy={p().y + VM_H} r="4" class="topo-port" />
                    )}
                  </Index>
                  {/* the "new NIC" drag handle */}
                  <circle
                    cx={p().x + 18 + vm.nics.length * 16}
                    cy={p().y + VM_H}
                    r="6"
                    class="topo-port-new"
                    onPointerDown={(e: PointerEvent) => portDown(e, vi())}
                  >
                    <title>Drag onto a segment to add a NIC</title>
                  </circle>
                </g>
              );
            }}
          </For>
        </g>
      </svg>
    </div>
  );
}
