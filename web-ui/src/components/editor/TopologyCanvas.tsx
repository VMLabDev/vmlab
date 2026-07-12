// The SVG topology canvas: segments as bus bars, machines as patch-panel
// cards, and one shortest-side cable per declaration-ordered NIC. One world-transform group gives pan/zoom;
// positions are cosmetic and live in localStorage (editor/layout.ts).
//
// Interactions: click = select (background = lab), drag node = move,
// drag background = pan, wheel = zoom-to-cursor, Delete = remove selection.
// Cabling: a NIC's side port on the machine and its lit socket on the bar are two
// ends of the same cable — drag either onto a bar to re-home the NIC, or
// onto empty space to unplug it (the NIC stays on the VM as a loose port).
// Drag a bar's free socket onto a NIC dot to swap that NIC over, or onto
// the VM body to add a NIC there. Bars also wear interconnect ports on
// their short sides: drag one onto another switch to route the segments
// together (routes_to), or onto the WAN object to give the whole segment
// internet egress (segment nat) — grab an interconnect cable to re-home or
// remove it the same way.

import { For, Index, Show, createEffect, createMemo, createSignal, onCleanup } from "solid-js";
import { Button, Input, Modal } from "@forge/ui";
import {
  Container,
  Expand,
  EthernetPort,
  FileCode2,
  FlaskConical,
  FilePenLine,
  LayoutGrid,
  Monitor,
  Play,
  Plus,
  RotateCw,
  Router,
  Server,
  Square,
  SquareTerminal,
  Trash2,
  Waypoints,
} from "lucide-solid";
import type { Layout, NodePos } from "../../editor/layout";
import {
  MIN_FREE_PORTS,
  PORT_SIZE,
  PORT_SPACING,
  PORT_X0,
  SEG_H,
  NIC_ROW_H,
  PROVISION_H,
  PROVISION_W,
  VM_W,
  autoLayout,
  loadLayout,
  renameInLayout,
  saveLayout,
  segWidthFor,
  machineCardHeight,
  provisionLayoutKey,
} from "../../editor/layout";
import type { ContainerModel, LabModel, NicModel, VmModel } from "../../editor/model";
import type { MachineKind } from "../../editor/store";
import {
  addContainer,
  addEventHandlerTarget,
  addHostPort,
  addMachineDependency,
  addMachineNic,
  addProvisionTarget,
  addRemote,
  addSegment,
  addSegmentRoute,
  addVm,
  disconnectMachineNic,
  editor,
  eventTargetKind,
  attachHostPort,
  remoteHosts,
  removeContainer,
  removeEventHandlerTarget,
  removeMachineDependency,
  removeProvision,
  removeProvisionTarget,
  removeRemote,
  removeSegment,
  removeSegmentRoute,
  removeVm,
  select,
  setEditor,
  setHostPortDraft,
  setMachineNicGateway,
  setMachineNicTarget,
  setSegmentNat,
  setSegmentPeer,
  storeTemplateFor,
  removeHostPortDraft,
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
  showContainer,
  showVm,
  state,
  vmIsUp,
  vmRestart,
  vmStart,
  vmStop,
} from "../../store";
import { confirmDialog } from "../dialogs";
import { registerFxNode } from "../../fx";

interface Drag {
  kind: "vm" | "container" | "provision" | "segment" | "nat" | "remote" | "host";
  name: string;
  dx: number;
  dy: number;
  moved: boolean;
}

interface HostPortDrag {
  draftId: string;
  moved: boolean;
  x: number;
  y: number;
}

type HostPortEntry =
  | {
      key: string;
      source: "draft";
      draftId: string;
      hostPort: number;
      guestPort: null;
      target: null;
    }
  | {
      key: string;
      source: "forward";
      segmentIndex: number;
      portIndex: number;
      hostPort: number;
      guestPort: number;
      target: { kind: MachineKind; index: number; name: string } | null;
    }
  | {
      key: string;
      source: "container";
      containerIndex: number;
      portIndex: number;
      hostPort: number;
      guestPort: number;
      target: { kind: "container"; index: number; name: string };
    };

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
 *  re-homing an existing segment↔segment / segment↔WAN / segment↔peer link. */
interface LinkDrag {
  /** Source bar: a segment name, or NAT_KEY when cabling out of the WAN. */
  from: string;
  existing:
    | { kind: "route"; to: string }
    | { kind: "wan" }
    | { kind: "peer"; to: string }
    | null;
  moved: boolean;
  x: number;
  y: number;
}

/** A directional startup dependency being created or re-homed. */
interface DependencyDrag {
  kind: MachineKind;
  index: number;
  /** Target name when an existing arrow is being re-homed. */
  existing: string | null;
  moved: boolean;
  x: number;
  y: number;
}

interface ProvisionTargetDrag {
  provisionIndex: number;
  existing: string | null;
  moved: boolean;
  x: number;
  y: number;
}

interface EventTargetDrag {
  handlerIndex: number;
  existing: string | null;
  moved: boolean;
  x: number;
  y: number;
}

function PortNumberEditor(props: {
  x: number;
  y: number;
  value: number;
  valid: boolean;
  disabled?: boolean;
  label: string;
  onChange: (value: number) => void;
}) {
  let input!: HTMLInputElement;
  const [draft, setDraft] = createSignal(String(props.value));
  const [editing, setEditing] = createSignal(false);

  // External changes (revert/reload, or another editor surface) update the
  // chip, but never overwrite a value while the user is actively typing.
  createEffect(() => {
    const value = String(props.value);
    if (!editing()) setDraft(value);
  });

  const commit = () => {
    const raw = draft().trim();
    const value = raw === "" ? 0 : Number(raw);
    if (Number.isFinite(value)) props.onChange(value);
    else setDraft(String(props.value));
  };

  return (
    <foreignObject x={props.x} y={props.y} width="48" height="24" class="topo-port-editor">
      <input
        ref={input}
        aria-label={props.label}
        classList={{ invalid: !props.valid }}
        type="number"
        min="0"
        max="65535"
        disabled={props.disabled}
        value={draft()}
        onPointerDown={(e) => e.stopPropagation()}
        onClick={(e) => e.stopPropagation()}
        onFocus={() => setEditing(true)}
        onInput={(e) => setDraft(e.currentTarget.value)}
        onBlur={() => {
          commit();
          setEditing(false);
        }}
        onKeyDown={(e) => {
          e.stopPropagation();
          if (e.key === "Enter") {
            e.preventDefault();
            input.blur();
          } else if (e.key === "Escape") {
            setDraft(String(props.value));
            input.blur();
          }
        }}
      />
    </foreignObject>
  );
}

function NicIpEditor(props: {
  x: number;
  y: number;
  staticIp: string | null;
  assignedIp: string | null;
  disabled: boolean;
  staticAllowed: boolean;
  gateway: boolean;
  gatewayAllowed: boolean;
  validate: (value: string) => string | null;
  onChange: (value: string | null) => void;
  onGatewayChange: (enabled: boolean) => void;
}) {
  const [draft, setDraft] = createSignal(props.staticIp ?? "");
  const [open, setOpen] = createSignal(false);
  createEffect(() => {
    if (!open()) setDraft(props.staticIp ?? "");
  });
  const error = () => {
    const value = draft().trim();
    return value ? props.validate(value) : null;
  };
  const mode = () => (props.gateway ? "GATEWAY" : props.staticIp ? "STATIC" : "AUTO");
  const address = () => props.staticIp ?? props.assignedIp ?? "awaiting address";
  const close = () => setOpen(false);
  const canApply = () => {
    const value = draft().trim();
    return !props.disabled && (!value || (props.staticAllowed && !error()));
  };
  const apply = () => {
    if (!canApply()) return;
    props.onChange(draft().trim() || null);
    close();
  };

  return (
    <>
      <foreignObject
        x={props.x}
        y={props.y}
        width={124}
        height={23}
        class="topo-nic-ip-object"
        onPointerDown={(e) => e.stopPropagation()}
        onClick={(e) => e.stopPropagation()}
      >
        <Button size="sm" variant="ghost" onClick={() => setOpen(true)}>
          <span class="topo-nic-ip-label">
            <span
              class={`topo-nic-mode ${props.gateway ? "gateway" : props.staticIp ? "static" : "auto"}`}
            >
              {mode()}
            </span>
            <span classList={{ unknown: !props.staticIp && !props.assignedIp }}>{address()}</span>
          </span>
        </Button>
      </foreignObject>
      <Modal
        open={open()}
        title="IPv4 assignment"
        onClose={close}
        footer={
          <Button variant="ghost" onClick={close}>
            Close
          </Button>
        }
      >
        <div
          class="topo-nic-ip-modal"
          onPointerDown={(e) => e.stopPropagation()}
          onClick={(e) => e.stopPropagation()}
          onKeyDown={(e) => e.stopPropagation()}
          onKeyUp={(e) => e.stopPropagation()}
        >
          <Show
            when={props.gateway}
            fallback={
              <>
                <Input
                  label="IP address"
                  placeholder="Leave blank for automatic assignment"
                  value={draft()}
                  disabled={props.disabled}
                  error={
                    !!draft().trim() && (!props.staticAllowed || !!error())
                  }
                  onInput={(e) => setDraft(e.currentTarget.value)}
                  onKeyDown={(e) => {
                    if (e.key === "Enter") {
                      e.preventDefault();
                      apply();
                    }
                  }}
                />
                <div class="topo-nic-ip-hint">
                  Enter an address to make this NIC static. Leave it blank to use automatic
                  assignment.
                </div>
                <Show when={!props.staticAllowed}>
                  <div class="topo-nic-ip-error">
                    Static addresses require a declared network segment.
                  </div>
                </Show>
                <Show when={props.staticAllowed && draft().trim() && error()}>
                  {(message) => <div class="topo-nic-ip-error">{message()}</div>}
                </Show>
                <Button
                  variant="primary"
                  disabled={!canApply()}
                  onClick={apply}
                >
                  Apply address
                </Button>
                <div class="topo-nic-gateway-card">
                  <div>
                    <strong>Dedicated gateway mode</strong>
                    <span>
                      Claims the segment router address and sends routed traffic to this machine.
                    </span>
                  </div>
                  <Button
                    variant="primary"
                    disabled={props.disabled || !props.gatewayAllowed}
                    onClick={() => {
                      props.onGatewayChange(true);
                      close();
                    }}
                  >
                    Make gateway
                  </Button>
                </div>
              </>
            }
          >
            <div class="topo-nic-gateway-active">
              <span class="topo-nic-mode gateway">GATEWAY</span>
              <strong>{address()}</strong>
              <p>
                This NIC owns the segment router address. Automatic and ordinary static addressing
                are disabled while gateway mode is active.
              </p>
              <Button
                variant="danger"
                disabled={props.disabled}
                onClick={() => {
                  props.onGatewayChange(false);
                  setDraft("");
                  close();
                }}
              >
                Leave gateway mode
              </Button>
            </div>
          </Show>
          <Show when={props.disabled}>
            <div class="topo-nic-ip-error">Stop this machine before changing its address.</div>
          </Show>
        </div>
      </Modal>
    </>
  );
}

