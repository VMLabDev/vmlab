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
export const labAction = (lab: string, action: "up" | "down" | "destroy") =>
  post(`/api/labs/${encodeURIComponent(lab)}/${action}`);
export const vmAction = (
  lab: string,
  vm: string,
  action: "start" | "stop" | "restart" | "destroy",
) => post(`/api/labs/${encodeURIComponent(lab)}/vms/${encodeURIComponent(vm)}/${action}`);
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

// One line from the lab's logs (lab events + each VM's serial/qemu/swtpm),
// streamed over /api/labs/{lab}/logs. `ts` is set only for events.jsonl lines.
export interface LogEntry {
  source: string; // "lab" or the VM name
  stream: string; // "events" | "lab" | "serial" | "qemu" | "swtpm"
  ts?: string | null;
  text: string;
}

export const logsWsUrl = (lab: string): string =>
  wsUrl(`/api/labs/${encodeURIComponent(lab)}/logs`);
