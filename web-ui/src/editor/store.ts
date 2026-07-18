// Editor-scoped state: the baseline model (as fetched, spans intact), the
// editable draft, selection, and the save/validate flow. Module-level so
// the draft survives view switches; it resets on lab switch.

import { createStore, produce } from "solid-js/store";
import * as api from "../api";
import type { CatalogMeta, ConfigIssue, HostInfo, StoreTemplate } from "../api";
import { StaleRev, ValidationError } from "../api";
import { registerNavGuard, showToast } from "../store";
import { confirmDialog } from "../components/dialogs";
import type {
  ContainerModel,
  HandlerModel,
  LabModel,
  PlaybookModel,
  ProvisionModel,
  Span,
  TemplateSummary,
  VmModel,
  SegmentModel,
} from "./model";
import { deepClone, emptyContainer, emptySegment, emptyVm, uniqueName } from "./model";
import { buildOps } from "./ops";

export type Selection =
  | { kind: "lab" }
  | { kind: "segment"; index: number }
  | { kind: "vm"; index: number }
  | { kind: "container"; index: number }
  | { kind: "provision"; index: number }
  | { kind: "playbook"; index: number }
  | { kind: "nat" }
  | { kind: "remote"; host: string };

export interface HostPortDraft {
  id: string;
  hostPort: number;
}

interface EditorState {
  lab: string | null;
  loading: boolean;
  /** Set when the file exists but doesn't parse — the editor can't open it. */
  fallback: string | null;
  /** Set when a save hit a 409 — the file changed underneath the draft. */
  conflict: boolean;
  path: string;
  rev: string | null;
  /** Raw vmlab.wcl text matching `rev` — used to map issue lines to blocks. */
  source: string;
  baseline: LabModel | null;
  draft: LabModel | null;
  templatesInFile: TemplateSummary[];
  selection: Selection;
  busy: string | null;
  issues: ConfigIssue[];
  /** Remote-vmlab peer nodes not (yet) referenced by any segment's connect
   *  block — pure UI state, persisted per lab in localStorage like node
   *  positions (an unattached node writes nothing to WCL). */
  remoteDrafts: string[];
  /** Host sockets not yet attached to a machine. Once cabled they become a
   *  normal segment forward or container port block in the WCL draft. */
  hostPortDrafts: HostPortDraft[];
  catalog: {
    templates: StoreTemplate[];
    profiles: string[];
    meta: CatalogMeta | null;
    host: HostInfo | null;
  };
}

const [editor, setEditor] = createStore<EditorState>({
  lab: null,
  loading: false,
  fallback: null,
  conflict: false,
  path: "",
  rev: null,
  source: "",
  baseline: null,
  draft: null,
  templatesInFile: [],
  selection: { kind: "lab" },
  busy: null,
  issues: [],
  remoteDrafts: [],
  hostPortDrafts: [],
  catalog: { templates: [], profiles: [], meta: null, host: null },
});

export { editor, setEditor };

/** The op batch a Save would send right now. */
export function pendingOps() {
  if (!editor.baseline || !editor.draft) return [];
  return buildOps(editor.baseline, editor.draft);
}

export function editorDirty(): boolean {
  return pendingOps().length > 0;
}

// --- loading ----------------------------------------------------------------

export async function openEditor(lab: string) {
  if (editor.lab === lab && editor.draft) return; // keep the live draft
  setEditor({
    lab,
    loading: true,
    fallback: null,
    conflict: false,
    baseline: null,
    draft: null,
    issues: [],
    selection: { kind: "lab" },
    remoteDrafts: loadRemoteDrafts(lab),
    hostPortDrafts: loadHostPortDrafts(lab),
  });
  await Promise.all([reloadModel(lab), loadCatalogs()]);
}

// --- host port node -----------------------------------------------------------

const hostPortsKey = (lab: string) => `vmlab.editor.host-ports.${lab}`;

function loadHostPortDrafts(lab: string): HostPortDraft[] {
  try {
    const raw = localStorage.getItem(hostPortsKey(lab));
    const list = raw ? (JSON.parse(raw) as unknown) : [];
    if (!Array.isArray(list)) return [];
    return list.filter(
      (p): p is HostPortDraft =>
        typeof p === "object" &&
        p !== null &&
        typeof (p as HostPortDraft).id === "string" &&
        typeof (p as HostPortDraft).hostPort === "number",
    );
  } catch {
    return [];
  }
}

