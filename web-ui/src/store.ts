// Central reactive state + actions. Status is loaded on lab selection and
// patched live from the /api/events WebSocket; action calls hit the REST API
// and let the confirming event refresh the view.

import { createStore } from "solid-js/store";
import { toast } from "@forge/ui";
import type { StatusTone, Tone } from "@forge/ui";
import * as api from "./api";
import type {
  Container,
  FastpathInfo,
  LabEntry,
  LabStatus,
  TemplateInfo,
  Vm,
  DaemonEvent,
} from "./api";
import { playDestroyRecreate } from "./fx";

export type ViewKind = "lab" | "vm" | "container" | "templates";

// A template or container-image download in progress, driven by the
// template.pull.* / container.pull.* events the lab daemon streams while
// pulling on up/start or via the "Download templates" button (issue #1).
// Keyed by `lab/vm` (`vm` = machine name).
export interface Pull {
  lab: string;
  vm: string;
  reference: string;
  status: "checking" | "pulling" | "error";
  percent: number;
  bytesDone: number;
  bytesTotal: number;
  error?: string;
}

// A template build or push running on the supervisor, driven by the
// template.op.* events (started from the Templates page). Keyed by
// `lab/template`; settled entries stay visible until dismissed.
export interface TemplateOp {
  lab: string;
  template: string;
  kind: string; // "build" | "push"
  status: "running" | "done" | "error";
  log: string[];
  version?: string;
  error?: string;
}

interface State {
  ready: boolean; // initial auth probe done
  authRequired: boolean;
  authUser: string | null;
  loggedIn: boolean;
  labs: LabEntry[];
  currentLab: string | null;
  status: LabStatus | null;
  view: { kind: ViewKind; vm: string | null };
  connected: boolean;
  error: string | null;
  pulls: Record<string, Pull>;
  templates: TemplateInfo[];
  templateOps: Record<string, TemplateOp>;
  fastpath: FastpathInfo | null;
}

const [state, setState] = createStore<State>({
  ready: false,
  authRequired: false,
  authUser: null,
  loggedIn: false,
  labs: [],
  currentLab: null,
  status: null,
  view: { kind: "lab", vm: null },
  connected: false,
  error: null,
  pulls: {},
  templates: [],
  templateOps: {},
  fastpath: null,
});

export { state };

/** App-wide notification via the forge Toaster (mounted once in App). */
export function showToast(msg: string, tone: Tone = "success") {
  toast(msg, { tone });
}

let eventSocket: WebSocket | null = null;
let refreshTimer: number | undefined;

// --- lifecycle ------------------------------------------------------------

export async function init() {
  try {
    const probe = await api.authProbe();
    setState({ authRequired: probe.auth_required, authUser: probe.user });
    const loggedIn = !probe.auth_required || api.getToken() !== "";
    setState({ loggedIn });
    if (loggedIn) await afterLogin();
  } catch (e) {
    setState({ error: String(e) });
  } finally {
    setState({ ready: true });
  }
}

export async function doLogin(username: string, password: string) {
  const { token } = await api.login(username, password);
  api.setToken(token);
  setState({ loggedIn: true, error: null });
  await afterLogin();
}

export async function doLogout() {
  try {
    await api.logout();
  } catch {
    /* ignore */
  }
  api.clearToken();
  eventSocket?.close();
  setState({
    loggedIn: false,
    labs: [],
    status: null,
    currentLab: null,
    pulls: {},
    templates: [],
    templateOps: {},
    fastpath: null,
  });
}

async function afterLogin() {
  await loadLabs();
  connectEvents();
  loadFastpath();
}

/** The daemon's network fast-path tier for the Topbar badge; a server
 *  without the endpoint just leaves the badge hidden. */
async function loadFastpath() {
  try {
    setState({ fastpath: await api.fastpathInfo() });
  } catch {
    setState({ fastpath: null });
  }
}

// --- data -----------------------------------------------------------------