export default function TopologyCanvas(props: {
  onEditConfig: () => void;
  onAddProvision: () => void;
  onEditProvision: (path: string) => void;
}) {
  let svg: SVGSVGElement | undefined;
  const model = () => editor.draft!;
  const lab = () => editor.lab!;

  const [layout, setLayout] = createSignal<Layout>({
    vms: {},
    containers: {},
    segments: {},
    provisions: {},
  });
  const [view, setView] = createSignal({ tx: 0, ty: 0, k: 1 });
  const [drag, setDrag] = createSignal<Drag | null>(null);
  const [pan, setPan] = createSignal<{ sx: number; sy: number; tx: number; ty: number } | null>(
    null,
  );
  const [connDrag, setConnDrag] = createSignal<ConnDrag | null>(null);
  const [socketDrag, setSocketDrag] = createSignal<SocketDrag | null>(null);
  const [linkDrag, setLinkDrag] = createSignal<LinkDrag | null>(null);
  const [dependencyDrag, setDependencyDrag] = createSignal<DependencyDrag | null>(null);
  const [provisionTargetDrag, setProvisionTargetDrag] = createSignal<ProvisionTargetDrag | null>(
    null,
  );
  const [eventTargetDrag, setEventTargetDrag] = createSignal<EventTargetDrag | null>(null);
  const [hostPortDrag, setHostPortDrag] = createSignal<HostPortDrag | null>(null);

  // (Re)seed layout whenever the lab or the set of node names changes.
  createEffect(() => {
    const l = lab();
    const names = [
      ...model().vms.map((v) => v.name),
      ...model().containers.map((c) => c.name),
      ...model().segments.map((s) => s.name),
      ...model().provisions.map((provision) => provision.script),
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

  const vmPosIn = (l: Layout, name: string): NodePos => l.vms[name] ?? { x: 40, y: 40 };
  const ctrPosIn = (l: Layout, name: string): NodePos => l.containers[name] ?? { x: 40, y: 40 };
  const segPosIn = (l: Layout, name: string): NodePos => l.segments[name] ?? { x: 60, y: 200 };
  const vmPos = (name: string): NodePos => vmPosIn(layout(), name);
  const ctrPos = (name: string): NodePos => ctrPosIn(layout(), name);
  const provisionKey = (index: number) => provisionLayoutKey(model(), index);
  const provisionPosIn = (l: Layout, index: number): NodePos =>
    l.provisions[provisionLayoutKey(model(), index)] ?? { x: 80, y: 360 };
  const provisionPos = (index: number): NodePos => provisionPosIn(layout(), index);
  /** VMs and containers share the same node geometry on the canvas. */
  const machinePos = (kind: MachineKind, name: string): NodePos =>
    kind === "vm" ? vmPos(name) : ctrPos(name);
  const machinesOf = (kind: MachineKind): (VmModel | ContainerModel)[] =>
    kind === "vm" ? model().vms : model().containers;
  const machineByName = (kind: MachineKind, name: string) =>
    machinesOf(kind).find((machine) => machine.name === name);
  const machineHeight = (kind: MachineKind, name: string) =>
    machineCardHeight(machineByName(kind, name)?.nics.length ?? 0);
  const machineRef = (name: string) => {
    const vmIndex = model().vms.findIndex((machine) => machine.name === name);
    if (vmIndex >= 0) return { kind: "vm" as const, index: vmIndex };
    const containerIndex = model().containers.findIndex((machine) => machine.name === name);
    return containerIndex >= 0
      ? { kind: "container" as const, index: containerIndex }
      : null;
  };

  /** Every machine has one fan-out socket for startup dependencies. */
  const dependencyPort = (kind: MachineKind, name: string): NodePos => {
    const p = machinePos(kind, name);
    return { x: p.x + VM_W / 2, y: p.y };
  };

  /** Point where the dependency arrow meets the target card's nearest edge. */
  function dependencyTarget(kind: MachineKind, name: string, from: NodePos): NodePos {
    const p = machinePos(kind, name);
    const halfW = VM_W / 2;
    const halfH = machineHeight(kind, name) / 2;
    const center = { x: p.x + halfW, y: p.y + halfH };
    const dx = from.x - center.x;
    const dy = from.y - center.y;
    const ratio = Math.max(Math.abs(dx) / halfW, Math.abs(dy) / halfH, 0.0001);
    return { x: center.x + dx / ratio, y: center.y + dy / ratio };
  }

  function dependencyPath(
    sourceKind: MachineKind,
    sourceName: string,
    targetKind: MachineKind,
    targetName: string,
  ) {
    const from = dependencyPort(sourceKind, sourceName);
    const to = dependencyTarget(targetKind, targetName, from);
    const distance = Math.hypot(to.x - from.x, to.y - from.y);
    const bend = Math.min(72, Math.max(28, distance * 0.22));
    const sourceControl = { x: from.x, y: from.y - bend };
    const verticalTarget = Math.abs(to.y - from.y) >= Math.abs(to.x - from.x);
    const targetControl = verticalTarget
      ? { x: to.x, y: to.y + (to.y < from.y ? bend : -bend) }
      : { x: to.x + (to.x < from.x ? bend : -bend), y: to.y };
    return `M ${from.x} ${from.y} C ${sourceControl.x} ${sourceControl.y}, ${targetControl.x} ${targetControl.y}, ${to.x} ${to.y}`;
  }

  const dependencyLinks = createMemo(() => {
    const links: {
      sourceKind: MachineKind;
      sourceIndex: number;
      sourceName: string;
      targetKind: MachineKind;
      targetIndex: number;
      targetName: string;
    }[] = [];
    for (const sourceKind of ["vm", "container"] as const) {
      machinesOf(sourceKind).forEach((source, sourceIndex) => {
        for (const targetName of source.depends_on) {
          const target = machineRef(targetName);
          if (target) {
            links.push({
              sourceKind,
              sourceIndex,
              sourceName: source.name,
              targetKind: target.kind,
              targetIndex: target.index,
              targetName,
            });
          }
        }
      });
    }
    return links;
  });

  const provisionLinks = createMemo(() =>
    model().provisions.flatMap((provision, provisionIndex) =>
      provision.vms.flatMap((targetName) => {
        const target = machineRef(targetName);
        return target
          ? [{ provisionIndex, targetName, targetKind: target.kind, targetIndex: target.index }]
          : [];
      }),
    ),
  );

  const SCRIPT_EVENT_ROW_H = 24;
  const handlersForProvision = (provisionIndex: number) => {
    const script = model().provisions[provisionIndex]?.script;
    return model().handlers
      .map((handler, handlerIndex) => ({ handler, handlerIndex }))
      .filter(({ handler }) => handler.run === script);
  };
  const provisionCardHeight = (index: number) =>
    PROVISION_H + handlersForProvision(index).length * SCRIPT_EVENT_ROW_H;

  const provisionPort = (index: number): NodePos => {
    const position = provisionPos(index);
    return { x: position.x + PROVISION_W, y: position.y + PROVISION_H / 2 };
  };

  function provisionLinkPath(index: number, targetKind: MachineKind, targetName: string): string {
    const from = provisionPort(index);
    const to = dependencyTarget(targetKind, targetName, from);
    const bend = Math.max(34, Math.min(90, Math.abs(to.x - from.x) * 0.35));
    return `M ${from.x} ${from.y} C ${from.x + bend} ${from.y}, ${to.x - (to.x >= from.x ? bend : -bend)} ${to.y}, ${to.x} ${to.y}`;
  }

  const eventPort = (handlerIndex: number): NodePos | null => {
    const handler = model().handlers[handlerIndex];
    if (!handler || eventTargetKind(handler.event) === null) return null;
    const provisionIndex = model().provisions.findIndex(
      (provision) => provision.script === handler.run,
    );
    if (provisionIndex < 0) return null;
    const row = handlersForProvision(provisionIndex).findIndex(
      (entry) => entry.handlerIndex === handlerIndex,
    );
    const p = provisionPos(provisionIndex);
    return {
      x: p.x + PROVISION_W,
      y: p.y + PROVISION_H + row * SCRIPT_EVENT_ROW_H + SCRIPT_EVENT_ROW_H / 2,
    };
  };

  const eventLinks = createMemo(() =>
    model().handlers.flatMap((handler, handlerIndex) =>
      handler.targets.flatMap((targetName) => {
        const target = machineRef(targetName);
        return target && eventPort(handlerIndex)
          ? [{ handlerIndex, targetName, targetKind: target.kind }]
          : [];
      }),
    ),
  );

  function eventLinkPath(handlerIndex: number, targetKind: MachineKind, targetName: string) {
    const from = eventPort(handlerIndex)!;
    const to = dependencyTarget(targetKind, targetName, from);
    const bend = Math.max(30, Math.min(82, Math.abs(to.x - from.x) * 0.32));
    return `M ${from.x} ${from.y} C ${from.x + bend} ${from.y}, ${to.x - (to.x >= from.x ? bend : -bend)} ${to.y}, ${to.x} ${to.y}`;
  }

  const dependencyKey = (source: string, target: string) => `${source}\0${target}`;

  /** An edge source→target is cyclic exactly when target can reach source.
   *  Labs are deliberately small, so a DFS per edge is clearer and less
   *  error-prone than maintaining an incremental SCC index. */
  const cyclicDependencies = createMemo(() => {
    const adjacency = new Map<string, string[]>();
    for (const machine of [...model().vms, ...model().containers]) {
      adjacency.set(
        machine.name,
        machine.depends_on.filter((target) => machineRef(target) !== null),
      );
    }
    const reaches = (from: string, target: string, seen = new Set<string>()): boolean => {
      if (from === target) return true;
      if (seen.has(from)) return false;
      seen.add(from);
      return (adjacency.get(from) ?? []).some((next) => reaches(next, target, seen));
    };
    const cyclic = new Set<string>();
    for (const link of dependencyLinks()) {
      if (reaches(link.targetName, link.sourceName)) {
        cyclic.add(dependencyKey(link.sourceName, link.targetName));
      }
    }
    return cyclic;
  });
  const segPos = (name: string): NodePos => segPosIn(layout(), name);
  const natPosIn = (l: Layout): NodePos =>
    l.nat ?? { x: 60, y: 200 + model().segments.length * 170 };
  const natPos = (): NodePos => natPosIn(layout());

  // --- port assignment --------------------------------------------------------
  // Each bar (segment or the NAT bus) owns a bank of sockets; every attached
  // NIC claims the next one in declaration order (vms, then containers, then
  // nic order), so edges spread left-to-right like a patch panel. Machine
  // names are unique across VMs and containers (one namespace), so
  // `${name}:${nicIndex}` addresses a NIC unambiguously.

  const NAT_KEY = "__nat__";

  // Remote-vmlab peer nodes are addressed as bars under a prefixed key so
  // barAt/barPos/linkEnds treat them like fixed-size switches. Segment and
  // machine names are DNS labels, so the prefix can never collide.
  const REMOTE_PREFIX = "__remote__:";
  const remoteKey = (host: string) => REMOTE_PREFIX + host;
  const remoteHostOf = (key: string): string | null =>
    key.startsWith(REMOTE_PREFIX) ? key.slice(REMOTE_PREFIX.length) : null;
  /** Remote-vmlab node footprint (fixed, like the WAN cloud). */
  const REMOTE_W = 170;
  const remotePosIn = (l: Layout, host: string): NodePos =>
    l.remotes?.[host] ?? {
      x: 60,
      y: 200 + (model().segments.length + 1) * 170 + Math.max(0, remoteHosts().indexOf(host)) * 90,
    };
  const remotePos = (host: string): NodePos => remotePosIn(layout(), host);

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
  const barWidth = (key: string) =>
    key === NAT_KEY
      ? WAN_W
      : remoteHostOf(key) !== null
        ? REMOTE_W
        : segWidthFor(barUsed(key));

  /** Live enclosure around everything owned by the lab. NAT and remote peer
   *  nodes intentionally stay outside: they are external endpoints. */
  const computeLabBounds = (l: Layout) => {
    const boxes: { x: number; y: number; w: number; h: number }[] = [];
    const forwardedTo = (name: string) =>
      model().segments.reduce(
        (count, segment) => count + segment.forwards.filter((forward) => forward.vm === name).length,
        0,
      );
    for (const vm of model().vms) {
      const p = vmPosIn(l, vm.name);
      boxes.push({
        x: p.x,
        y: p.y,
        w: VM_W,
        h: machineCardHeight(vm.nics.length) + 18 + forwardedTo(vm.name) * 18,
      });
    }
    for (const ctr of model().containers) {
      const p = ctrPosIn(l, ctr.name);
      boxes.push({
        x: p.x,
        y: p.y,
        w: VM_W,
        h:
          machineCardHeight(ctr.nics.length) +
          18 +
          (forwardedTo(ctr.name) + ctr.ports.length) * 18,
      });
    }
    for (const seg of model().segments) {
      const p = segPosIn(l, seg.name);
      boxes.push({ x: p.x, y: p.y, w: barWidth(seg.name), h: SEG_H });
    }
    model().provisions.forEach((_, index) => {
      const p = provisionPosIn(l, index);
      boxes.push({ x: p.x, y: p.y, w: PROVISION_W, h: provisionCardHeight(index) });
    });

    const minWidth = Math.max(320, 188 + model().name.length * 8);
    if (!boxes.length) return { x: 60, y: 30, width: minWidth, height: 92 };

    // Service-port editors sit just outside machine boxes; the wider padding
    // keeps those labels inside the enclosure too.
    const pad = 70;
    const header = 70;
    const minX = Math.min(...boxes.map((b) => b.x));
    const minY = Math.min(...boxes.map((b) => b.y));
    const maxX = Math.max(...boxes.map((b) => b.x + b.w));
    const maxY = Math.max(...boxes.map((b) => b.y + b.h));
    const x = minX - pad;
    const y = minY - header - 18;
    return {
      x,
      y,
      width: Math.max(minWidth, maxX - x + pad),
      height: maxY - y + pad,
    };
  };
  const labBounds = createMemo(() => computeLabBounds(layout()));

  const HOST_W = 190;
  const HOST_HEADER_H = 44;
  const HOST_PORT_H = 34;

  const hostPorts = createMemo<HostPortEntry[]>(() => {
    const entries: HostPortEntry[] = editor.hostPortDrafts.map((port) => ({
      key: `draft:${port.id}`,
      source: "draft" as const,
      draftId: port.id,
      hostPort: port.hostPort,
      guestPort: null,
      target: null,
    }));

    const target = (name: string): HostPortEntry["target"] => {
      const vm = model().vms.findIndex((machine) => machine.name === name);
      if (vm >= 0) return { kind: "vm", index: vm, name };
      const container = model().containers.findIndex((machine) => machine.name === name);
      return container >= 0 ? { kind: "container", index: container, name } : null;
    };

    model().segments.forEach((segment, segmentIndex) => {
      segment.forwards.forEach((forward, portIndex) => {
        entries.push({
          key: `forward:${segment.name}:${portIndex}`,
          source: "forward",
          segmentIndex,
          portIndex,
          hostPort: forward.host_port,
          guestPort: forward.guest_port,
          target: target(forward.vm),
        });
      });
    });
    model().containers.forEach((container, containerIndex) => {
      container.ports.forEach((port, portIndex) => {
        entries.push({
          key: `container:${container.name}:${portIndex}`,
          source: "container",
          containerIndex,
          portIndex,
          hostPort: port.host,
          guestPort: port.container,
          target: { kind: "container", index: containerIndex, name: container.name },
        });
      });
    });
    return entries;
  });

  const hostPosIn = (l: Layout, bounds = computeLabBounds(l)): NodePos =>
    l.host ?? { x: bounds.x + bounds.width + 90, y: bounds.y + 54 };
  const hostPos = (): NodePos => hostPosIn(layout(), labBounds());
  const hostHeight = () => HOST_HEADER_H + Math.max(1, hostPorts().length) * HOST_PORT_H + 10;

  /** Move an external node by the shortest distance that clears the lab plus
   *  breathing room. Existing positions only move when an expanding lab
   *  collides with them; contraction never pulls them back. */
  function pushExternalNodes(l: Layout): Layout {
    const bounds = computeLabBounds(l);
    const margin = 28;
    const left = bounds.x - margin;
    const top = bounds.y - margin;
    const right = bounds.x + bounds.width + margin;
    const bottom = bounds.y + bounds.height + margin;

    const push = (pos: NodePos, width: number, height: number, topOffset = 0): NodePos => {
      const rx = pos.x;
      const ry = pos.y + topOffset;
      if (rx + width <= left || rx >= right || ry + height <= top || ry >= bottom) return pos;
      const candidates: NodePos[] = [
        { x: left - width, y: pos.y },
        { x: right, y: pos.y },
        { x: pos.x, y: top - height - topOffset },
        { x: pos.x, y: bottom - topOffset },
      ];
      return candidates.reduce((best, candidate) => {
        const distance = (candidate.x - pos.x) ** 2 + (candidate.y - pos.y) ** 2;
        const bestDistance = (best.x - pos.x) ** 2 + (best.y - pos.y) ** 2;
        return distance < bestDistance ? candidate : best;
      });
    };

    let out = l;
    const nat = push(natPosIn(out), WAN_W, SEG_H + 26, -18);
    if (nat.x !== natPosIn(out).x || nat.y !== natPosIn(out).y) out = { ...out, nat };

    for (const remote of remoteHosts()) {
      const before = remotePosIn(out, remote);
      const after = push(before, REMOTE_W, SEG_H + 14, -14);
      if (after.x !== before.x || after.y !== before.y) {
        out = { ...out, remotes: { ...(out.remotes ?? {}), [remote]: after } };
      }
    }

    const beforeHost = hostPosIn(out, bounds);
    const afterHost = push(beforeHost, HOST_W, hostHeight());
    if (afterHost.x !== beforeHost.x || afterHost.y !== beforeHost.y) {
      out = { ...out, host: afterHost };
    }
    return out;
  }
  const hostPortY = (index: number) => hostPos().y + HOST_HEADER_H + index * HOST_PORT_H + 17;
  const hostPortValid = (entry: HostPortEntry) =>
    entry.hostPort > 0 &&
    entry.hostPort <= 65535 &&
    hostPorts().filter((candidate) => candidate.hostPort === entry.hostPort).length === 1 &&
    entry.guestPort !== null &&
    entry.guestPort > 0 &&
    entry.guestPort <= 65535;

  const machinePortOrdinal = (entry: HostPortEntry) => {
    if (!entry.target) return 0;
    return hostPorts()
      .filter((candidate) => candidate.target?.name === entry.target!.name)
      .findIndex((candidate) => candidate.key === entry.key);
  };

  const machineServicePort = (entry: HostPortEntry) => {
    const target = entry.target!;
    const p = machinePos(target.kind, target.name);
    const hostIsLeft = hostPos().x + HOST_W / 2 < p.x + VM_W / 2;
    return {
      x: hostIsLeft ? p.x : p.x + VM_W,
      y: p.y + machineHeight(target.kind, target.name) + 12 + machinePortOrdinal(entry) * 18,
      side: hostIsLeft ? ("left" as const) : ("right" as const),
    };
  };

  const hostSocket = (entry: HostPortEntry, index: number) => {
    const target = entry.target;
    const targetX = target ? machinePos(target.kind, target.name).x + VM_W / 2 : labBounds().x;
    const side = targetX < hostPos().x + HOST_W / 2 ? "left" : "right";
    return {
      x: side === "left" ? hostPos().x : hostPos().x + HOST_W,
      y: hostPortY(index),
    };
  };

  function setHostEntryPort(entry: HostPortEntry, end: "host" | "guest", value: number) {
    if (entry.source === "draft") {
      if (end === "host") setHostPortDraft(entry.draftId, value);
      return;
    }
    if (entry.source === "forward") {
      setEditor(
        "draft",
        "segments",
        entry.segmentIndex,
        "forwards",
        entry.portIndex,
        end === "host" ? "host_port" : "guest_port",
        value,
      );
      return;
    }
    setEditor(
      "draft",
      "containers",
      entry.containerIndex,
      "ports",
      entry.portIndex,
      end === "host" ? "host" : "container",
      value,
    );
  }

  function removeHostEntry(entry: HostPortEntry) {
    if (entry.source === "draft") {
      removeHostPortDraft(entry.draftId);
    } else if (entry.source === "forward") {
      const forwards = model().segments[entry.segmentIndex]?.forwards ?? [];
      setEditor(
        "draft",
        "segments",
        entry.segmentIndex,
        "forwards",
        forwards.filter((_, index) => index !== entry.portIndex),
      );
    } else {
      const ports = model().containers[entry.containerIndex]?.ports ?? [];
      setEditor(
        "draft",
        "containers",
        entry.containerIndex,
        "ports",
        ports.filter((_, index) => index !== entry.portIndex),
      );
    }
  }
  // The socket bank is exactly the connections plus a couple of spares.
  const barCapacity = (key: string) => barUsed(key) + MIN_FREE_PORTS;
  const socketX = (barX: number, idx: number) => barX + PORT_X0 + idx * PORT_SPACING;
  const barPos = (key: string): NodePos => {
    const host = remoteHostOf(key);
    return key === NAT_KEY ? natPos() : host !== null ? remotePos(host) : segPos(key);
  };

  const nicRowY = (kind: MachineKind, name: string, nicIndex: number) =>
    machinePos(kind, name).y + 48 + nicIndex * NIC_ROW_H + NIC_ROW_H / 2;

  /** One port per NIC row. Its side is purely derived from the opposite
   *  endpoint, so it flips live as either node moves and is never persisted. */
  function machineNicPort(kind: MachineKind, name: string, nicIndex: number, nic: NicModel) {
    const p = machinePos(kind, name);
    let targetX = p.x + VM_W;
    if (nic.nat) {
      targetX = natPos().x + WAN_W / 2;
    } else if (nic.segment) {
      const socket = ports().byNic.get(`${name}:${nicIndex}`);
      const bar = segPos(nic.segment);
      targetX = socket === undefined ? bar.x + barWidth(nic.segment) / 2 : socketX(bar.x, socket);
    }
    const side = targetX < p.x + VM_W / 2 ? "left" : "right";
    return {
      x: side === "left" ? p.x : p.x + VM_W,
      y: nicRowY(kind, name, nicIndex),
      side,
    } as const;
  }

  // Interconnects: declared segment↔segment routes, segment↔WAN (nat), and
  // segment↔remote-vmlab peers (connect blocks).
  const links = createMemo(() => {
    const names = new Set(model().segments.map((s) => s.name));
    const out: { from: string; kind: "route" | "wan" | "peer"; to: string }[] = [];
    for (const s of model().segments) {
      for (const t of s.routes_to) {
        if (names.has(t)) out.push({ from: s.name, kind: "route", to: t });
      }
      if (s.nat) out.push({ from: s.name, kind: "wan", to: NAT_KEY });
      if (s.connect) out.push({ from: s.name, kind: "peer", to: remoteKey(s.connect.host) });
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
      // Segments and remote nodes both expose left/right mid-edge ports.
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
          const port = machineNicPort(kind, m.name, i, nic);
          used[closestWanSide(port.x, port.y)] = true;
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
          : kind === "provision"
            ? provisionPos(model().provisions.findIndex((_, index) => provisionKey(index) === name))
          : kind === "segment"
            ? segPos(name)
            : kind === "remote"
              ? remotePos(name)
              : kind === "host"
                ? hostPos()
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

  /** Cable out from a machine's dependency socket. One socket fans out to
   *  any number of VM/container targets. */
  function dependencyDown(e: PointerEvent, kind: MachineKind, index: number) {
    e.stopPropagation();
    if (anyVmRunning()) {
      select({ kind, index });
      return;
    }
    const w = world(e);
    setDependencyDrag({ kind, index, existing: null, moved: false, x: w.x, y: w.y });
  }

  /** Re-home an existing dependency arrow; dropping it in empty space removes it. */
  function dependencyGrab(
    e: PointerEvent,
    link: { sourceKind: MachineKind; sourceIndex: number; targetName: string },
  ) {
    e.stopPropagation();
    if (anyVmRunning()) return;
    const w = world(e);
    setDependencyDrag({
      kind: link.sourceKind,
      index: link.sourceIndex,
      existing: link.targetName,
      moved: false,
      x: w.x,
      y: w.y,
    });
  }

  function provisionTargetDown(e: PointerEvent, provisionIndex: number) {
    e.stopPropagation();
    if (anyVmRunning()) {
      select({ kind: "provision", index: provisionIndex });
      return;
    }
    const point = world(e);
    setProvisionTargetDrag({ provisionIndex, existing: null, moved: false, ...point });
  }

  function provisionLinkGrab(
    e: PointerEvent,
    link: { provisionIndex: number; targetName: string },
  ) {
    e.stopPropagation();
    if (anyVmRunning()) return;
    const point = world(e);
    setProvisionTargetDrag({
      provisionIndex: link.provisionIndex,
      existing: link.targetName,
      moved: false,
      ...point,
    });
  }

  function eventTargetDown(e: PointerEvent, handlerIndex: number) {
    e.stopPropagation();
    if (anyVmRunning()) return;
    const point = world(e);
    setEventTargetDrag({ handlerIndex, existing: null, moved: false, ...point });
  }

  function eventLinkGrab(
    e: PointerEvent,
    link: { handlerIndex: number; targetName: string },
  ) {
    e.stopPropagation();
    if (anyVmRunning()) return;
    const point = world(e);
    setEventTargetDrag({
      handlerIndex: link.handlerIndex,
      existing: link.targetName,
      moved: false,
      ...point,
    });
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

  function hostPortDown(e: PointerEvent, entry: HostPortEntry) {
    e.stopPropagation();
    if (anyVmRunning() || entry.source !== "draft") return;
    const w = world(e);
    setHostPortDrag({ draftId: entry.draftId, moved: false, x: w.x, y: w.y });
  }

  /** Grab an existing interconnect cable to re-home or remove it. */
  function linkGrab(
    e: PointerEvent,
    link: { from: string; kind: "route" | "wan" | "peer"; to: string },
  ) {
    e.stopPropagation();
    if (anyVmRunning()) return;
    const w = world(e);
    setLinkDrag({
      from: link.from,
      existing:
        link.kind === "route"
          ? { kind: "route", to: link.to }
          : link.kind === "peer"
            ? { kind: "peer", to: link.to }
            : { kind: "wan" },
      moved: false,
      x: w.x,
      y: w.y,
    });
  }

  /** The bar (segment, the WAN, or a remote-vmlab node) under a world
   *  point, with a little slack. */
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
    for (const host of remoteHosts()) {
      const rp = remotePos(host);
      if (x >= rp.x && x <= rp.x + REMOTE_W && y >= rp.y - 8 && y <= rp.y + SEG_H + 8) {
        return remoteKey(host);
      }
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
        const machine = machines[mi];
        const mp = machinePos(kind, machine.name);
        for (let i = 0; i < machines[mi].nics.length; i++) {
          const port = machineNicPort(kind, machine.name, i, machine.nics[i]);
          const dx = x - port.x;
          const dy = y - port.y;
          if (dx * dx + dy * dy <= 100) return { kind, index: mi, nicIndex: i };
        }
        if (
          x >= mp.x &&
          x <= mp.x + VM_W &&
          y >= mp.y &&
          y <= mp.y + machineHeight(kind, machine.name) + 8
        ) {
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
      setLayout((l) => {
        const next = d.kind === "vm"
          ? { ...l, vms: { ...l.vms, [d.name]: pos } }
          : d.kind === "container"
            ? { ...l, containers: { ...l.containers, [d.name]: pos } }
            : d.kind === "provision"
              ? { ...l, provisions: { ...l.provisions, [d.name]: pos } }
            : d.kind === "segment"
              ? { ...l, segments: { ...l.segments, [d.name]: pos } }
              : d.kind === "remote"
                ? { ...l, remotes: { ...(l.remotes ?? {}), [d.name]: pos } }
                : d.kind === "host"
                  ? { ...l, host: pos }
                  : { ...l, nat: pos };
        return d.kind === "vm" ||
          d.kind === "container" ||
          d.kind === "provision" ||
          d.kind === "segment"
          ? pushExternalNodes(next)
          : next;
      });
      return;
    }
    const cd = connDrag();
    if (cd) {
      const w = world(e);
      setConnDrag({ ...cd, moved: true, x: w.x, y: w.y });
      return;
    }
    const dd = dependencyDrag();
    if (dd) {
      const w = world(e);
      setDependencyDrag({ ...dd, moved: true, x: w.x, y: w.y });
      return;
    }
    const pd = provisionTargetDrag();
    if (pd) {
      const point = world(e);
      setProvisionTargetDrag({ ...pd, moved: true, ...point });
      return;
    }
    const ed = eventTargetDrag();
    if (ed) {
      const point = world(e);
      setEventTargetDrag({ ...ed, moved: true, ...point });
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
    const hd = hostPortDrag();
    if (hd) {
      const w = world(e);
      setHostPortDrag({ ...hd, moved: true, x: w.x, y: w.y });
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
        } else if (d.kind === "provision") {
          const index = model().provisions.findIndex((_, i) => provisionKey(i) === d.name);
          if (index >= 0) select({ kind: "provision", index });
        } else if (d.kind === "segment") {
          const i = model().segments.findIndex((s) => s.name === d.name);
          if (i >= 0) select({ kind: "segment", index: i });
        } else if (d.kind === "remote") {
          select({ kind: "remote", host: d.name });
        } else if (d.kind === "nat") {
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
    const dd = dependencyDrag();
    if (dd) {
      setDependencyDrag(null);
      if (!dd.moved) {
        select({ kind: dd.kind, index: dd.index });
        return;
      }
      const source = machinesOf(dd.kind)[dd.index];
      if (!source) return;
      const w = world(e);
      const target = machineTargetAt(w.x, w.y);
      const targetMachine = target ? machinesOf(target.kind)[target.index] : null;
      const targetName = targetMachine?.name ?? null;

      // Dropping an existing arrow back on the same target is a no-op. Any
      // other drop first detaches its old end, then optionally attaches the
      // new one. Self-dependencies are intentionally ignored.
      if (dd.existing && targetName !== dd.existing) {
        removeMachineDependency(dd.kind, dd.index, dd.existing);
      }
      if (targetName && targetName !== source.name) {
        addMachineDependency(dd.kind, dd.index, targetName);
      }
      return;
    }
    const pd = provisionTargetDrag();
    if (pd) {
      setProvisionTargetDrag(null);
      if (!pd.moved) {
        select({ kind: "provision", index: pd.provisionIndex });
        return;
      }
      const point = world(e);
      const target = machineTargetAt(point.x, point.y);
      const targetName = target ? machinesOf(target.kind)[target.index]?.name ?? null : null;
      if (pd.existing && targetName !== pd.existing) {
        removeProvisionTarget(pd.provisionIndex, pd.existing);
      }
      if (targetName) addProvisionTarget(pd.provisionIndex, targetName);
      return;
    }
    const ed = eventTargetDrag();
    if (ed) {
      setEventTargetDrag(null);
      if (!ed.moved) return;
      const point = world(e);
      const target = machineTargetAt(point.x, point.y);
      const targetName = target ? machinesOf(target.kind)[target.index]?.name ?? null : null;
      if (ed.existing && targetName !== ed.existing) {
        removeEventHandlerTarget(ed.handlerIndex, ed.existing);
      }
      if (targetName && target) {
        addEventHandlerTarget(ed.handlerIndex, target.kind, targetName);
      }
      return;
    }
    const sd = socketDrag();
    if (sd) {
      setSocketDrag(null);
      if (!sd.moved) return;
      const w = world(e);
      // A remote node's port only cables onto switches (peer attach) —
      // remote keys must never become a NIC's segment.
      const srcHost = remoteHostOf(sd.barKey);
      if (srcHost !== null) {
        const drop = barAt(w.x, w.y);
        if (drop && drop !== NAT_KEY && remoteHostOf(drop) === null) {
          const i = model().segments.findIndex((s) => s.name === drop);
          if (i >= 0) setSegmentPeer(i, srcHost);
        }
        return;
      }
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
      if (ld.existing?.kind === "peer" && drop !== ld.existing.to) setSegmentPeer(from, null);
      const dropHost = drop ? remoteHostOf(drop) : null;
      if (dropHost !== null) {
        setSegmentPeer(from, dropHost);
        // Cable-first onto the unaddressed placeholder: open its inspector
        // so the address field is front and center.
        if (dropHost === "") select({ kind: "remote", host: "" });
      } else if (drop === NAT_KEY) setSegmentNat(from, true);
      else if (drop) addSegmentRoute(from, drop);
      return;
    }
    const hd = hostPortDrag();
    if (hd) {
      setHostPortDrag(null);
      if (!hd.moved) return;
      const w = world(e);
      const target = machineTargetAt(w.x, w.y);
      if (target) attachHostPort(hd.draftId, target.kind, target.index);
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
    if (anyVmRunning()) return;
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
    } else if (sel.kind === "provision") {
      const provision = model().provisions[sel.index];
      if (
        provision &&
        (await confirmDialog({
          title: `Delete provision "${provision.script}"?`,
          body: "The script file will be preserved.",
          danger: true,
        }))
      ) {
        removeProvision(sel.index);
      }
    } else if (sel.kind === "remote") {
      if (anyVmRunning()) return;
      const host = sel.host;
      if (
        await confirmDialog({
          title: `Delete remote vmlab "${host || "(no address)"}"?`,
          body: "Segments cabled to it lose their peer link.",
          danger: true,
        })
      ) {
        removeRemote(host);
      }
    }
  }

  function arrange() {
    const fresh = autoLayout(model(), { vms: {}, containers: {}, segments: {}, provisions: {} });
    setLayout(fresh);
    saveLayout(lab(), { ...fresh, view: view() });
  }

  function zoomFit() {
    const xs: number[] = [];
    const ys: number[] = [];
    for (const v of model().vms) {
      const p = vmPos(v.name);
      xs.push(p.x, p.x + VM_W);
      ys.push(p.y, p.y + machineHeight("vm", v.name));
    }
    for (const c of model().containers) {
      const p = ctrPos(c.name);
      xs.push(p.x, p.x + VM_W);
      ys.push(p.y, p.y + machineHeight("container", c.name));
    }
    for (const s of model().segments) {
      const p = segPos(s.name);
      xs.push(p.x, p.x + barWidth(s.name));
      ys.push(p.y, p.y + SEG_H);
    }
    model().provisions.forEach((_, index) => {
      const p = provisionPos(index);
      xs.push(p.x, p.x + PROVISION_W);
      ys.push(p.y, p.y + provisionCardHeight(index));
    });
    {
      const p = natPos();
      xs.push(p.x, p.x + barWidth(NAT_KEY));
      ys.push(p.y, p.y + SEG_H);
    }
    {
      const p = hostPos();
      xs.push(p.x, p.x + HOST_W);
      ys.push(p.y, p.y + hostHeight());
    }
    {
      const b = labBounds();
      xs.push(b.x, b.x + b.width);
      ys.push(b.y, b.y + b.height);
    }
    for (const host of remoteHosts()) {
      const p = remotePos(host);
      xs.push(p.x, p.x + REMOTE_W);
      ys.push(p.y - 14, p.y + SEG_H); // antennas poke above the box
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

  // Same trick for remote nodes: an inspector keystroke is one host rename —
  // migrate its stored position so the node doesn't jump mid-edit.
  let lastRemotes: string[] = [];
  createEffect(() => {
    const hosts = remoteHosts();
    if (lastRemotes.length === hosts.length) {
      const changed = hosts.findIndex((h, i) => h !== lastRemotes[i]);
      if (changed >= 0 && hosts.filter((h, i) => h !== lastRemotes[i]).length === 1) {
        setLayout((l) => renameInLayout(l, "remotes", lastRemotes[changed], hosts[changed]));
      }
    }
    lastRemotes = [...hosts];
  });

  // Drags are tracked at the window level so a release outside the canvas
  // (over the inspector, toolbar, …) still completes/cancels cleanly —
  // per-element pointer capture proved unreliable for that. A pointercancel
  // (browser took the gesture over) just abandons whatever was in flight.
  const cancelDrags = () => {
    setDrag(null);
    setConnDrag(null);
    setDependencyDrag(null);
    setProvisionTargetDrag(null);
    setEventTargetDrag(null);
    setSocketDrag(null);
    setLinkDrag(null);
    setHostPortDrag(null);
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
  const selectedProvision = () =>
    editor.selection.kind === "provision" ? editor.selection.index : null;
  const eventDragAccepts = (kind: MachineKind) => {
    const drag = eventTargetDrag();
    const accepted = drag ? eventTargetKind(model().handlers[drag.handlerIndex]?.event ?? "") : null;
    return accepted === "machine" || accepted === kind;
  };

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
            const nic = m.nics[o.nicIndex];
            return nic
              ? machineNicPort(o.kind, m.name, o.nicIndex, nic).y <= props.pos.y + SEG_H / 2
              : null;
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
  const runtimeNic = (kind: MachineKind, name: string, nicIndex: number) =>
    (kind === "vm" ? runtimeOf(name) : ctrRuntimeOf(name))?.nics[nicIndex] ?? null;

  const ipv4Number = (value: string): number | null => {
    const parts = value.split(".");
    if (parts.length !== 4) return null;
    const octets = parts.map(Number);
    if (octets.some((part, index) => !/^\d+$/.test(parts[index]) || part < 0 || part > 255)) {
      return null;
    }
    return (((octets[0] << 24) >>> 0) + (octets[1] << 16) + (octets[2] << 8) + octets[3]) >>> 0;
  };

  function staticIpError(
    kind: MachineKind,
    machineIndex: number,
    nicIndex: number,
    value: string,
  ): string | null {
    const ip = ipv4Number(value);
    if (ip === null) return "Enter a valid IPv4 address.";
    const machine = machinesOf(kind)[machineIndex];
    const nic = machine?.nics[nicIndex];
    if (!nic?.segment || nic.nat) return "Static addresses require a declared segment.";
    const segment = model().segments.find((candidate) => candidate.name === nic.segment);
    const [networkText, prefixText] = segment?.subnet?.split("/") ?? [];
    const networkIp = networkText ? ipv4Number(networkText) : null;
    const prefix = Number(prefixText);
    if (networkIp === null || !Number.isInteger(prefix) || prefix < 0 || prefix > 32) {
      return `${nic.segment} needs a valid subnet before assigning a static address.`;
    }
    const mask = prefix === 0 ? 0 : (0xffffffff << (32 - prefix)) >>> 0;
    const network = (networkIp & mask) >>> 0;
    const broadcast = (network | (~mask >>> 0)) >>> 0;
    if (((ip & mask) >>> 0) !== network) return `${value} is outside ${segment!.subnet}.`;
    if (
      ip === network ||
      ip === broadcast ||
      (ip === ((network + 1) >>> 0) && !nic.gateway)
    ) {
      return "That address is reserved for the network, gateway, or broadcast.";
    }
    for (const candidateKind of ["vm", "container"] as const) {
      for (const [candidateMachineIndex, candidate] of machinesOf(candidateKind).entries()) {
        for (const [candidateNicIndex, candidateNic] of candidate.nics.entries()) {
          if (
            candidateNic.ip === value &&
            !(
              candidateKind === kind &&
              candidateMachineIndex === machineIndex &&
              candidateNicIndex === nicIndex
            )
          ) {
            return `${value} is already assigned to another NIC.`;
          }
        }
      }
    }
    return null;
  }

  function setNicStaticIp(
    kind: MachineKind,
    machineIndex: number,
    nicIndex: number,
    value: string | null,
  ) {
    if (value === null) setMachineNicGateway(kind, machineIndex, nicIndex, false);
    if (kind === "vm") {
      setEditor("draft", "vms", machineIndex, "nics", nicIndex, "ip", value);
    } else {
      setEditor("draft", "containers", machineIndex, "nics", nicIndex, "ip", value);
    }
  }

  function nicGatewayAllowed(nic: NicModel) {
    if (!nic.segment || nic.nat) return false;
    const segment = model().segments.find((candidate) => candidate.name === nic.segment);
    return !!segment?.subnet && !segment.global;
  }
  const hostPortLive = (entry: HostPortEntry) =>
    !!entry.target && hostPortValid(entry) && machineRunning(entry.target.kind, entry.target.name);

  /** Live cross-host trunk state for a segment (daemon status; null-ish =
   *  unknown / no daemon / not global). */
  const segPeerConnected = (segName: string) =>
    state.status?.segments.find((s) => s.name === segName)?.peer_connected === true;
  /** Draft segments cabled to a remote node. */
  const remoteAttached = (host: string) =>
    model().segments.filter((s) => s.connect?.host === host);
  /** Remote node LED: neutral when unattached/unknown, success when any
   *  attached segment's trunk is up, danger when attached but down. */
  const remoteLed = (host: string): { tone: string; label: string } => {
    const attached = remoteAttached(host);
    if (!attached.length) return { tone: "neutral", label: "not cabled to a segment" };
    const states = attached.map(
      (s) => state.status?.segments.find((r) => r.name === s.name)?.peer_connected,
    );
    if (states.some((v) => v === true)) return { tone: "success", label: "peer connected" };
    if (states.some((v) => v === false)) return { tone: "danger", label: "peer disconnected" };
    return { tone: "neutral", label: "no daemon" };
  };

  /** A small in-canvas icon button on a VM box. */
  function VmBtn(props: {
    x: number;
    y: number;
    act: string;
    title: string;
    disabled?: boolean;
    onClick: () => void;
    children: any;
  }) {
    return (
      <g
        class={`topo-console act-${props.act}`}
        classList={{ disabled: props.disabled }}
        transform={`translate(${props.x} ${props.y})`}
        aria-disabled={props.disabled ? "true" : undefined}
        onPointerDown={(e: PointerEvent) => e.stopPropagation()}
        onClick={(e: MouseEvent) => {
          e.stopPropagation();
          if (!props.disabled) props.onClick();
        }}
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
    const port = machineNicPort(kind, name, nicIndex, nic);
    const px = port.x;
    const py = port.y;
    if (nic.nat) {
      const side = closestWanSide(px, py);
      const pt = wanPorts()[side];
      if (side === "left" || side === "right") {
        const sx = side === "left" ? pt.x - PORT_SIZE / 2 : pt.x + PORT_SIZE / 2;
        const elbow = port.side === "left" ? px - 16 : px + 16;
        return `M ${px} ${py} L ${elbow} ${py} L ${elbow} ${pt.y} L ${sx} ${pt.y}`;
      }
      const sy = side === "top" ? pt.y - PORT_SIZE / 2 : pt.y + PORT_SIZE / 2;
      const elbowX = port.side === "left" ? px - 16 : px + 16;
      const elbowY = side === "top" ? sy - 14 : sy + 14;
      return `M ${px} ${py} L ${elbowX} ${py} L ${elbowX} ${elbowY} L ${pt.x} ${elbowY} L ${pt.x} ${sy}`;
    }
    const bar = segPos(key);
    const socket = ports().byNic.get(`${name}:${nicIndex}`);
    if (socket === undefined) return null;
    const sx = socketX(bar.x, socket);
    // Enter through the socket face nearest the VM (sockets straddle both
    // edges, like through-ports on a patch panel).
    const fromAbove = py <= bar.y + SEG_H / 2;
    const sy = fromAbove ? bar.y - PORT_SIZE / 2 : bar.y + SEG_H + PORT_SIZE / 2;
    const leaveX = port.side === "left" ? px - 16 : px + 16;
    const elbowY = fromAbove ? sy - 14 : sy + 14;
    return `M ${px} ${py} L ${leaveX} ${py} L ${leaveX} ${elbowY} L ${sx} ${elbowY} L ${sx} ${sy}`;
  }

  return (
    <div class="topo-wrap">
      <div class="topo-toolbar">
        <Button
          size="sm"
          icon={Monitor}
          onClick={() => addVm()}
          disabled={anyVmRunning()}
          title={anyVmRunning() ? "Configuration is locked while a machine is up" : undefined}
        >
          Add VM
        </Button>
        <Button
          size="sm"
          icon={Container}
          onClick={() => addContainer()}
          disabled={anyVmRunning()}
          title={anyVmRunning() ? "Configuration is locked while a machine is up" : undefined}
        >
          Add container
        </Button>
        <Button
          size="sm"
          icon={FileCode2}
          onClick={props.onAddProvision}
          disabled={anyVmRunning()}
          title={anyVmRunning() ? "Stop all machines before adding a provision script" : undefined}
        >
          Add provision
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
        <Button
          size="sm"
          icon={Router}
          onClick={() => addRemote()}
          disabled={anyVmRunning()}
          title={
            anyVmRunning()
              ? "Networking is read-only while a machine is up"
              : "Bridge a segment to another vmlab instance over a cross-host trunk"
          }
        >
          Add remote vmlab
        </Button>
        <Button size="sm" variant="ghost" icon={LayoutGrid} onClick={arrange} title="Auto-arrange">
          Arrange
        </Button>
        <Button size="sm" variant="ghost" icon={Expand} onClick={zoomFit} title="Zoom to fit">
          Fit
        </Button>
        <Button
          class="topo-edit-config"
          size="sm"
          variant="ghost"
          icon={FilePenLine}
          onClick={props.onEditConfig}
          disabled={anyVmRunning()}
          title={anyVmRunning() ? "Configuration is locked while a machine is up" : undefined}
        >
          Edit vmlab.wcl
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
        <defs>
          <marker
            id="topo-dependency-arrow"
            viewBox="0 0 10 10"
            refX="9"
            refY="5"
            markerWidth="7"
            markerHeight="7"
            orient="auto-start-reverse"
          >
            <path d="M 0 0 L 10 5 L 0 10 z" class="topo-dependency-arrowhead" />
          </marker>
          <marker
            id="topo-dependency-arrow-cycle"
            viewBox="0 0 10 10"
            refX="9"
            refY="5"
            markerWidth="7"
            markerHeight="7"
            orient="auto-start-reverse"
          >
            <path d="M 0 0 L 10 5 L 0 10 z" class="topo-dependency-arrowhead cycle" />
          </marker>
          <marker
            id="topo-provision-arrow"
            viewBox="0 0 10 10"
            refX="9"
            refY="5"
            markerWidth="7"
            markerHeight="7"
            orient="auto-start-reverse"
          >
            <path d="M 0 0 L 10 5 L 0 10 z" class="topo-provision-arrowhead" />
          </marker>
          <marker
            id="topo-event-arrow"
            viewBox="0 0 10 10"
            refX="9"
            refY="5"
            markerWidth="7"
            markerHeight="7"
            orient="auto-start-reverse"
          >
            <path d="M 0 0 L 10 5 L 0 10 z" class="topo-event-arrowhead" />
          </marker>
        </defs>
        <g transform={`translate(${view().tx} ${view().ty}) scale(${view().k})`}>
          {/* The lab is a live enclosure, rendered first so every owned node
              and cable remains above it. Its bounds follow node drags. */}
          {(() => {
            const b = labBounds;
            return (
              <g
                class="topo-lab"
                classList={{ selected: editor.selection.kind === "lab" }}
                onPointerDown={(e: PointerEvent) => {
                  e.stopPropagation();
                  backgroundDown(e);
                }}
              >
                <rect
                  class="topo-lab-frame"
                  x={b().x}
                  y={b().y}
                  width={b().width}
                  height={b().height}
                  rx="14"
                />
                <line
                  class="topo-lab-divider"
                  x1={b().x}
                  y1={b().y + 52}
                  x2={b().x + b().width}
                  y2={b().y + 52}
                />
                <g
                  class="topo-lab-glyph"
                  transform={`translate(${b().x + 14} ${b().y + 17})`}
                >
                  <FlaskConical size={17} />
                </g>
                <text class="topo-lab-kind" x={b().x + 38} y={b().y + 18}>
                  LAB
                </text>
                <text class="topo-lab-name" x={b().x + 38} y={b().y + 35}>
                  {model().name}
                </text>
                <title>Lab contents: click to edit lab-wide properties</title>
              </g>
            );
          })()}

          {/* Host port forwards are a separate cable layer: host socket to a
              service port on the target VM/container. Invalid port pairs stay
              red until both ends are in the 1–65535 range. */}
          <For each={hostPorts()}>
            {(entry, index) => (
              <Show when={entry.target} keyed>
                {(_target) => {
                  const a = () => hostSocket(entry, index());
                  const b = () => machineServicePort(entry);
                  const path = () => {
                    const mid = (a().x + b().x) / 2;
                    return `M ${a().x} ${a().y} L ${mid} ${a().y} L ${mid} ${b().y} L ${b().x} ${b().y}`;
                  };
                  return (
                    <g
                      class="topo-host-link"
                      classList={{
                        invalid: !hostPortValid(entry),
                        live: hostPortLive(entry),
                      }}
                    >
                      <path d={path()} class="topo-host-link-line" />
                    </g>
                  );
                }}
              </Show>
            )}
          </For>

          <Show when={hostPortDrag()}>
            {(hd) => {
              const entry = () => hostPorts().find((port) => port.key === `draft:${hd().draftId}`);
              const index = () => hostPorts().findIndex((port) => port.key === `draft:${hd().draftId}`);
              return (
                <Show when={entry()}>
                  {(port) => {
                    const a = () => hostSocket(port(), index());
                    return (
                      <path
                        class="topo-host-link-draft"
                        d={`M ${a().x} ${a().y} L ${hd().x} ${hd().y}`}
                      />
                    );
                  }}
                </Show>
              );
            }}
          </Show>

          <For each={provisionLinks()}>
            {(link) => {
              const grabbed = () => {
                const drag = provisionTargetDrag();
                return (
                  drag?.provisionIndex === link.provisionIndex &&
                  drag.existing === link.targetName
                );
              };
              const path = () =>
                provisionLinkPath(link.provisionIndex, link.targetKind, link.targetName);
              return (
                <Show when={!grabbed()}>
                  <g
                    class="topo-provision-link"
                    classList={{ locked: anyVmRunning() }}
                    onPointerDown={(event: PointerEvent) => provisionLinkGrab(event, link)}
                  >
                    <path class="topo-provision-link-hit" d={path()}>
                      <title>
                        {anyVmRunning()
                          ? "Configuration is locked while a machine is up"
                          : `${model().provisions[link.provisionIndex]?.script} applies to ${link.targetName} — drag to re-home or remove`}
                      </title>
                    </path>
                    <path
                      class="topo-provision-link-line"
                      d={path()}
                      marker-end="url(#topo-provision-arrow)"
                    />
                  </g>
                </Show>
              );
            }}
          </For>

          <Show when={provisionTargetDrag()}>
            {(drag) => {
              const from = () => provisionPort(drag().provisionIndex);
              return (
                <path
                  class="topo-provision-link-draft"
                  d={`M ${from().x} ${from().y} C ${from().x + 38} ${from().y}, ${drag().x - 38} ${drag().y}, ${drag().x} ${drag().y}`}
                  marker-end="url(#topo-provision-arrow)"
                />
              );
            }}
          </Show>

          <For each={eventLinks()}>
            {(link) => {
              const handler = () => model().handlers[link.handlerIndex];
              const grabbed = () => {
                const drag = eventTargetDrag();
                return drag?.handlerIndex === link.handlerIndex && drag.existing === link.targetName;
              };
              const path = () =>
                eventLinkPath(link.handlerIndex, link.targetKind, link.targetName);
              return (
                <Show when={!grabbed()}>
                  <g
                    class="topo-event-link"
                    classList={{ locked: anyVmRunning() }}
                    onPointerDown={(event: PointerEvent) => eventLinkGrab(event, link)}
                  >
                    <path class="topo-event-link-hit" d={path()}>
                      <title>
                        {anyVmRunning()
                          ? "Configuration is locked while a machine is up"
                          : `${handler()?.event} → ${link.targetName} — drag to re-home or remove`}
                      </title>
                    </path>
                    <path
                      class="topo-event-link-line"
                      d={path()}
                      marker-end="url(#topo-event-arrow)"
                    />
                  </g>
                </Show>
              );
            }}
          </For>

          <Show when={eventTargetDrag()}>
            {(drag) => {
              const from = () => eventPort(drag().handlerIndex)!;
              return (
                <path
                  class="topo-event-link-draft"
                  d={`M ${from().x} ${from().y} C ${from().x + 34} ${from().y}, ${drag().x - 34} ${drag().y}, ${drag().x} ${drag().y}`}
                  marker-end="url(#topo-event-arrow)"
                />
              );
            }}
          </Show>

          {/* Startup dependencies are deliberately a separate amber layer:
              the arrow leaves the dependent machine and points at the VM or
              container that must be ready first. */}
          <For each={dependencyLinks()}>
            {(link) => {
              const cycle = () =>
                cyclicDependencies().has(dependencyKey(link.sourceName, link.targetName));
              const grabbed = () => {
                const drag = dependencyDrag();
                return (
                  drag?.existing === link.targetName &&
                  drag.kind === link.sourceKind &&
                  drag.index === link.sourceIndex
                );
              };
              const path = () =>
                dependencyPath(
                  link.sourceKind,
                  link.sourceName,
                  link.targetKind,
                  link.targetName,
                );
              return (
                <Show when={!grabbed()}>
                  <g
                    class="topo-dependency"
                    classList={{ cycle: cycle(), locked: anyVmRunning() }}
                    onPointerDown={(e: PointerEvent) => dependencyGrab(e, link)}
                  >
                    <path class="topo-dependency-hit" d={path()}>
                      <title>
                        {anyVmRunning()
                          ? "Configuration is locked while a machine is up"
                          : `${cycle() ? "Dependency cycle: " : ""}${link.sourceName} depends on ${link.targetName} — drag to re-home, or drag into empty space to remove`}
                      </title>
                    </path>
                    <path
                      class="topo-dependency-line"
                      d={path()}
                      marker-end={
                        cycle()
                          ? "url(#topo-dependency-arrow-cycle)"
                          : "url(#topo-dependency-arrow)"
                      }
                    />
                  </g>
                </Show>
              );
            }}
          </For>

          <Show when={dependencyDrag()}>
            {(dd) => {
              const source = () => machinesOf(dd().kind)[dd().index];
              const from = () => dependencyPort(dd().kind, source().name);
              return (
                <Show when={source()}>
                  <path
                    class="topo-dependency-draft"
                    d={`M ${from().x} ${from().y} C ${from().x} ${from().y - 32}, ${dd().x} ${dd().y - 24}, ${dd().x} ${dd().y}`}
                    marker-end="url(#topo-dependency-arrow)"
                  />
                </Show>
              );
            }}
          </Show>

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
                  : ld.existing.kind === "peer"
                    ? link.kind === "peer" && ld.existing.to === link.to
                    : link.kind === "route" && ld.existing.to === link.to;
              };
              return (
                <Show when={!grabbed()}>
                  <g
                    class="topo-link"
                    classList={{
                      live:
                        link.kind === "wan" ||
                        (link.kind === "peer" && segPeerConnected(link.from)),
                      locked: anyVmRunning(),
                    }}
                    onPointerDown={(e: PointerEvent) => linkGrab(e, link)}
                  >
                    <path d={linkPath(link.from, link.to, li())} class="topo-link-hit">
                      <title>
                        {anyVmRunning()
                          ? "Networking is read-only while a machine is up"
                          : link.kind === "wan"
                          ? `${link.from} ⇄ WAN — drag off to disconnect`
                          : link.kind === "peer"
                          ? `${link.from} ⇄ remote vmlab ${remoteHostOf(link.to) || "(no address)"} — drag off to disconnect`
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
              const port = () =>
                machineNicPort(cd().kind, m().name, cd().nicIndex, m().nics[cd().nicIndex]);
              return (
                <Show when={m()}>
                  <path
                    class="topo-edge-draft"
                    d={`M ${port().x} ${port().y} L ${cd().x} ${cd().y}`}
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

          {/* remote-vmlab peer nodes: routers representing another vmlab
              instance — cable a switch's side port here to bridge that
              (global) segment over a cross-host trunk (connect { host }).
              The LED and cable animation follow the live trunk state. */}
          <For each={remoteHosts()}>
            {(host) => {
              const p = () => remotePos(host);
              const isSel = () =>
                editor.selection.kind === "remote" && editor.selection.host === host;
              const led = () => remoteLed(host);
              const portLit = (side: "left" | "right") =>
                links().some(
                  (l) =>
                    l.kind === "peer" &&
                    l.to === remoteKey(host) &&
                    linkEnds(l.from, l.to).bSide === side,
                );
              return (
                <g
                  class="topo-seg topo-remote"
                  classList={{ selected: isSel() }}
                  onPointerDown={(e: PointerEvent) => nodeDown(e, "remote", host)}
                >
                  <Show when={isSel()}>
                    <rect
                      x={p().x - 6}
                      y={p().y - 22}
                      width={REMOTE_W + 12}
                      height={SEG_H + 30}
                      rx="12"
                      class="topo-wan-outline"
                    />
                  </Show>
                  {/* two antennas poking above the box */}
                  <line
                    x1={p().x + 22}
                    y1={p().y + 2}
                    x2={p().x + 14}
                    y2={p().y - 14}
                    class="topo-remote-antenna"
                  />
                  <circle cx={p().x + 14} cy={p().y - 14} r="2.2" class="topo-remote-antenna-tip" />
                  <line
                    x1={p().x + REMOTE_W - 22}
                    y1={p().y + 2}
                    x2={p().x + REMOTE_W - 14}
                    y2={p().y - 14}
                    class="topo-remote-antenna"
                  />
                  <circle
                    cx={p().x + REMOTE_W - 14}
                    cy={p().y - 14}
                    r="2.2"
                    class="topo-remote-antenna-tip"
                  />
                  <rect
                    x={p().x}
                    y={p().y}
                    width={REMOTE_W}
                    height={SEG_H}
                    rx="10"
                    class="topo-remote-box"
                  />
                  <g
                    class="topo-remote-glyph"
                    transform={`translate(${p().x + 10} ${p().y + 11})`}
                  >
                    <Router size={17} />
                  </g>
                  <text class="topo-remote-kind" x={p().x + 34} y={p().y + 15}>
                    REMOTE VMLAB
                  </text>
                  <text
                    class="topo-remote-name"
                    classList={{ placeholder: !host }}
                    x={p().x + 34}
                    y={p().y + 30}
                  >
                    {host || "set address…"}
                  </text>
                  {/* live trunk LED */}
                  <circle
                    cx={p().x + REMOTE_W - 12}
                    cy={p().y + 12}
                    r="4"
                    class={`topo-led ${led().tone}`}
                  >
                    <title>{led().label}</title>
                  </circle>
                  {/* one peer port per short side */}
                  <For each={["left", "right"] as const}>
                    {(side) => {
                      const px = () => (side === "left" ? p().x : p().x + REMOTE_W);
                      const py = () => p().y + SEG_H / 2;
                      return (
                        <rect
                          x={px() - (PORT_SIZE + 2) / 2}
                          y={py() - (PORT_SIZE + 2) / 2}
                          width={PORT_SIZE + 2}
                          height={PORT_SIZE + 2}
                          rx="2"
                          class="topo-socket"
                          classList={{ lit: portLit(side), locked: anyVmRunning() }}
                          onPointerDown={(e: PointerEvent) =>
                            socketDown(e, remoteKey(host), 0, px(), py())
                          }
                        >
                          <title>
                            {anyVmRunning()
                              ? "Networking is read-only while a machine is up"
                              : "Drag onto a switch to bridge that segment to this remote vmlab"}
                          </title>
                        </rect>
                      );
                    }}
                  </For>
                </g>
              );
            }}
          </For>

          {/* Physical host: each row is a host-side listening port. New rows
              begin at 0 and can be dragged onto a VM/container body. */}
          <g
            class="topo-host"
            onPointerDown={(e: PointerEvent) => nodeDown(e, "host", "__host__")}
          >
            <rect
              class="topo-host-box"
              x={hostPos().x}
              y={hostPos().y}
              width={HOST_W}
              height={hostHeight()}
              rx="12"
            />
            <g
              class="topo-host-glyph"
              transform={`translate(${hostPos().x + 12} ${hostPos().y + 13})`}
            >
              <Server size={17} />
            </g>
            <text class="topo-host-kind" x={hostPos().x + 38} y={hostPos().y + 18}>
              HOST
            </text>
            <text class="topo-host-name" x={hostPos().x + 38} y={hostPos().y + 34}>
              this machine
            </text>
            <g
                class="topo-host-add"
                classList={{ locked: anyVmRunning() }}
              transform={`translate(${hostPos().x + HOST_W - 28} ${hostPos().y + 11})`}
              onPointerDown={(e: PointerEvent) => e.stopPropagation()}
              onClick={(e: MouseEvent) => {
                  e.stopPropagation();
                  if (!anyVmRunning()) addHostPort();
              }}
            >
              <rect width="20" height="20" rx="4" />
              <g transform="translate(4 4)">
                <Plus size={12} />
              </g>
              <title>Add host port</title>
            </g>
            <Show when={hostPorts().length} fallback={
              <text class="topo-host-empty" x={hostPos().x + 12} y={hostPos().y + 65}>
                Add a port, then drag its socket to a machine
              </text>
            }>
              <For each={hostPorts()}>
                {(entry, index) => {
                  const socket = () => hostSocket(entry, index());
                  const valid = () => hostPortValid(entry);
                  return (
                    <g>
                      <line
                        class="topo-host-row-line"
                        x1={hostPos().x + 8}
                        x2={hostPos().x + HOST_W - 8}
                        y1={hostPortY(index()) - 17}
                        y2={hostPortY(index()) - 17}
                      />
                      <rect
                        class="topo-host-socket"
                        classList={{
                          invalid: !valid(),
                          attached: entry.target !== null,
                          live: hostPortLive(entry),
                        }}
                        x={socket().x - 5}
                        y={socket().y - 5}
                        width="10"
                        height="10"
                        rx="2"
                        onPointerDown={(e: PointerEvent) => hostPortDown(e, entry)}
                      >
                        <title>
                          {entry.source === "draft"
                            ? "Drag this host port onto a VM or container"
                            : "Attached host port"}
                        </title>
                      </rect>
                      <PortNumberEditor
                        x={hostPos().x + 18}
                        y={hostPortY(index()) - 12}
                        value={entry.hostPort}
                        valid={valid()}
                        disabled={anyVmRunning()}
                        label="Host listening port"
                        onChange={(value) => setHostEntryPort(entry, "host", value)}
                      />
                      <text
                        class="topo-host-target"
                        x={hostPos().x + 72}
                        y={hostPortY(index()) + 4}
                      >
                        {entry.target?.name ?? "unattached"}
                      </text>
                      <g
                        class="topo-host-remove"
                        classList={{ locked: anyVmRunning() }}
                        transform={`translate(${hostPos().x + HOST_W - 27} ${hostPortY(index()) - 10})`}
                        onPointerDown={(e: PointerEvent) => e.stopPropagation()}
                        onClick={(e: MouseEvent) => {
                          e.stopPropagation();
                          if (!anyVmRunning()) removeHostEntry(entry);
                        }}
                      >
                        <rect width="18" height="18" rx="4" />
                        <g transform="translate(4 4)">
                          <Trash2 size={10} />
                        </g>
                        <title>Remove host port</title>
                      </g>
                    </g>
                  );
                }}
              </For>
            </Show>
          </g>

          {/* Provision scripts are lab-owned workflow nodes. Empty scope is
              intentionally shown as LAB-WIDE rather than drawing an edge to
              every machine. */}
          <For each={model().provisions}>
            {(provision, index) => {
              const p = () => provisionPos(index());
              const key = () => provisionKey(index());
              const file = () => provision.script.split("/").pop() || provision.script;
              const directory = () =>
                provision.script.includes("/")
                  ? provision.script.slice(0, provision.script.lastIndexOf("/"))
                  : ".";
              const badge = () =>
                provision.vms.length ? `${provision.vms.length} TARGETED` : "LAB-WIDE";
              return (
                <g
                  class="topo-provision"
                  classList={{ selected: selectedProvision() === index() }}
                  onPointerDown={(event: PointerEvent) => nodeDown(event, "provision", key())}
                >
                  <rect
                    class="topo-provision-box"
                    x={p().x}
                    y={p().y}
                    width={PROVISION_W}
                    height={provisionCardHeight(index())}
                    rx="10"
                  />
                  <g
                    class="topo-provision-glyph"
                    transform={`translate(${p().x + 12} ${p().y + 13})`}
                  >
                    <FileCode2 size={18} />
                  </g>
                  <text class="topo-provision-kind" x={p().x + 38} y={p().y + 16}>
                    PROVISION #{index() + 1}
                  </text>
                  <text class="topo-provision-name" x={p().x + 38} y={p().y + 34}>
                    {file().length > 23 ? `${file().slice(0, 22)}…` : file()}
                    <title>{provision.script}</title>
                  </text>
                  <text class="topo-provision-path" x={p().x + 12} y={p().y + 53}>
                    {directory().length > 24 ? `…${directory().slice(-23)}` : directory()}
                  </text>
                  <g class="topo-provision-badge">
                    <rect x={p().x + 12} y={p().y + 59} width={badge().length * 5.2 + 12} height="13" rx="6.5" />
                    <text x={p().x + 18} y={p().y + 68.5}>{badge()}</text>
                  </g>
                  <g
                    class="topo-provision-edit"
                    classList={{ locked: anyVmRunning() }}
                    transform={`translate(${p().x + PROVISION_W - 68} ${p().y + 8})`}
                    onPointerDown={(event: PointerEvent) => event.stopPropagation()}
                    onClick={(event: MouseEvent) => {
                      event.stopPropagation();
                      if (!anyVmRunning()) props.onEditProvision(provision.script);
                    }}
                  >
                    <rect width="20" height="20" rx="5" />
                    <g transform="translate(4 4)"><FilePenLine size={12} /></g>
                    <title>
                      {anyVmRunning()
                        ? "Configuration is locked while a machine is up"
                        : "Edit provision script"}
                    </title>
                  </g>
                  <g
                    class="topo-provision-port"
                    classList={{
                      active: provisionTargetDrag()?.provisionIndex === index(),
                      locked: anyVmRunning(),
                    }}
                    onPointerDown={(event: PointerEvent) => provisionTargetDown(event, index())}
                  >
                    <circle cx={p().x + PROVISION_W} cy={p().y + PROVISION_H / 2} r="5" />
                    <text x={p().x + PROVISION_W - 9} y={p().y + PROVISION_H / 2 - 9}>
                      TARGETS
                    </text>
                    <title>
                      {anyVmRunning()
                        ? "Configuration is locked while a machine is up"
                        : "Drag to a VM or container this script applies to"}
                    </title>
                  </g>
                  <Show when={handlersForProvision(index()).length}>
                    <line
                      class="topo-provision-event-divider"
                      x1={p().x}
                      x2={p().x + PROVISION_W}
                      y1={p().y + PROVISION_H}
                      y2={p().y + PROVISION_H}
                    />
                  </Show>
                  <For each={handlersForProvision(index())}>
                    {({ handler, handlerIndex }, row) => {
                      const y = () => p().y + PROVISION_H + row() * SCRIPT_EVENT_ROW_H;
                      const targetable = () => eventTargetKind(handler.event) !== null;
                      const scope = () =>
                        handler.targets.length
                          ? `${handler.targets.length}`
                          : handler.event.startsWith("vm.")
                            ? "ALL VMS"
                            : handler.event.startsWith("container.")
                              ? "ALL CTRS"
                              : handler.event.startsWith("snapshot.")
                                ? "ALL"
                                : "GLOBAL";
                      return (
                        <g class="topo-event-row">
                          <rect
                            x={p().x + 1}
                            y={y()}
                            width={PROVISION_W - 2}
                            height={SCRIPT_EVENT_ROW_H}
                          />
                          <text x={p().x + 10} y={y() + 15.5}>{handler.event}</text>
                          <text class="scope" x={p().x + PROVISION_W - 14} y={y() + 15.5}>
                            {scope()}
                          </text>
                          <Show when={targetable()}>
                            <g
                              class="topo-event-port"
                              classList={{
                                active: eventTargetDrag()?.handlerIndex === handlerIndex,
                                locked: anyVmRunning(),
                              }}
                              onPointerDown={(event: PointerEvent) =>
                                eventTargetDown(event, handlerIndex)
                              }
                            >
                              <circle
                                cx={p().x + PROVISION_W}
                                cy={y() + SCRIPT_EVENT_ROW_H / 2}
                                r="4.5"
                              />
                              <title>
                                {anyVmRunning()
                                  ? "Configuration is locked while a machine is up"
                                  : `Drag to scope ${handler.event} to a compatible machine`}
                              </title>
                            </g>
                          </Show>
                        </g>
                      );
                    }}
                  </For>
                </g>
              );
            }}
          </For>

          {/* VM nodes */}
          <For each={model().vms}>
            {(vm, vi) => {
              const p = () => vmPos(vm.name);
              const h = () => machineCardHeight(vm.nics.length);
              const footerY = () => p().y + h() - 30;
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
                  classList={{
                    selected: selectedVm() === vm.name,
                    locked: vmIsUp(vm.name),
                    "dependency-target":
                      dependencyDrag() !== null &&
                      !(dependencyDrag()!.kind === "vm" && dependencyDrag()!.index === vi()),
                    "provision-target": provisionTargetDrag() !== null,
                    "event-target": eventDragAccepts("vm"),
                  }}
                  onPointerDown={(e: PointerEvent) => nodeDown(e, "vm", vm.name)}
                >
                  <rect x={p().x} y={p().y} width={VM_W} height={h()} rx="10" />
                  <line
                    class="topo-machine-divider"
                    x1={p().x}
                    x2={p().x + VM_W}
                    y1={p().y + 48}
                    y2={p().y + 48}
                  />
                  <line
                    class="topo-machine-divider"
                    x1={p().x}
                    x2={p().x + VM_W}
                    y1={footerY()}
                    y2={footerY()}
                  />
                  <g class="topo-vm-glyph" transform={`translate(${p().x + 10} ${p().y + 9})`}>
                    <Monitor size={18} />
                  </g>
                  <text x={p().x + 34} y={p().y + 21} class="topo-vm-name">
                    {vm.name.length > 16 ? `${vm.name.slice(0, 15)}…` : vm.name}
                    <title>{vm.name}</title>
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
                      y={p().y + h() - 22}
                      width={badgeW()}
                      height="14"
                      rx="7"
                    />
                    <text x={p().x + 16} y={p().y + h() - 11.5}>
                      {hw()}
                    </text>
                  </g>
                  {/* power LED (live daemon state) */}
                  <circle
                    cx={p().x + 19}
                    cy={p().y + 38}
                    r="4"
                    class={`topo-led ${ledTone(vm.name)}`}
                  >
                    <title>{ledLabel(vm.name)}</title>
                  </circle>
                  <g
                    class="topo-dependency-port"
                    classList={{
                      locked: anyVmRunning(),
                      active:
                        dependencyDrag()?.kind === "vm" && dependencyDrag()?.index === vi(),
                      target:
                        dependencyDrag() !== null &&
                        !(dependencyDrag()!.kind === "vm" && dependencyDrag()!.index === vi()),
                    }}
                    onPointerDown={(e: PointerEvent) => dependencyDown(e, "vm", vi())}
                  >
                    <circle cx={p().x + VM_W / 2} cy={p().y} r="5" />
                    <text x={p().x + VM_W / 2 + 9} y={p().y + 3.5}>
                      DEP
                    </text>
                    <title>
                      {anyVmRunning()
                        ? "Configuration is locked while a machine is up"
                        : "Drag to a VM or container that this VM depends on"}
                    </title>
                  </g>
                  {/* Machine actions stay in a predictable top-right control bank. */}
                  <VmBtn
                    x={p().x + VM_W - 70}
                    y={p().y + 8}
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
                    y={p().y + 8}
                    act="restart"
                    title={vmRunning(vm.name) ? "Restart" : "Restart unavailable while powered off"}
                    disabled={!vmRunning(vm.name)}
                    onClick={() => vmRestart(vm.name)}
                  >
                    <RotateCw size={11} />
                  </VmBtn>
                  <VmBtn
                    x={p().x + VM_W - 28}
                    y={p().y + 8}
                    act="console"
                    title="Open the console"
                    onClick={() => showVm(vm.name)}
                  >
                    <SquareTerminal size={11} />
                  </VmBtn>
                  <Index each={vm.nics}>
                    {(nic, i) => {
                      const port = () => machineNicPort("vm", vm.name, i, nic());
                      const runtime = () => runtimeNic("vm", vm.name, i);
                      const rowY = () => p().y + 48 + i * NIC_ROW_H;
                      const target = () => (nic().nat ? "NAT" : nic().segment ?? "unplugged");
                      const compactTarget = () =>
                        target().length > 6 ? `${target().slice(0, 5)}…` : target();
                      return (
                        <>
                          <rect
                            class="topo-nic-row-bg"
                            x={p().x + 1}
                            y={rowY()}
                            width={VM_W - 2}
                            height={NIC_ROW_H}
                          />
                          <g
                            class="topo-nic-icon"
                            transform={`translate(${p().x + 8} ${rowY() + 7})`}
                          >
                            <EthernetPort size={10} />
                          </g>
                          <text class="topo-nic-index" x={p().x + 21} y={rowY() + 16.5}>
                            NIC {i}
                          </text>
                          <text class="topo-nic-target" x={p().x + 55} y={rowY() + 16.5}>
                            {compactTarget()}
                            <title>{target()}</title>
                          </text>
                          <NicIpEditor
                            x={p().x + 110}
                            y={rowY() + 1}
                            staticIp={nic().ip}
                            assignedIp={runtime()?.ip ?? null}
                            disabled={anyVmRunning()}
                            staticAllowed={!!nic().segment && !nic().nat}
                            gateway={nic().gateway}
                            gatewayAllowed={nicGatewayAllowed(nic())}
                            validate={(value) => staticIpError("vm", vi(), i, value)}
                            onChange={(value) => setNicStaticIp("vm", vi(), i, value)}
                            onGatewayChange={(enabled) =>
                              setMachineNicGateway("vm", vi(), i, enabled)
                            }
                          />
                          <circle
                            cx={port().x}
                            cy={port().y}
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
                        </>
                      );
                    }}
                  </Index>
                </g>
              );
            }}
          </For>

          {/* Container nodes share the VM action layout, but link to their
              shell-oriented detail page instead of a display console. */}
          <For each={model().containers}>
            {(ctr, ci) => {
              const p = () => ctrPos(ctr.name);
              const h = () => machineCardHeight(ctr.nics.length);
              const footerY = () => p().y + h() - 30;
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
                  classList={{
                    selected: selectedCtr() === ctr.name,
                    locked: containerIsUp(ctr.name),
                    "dependency-target":
                      dependencyDrag() !== null &&
                      !(
                        dependencyDrag()!.kind === "container" &&
                        dependencyDrag()!.index === ci()
                      ),
                    "provision-target": provisionTargetDrag() !== null,
                    "event-target": eventDragAccepts("container"),
                  }}
                  onPointerDown={(e: PointerEvent) => nodeDown(e, "container", ctr.name)}
                >
                  <rect x={p().x} y={p().y} width={VM_W} height={h()} rx="10" />
                  <line
                    class="topo-machine-divider"
                    x1={p().x}
                    x2={p().x + VM_W}
                    y1={p().y + 48}
                    y2={p().y + 48}
                  />
                  <line
                    class="topo-machine-divider"
                    x1={p().x}
                    x2={p().x + VM_W}
                    y1={footerY()}
                    y2={footerY()}
                  />
                  <g class="topo-ctr-glyph" transform={`translate(${p().x + 10} ${p().y + 9})`}>
                    <Container size={18} />
                  </g>
                  <text x={p().x + 34} y={p().y + 21} class="topo-vm-name">
                    {ctr.name.length > 16 ? `${ctr.name.slice(0, 15)}…` : ctr.name}
                    <title>{ctr.name}</title>
                  </text>
                  <text x={p().x + 34} y={p().y + 35} class="topo-vm-meta">
                    {ctr.image || "(no image)"}
                  </text>
                  {/* hardware badge */}
                  <g class="topo-vm-badge">
                    <rect
                      x={p().x + 10}
                      y={p().y + h() - 22}
                      width={badgeW()}
                      height="14"
                      rx="7"
                    />
                    <text x={p().x + 16} y={p().y + h() - 11.5}>
                      {hw()}
                    </text>
                  </g>
                  {/* power LED (live daemon state, incl. health) */}
                  <circle
                    cx={p().x + 19}
                    cy={p().y + 38}
                    r="4"
                    class={`topo-led ${ctrLedTone(ctr.name)}`}
                  >
                    <title>{ctrLedLabel(ctr.name)}</title>
                  </circle>
                  <g
                    class="topo-dependency-port"
                    classList={{
                      locked: anyVmRunning(),
                      active:
                        dependencyDrag()?.kind === "container" &&
                        dependencyDrag()?.index === ci(),
                      target:
                        dependencyDrag() !== null &&
                        !(
                          dependencyDrag()!.kind === "container" &&
                          dependencyDrag()!.index === ci()
                        ),
                    }}
                    onPointerDown={(e: PointerEvent) => dependencyDown(e, "container", ci())}
                  >
                    <circle cx={p().x + VM_W / 2} cy={p().y} r="5" />
                    <text x={p().x + VM_W / 2 + 9} y={p().y + 3.5}>
                      DEP
                    </text>
                    <title>
                      {anyVmRunning()
                        ? "Configuration is locked while a machine is up"
                        : "Drag to a VM or container that this container depends on"}
                    </title>
                  </g>
                  {/* Power, restart, and shell actions mirror the VM card. */}
                  <VmBtn
                    x={p().x + VM_W - 70}
                    y={p().y + 8}
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
                    x={p().x + VM_W - 49}
                    y={p().y + 8}
                    act="restart"
                    title={
                      ctrRunning(ctr.name) ? "Restart" : "Restart unavailable while powered off"
                    }
                    disabled={!ctrRunning(ctr.name)}
                    onClick={() => containerRestart(ctr.name)}
                  >
                    <RotateCw size={11} />
                  </VmBtn>
                  <VmBtn
                    x={p().x + VM_W - 28}
                    y={p().y + 8}
                    act="shell"
                    title="Open the container shell"
                    onClick={() => showContainer(ctr.name)}
                  >
                    <SquareTerminal size={11} />
                  </VmBtn>
                  <Index each={ctr.nics}>
                    {(nic, i) => {
                      const port = () => machineNicPort("container", ctr.name, i, nic());
                      const runtime = () => runtimeNic("container", ctr.name, i);
                      const rowY = () => p().y + 48 + i * NIC_ROW_H;
                      const target = () => (nic().nat ? "NAT" : nic().segment ?? "unplugged");
                      const compactTarget = () =>
                        target().length > 6 ? `${target().slice(0, 5)}…` : target();
                      return (
                        <>
                          <rect
                            class="topo-nic-row-bg"
                            x={p().x + 1}
                            y={rowY()}
                            width={VM_W - 2}
                            height={NIC_ROW_H}
                          />
                          <g
                            class="topo-nic-icon"
                            transform={`translate(${p().x + 8} ${rowY() + 7})`}
                          >
                            <EthernetPort size={10} />
                          </g>
                          <text class="topo-nic-index" x={p().x + 21} y={rowY() + 16.5}>
                            NIC {i}
                          </text>
                          <text class="topo-nic-target" x={p().x + 55} y={rowY() + 16.5}>
                            {compactTarget()}
                            <title>{target()}</title>
                          </text>
                          <NicIpEditor
                            x={p().x + 110}
                            y={rowY() + 1}
                            staticIp={nic().ip}
                            assignedIp={runtime()?.ip ?? null}
                            disabled={anyVmRunning()}
                            staticAllowed={!!nic().segment && !nic().nat}
                            gateway={nic().gateway}
                            gatewayAllowed={nicGatewayAllowed(nic())}
                            validate={(value) => staticIpError("container", ci(), i, value)}
                            onChange={(value) => setNicStaticIp("container", ci(), i, value)}
                            onGatewayChange={(enabled) =>
                              setMachineNicGateway("container", ci(), i, enabled)
                            }
                          />
                          <circle
                            cx={port().x}
                            cy={port().y}
                            r={socketDrag() ? 7 : 4}
                            class="topo-port"
                            classList={{
                              target: socketDrag() !== null,
                              loose: !nic().segment && !nic().nat,
                              locked: anyVmRunning(),
                            }}
                            onPointerDown={(e: PointerEvent) =>
                              dotDown(e, "container", ci(), i)
                            }
                          >
                            <title>
                              {anyVmRunning()
                                ? "Networking is read-only while a machine is up"
                                : !nic().segment && !nic().nat
                                  ? "Unplugged NIC — drag onto a switch or the WAN to connect"
                                  : "Drag to another bar to move (empty space unplugs)"}
                            </title>
                          </circle>
                        </>
                      );
                    }}
                  </Index>
                </g>
              );
            }}
          </For>

          {/* Clickable guest/container service-port numbers sit beside the
              machine endpoint of each host-forward cable. */}
          <For each={hostPorts()}>
            {(entry) => (
              <Show when={entry.target} keyed>
                {(target) => {
                  const p = () => machineServicePort(entry);
                  const valid = () => hostPortValid(entry);
                  return (
                    <g
                      class="topo-service-port"
                      classList={{ invalid: !valid(), live: hostPortLive(entry) }}
                    >
                      <circle cx={p().x} cy={p().y} r="4" />
                      <PortNumberEditor
                        x={p().side === "left" ? p().x - 54 : p().x + 6}
                        y={p().y - 12}
                        value={entry.guestPort ?? 0}
                        valid={valid()}
                        disabled={anyVmRunning()}
                        label={`${target.name} service port`}
                        onChange={(value) => setHostEntryPort(entry, "guest", value)}
                      />
                    </g>
                  );
                }}
              </Show>
            )}
          </For>
        </g>
      </svg>
    </div>
  );
}