function persistHostPortDrafts() {
  if (!editor.lab) return;
  try {
    localStorage.setItem(hostPortsKey(editor.lab), JSON.stringify(editor.hostPortDrafts));
  } catch {
    /* quota — unattached visual sockets are best-effort */
  }
}

export function addHostPort(): string {
  const id = globalThis.crypto?.randomUUID?.() ?? `port-${Date.now()}`;
  setEditor("hostPortDrafts", editor.hostPortDrafts.length, { id, hostPort: 0 });
  persistHostPortDrafts();
  return id;
}

export function setHostPortDraft(id: string, hostPort: number) {
  const index = editor.hostPortDrafts.findIndex((p) => p.id === id);
  if (index < 0) return;
  setEditor("hostPortDrafts", index, "hostPort", hostPort);
  persistHostPortDrafts();
}

export function removeHostPortDraft(id: string) {
  setEditor(
    "hostPortDrafts",
    editor.hostPortDrafts.filter((p) => p.id !== id),
  );
  persistHostPortDrafts();
}

/** Attach an unbound host socket to a machine. Container mappings use their
 *  native `port {}` block; VM mappings use a `forward {}` on the VM's first
 *  declared segment NIC. Both guest ends start invalid at port 0 by design. */
export function attachHostPort(id: string, kind: MachineKind, index: number): boolean {
  const draft = editor.hostPortDrafts.find((p) => p.id === id);
  const lab = editor.draft;
  if (!draft || !lab) return false;

  if (kind === "container") {
    const container = lab.containers[index];
    if (!container?.nics.length) {
      showToast("Add a NIC before forwarding a host port to this container", "warning");
      return false;
    }
    setEditor(
      "draft",
      "containers",
      index,
      "ports",
      container.ports.length,
      { span: null, host: draft.hostPort, container: 0, proto: "tcp" },
    );
  } else {
    const vm = lab.vms[index];
    const segmentName = vm?.nics.find((nic) => !nic.nat && nic.segment)?.segment;
    const segmentIndex = lab.segments.findIndex((segment) => segment.name === segmentName);
    if (!vm || segmentIndex < 0) {
      showToast("A VM needs a segment NIC before a host port can be attached", "warning");
      return false;
    }
    const forwards = lab.segments[segmentIndex].forwards;
    setEditor(
      "draft",
      "segments",
      segmentIndex,
      "forwards",
      forwards.length,
      {
        span: null,
        host_port: draft.hostPort,
        vm: vm.name,
        guest_port: 0,
        proto: "tcp",
      },
    );
  }

  removeHostPortDraft(id);
  return true;
}

// --- remote-vmlab peer nodes ---------------------------------------------------

const remotesKey = (lab: string) => `vmlab.editor.remotes.${lab}`;

function loadRemoteDrafts(lab: string): string[] {
  try {
    const raw = localStorage.getItem(remotesKey(lab));
    const list = raw ? (JSON.parse(raw) as unknown) : [];
    return Array.isArray(list) ? list.filter((h) => typeof h === "string") : [];
  } catch {
    return [];
  }
}

function persistRemoteDrafts() {
  if (!editor.lab) return;
  try {
    localStorage.setItem(remotesKey(editor.lab), JSON.stringify(editor.remoteDrafts));
  } catch {
    /* quota — cosmetic data, ignore */
  }
}

/** Distinct remote-vmlab nodes: every host referenced by a segment's connect
 *  block, plus unattached drafts (`""` = the not-yet-addressed placeholder). */
export function remoteHosts(): string[] {
  return [
    ...new Set([
      ...(editor.draft?.segments ?? []).flatMap((s) => (s.connect ? [s.connect.host] : [])),
      ...editor.remoteDrafts,
    ]),
  ];
}

/** Toolbar: create (or reuse) the unaddressed placeholder node, selected so
 *  the inspector opens on its address field. */