export async function loadLabs() {
  try {
    const labs = await api.listLabs();
    setState({ labs });
    if (!state.currentLab && labs.length) {
      await selectLab(labs[0].name);
    }
  } catch (e) {
    setState({ error: String(e) });
  }
}

/** Guard consulted before switching labs (the visual editor registers one
 *  while it holds unsaved edits). Returns false to cancel the switch. */
let navGuard: (() => Promise<boolean>) | null = null;
export function setNavGuard(guard: (() => Promise<boolean>) | null) {
  navGuard = guard;
}

export async function selectLab(name: string) {
  if (name === state.currentLab) return;
  if (navGuard && !(await navGuard())) return;
  setState({ currentLab: name, view: { kind: "lab", vm: null }, templates: [] });
  await refreshStatus();
  await loadTemplates();
  await resyncTemplateOps();
}

/** Load the lab's `template {}` definitions (drives the Templates page and
 *  its sidebar entry — no templates in vmlab.wcl means neither appears). */
export async function loadTemplates() {
  const lab = state.currentLab;
  if (!lab) return;
  try {
    const templates = await api.listTemplates(lab);
    if (state.currentLab === lab) setState({ templates });
  } catch {
    // Unreachable daemon or an unparsable lab file: just hide the page.
    if (state.currentLab === lab) setState({ templates: [] });
  }
}

/** Re-fetch running build/push ops (with their log tails) after a WS
 *  (re)connect or lab switch, so a reloaded browser picks up mid-flight ops. */
async function resyncTemplateOps() {
  const lab = state.currentLab;
  if (!lab) return;
  try {
    const ops = await api.templateOps(lab);
    if (state.currentLab !== lab) return;
    // Solid stores shallow-merge objects: keep settled entries (recent
    // results stay visible), delete stale running ones (undefined removes a
    // key), and overwrite with the server's view of what is running.
    setState("templateOps", (prev) => {
      const next: Record<string, TemplateOp> = {};
      for (const [key, op] of Object.entries(prev)) {
        if (op && op.status === "running") next[key] = undefined as unknown as TemplateOp;
      }
      for (const op of ops) {
        next[`${lab}/${op.template}`] = {
          lab,
          template: op.template,
          kind: op.kind,
          status: "running",
          log: op.log_tail.slice(),
        };
      }
      return next;
    });
  } catch {
    /* ignore — events still arrive */
  }
}

export async function refreshStatus() {
  if (!state.currentLab) return;
  try {
    const status = await api.labStatus(state.currentLab);
    setState({ status, error: null });
  } catch (e) {
    setState({ error: String(e) });
  }
}

function scheduleRefresh() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(() => refreshStatus(), 350) as unknown as number;
}

// --- navigation -----------------------------------------------------------

export function showLab() {
  setState("view", { kind: "lab", vm: null });
}
export function showTemplates() {
  setState("view", { kind: "templates", vm: null });
  loadTemplates();
}
export function showVm(vm: string) {
  setState("view", { kind: "vm", vm });
}
export function showContainer(name: string) {
  setState("view", { kind: "container", vm: name });
}
/** Create a new lab, refresh the list, and jump to its page (the designer
 *  lives there under the stats). */
export async function createLabAndOpen(name: string, path?: string): Promise<void> {
  await api.createLab(name, path);
  const labs = await api.listLabs();
  setState({ labs });
  if (navGuard && !(await navGuard())) return;
  setState({ currentLab: name, view: { kind: "lab", vm: null }, templates: [] });
  await refreshStatus();
  await loadTemplates();
}

/** True when some machine's template/image still needs downloading — shows
 *  the "Download templates" button on the lab overview. */
export function needsPull(): boolean {
  return (
    (state.status?.vms ?? []).some((v) => v.template_cached === false) ||
    (state.status?.containers ?? []).some((c) => c.image_cached === false)
  );
}

/** True if any VM or container in the current lab is not stopped (gates a
 *  reload — the daemon can't re-adopt running machines). */
