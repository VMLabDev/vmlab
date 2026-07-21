// Canvas node positions. Purely cosmetic (never part of vmlab.wcl), so they
// live in localStorage per lab, keyed by block name (names survive the
// span churn of a save; renames migrate the key).

import type { LabModel } from "./model";

export interface NodePos {
  x: number;
  y: number;
}

export interface ViewTransform {
  tx: number;
  ty: number;
  k: number;
}

export interface Layout {
  vms: Record<string, NodePos>;
  /** Container nodes (kept separate from VMs even though names never clash). */
  containers: Record<string, NodePos>;
  segments: Record<string, NodePos>;
  provisions: Record<string, NodePos>;
  /** config-weave playbook nodes, keyed like provisions (path + occurrence). */
  playbooks?: Record<string, NodePos>;
  /** Remote-vmlab peer nodes, keyed by the remote's `host[:port]` string
   *  (`""` = the not-yet-addressed placeholder). */
  remotes?: Record<string, NodePos>;
  nat?: NodePos;
  /** Physical host node (port-forward source), outside the lab enclosure. */
  host?: NodePos;
  view?: ViewTransform;
}

export const VM_W = 236;
/** Header + footer footprint before declaration-ordered NIC rows. */
export const VM_H = 78;
export const NIC_ROW_H = 25;
export function machineCardHeight(nicCount: number): number {
  return VM_H + nicCount * NIC_ROW_H;
}

export const PROVISION_W = 220;
export const PROVISION_H = 78;
export const PLAYBOOK_W = 220;
/** Folder-node header footprint before the per-play card rows. */
export const PLAYBOOK_HEADER_H = 46;
export const PLAY_ROW_H = 26;
const PLAYBOOK_FOOT_H = 10;
/** Node height for a folder with `cardCount` play cards (min one row —
 *  an empty/unscanned folder still shows a placeholder row). */
export function playbookCardHeight(cardCount: number): number {
  return PLAYBOOK_HEADER_H + Math.max(1, cardCount) * PLAY_ROW_H + PLAYBOOK_FOOT_H;
}

/** Stable-enough cosmetic identity for provisions, including repeated paths. */
export function provisionLayoutKey(model: LabModel, index: number): string {
  const script = model.provisions[index]?.script || "(new provision)";
  const occurrence = model.provisions
    .slice(0, index)
    .filter((provision) => provision.script === script).length;
  return `${script}\0${occurrence}`;
}

/** Cosmetic identity for playbook folder nodes — the bare path (one node
 *  per folder). [`playbookPos`] also consults the legacy per-block
 *  `path\0occurrence` key so saved layouts keep their positions. */
export function playbookLayoutKey(path: string): string {
  return path || "(new playbook)";
}

/** Stored position for a playbook folder node, with legacy-key fallback. */
export function playbookPos(layout: Layout, path: string): NodePos | undefined {
  const map = layout.playbooks ?? {};
  return map[playbookLayoutKey(path)] ?? map[`${playbookLayoutKey(path)}\0${0}`];
}

/** Minimum segment-bar width (room for the name + meta text); bars widen
 *  when their port bank outgrows it. */
export const SEG_W = 320;
export const SEG_H = 40;

// Segment bars wear a router-style bank of port sockets along their edges:
// one socket per connected NIC plus MIN_FREE_PORTS spares to drop onto.
export const PORT_X0 = 18;
export const PORT_SPACING = 18;
export const PORT_SIZE = 8;
export const MIN_FREE_PORTS = 2;

/** Bar width for `used` connected NICs, keeping MIN_FREE_PORTS spare. */
export function segWidthFor(used: number): number {
  const needed = 2 * PORT_X0 + (used + MIN_FREE_PORTS - 1) * PORT_SPACING + PORT_SIZE;
  return Math.max(SEG_W, needed);
}

const keyFor = (lab: string) => `vmlab.editor.layout.${lab}`;

export function loadLayout(lab: string): Layout {
  try {
    const raw = localStorage.getItem(keyFor(lab));
    if (raw) {
      const layout = JSON.parse(raw) as Layout;
      layout.containers ??= {};
      layout.provisions ??= {};
      layout.playbooks ??= {};
      layout.remotes ??= {};
      return layout;
    }
  } catch {
    /* corrupted layout: start fresh */
  }
  return { vms: {}, containers: {}, segments: {}, provisions: {}, playbooks: {}, remotes: {} };
}