export function addRemote() {
  if (!remoteHosts().includes("")) {
    setEditor("remoteDrafts", editor.remoteDrafts.length, "");
    persistRemoteDrafts();
  }
  select({ kind: "remote", host: "" });
}

/** Cable segment `index` to a remote peer (`host`), or detach (`null`).
 *  Attaching also sets `global = true` — cross-host peering rides the
 *  supervisor's shared switch, and validation rejects connect without it.
 *  Detaching keeps `global` (it has independent meaning) and keeps the node
 *  on the canvas as an unattached draft. */
export function setSegmentPeer(index: number, host: string | null) {
  const seg = editor.draft?.segments[index];
  if (!seg) return;
  if (host === null) {
    const old = seg.connect?.host;
    setEditor("draft", "segments", index, "connect", null);
    if (old !== undefined && !editor.remoteDrafts.includes(old)) {
      setEditor("remoteDrafts", editor.remoteDrafts.length, old);
      persistRemoteDrafts();
    }
    return;
  }
  setEditor(
    "draft",
    "segments",
    index,
    produce((s: SegmentModel) => {
      if (s.connect) s.connect.host = host; // keep span → surgical set_field
      else s.connect = { span: null, host };
      s.global = true;
    }),
  );
}

/** Re-address a node: rewrite every attached segment's connect block, the
 *  draft entry, and the selection. Renaming onto an existing node merges. */
export function renameRemote(from: string, to: string) {
  if (from === to) return;
  setEditor(
    "draft",
    produce((d: LabModel | null) => {
      if (!d) return;
      for (const s of d.segments) {
        if (s.connect?.host === from) s.connect.host = to;
      }
    }),
  );
  setEditor(
    "remoteDrafts",
    editor.remoteDrafts.filter((h) => h !== from && h !== to).concat(
      // Keep a draft entry only while nothing references the new host —
      // attached nodes are derived from the connect blocks.
      editor.draft?.segments.some((s) => s.connect?.host === to) ? [] : [to],
    ),
  );
  persistRemoteDrafts();
  if (editor.selection.kind === "remote" && editor.selection.host === from) {
    select({ kind: "remote", host: to });
  }
}

/** Delete the node: detach every attached segment (connect = null, global
 *  kept) and drop the draft entry. */
export function removeRemote(host: string) {
  setEditor(
    "draft",
    produce((d: LabModel | null) => {
      if (!d) return;
      for (const s of d.segments) {
        if (s.connect?.host === host) s.connect = null;
      }
    }),
  );
  setEditor(
    "remoteDrafts",
    editor.remoteDrafts.filter((h) => h !== host),
  );
  persistRemoteDrafts();
  select({ kind: "lab" });
}

/** Discard the draft and re-fetch the model from disk. */
export async function reloadModel(lab = editor.lab): Promise<void> {
  if (!lab) return;
  setEditor({ loading: true, fallback: null, conflict: false, issues: [] });
  try {
    const [doc, raw] = await Promise.all([
      api.getLabModel(lab),
      api.getConfig(lab).catch(() => null),
    ]);
    if (editor.lab !== lab) return;
    setEditor({
      loading: false,
      path: doc.path,
      rev: doc.rev,
      source: raw?.content ?? "",
      baseline: doc.lab,
      draft: deepClone(doc.lab),
      templatesInFile: doc.templates,
    });
  } catch (e) {
    if (editor.lab !== lab) return;
    const msg =
      e instanceof ValidationError
        ? `vmlab.wcl has ${e.issues.length} problem(s) — fix it in the Config page first`
        : e instanceof Error
          ? e.message
          : String(e);
    setEditor({ loading: false, fallback: msg });
  }
}

async function loadCatalogs() {
  const [templates, profiles, meta, host] = await Promise.all([
    api.listStoreTemplates().catch(() => [] as StoreTemplate[]),
    api.listProfiles().catch(() => [] as string[]),
    api.catalogMeta().catch(() => null),
    api.hostInfo().catch(() => null),
  ]);
  setEditor("catalog", { templates, profiles, meta, host });
}

// --- selection ---------------------------------------------------------------

export function select(sel: Selection) {
  setEditor("selection", sel);
}

// --- draft mutations ----------------------------------------------------------