export function anyVmRunning(): boolean {
  return (
    (state.status?.vms ?? []).some((v) => v.state !== "stopped") ||
    (state.status?.containers ?? []).some((c) => c.state !== "stopped")
  );
}

/** Runtime configuration is mutable only once a machine is fully stopped. */
export function vmIsUp(name: string): boolean {
  const vm = state.status?.vms.find((v) => v.name === name);
  return vm !== undefined && vm.state !== "stopped";
}

/** Containers follow the same conservative lifecycle rule as VMs. */
export function containerIsUp(name: string): boolean {
  const container = state.status?.containers.find((c) => c.name === name);
  return container !== undefined && container.state !== "stopped";
}

/** Restart the lab daemon so it re-reads vmlab.wcl, then refresh the view. */
export async function reloadLab(): Promise<void> {
  const lab = state.currentLab;
  if (!lab) return;
  await api.reloadLab(lab);
  await loadLabs();
  await refreshStatus();
}

// --- destroy fx session (armed only by destroyLab; never by stop-all) ------

// While a session is armed, each machine's power-off shatters its topology
// node into particles that reform (the lab definition survives a destroy).
let destroyFx: { lab: string; fired: Set<string>; timer: number } | null = null;

function fireDestroyFx(key: string) {
  if (!destroyFx || destroyFx.fired.has(key)) return; // once per machine
  destroyFx.fired.add(key);
  playDestroyRecreate(key);
}

function beginDestroyFx(lab: string) {
  endDestroyFx();
  destroyFx = {
    lab,
    fired: new Set(),
    // Safety net: a dropped WS means lab.down never arrives to disarm us.
    timer: setTimeout(endDestroyFx, 60_000) as unknown as number,
  };
  // Machines already powered off emit no stop event — explode them now.
  for (const v of state.status?.vms ?? [])
    if (v.state === "stopped") fireDestroyFx(`vm:${v.name}`);
  for (const c of state.status?.containers ?? [])
    if (c.state === "stopped") fireDestroyFx(`container:${c.name}`);
}

function endDestroyFx() {
  if (!destroyFx) return;
  clearTimeout(destroyFx.timer);
  destroyFx = null;
}

// --- actions --------------------------------------------------------------

async function run(label: string, fn: () => Promise<unknown>) {
  try {
    await fn();
    showToast(label);
    scheduleRefresh();
  } catch (e) {
    showToast(`Failed: ${e}`, "danger");
  }
}

// `force` = kill instead of the graceful stop ladder (down / stop / the
// stop half of restart).
const f = (force?: boolean) => (force ? "Force stopping" : "Stopping");

export const startAll = () =>
  run("Starting lab", () => api.labAction(state.currentLab!, "up"));
/** Download missing templates/images without starting anything; progress
 *  arrives as template.pull.* / container.pull.* events (PullPanel). */
export const pullLab = () =>
  run("Templates downloaded", () => api.labAction(state.currentLab!, "pull"));
export const stopAll = (force?: boolean) =>
  run(`${f(force)} lab`, () => api.labAction(state.currentLab!, "down", force));
// Not via run(): the fx session must be armed BEFORE the await — the
// per-machine stop events stream while the destroy POST is still in flight.
export async function destroyLab() {
  const lab = state.currentLab;
  if (!lab) return;
  beginDestroyFx(lab);
  try {
    await api.labAction(lab, "destroy");
    showToast("Destroying lab");
    scheduleRefresh();
  } catch (e) {
    endDestroyFx();
    showToast(`Failed: ${e}`, "danger");
  }
}

export const vmStart = (vm: string) =>
  run(`Starting ${vm}`, () => api.vmAction(state.currentLab!, vm, "start"));
export const vmStop = (vm: string, force?: boolean) =>
  run(`${f(force)} ${vm}`, () => api.vmAction(state.currentLab!, vm, "stop", force));
export const vmRestart = (vm: string, force?: boolean) =>
  run(`Restarting ${vm}`, () => api.vmAction(state.currentLab!, vm, "restart", force));

