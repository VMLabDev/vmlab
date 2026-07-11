// Typed fetch + WebSocket helpers against the vmlab-web backend, with bearer
// token handling. The token (issued by /api/login) lives in localStorage and
// is sent as an Authorization header on REST calls and a ?token= query param
// on WebSocket upgrades (browsers can't set WS headers).

const TOKEN_KEY = "vmlab_token";

export function getToken(): string {
  return localStorage.getItem(TOKEN_KEY) ?? "";
}
export function setToken(t: string): void {
  localStorage.setItem(TOKEN_KEY, t);
}
export function clearToken(): void {
  localStorage.removeItem(TOKEN_KEY);
}

export class Unauthorized extends Error {}

async function req(path: string, opts: RequestInit = {}): Promise<any> {
  const headers: Record<string, string> = {
    ...((opts.headers as Record<string, string>) ?? {}),
  };
  const t = getToken();
  if (t) headers["Authorization"] = `Bearer ${t}`;
  if (opts.body && !headers["Content-Type"]) {
    headers["Content-Type"] = "application/json";
  }
  const res = await fetch(path, { ...opts, headers });
  if (res.status === 401) {
    clearToken();
    throw new Unauthorized("authentication required");
  }
  if (!res.ok) {
    let msg = res.statusText;
    try {
      msg = (await res.json()).error ?? msg;
    } catch {
      /* keep statusText */
    }
    throw new Error(msg);
  }
  const ct = res.headers.get("content-type") ?? "";
  return ct.includes("json") ? res.json() : res;
}

const post = (path: string, body?: unknown) =>
  req(path, { method: "POST", body: body ? JSON.stringify(body) : undefined });

// --- auth -----------------------------------------------------------------

export interface AuthProbe {
  auth_required: boolean;
  user: string | null;
}
export const authProbe = (): Promise<AuthProbe> => req("/api/auth");
export const login = (username: string, password: string): Promise<{ token: string }> =>
  post("/api/login", { username, password });
export const logout = (): Promise<unknown> => post("/api/logout");

// --- labs -----------------------------------------------------------------

export interface LabEntry {
  name: string;
  root?: string;
  state?: string;
}
export interface Nic {
  segment: string | null;
  mac: string | null;
  static_ip: string | null;
}
export interface Vm {
  name: string;
  state: string;
  ready: boolean;
  ip: string | null;
  template: string;
  arch: string | null;
  cpus: number | null;
  memory: number | null;
  nics: Nic[];
}
/** One container's runtime status (labd `status()` containers array). */
export interface Container {
  name: string;
  state: string;
  ready: boolean;
  /** Latest healthcheck verdict; null = no check, or no report yet. */
  health: boolean | null;
  ip: string | null;
  image: string;
  digest: string | null;
  restarts: number;
  exit_code: number | null;
  nics: Nic[];
}
export interface Segment {
  name: string;
  subnet: string;
  gateway: string;
  nat: boolean;
  dhcp: boolean;
}
export interface LabStatus {
  lab: string;
  vms: Vm[];
  containers: Container[];
  segments: Segment[];
}
export interface Snapshot {
  name: string;
  online: boolean;
  taken_at?: string;
}

export const listLabs = (): Promise<LabEntry[]> => req("/api/labs");
export const labStatus = (lab: string): Promise<LabStatus> =>
  req(`/api/labs/${encodeURIComponent(lab)}`);
// `force` applies to the stop-shaped actions (down / stop / restart's stop
// half): kill instead of the graceful ladder.
const forceQs = (force?: boolean) => (force ? "?force=true" : "");
export const labAction = (lab: string, action: "up" | "down" | "destroy", force?: boolean) =>
  post(`/api/labs/${encodeURIComponent(lab)}/${action}${forceQs(force)}`);
export const vmAction = (
  lab: string,
  vm: string,
  action: "start" | "stop" | "restart" | "destroy",
  force?: boolean,
) =>
  post(
    `/api/labs/${encodeURIComponent(lab)}/vms/${encodeURIComponent(vm)}/${action}${forceQs(force)}`,
  );
export const containerAction = (
  lab: string,
  container: string,
  action: "start" | "stop" | "restart" | "destroy",
  force?: boolean,
) =>
  post(
    `/api/labs/${encodeURIComponent(lab)}/containers/${encodeURIComponent(container)}/${action}${forceQs(force)}`,
  );
export const sendKeys = (lab: string, vm: string, keys: string) =>
  post(`/api/labs/${encodeURIComponent(lab)}/vms/${encodeURIComponent(vm)}/sendkeys`, {
    keys,
  });
export const vmSnapshots = (lab: string, vm: string): Promise<Snapshot[]> =>
  req(`/api/labs/${encodeURIComponent(lab)}/vms/${encodeURIComponent(vm)}/snapshots`);