export function addVm(): number {
  const draft = editor.draft;
  if (!draft) return -1;
  const name = uniqueName("vm", draft.vms.map((v) => v.name));
  // Default to the first store template so a fresh VM validates once the
  // user picks nothing else; empty string still forces a choice visually.
  const tpl = editor.catalog.templates[0];
  const template = tpl ? `${tpl.arch}/${tpl.name}` : "";
  setEditor("draft", "vms", draft.vms.length, emptyVm(name, template));
  const index = editor.draft!.vms.length - 1;
  select({ kind: "vm", index });
  return index;
}

export function addContainer(image = ""): number {
  const draft = editor.draft;
  if (!draft) return -1;
  // VMs and containers share one name namespace.
  const name = uniqueName(
    "container",
    [...draft.vms.map((v) => v.name), ...draft.containers.map((c) => c.name)],
  );
  setEditor("draft", "containers", draft.containers.length, emptyContainer(name, image));
  const index = editor.draft!.containers.length - 1;
  select({ kind: "container", index });
  return index;
}

export function addSegment(): number {
  const draft = editor.draft;
  if (!draft) return -1;
  const name = uniqueName("segment", draft.segments.map((s) => s.name));
  setEditor("draft", "segments", draft.segments.length, emptySegment(name));
  const index = editor.draft!.segments.length - 1;
  select({ kind: "segment", index });
  return index;
}

export function addProvision(script: string): number {
  const draft = editor.draft;
  if (!draft) return -1;
  setEditor("draft", "provisions", draft.provisions.length, { span: null, script, vms: [] });
  const index = editor.draft!.provisions.length - 1;
  select({ kind: "provision", index });
  return index;
}

export function removeProvision(index: number) {
  setEditor(
    "draft",
    "provisions",
    produce((provisions: ProvisionModel[]) => {
      provisions.splice(index, 1);
    }),
  );
  select({ kind: "lab" });
}

export function addPlaybook(path: string): number {
  const draft = editor.draft;
  if (!draft) return -1;
  const play = path.split("/").pop() || "main";
  setEditor("draft", "playbooks", draft.playbooks.length, { span: null, path, play, vms: [] });
  const index = editor.draft!.playbooks.length - 1;
  select({ kind: "playbook", index });
  return index;
}

export function removePlaybook(index: number) {
  setEditor(
    "draft",
    "playbooks",
    produce((playbooks: PlaybookModel[]) => {
      playbooks.splice(index, 1);
    }),
  );
  select({ kind: "lab" });
}

export function removeVm(index: number) {
  setEditor(
    "draft",
    produce((draft: LabModel | null) => {
      const removed = draft?.vms[index]?.name;
      if (!draft || !removed) return;
      draft.vms.splice(index, 1);
      for (const machine of [...draft.vms, ...draft.containers]) {
        machine.depends_on = machine.depends_on.filter((name) => name !== removed);
      }
      for (const provision of draft.provisions) {
        provision.vms = provision.vms.filter((name) => name !== removed);
      }
      for (const playbook of draft.playbooks) {
        playbook.vms = playbook.vms.filter((name) => name !== removed);
      }
      for (const handler of draft.handlers) {
        handler.targets = handler.targets.filter((name) => name !== removed);
      }
    }),
  );
  select({ kind: "lab" });
}

export function removeContainer(index: number) {
  setEditor(
    "draft",
    produce((draft: LabModel | null) => {
      const removed = draft?.containers[index]?.name;
      if (!draft || !removed) return;
      draft.containers.splice(index, 1);
      for (const machine of [...draft.vms, ...draft.containers]) {
        machine.depends_on = machine.depends_on.filter((name) => name !== removed);
      }
      for (const provision of draft.provisions) {
        provision.vms = provision.vms.filter((name) => name !== removed);
      }
      for (const playbook of draft.playbooks) {
        playbook.vms = playbook.vms.filter((name) => name !== removed);
      }
      for (const handler of draft.handlers) {
        handler.targets = handler.targets.filter((name) => name !== removed);
      }
    }),
  );
  select({ kind: "lab" });
}