export const containerStart = (name: string) =>
  run(`Starting ${name}`, () => api.containerAction(state.currentLab!, name, "start"));
export const containerStop = (name: string, force?: boolean) =>
  run(`${f(force)} ${name}`, () => api.containerAction(state.currentLab!, name, "stop", force));
export const containerRestart = (name: string, force?: boolean) =>
  run(`Restarting ${name}`, () =>
    api.containerAction(state.currentLab!, name, "restart", force),
  );

export const takeSnapshot = (name: string, vm?: string) =>
  run("Snapshot saved", () => api.takeSnapshot(state.currentLab!, name, vm));
export const restoreSnapshot = (name: string, vm?: string) =>
  run("Snapshot restored", () => api.restoreSnapshot(state.currentLab!, name, vm));
export const deleteSnapshot = (vm: string, name: string) =>
  run("Snapshot deleted", () => api.deleteSnapshot(state.currentLab!, vm, name));

/** Start a template build; progress arrives as template.op.* events. */
export async function buildTemplate(tpl: string) {
  try {
    await api.buildTemplate(state.currentLab!, tpl);
    showToast(`Building ${tpl}`);
  } catch (e) {
    showToast(`Failed: ${e}`);
  }
}

/** Push a stored template version (default: newest) to its registry. */
export async function publishTemplate(tpl: string, version?: string) {
  try {
    await api.publishTemplate(state.currentLab!, tpl, version);
    showToast(`Publishing ${tpl}`);
  } catch (e) {
    showToast(`Failed: ${e}`);
  }
}

/** Delete a snapshot from every VM in the lab that has it (lab-wide delete). */
export async function deleteLabSnapshot(name: string) {
  const lab = state.currentLab;
  const st = state.status;
  if (!lab || !st) return;
  await Promise.allSettled(
    st.vms.map((v) => api.deleteSnapshot(lab, v.name, name)),
  );
  showToast("Snapshot deleted");
  scheduleRefresh();
}

/** Snapshot names across all VMs in the current lab (for the lab-wide restore
 *  picker), de-duplicated by name and sorted newest first. */
export async function labSnapshotList(): Promise<{ name: string; taken_at: string }[]> {
  const lab = state.currentLab;
  const st = state.status;
  if (!lab || !st) return [];
  const lists = await Promise.all(
    st.vms.map((v) => api.vmSnapshots(lab, v.name).catch(() => [])),
  );
  const latest = new Map<string, string>();
  for (const list of lists) {
    for (const snap of list) {
      const at = snap.taken_at ?? "";
      const prev = latest.get(snap.name);
      if (prev === undefined || at > prev) latest.set(snap.name, at);
    }
  }
  return [...latest.entries()]
    .map(([name, taken_at]) => ({ name, taken_at }))
    .sort((a, b) => b.taken_at.localeCompare(a.taken_at));
}

// --- events ---------------------------------------------------------------

function connectEvents() {
  eventSocket?.close();
  const ws = new WebSocket(api.wsUrl("/api/events"));
  eventSocket = ws;
  ws.onopen = () => {
    setState({ connected: true });
    // Events missed while disconnected are gone; re-sync running ops.
    resyncTemplateOps();
  };
  ws.onclose = () => {
    setState({ connected: false });
    // Reconnect after a short delay while still logged in.
    if (state.loggedIn) setTimeout(connectEvents, 2000);
  };
  ws.onmessage = (msg) => {
    try {
      const ev: DaemonEvent = JSON.parse(msg.data);
      handleEvent(ev);
    } catch {
      /* ignore malformed */
    }
  };
}