export const deleteSnapshot = (lab: string, vm: string, name: string) =>
  req(
    `/api/labs/${encodeURIComponent(lab)}/vms/${encodeURIComponent(vm)}/snapshots/${encodeURIComponent(name)}`,
    { method: "DELETE" },
  );
export const takeSnapshot = (lab: string, name: string, vm?: string) =>
  post(`/api/labs/${encodeURIComponent(lab)}/snapshots`, { name, vm });
export const restoreSnapshot = (lab: string, name: string, vm?: string) =>
  post(
    `/api/labs/${encodeURIComponent(lab)}/snapshots/${encodeURIComponent(name)}/restore`,
    { vm },
  );

// --- templates (build + publish) --------------------------------------------

/** One `template {}` block from the lab's vmlab.wcl, joined with the local
 *  store and any in-flight operation (GET /templates). */
export interface TemplateInfo {
  name: string;
  arch: string;
  version_prefix: string;
  registry: string | null;
  /** Locally stored builds, newest first. */
  local_versions: string[];
  op: { kind: string; started: string } | null;
}
export interface RemoteTag {
  tag: string;
  arches: string[];
}
/** Published tags on the template's registry (GET /templates/{tpl}/remote). */
export interface RemoteStatus {
  registry: string;
  tags: RemoteTag[];
}
/** A running build/push with its recent log (GET /templates/ops). */
export interface TemplateOpStatus {
  template: string;
  kind: string;
  started: string;
  log_tail: string[];
}

export const listTemplates = (lab: string): Promise<TemplateInfo[]> =>
  req(`/api/labs/${encodeURIComponent(lab)}/templates`);
export const templateOps = (lab: string): Promise<TemplateOpStatus[]> =>
  req(`/api/labs/${encodeURIComponent(lab)}/templates/ops`);
export const templateRemote = (lab: string, tpl: string): Promise<RemoteStatus> =>
  req(
    `/api/labs/${encodeURIComponent(lab)}/templates/${encodeURIComponent(tpl)}/remote`,
  );
export const buildTemplate = (lab: string, tpl: string) =>
  post(`/api/labs/${encodeURIComponent(lab)}/templates/${encodeURIComponent(tpl)}/build`, {});
export const publishTemplate = (lab: string, tpl: string, version?: string) =>
  post(
    `/api/labs/${encodeURIComponent(lab)}/templates/${encodeURIComponent(tpl)}/publish`,
    { version },
  );

// --- config editing -------------------------------------------------------

export interface ConfigDoc {
  path: string;
  content: string;
}
export interface ConfigIssue {
  message: string;
  line: number | null;
}

/** Thrown by saveConfig/validateConfig on a 422; carries the WCL issues. */
export class ValidationError extends Error {
  issues: ConfigIssue[];
  constructor(issues: ConfigIssue[]) {
    super(`${issues.length} validation issue(s)`);
    this.issues = issues;
  }
}

export const getConfig = (lab: string): Promise<ConfigDoc> =>
  req(`/api/labs/${encodeURIComponent(lab)}/config`);

// Validate (+ optionally write) the config. A 422 carries the issue list, which
// the generic `req` would flatten away, so post directly and parse the body.
async function putConfig(lab: string, content: string, validateOnly: boolean): Promise<void> {
  const headers: Record<string, string> = { "Content-Type": "application/json" };
  const t = getToken();
  if (t) headers["Authorization"] = `Bearer ${t}`;
  const res = await fetch(`/api/labs/${encodeURIComponent(lab)}/config`, {
    method: "POST",
    headers,
    body: JSON.stringify({ content, validate_only: validateOnly }),
  });
  if (res.status === 401) {
    clearToken();
    throw new Unauthorized("authentication required");
  }
  if (res.status === 422) {
    const body = await res.json().catch(() => ({ issues: [] }));
    throw new ValidationError(body.issues ?? []);
  }
  if (!res.ok) {
    let msg = res.statusText;
    try {
      msg = (await res.json()).error ?? msg;
    } catch {
      /* keep statusText */
    }
    throw new Error(msg);
  }
}

export const validateConfig = (lab: string, content: string): Promise<void> =>
  putConfig(lab, content, true);
export const saveConfig = (lab: string, content: string): Promise<void> =>
  putConfig(lab, content, false);
export const reloadLab = (lab: string): Promise<unknown> =>
  post(`/api/labs/${encodeURIComponent(lab)}/reload`);

// --- visual editor ----------------------------------------------------------

import type { LabDocument, LabModel, ModelOp, TemplateSummary } from "./editor/model";

/** Thrown by editLabModel when the file changed underneath the editor. */
export class StaleRev extends Error {
  rev: string;
  constructor(rev: string) {
    super("config changed on disk — reload the editor");
    this.rev = rev;
  }
}

export const createLab = (name: string, path?: string): Promise<{ name: string; root: string }> =>
  post("/api/labs", { name, path });