export function removeSegment(index: number) {
  setEditor(
    "draft",
    "segments",
    produce((segments: SegmentModel[]) => {
      segments.splice(index, 1);
    }),
  );
  select({ kind: "lab" });
}

/** VMs and containers attach to segments the same way; NIC edits address a
 *  machine by kind + index into the matching draft collection. */
export type MachineKind = "vm" | "container";

/** Add a directional machine dependency. VM and container names share one
 *  namespace, so either kind can depend on either kind. */
export function addMachineDependency(kind: MachineKind, index: number, target: string) {
  const draft = editor.draft;
  const machine = kind === "vm" ? draft?.vms[index] : draft?.containers[index];
  if (!draft || !machine || machine.name === target || machine.depends_on.includes(target)) return;
  if (![...draft.vms, ...draft.containers].some((candidate) => candidate.name === target)) return;
  if (kind === "vm") {
    setEditor("draft", "vms", index, "depends_on", [...machine.depends_on, target]);
  } else {
    setEditor("draft", "containers", index, "depends_on", [...machine.depends_on, target]);
  }
}

/** Remove one directional machine dependency. */
export function removeMachineDependency(kind: MachineKind, index: number, target: string) {
  const machine = kind === "vm" ? editor.draft?.vms[index] : editor.draft?.containers[index];
  if (!machine) return;
  const next = machine.depends_on.filter((name) => name !== target);
  if (kind === "vm") setEditor("draft", "vms", index, "depends_on", next);
  else setEditor("draft", "containers", index, "depends_on", next);
}

export function addProvisionTarget(index: number, target: string) {
  const provision = editor.draft?.provisions[index];
  if (!provision || provision.vms.includes(target) || !machineNames().includes(target)) return;
  setEditor("draft", "provisions", index, "vms", [...provision.vms, target]);
}

export function removeProvisionTarget(index: number, target: string) {
  const provision = editor.draft?.provisions[index];
  if (!provision) return;
  setEditor(
    "draft",
    "provisions",
    index,
    "vms",
    provision.vms.filter((name) => name !== target),
  );
}

export function addPlaybookTarget(index: number, target: string) {
  const playbook = editor.draft?.playbooks[index];
  if (!playbook || playbook.vms.includes(target) || !machineNames().includes(target)) return;
  setEditor("draft", "playbooks", index, "vms", [...playbook.vms, target]);
}

export function removePlaybookTarget(index: number, target: string) {
  const playbook = editor.draft?.playbooks[index];
  if (!playbook) return;
  setEditor(
    "draft",
    "playbooks",
    index,
    "vms",
    playbook.vms.filter((name) => name !== target),
  );
}

export function addScriptEventHandler(script: string, event: string): number {
  const draft = editor.draft;
  if (!draft) return -1;
  const existing = draft.handlers.findIndex(
    (handler) => handler.run === script && handler.event === event,
  );
  if (existing >= 0) return existing;
  setEditor("draft", "handlers", draft.handlers.length, {
    span: null,
    event,
    run: script,
    targets: [],
  });
  return editor.draft!.handlers.length - 1;
}

export function removeEventHandler(index: number) {
  setEditor(
    "draft",
    "handlers",
    produce((handlers: HandlerModel[]) => {
      handlers.splice(index, 1);
    }),
  );
}

export function eventTargetKind(event: string): MachineKind | "machine" | null {
  if (event.startsWith("vm.")) return "vm";
  if (event.startsWith("container.")) return "container";
  if (event.startsWith("snapshot.")) return "machine";
  return null;
}

export function addEventHandlerTarget(index: number, kind: MachineKind, target: string) {
  const handler = editor.draft?.handlers[index];
  const accepts = handler ? eventTargetKind(handler.event) : null;
  if (
    !handler ||
    accepts === null ||
    (accepts !== "machine" && accepts !== kind) ||
    handler.targets.includes(target)
  ) {
    return;
  }
  setEditor("draft", "handlers", index, "targets", [...handler.targets, target]);
}

export function removeEventHandlerTarget(index: number, target: string) {
  const handler = editor.draft?.handlers[index];
  if (!handler) return;
  setEditor(
    "draft",
    "handlers",
    index,
    "targets",
    handler.targets.filter((name) => name !== target),
  );
}