function handleEvent(ev: DaemonEvent) {
  // Template / container-image pulls (issue #1): track download progress
  // separately and DON'T schedule a status refresh on every chunk — the
  // status call blocks behind the pull, so one queued refresh per tick would
  // pile up. Refresh only when a pull settles, by which point the daemon is
  // (about to be) up.
  if (ev.event.startsWith("template.pull.") || ev.event.startsWith("container.pull.")) {
    handlePullEvent(ev);
    return;
  }
  // Template builds/pushes: tracked separately, no status refresh per log line.
  if (ev.event.startsWith("template.op.")) {
    handleTemplateOpEvent(ev);
    return;
  }
  // Destroy fx: shatter each node as its machine powers off. Falls through —
  // the same events still drive loadLabs()/scheduleRefresh() below.
  if (destroyFx && ev.lab === destroyFx.lab) {
    if (ev.event === "vm.stopped" && ev.data?.vm) {
      fireDestroyFx(`vm:${ev.data.vm}`);
    } else if (ev.event === "container.stopped" && ev.data?.container) {
      fireDestroyFx(`container:${ev.data.container}`);
    } else if (ev.event === "lab.down") {
      // Sweep: a machine whose stop was a no-op (stale status said running)
      // emitted nothing — every node gets its effect exactly once.
      for (const v of state.status?.vms ?? []) fireDestroyFx(`vm:${v.name}`);
      for (const c of state.status?.containers ?? [])
        fireDestroyFx(`container:${c.name}`);
      endDestroyFx();
    }
  }
  // Host-scoped registry changes refresh the lab list; lab-scoped VM/state
  // events refresh the current lab's status. Host-scoped events with no lab
  // (segment.peer.up/down from the supervisor's trunk table, host.disk_low)
  // also land in the refetch branch — the peer LED is status-driven.
  if (ev.event.startsWith("lab.")) {
    loadLabs();
  }
  if (!ev.lab || ev.lab === state.currentLab) {
    scheduleRefresh();
  }
}

function handlePullEvent(ev: DaemonEvent) {
  // template.pull.* carries the VM name under `vm`; container.pull.* the
  // container name under `container`. Both feed the same progress panel.
  const vm = (ev.data?.vm ?? ev.data?.container) as string | undefined;
  if (!ev.lab || !vm) return;
  const key = `${ev.lab}/${vm}`;
  // "template.pull.start" / "container.pull.start" → "start", etc.
  const phase = ev.event.split(".").pop();
  switch (phase) {
    case "start":
      setState("pulls", key, {
        lab: ev.lab,
        vm,
        reference: ev.data.reference ?? "",
        status: "checking",
        percent: 0,
        bytesDone: 0,
        bytesTotal: 0,
      });
      break;
    case "progress":
      setState("pulls", key, {
        lab: ev.lab,
        vm,
        reference: ev.data.reference ?? state.pulls[key]?.reference ?? "",
        status: "pulling",
        percent: ev.data.percent ?? 0,
        bytesDone: ev.data.bytes_done ?? 0,
        bytesTotal: ev.data.bytes_total ?? 0,
      });
      break;
    case "done":
      clearPull(key);
      scheduleRefresh();
      break;
    case "error":
      setState("pulls", key, (p) =>
        p ? { ...p, status: "error" as const, error: String(ev.data.error ?? "pull failed") } : p,
      );
      // Leave the error visible briefly, then drop it.
      setTimeout(() => clearPull(key), 6000);
      scheduleRefresh();
      break;
  }
}

function clearPull(key: string) {
  setState("pulls", key, undefined as unknown as Pull);
}

// Log lines kept per operation (matches the supervisor's ring).
const MAX_OP_LOG = 500;