/** One template in the local store (GET /api/catalog/templates). */
export interface StoreTemplate {
  name: string;
  arch: string;
  version: string;
  profile: string | null;
  cpus: number | null;
  memory: number | null;
  disk: number | null;
  firmware: string | null;
  tpm: boolean | null;
  secure_boot: boolean | null;
  display: string | null;
  created: string;
  origin: string | null;
  registry: string | null;
}

/** Schema enums for the editor's pickers (GET /api/catalog/meta). */
export interface CatalogMeta {
  arches: string[];
  events: string[];
  firmware: string[];
  gpu_modes: string[];
  sinkhole_modes: string[];
  forward_protos: string[];
  l4_protos: string[];
  media_kinds: string[];
  restart_policies: string[];
  /** `healthcheck {}` schema defaults (seconds / count). */
  healthcheck_defaults: {
    interval: number;
    timeout: number;
    retries: number;
    start_period: number;
  };
}

export const listStoreTemplates = (): Promise<StoreTemplate[]> => req("/api/catalog/templates");
export const listProfiles = (): Promise<string[]> => req("/api/catalog/profiles");
export const catalogMeta = (): Promise<CatalogMeta> => req("/api/catalog/meta");

/** Host capacity for the editor's hardware sliders (GET /api/host). */
export interface HostInfo {
  cpus: number;
  memory: number; // bytes
}

export const hostInfo = (): Promise<HostInfo> => req("/api/host");

/** One entry in a server-side directory listing (GET /api/host/fs). */
export interface FsEntry {
  name: string;
  dir: boolean;
  size: number | null;
}

export interface FsListing {
  path: string;
  parent: string | null;
  entries: FsEntry[];
}

export const browseFs = (path: string): Promise<FsListing> =>
  req(`/api/host/fs?path=${encodeURIComponent(path)}`);

/** The parsed lab model; 422 (unparsable file) throws ValidationError. */
export async function getLabModel(lab: string): Promise<LabDocument> {
  const res = await rawFetch(`/api/labs/${encodeURIComponent(lab)}/model`);
  if (res.status === 422) {
    const body = await res.json().catch(() => ({ issues: [] }));
    throw new ValidationError(body.issues ?? []);
  }
  return finish(res);
}

export interface EditResult {
  ok: boolean;
  rev?: string;
  lab?: LabModel;
  templates?: TemplateSummary[];
  source?: string;
}

/** Apply a surgical op batch. 422 → ValidationError, 409 → StaleRev. */
export async function editLabModel(
  lab: string,
  baseRev: string,
  ops: ModelOp[],
  validateOnly: boolean,
): Promise<EditResult> {
  const res = await rawFetch(`/api/labs/${encodeURIComponent(lab)}/model/edit`, {
    method: "POST",
    body: JSON.stringify({ base_rev: baseRev, validate_only: validateOnly, ops }),
  });
  if (res.status === 422) {
    const body = await res.json().catch(() => ({ issues: [] }));
    throw new ValidationError(body.issues ?? []);
  }
  if (res.status === 409) {
    const body = await res.json().catch(() => ({}));
    throw new StaleRev(body.rev ?? "");
  }
  return finish(res);
}

/** Authenticated fetch that leaves non-2xx handling to the caller (for
 *  endpoints with structured error bodies the generic `req` would flatten). */
async function rawFetch(path: string, opts: RequestInit = {}): Promise<Response> {
  const headers: Record<string, string> = {
    ...((opts.headers as Record<string, string>) ?? {}),
  };
  const t = getToken();
  if (t) headers["Authorization"] = `Bearer ${t}`;
  if (opts.body && !headers["Content-Type"]) headers["Content-Type"] = "application/json";
  const res = await fetch(path, { ...opts, headers });
  if (res.status === 401) {
    clearToken();
    throw new Unauthorized("authentication required");
  }
  return res;
}

async function finish(res: Response): Promise<any> {
  if (!res.ok) {
    let msg = res.statusText;
    try {
      msg = (await res.json()).error ?? msg;
    } catch {
      /* keep statusText */
    }
    throw new Error(msg);
  }
  return res.json();
}

// --- websockets -----------------------------------------------------------

export function wsUrl(path: string): string {
  const proto = location.protocol === "https:" ? "wss" : "ws";
  const t = getToken();
  const q = t ? `?token=${encodeURIComponent(t)}` : "";
  return `${proto}://${location.host}${path}${q}`;
}

export interface DaemonEvent {
  event: string;
  lab: string;
  data: any;
  ts: string;
}

// One line from the lab's logs (lab events, each VM's serial/qemu/swtpm, each
// container's console), streamed over /api/labs/{lab}/logs. `ts` is set only
// for events.jsonl lines.
export interface LogEntry {
  source: string; // "lab", the VM name, or the container name
  stream: string; // "events" | "lab" | "serial" | "qemu" | "swtpm" | "console"
  ts?: string | null;
  text: string;
}

export const logsWsUrl = (lab: string): string =>
  wsUrl(`/api/labs/${encodeURIComponent(lab)}/logs`);