export function saveLayout(lab: string, layout: Layout): void {
  try {
    localStorage.setItem(keyFor(lab), JSON.stringify(layout));
  } catch {
    /* cosmetic data: ignore storage quota failures */
  }
}

export function renameInLayout(
  layout: Layout,
  kind: "vms" | "containers" | "segments" | "provisions" | "playbooks" | "remotes",
  from: string,
  to: string,
): Layout {
  const map = { ...(layout[kind] ?? {}) };
  if (map[from] && !map[to]) map[to] = map[from];
  delete map[from];
  return { ...layout, [kind]: map };
}

/** True when any NIC uses the built-in NAT segment (draws the NAT bus). */
export function hasNatNic(model: LabModel): boolean {
  return [...model.vms, ...model.containers].some((machine) =>
    machine.nics.some((nic) => nic.nat),
  );
}

/** Deterministic auto-layout. Unknown machines sit above their first NIC's
 *  target; stored positions are retained until the user chooses Arrange. */
export function autoLayout(model: LabModel, existing: Layout): Layout {
  const out: Layout = {
    vms: { ...existing.vms },
    containers: { ...existing.containers },
    segments: { ...existing.segments },
    provisions: { ...(existing.provisions ?? {}) },
    playbooks: { ...(existing.playbooks ?? {}) },
    remotes: { ...(existing.remotes ?? {}) },
    nat: existing.nat,
    host: existing.host,
    view: existing.view,
  };
  const laneHeight = 170;
  const firstBarY = 230;

  model.segments.forEach((segment, index) => {
    if (!out.segments[segment.name]) {
      out.segments[segment.name] = { x: 60, y: firstBarY + index * laneHeight };
    }
  });
  if (hasNatNic(model) && !out.nat) {
    out.nat = { x: 60, y: firstBarY + model.segments.length * laneHeight };
  }

  const hosts = [
    ...new Set(model.segments.flatMap((segment) => (segment.connect ? [segment.connect.host] : []))),
  ];
  hosts.forEach((host, index) => {
    if (!out.remotes![host]) {
      out.remotes![host] = {
        x: 60,
        y: firstBarY + (model.segments.length + 1 + index) * laneHeight,
      };
    }
  });

  const laneCounts = new Map<string, number>();
  const place = (
    map: Record<string, NodePos>,
    name: string,
    nic: { nat: boolean; segment: string | null } | undefined,
    nicCount: number,
  ) => {
    if (map[name]) return;
    const lane = nic ? (nic.nat ? "__nat" : (nic.segment ?? "__none")) : "__none";
    const barY =
      lane === "__nat"
        ? (out.nat?.y ?? firstBarY)
        : lane === "__none"
          ? 120 + VM_H + 40
          : (out.segments[lane]?.y ?? firstBarY);
    const index = laneCounts.get(lane) ?? 0;
    laneCounts.set(lane, index + 1);
    map[name] = {
      x: 80 + index * (VM_W + 26),
      y: barY - machineCardHeight(nicCount) - 40,
    };
  };

  for (const vm of model.vms) place(out.vms, vm.name, vm.nics[0], vm.nics.length);
  for (const container of model.containers) {
    place(out.containers, container.name, container.nics[0], container.nics.length);
  }
  model.provisions.forEach((_, index) => {
    const key = provisionLayoutKey(model, index);
    if (!out.provisions[key]) {
      out.provisions[key] = {
        x: 80 + index * (PROVISION_W + 28),
        y: firstBarY + model.segments.length * laneHeight + 90,
      };
    }
  });
  [...new Set(model.playbooks.map((playbook) => playbook.path))].forEach((path, index) => {
    const key = playbookLayoutKey(path);
    if (!playbookPos(out, path)) {
      out.playbooks![key] = {
        // Their own row, below the provisions row.
        x: 80 + index * (PLAYBOOK_W + 28),
        y: firstBarY + model.segments.length * laneHeight + 90 + PROVISION_H + 40,
      };
    }
  });
  return out;
}