/** The addressed machine's NIC list, or null when it doesn't exist. */
function machineNics(kind: MachineKind, index: number) {
  const d = editor.draft;
  if (!d) return null;
  return (kind === "vm" ? d.vms[index] : d.containers[index])?.nics ?? null;
}

/** Attach a new NIC on machine `index` (segment name, or null = NAT). */
export function addMachineNic(kind: MachineKind, index: number, segment: string | null) {
  const nics = machineNics(kind, index);
  if (!nics) return;
  const nic = {
    span: null as Span | null,
    segment,
    nat: segment === null,
    ip: null,
    gateway: false,
    mac: null,
    isolated: false,
  };
  if (kind === "vm") setEditor("draft", "vms", index, "nics", nics.length, nic);
  else setEditor("draft", "containers", index, "nics", nics.length, nic);
}

/** Re-home NIC `nicIndex` of machine `index` (segment name, or null = NAT/WAN). */
export function setMachineNicTarget(
  kind: MachineKind,
  index: number,
  nicIndex: number,
  segment: string | null,
) {
  if (!machineNics(kind, index)?.[nicIndex]) return;
  const patch = { segment, nat: segment === null, gateway: false };
  if (kind === "vm") setEditor("draft", "vms", index, "nics", nicIndex, patch);
  else setEditor("draft", "containers", index, "nics", nicIndex, patch);
}

/** Unplug NIC `nicIndex` of machine `index`: it stays on the machine as a
 *  loose port (no segment, no NAT) ready to be cabled somewhere else. */
export function disconnectMachineNic(kind: MachineKind, index: number, nicIndex: number) {
  if (!machineNics(kind, index)?.[nicIndex]) return;
  const patch = { segment: null, nat: false, gateway: false };
  if (kind === "vm") setEditor("draft", "vms", index, "nics", nicIndex, patch);
  else setEditor("draft", "containers", index, "nics", nicIndex, patch);
}

/** Designate one static NIC as its segment's router. The mutation is atomic:
 *  one gateway wins across VMs and containers, and daemon NAT on that
 *  segment is disabled so DHCP clients follow the selected machine. */
export function setMachineNicGateway(
  kind: MachineKind,
  index: number,
  nicIndex: number,
  enabled: boolean,
) {
  setEditor(
    "draft",
    produce((draft: LabModel | null) => {
      if (!draft) return;
      const machine = kind === "vm" ? draft.vms[index] : draft.containers[index];
      const nic = machine?.nics[nicIndex];
      if (!nic) return;
      if (!enabled) {
        nic.gateway = false;
        nic.ip = null;
        return;
      }
      const segment = nic.segment;
      if (!segment || nic.nat) return;
      const segmentModel = draft.segments.find((candidate) => candidate.name === segment);
      const routerIp = segmentModel?.subnet ? firstUsableAddress(segmentModel.subnet) : null;
      if (!segmentModel || segmentModel.global || !routerIp) return;
      for (const candidate of [...draft.vms, ...draft.containers]) {
        for (const candidateNic of candidate.nics) {
          if (candidateNic.segment === segment && candidateNic.gateway) {
            candidateNic.gateway = false;
            candidateNic.ip = null;
          }
        }
      }
      nic.gateway = true;
      nic.ip = routerIp;
      nic.nat = false;
      segmentModel.nat = false;
    }),
  );
}

/** First usable IPv4 address in a CIDR, used by dedicated gateway mode. */
function firstUsableAddress(cidr: string): string | null {
  const [address, prefixText] = cidr.split("/");
  const octets = address?.split(".").map(Number);
  const prefix = Number(prefixText);
  if (
    octets?.length !== 4 ||
    octets.some((octet) => !Number.isInteger(octet) || octet < 0 || octet > 255) ||
    !Number.isInteger(prefix) ||
    prefix < 0 ||
    prefix > 30
  ) {
    return null;
  }
  const ip =
    (((octets[0] << 24) >>> 0) + (octets[1] << 16) + (octets[2] << 8) + octets[3]) >>> 0;
  const mask = prefix === 0 ? 0 : (0xffffffff << (32 - prefix)) >>> 0;
  const router = ((ip & mask) + 1) >>> 0;
  return [router >>> 24, (router >>> 16) & 255, (router >>> 8) & 255, router & 255].join(".");
}