function handleTemplateOpEvent(ev: DaemonEvent) {
  const template = ev.data?.template as string | undefined;
  if (!ev.lab || !template) return;
  const key = `${ev.lab}/${template}`;
  const fresh = (): TemplateOp => ({
    lab: ev.lab,
    template,
    kind: String(ev.data.kind ?? "build"),
    status: "running",
    log: [],
  });
  switch (ev.event) {
    case "template.op.start":
      // Merge semantics: explicitly clear leftovers from a previous settled
      // op under the same key (undefined deletes the field).
      setState("templateOps", key, {
        ...fresh(),
        version: ev.data.version,
        error: undefined,
      });
      break;
    case "template.op.log": {
      const line = String(ev.data.line ?? "");
      // A missed start (reconnect race) still gets a live entry.
      setState("templateOps", key, (op) => {
        const base = op ?? fresh();
        const log =
          base.log.length >= MAX_OP_LOG
            ? [...base.log.slice(1), line]
            : [...base.log, line];
        return { ...base, log };
      });
      break;
    }
    case "template.op.done":
      setState("templateOps", key, (op) => ({
        ...(op ?? fresh()),
        status: "done" as const,
        version: ev.data.version,
      }));
      showToast(
        `${ev.data.kind === "push" ? "Published" : "Built"} ${template}@${ev.data.version ?? ""}`,
      );
      loadTemplates();
      break;
    case "template.op.error":
      setState("templateOps", key, (op) => ({
        ...(op ?? fresh()),
        status: "error" as const,
        error: String(ev.data.error ?? "operation failed"),
      }));
      showToast(`Failed: ${template}`);
      loadTemplates();
      break;
  }
}

/** Drop a settled (done/error) operation from the Templates page. */
export function dismissTemplateOp(key: string) {
  setState("templateOps", key, undefined as unknown as TemplateOp);
}

/** Build/push operations for the current lab, stable order by template. */
export function currentTemplateOps(): TemplateOp[] {
  const lab = state.currentLab;
  if (!lab) return [];
  return Object.values(state.templateOps)
    .filter((o): o is TemplateOp => !!o && o.lab === lab)
    .sort((a, b) => a.template.localeCompare(b.template));
}

/** Active template pulls for the current lab, newest-stable order by vm name. */
export function currentPulls(): Pull[] {
  const lab = state.currentLab;
  if (!lab) return [];
  return Object.values(state.pulls)
    .filter((p): p is Pull => !!p && p.lab === lab)
    .sort((a, b) => a.vm.localeCompare(b.vm));
}

// --- derived helpers (shared by views) ------------------------------------

export interface StateLook {
  label: string;
  tone: StatusTone; // forge Badge/StatusDot tone
}

export function look(vm: Vm): StateLook {
  switch (vm.state) {
    case "running":
      return vm.ready
        ? { label: "running", tone: "success" }
        : { label: "booting", tone: "warning" };
    case "starting":
      return { label: "booting", tone: "warning" };
    default:
      return { label: "stopped", tone: "neutral" };
  }
}

/** Container state badge: like [`look`], plus the healthcheck verdict. */
export function containerLook(c: Container): StateLook {
  switch (c.state) {
    case "running":
      if (!c.ready) return { label: "starting", tone: "warning" };
      if (c.health === false) return { label: "unhealthy", tone: "danger" };
      return { label: "running", tone: "success" };
    case "starting":
      return { label: "starting", tone: "warning" };
    default:
      return c.exit_code != null && c.exit_code !== 0
        ? { label: `exited (${c.exit_code})`, tone: "danger" }
        : { label: "stopped", tone: "neutral" };
  }
}

export function archOf(vm: Vm): string {
  if (vm.arch) return vm.arch;
  const slash = vm.template.indexOf("/");
  return slash > 0 ? vm.template.slice(0, slash) : "x86_64";
}

export function osOf(vm: Vm): string {
  const slash = vm.template.indexOf("/");
  return slash > 0 ? vm.template.slice(slash + 1) : vm.template;
}

export function fmtMem(bytes: number | null): string {
  if (!bytes) return "—";
  const mb = bytes / (1024 * 1024);
  return mb >= 1024 ? `${Math.round(mb / 102.4) / 10} GB` : `${Math.round(mb)} MB`;
}

/** Compact byte size for download progress (e.g. "734 MB", "1.4 GB"). */
export function fmtBytes(bytes: number): string {
  if (bytes <= 0) return "0 MB";
  const mb = bytes / (1024 * 1024);
  return mb >= 1024 ? `${(mb / 1024).toFixed(1)} GB` : `${Math.round(mb)} MB`;
}