/** Attach a new NIC on VM `vmIndex` (segment name, or null = NAT). */
export const addNic = (vmIndex: number, segment: string | null) =>
  addMachineNic("vm", vmIndex, segment);

/** Interconnect: segment `fromIndex` routes to segment `to` (idempotent). */
export function addSegmentRoute(fromIndex: number, to: string) {
  const seg = editor.draft?.segments[fromIndex];
  if (!seg || seg.name === to || seg.routes_to.includes(to)) return;
  setEditor("draft", "segments", fromIndex, "routes_to", [...seg.routes_to, to]);
}

/** Remove the `fromIndex` → `to` interconnect. */
export function removeSegmentRoute(fromIndex: number, to: string) {
  const seg = editor.draft?.segments[fromIndex];
  if (!seg) return;
  setEditor(
    "draft",
    "segments",
    fromIndex,
    "routes_to",
    seg.routes_to.filter((n) => n !== to),
  );
}

/** Connect/disconnect segment `index` to the WAN (NAT egress). */
export function setSegmentNat(index: number, on: boolean) {
  if (!editor.draft?.segments[index]) return;
  setEditor("draft", "segments", index, "nat", on);
}

/** Rewrite every name reference shared by VMs and containers (depends_on
 *  spans both, provisions and forwards target machines by name). */
function rewriteMachineRefs(d: LabModel, from: string, to: string) {
  for (const vm of d.vms) {
    vm.depends_on = vm.depends_on.map((n) => (n === from ? to : n));
  }
  for (const c of d.containers) {
    c.depends_on = c.depends_on.map((n) => (n === from ? to : n));
  }
  for (const p of d.provisions) {
    p.vms = p.vms.map((n) => (n === from ? to : n));
  }
  for (const p of d.playbooks) {
    p.vms = p.vms.map((n) => (n === from ? to : n));
  }
  for (const handler of d.handlers) {
    handler.targets = handler.targets.map((name) => (name === from ? to : name));
  }
  for (const s of d.segments) {
    for (const f of s.forwards) {
      if (f.vm === from) f.vm = to;
    }
  }
}

/** Rename a VM and rewrite every reference to it in the draft. */
export function renameVm(index: number, to: string) {
  const draft = editor.draft;
  if (!draft) return;
  const from = draft.vms[index].name;
  if (from === to) return;
  setEditor(
    "draft",
    produce((d: LabModel | null) => {
      if (!d) return;
      d.vms[index].name = to;
      rewriteMachineRefs(d, from, to);
    }),
  );
}

/** Rename a container and rewrite every reference to it in the draft. */
export function renameContainer(index: number, to: string) {
  const draft = editor.draft;
  if (!draft) return;
  const from = draft.containers[index].name;
  if (from === to) return;
  setEditor(
    "draft",
    produce((d: LabModel | null) => {
      if (!d) return;
      d.containers[index].name = to;
      rewriteMachineRefs(d, from, to);
    }),
  );
}

/** Rename a segment and rewrite every reference to it in the draft. */
export function renameSegment(index: number, to: string) {
  const draft = editor.draft;
  if (!draft) return;
  const from = draft.segments[index].name;
  if (from === to) return;
  setEditor(
    "draft",
    produce((d: LabModel | null) => {
      if (!d) return;
      d.segments[index].name = to;
      for (const s of d.segments) {
        s.routes_to = s.routes_to.map((n) => (n === from ? to : n));
      }
      for (const m of [...d.vms, ...d.containers]) {
        for (const n of m.nics) {
          if (n.segment === from) n.segment = to;
        }
      }
    }),
  );
}

// --- save / validate ----------------------------------------------------------

export function revertDraft() {
  if (!editor.baseline) return;
  setEditor({ draft: deepClone(editor.baseline), issues: [], conflict: false });
}

function onSaveError(e: unknown) {
  if (e instanceof ValidationError) {
    setEditor("issues", e.issues);
    showToast(`${e.issues.length} validation issue(s)`, "danger");
  } else if (e instanceof StaleRev) {
    setEditor("conflict", true);
    showToast("Config changed on disk — reload the editor", "danger");
  } else {
    showToast(`Error: ${e instanceof Error ? e.message : String(e)}`, "danger");
  }
}

export async function validateDraft() {
  const lab = editor.lab;
  if (!lab || !editor.rev) return;
  setEditor("busy", "validate");
  try {
    await api.editLabModel(lab, editor.rev, pendingOps(), true);
    setEditor("issues", []);
    showToast("Config is valid");
  } catch (e) {
    onSaveError(e);
  } finally {
    setEditor("busy", null);
  }
}

export async function saveDraft(): Promise<boolean> {
  const lab = editor.lab;
  if (!lab || !editor.rev) return false;
  setEditor("busy", "save");
  try {
    const res = await api.editLabModel(lab, editor.rev, pendingOps(), false);
    if (res.lab && res.rev) {
      setEditor({
        rev: res.rev,
        source: res.source ?? editor.source,
        baseline: res.lab,
        draft: deepClone(res.lab),
        templatesInFile: res.templates ?? editor.templatesInFile,
        issues: [],
        conflict: false,
      });
    }
    showToast("Config saved");
    return true;
  } catch (e) {
    onSaveError(e);
    return false;
  } finally {
    setEditor("busy", null);
  }
}

/** Names usable as references in the draft (pickers). */
export const vmNames = () => editor.draft?.vms.map((v) => v.name) ?? [];
export const containerNames = () => editor.draft?.containers.map((c) => c.name) ?? [];
/** VM + container names — `depends_on` spans both kinds. */
export const machineNames = () => [...vmNames(), ...containerNames()];
export const segmentNames = () => editor.draft?.segments.map((s) => s.name) ?? [];

/** The store-catalog entry a `<arch>/<name>[@version]` ref resolves to —
 *  the source of a VM's inherited defaults (cpus/memory/profile/…). */
export function storeTemplateFor(ref: string): StoreTemplate | undefined {
  const m = /^([^/@]+)\/([^/@]+)(?:@(.+))?$/.exec(ref);
  if (!m) return undefined;
  const [, arch, name, version] = m;
  const candidates = editor.catalog.templates.filter(
    (t) => t.arch === arch && t.name === name,
  );
  return (version && candidates.find((t) => t.version === version)) || candidates[0];
}

// Lab switches consult this guard so an unsaved draft is never silently
// dropped (the draft itself survives view switches — it dies only when a
// different lab's model loads over it).
registerNavGuard(async () => {
  if (!editorDirty()) return true;
  return confirmDialog({
    title: "Discard unsaved lab changes?",
    body: "The designer has edits that haven't been saved.",
    confirmLabel: "Discard",
    danger: true,
  });
});

/** Map a validation issue's 1-based line to the VM/segment whose span
 *  contains it (best effort — the canvas rings that node, clicking the
 *  issue selects it). Issues in edited-but-unsaved text may drift a little;
 *  the panel still shows the message either way. */
export function selectionForLine(line: number | null): Selection | null {
  const base = editor.baseline;
  if (line == null || !base || !editor.source) return null;
  // Byte offset of the line start (source is the rev the spans bind to).
  let offset = 0;
  const lines = editor.source.split("\n");
  for (let i = 0; i < Math.min(line - 1, lines.length); i++) {
    offset += lines[i].length + 1;
  }
  const contains = (span: Span | null) => span && offset >= span[0] && offset < span[1];
  for (let i = 0; i < base.vms.length; i++) {
    if (contains(base.vms[i].span)) return { kind: "vm", index: i };
  }
  for (let i = 0; i < base.containers.length; i++) {
    if (contains(base.containers[i].span)) return { kind: "container", index: i };
  }
  for (let i = 0; i < base.segments.length; i++) {
    if (contains(base.segments[i].span)) return { kind: "segment", index: i };
  }
  for (let i = 0; i < base.provisions.length; i++) {
    if (contains(base.provisions[i].span)) return { kind: "provision", index: i };
  }
  for (let i = 0; i < base.playbooks.length; i++) {
    if (contains(base.playbooks[i].span)) return { kind: "playbook", index: i };
  }
  return null;
}
