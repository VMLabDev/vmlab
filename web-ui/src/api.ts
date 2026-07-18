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
const del = (path: string) => req(path, { method: "DELETE" });

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
  /** Live IPv4 address reported for this interface, null while unknown/offline. */
  ip: string | null;
}
/** A declared HTTP UI on a machine, launchable in the Web tab (no
 *  credentials — the proxy fetches those separately). */
export interface WebPage {
  name: string;
  port: number;
  path: string;
}
export interface Vm {
  name: string;
  state: string;
  ready: boolean;
  ip: string | null;
  /** False while the registry template still needs downloading. */
  template_cached?: boolean;
  template: string;
  arch: string | null;
  cpus: number | null;
  memory: number | null;
  nics: Nic[];
  web?: WebPage[];
  /** vmlab-agent stamp baked into the template — null means the template
   *  predates agent support (no interactive terminal). */
  agent_version?: string | null;
}
/** One container's runtime status (labd `status()` containers array). */
export interface Container {
  name: string;
  state: string;
  ready: boolean;
  /** Latest healthcheck verdict; null = no check, or no report yet. */
  health: boolean | null;
  ip: string | null;
  /** False while the container image still needs downloading. */
  image_cached?: boolean;
  image: string;
  digest: string | null;
  restarts: number;
  exit_code: number | null;
  nics: Nic[];
  web?: WebPage[];
}
export interface Segment {
  name: string;
  subnet: string;
  gateway: string;
  nat: boolean;
  dhcp: boolean;
  global?: boolean;
  /** Cross-host peer target (`connect { host }`), when declared. */
  connect?: string | null;
  /** Live trunk state: true/false for global segments (any trunk up, keyed
   *  by segment name), null when not global or the supervisor is
   *  unreachable. Drives the remote-vmlab node's LED. */
  peer_connected?: boolean | null;
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
/** One exact record in a live DNS zone snapshot (`dns.table`). */
export interface DnsLiveRecord {
  name: string;
  ip: string;
  /** dynamic = auto-registered guest name; static = declared entry. */
  kind: "dynamic" | "static";
}
export interface DnsSegmentZone {
  segment: string;
  zone: {
    suffix: string;
    records: DnsLiveRecord[];
    wildcards: { id: number; pattern: string; ip: string }[];
    sinkholes: { id: number; pattern: string; mode: string }[];
  };
}

export const listLabs = (): Promise<LabEntry[]> => req("/api/labs");
export const labStatus = (lab: string): Promise<LabStatus> =>
  req(`/api/labs/${encodeURIComponent(lab)}`);
export const dnsTable = (lab: string): Promise<{ segments: DnsSegmentZone[] }> =>
  req(`/api/labs/${encodeURIComponent(lab)}/dns`);
// `force` applies to the stop-shaped actions (down / stop / restart's stop
// half): kill instead of the graceful ladder.
const forceQs = (force?: boolean) => (force ? "?force=true" : "");
export const labAction = (
  lab: string,
  action: "up" | "down" | "destroy" | "pull",
  force?: boolean,
) =>
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

/** One guest metrics sample (vmlab-agent `metrics` feature). */
export interface GuestStats {
  cpu_pct: number;
  mem_used: number;
  mem_total: number;
  disks: { mount: string; used: number; total: number }[];
}
export const vmStats = (lab: string, vm: string): Promise<GuestStats> =>
  req(`/api/labs/${encodeURIComponent(lab)}/vms/${encodeURIComponent(vm)}/stats`);
export const containerStats = (lab: string, container: string): Promise<GuestStats> =>
  req(
    `/api/labs/${encodeURIComponent(lab)}/containers/${encodeURIComponent(container)}/stats`,
  );
export const vmClipboardGet = (lab: string, vm: string): Promise<{ text: string }> =>
  req(`/api/labs/${encodeURIComponent(lab)}/vms/${encodeURIComponent(vm)}/clipboard`);
export const vmClipboardSet = (lab: string, vm: string, text: string) =>
  post(`/api/labs/${encodeURIComponent(lab)}/vms/${encodeURIComponent(vm)}/clipboard`, {
    text,
  });
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
  arch: string;
  kind: string;
  started: string;
  log_tail: string[];
  /** Raw forwarded `playbook.op.*` events from the build's playbooks
   *  (the same payloads that ride `template.op.step`), for resync. */
  steps?: { event?: string; data?: { cw?: unknown } }[];
  console_ready: boolean;
}

export const listTemplates = (lab: string): Promise<TemplateInfo[]> =>
  req(`/api/labs/${encodeURIComponent(lab)}/templates`);
export const templateOps = (lab: string): Promise<TemplateOpStatus[]> =>
  req(`/api/labs/${encodeURIComponent(lab)}/templates/ops`);
export const templateRemote = (lab: string, tpl: string, arch?: string): Promise<RemoteStatus> =>
  req(
    `/api/labs/${encodeURIComponent(lab)}/templates/${encodeURIComponent(tpl)}/remote${arch ? `?arch=${encodeURIComponent(arch)}` : ""}`,
  );
export const buildTemplate = (lab: string, tpl: string, arch?: string) =>
  post(`/api/labs/${encodeURIComponent(lab)}/templates/${encodeURIComponent(tpl)}/build`, {
    arch,
  });
export const stopTemplateBuild = (lab: string, tpl: string, arch: string) =>
  post(`/api/labs/${encodeURIComponent(lab)}/templates/${encodeURIComponent(tpl)}/stop`, {
    arch,
  });
export const publishTemplate = (lab: string, tpl: string, arch?: string, version?: string) =>
  post(
    `/api/labs/${encodeURIComponent(lab)}/templates/${encodeURIComponent(tpl)}/publish`,
    { arch, version },
  );

// --- playbooks (config-weave) -----------------------------------------------

/** One `playbook {}` block from the lab's vmlab.wcl (GET /playbooks). */
export interface PlaybookInfo {
  path: string;
  play: string;
  /** Targeted machine names; empty = every machine. */
  vms: string[];
}
/** One node of a playbook folder listing (GET /playbooks/tree). */
export interface PlaybookTreeEntry {
  name: string;
  /** Path relative to the playbook folder, `/`-separated. */
  path: string;
  dir: boolean;
  size?: number | null;
  children?: PlaybookTreeEntry[];
}
/** An in-flight check/apply with its recent log (GET /playbooks/ops). */
export interface PlaybookOpStatus {
  machine: string;
  playbook: string;
  play: string;
  kind: "check" | "apply";
  op_id: number;
  started: string;
  log_tail: string[];
  /** "running" while config-weave executes, "rebooting" between attempts. */
  phase?: "running" | "rebooting";
  reboot_attempt?: number;
  reboot_max?: number;
}

const pbBase = (lab: string) => `/api/labs/${encodeURIComponent(lab)}/playbooks`;

export const listPlaybooks = (lab: string): Promise<PlaybookInfo[]> => req(pbBase(lab));
export const playbookOps = (lab: string): Promise<PlaybookOpStatus[]> =>
  req(`${pbBase(lab)}/ops`);
/** Kick off a check/apply. 200 = finished fast (body: the run result),
 *  202 = detached; progress and the verdict arrive as playbook.op.* events. */
export const runPlaybook = (
  lab: string,
  kind: "vms" | "containers",
  machine: string,
  action: "check" | "apply",
  path?: string,
  play?: string,
) =>
  post(
    `/api/labs/${encodeURIComponent(lab)}/${kind}/${encodeURIComponent(machine)}/playbook/${action}`,
    { path, play },
  );
/** Create a declared playbook's folder + starter playbook.wcl if missing
 *  (idempotent; 403 = not declared in vmlab.wcl). */
export const scaffoldPlaybook = (lab: string, playbook: string) =>
  post(`${pbBase(lab)}/scaffold`, { playbook });

// --- config-weave packages (over a declared playbook folder) ----------------

export interface PkgSearchHit {
  repo: string;
  package: string;
  description: string;
  installed: boolean;
  installed_from: string | null;
}

export interface PkgRepo {
  name: string;
  url: string;
  branch: string | null;
  subdir: string | null;
  /** "not synced", "dirty", or a short commit. */
  cache: string;
}

/** `pkg add/remove/update` (update is always all packages). Slow ops (git
 *  network sync) run synchronously server-side — keep a busy state up. */
export const pkgAction = (
  lab: string,
  playbook: string,
  action: "add" | "remove" | "update",
  pkg?: string,
): Promise<{ ok: boolean; output: string }> =>
  post(`${pbBase(lab)}/pkg`, { playbook, action, package: pkg });

export const pkgSearch = (lab: string, playbook: string, term: string): Promise<PkgSearchHit[]> =>
  post(`${pbBase(lab)}/pkg/search`, { playbook, term });

/** Registered package repos; seeds the stdlib repo when none exist. */
export const pkgRepos = (
  lab: string,
  playbook: string,
): Promise<{ repos: PkgRepo[]; seeded: boolean; warning: string | null }> =>
  req(`${pbBase(lab)}/repos?playbook=${encodeURIComponent(playbook)}`);

export const pkgRepoEdit = (
  lab: string,
  playbook: string,
  action: "add" | "remove",
  name: string,
  opts?: { url?: string; branch?: string; subdir?: string },
): Promise<{ ok: boolean; output: string }> =>
  post(`${pbBase(lab)}/repos`, { playbook, action, name, ...opts });

// --- lab files (Files tab) --------------------------------------------------

/** One file from GET /files/file: text docs carry content+rev; binary or
 *  oversized files carry only metadata for a placeholder. */
export interface LabFileDoc {
  path: string;
  rev?: string;
  content?: string;
  binary?: boolean;
  tooLarge?: boolean;
  size?: number;
}

/** Thrown by deleteLabPath when a non-empty directory needs `recursive`. */
export class DirNotEmpty extends Error {}

const filesBase = (lab: string) => `/api/labs/${encodeURIComponent(lab)}/files`;

export const labFilesTree = (lab: string): Promise<{ entries: PlaybookTreeEntry[] }> =>
  req(`${filesBase(lab)}/tree`);

export const getLabFile = (lab: string, path: string): Promise<LabFileDoc> =>
  req(`${filesBase(lab)}/file?path=${encodeURIComponent(path)}`);

export async function saveLabFile(
  lab: string,
  path: string,
  content: string,
  baseRev: string | null,
): Promise<string> {
  const result = await req(`${filesBase(lab)}/file`, {
    method: "PUT",
    body: JSON.stringify({ path, content, base_rev: baseRev }),
  }).catch((error) => {
    if (error instanceof Error && error.message.includes("changed on disk")) {
      throw new ScriptStale();
    }
    throw error;
  });
  return result.rev;
}

export const mkdirLab = (lab: string, path: string) => post(`${filesBase(lab)}/mkdir`, { path });

export const renameLabPath = (lab: string, from: string, to: string) =>
  post(`${filesBase(lab)}/rename`, { from, to });

export async function deleteLabPath(lab: string, path: string, recursive = false): Promise<void> {
  await del(
    `${filesBase(lab)}/file?path=${encodeURIComponent(path)}&recursive=${recursive}`,
  ).catch((error) => {
    if (error instanceof Error && error.message.includes("is not empty")) {
      throw new DirNotEmpty(error.message);
    }
    throw error;
  });
}

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

export interface ScriptDoc {
  path: string;
  content: string;
  rev: string;
}

export class ScriptStale extends Error {
  constructor() {
    super("Script changed on disk — reload it before saving");
  }
}

const scriptUrl = (lab: string, path?: string) =>
  `/api/labs/${encodeURIComponent(lab)}/scripts${path ? `?path=${encodeURIComponent(path)}` : ""}`;

export async function getProvisionScript(lab: string, path: string): Promise<ScriptDoc | null> {
  const headers: Record<string, string> = {};
  const token = getToken();
  if (token) headers.Authorization = `Bearer ${token}`;
  const response = await fetch(scriptUrl(lab, path), { headers });
  if (response.status === 401) {
    clearToken();
    throw new Unauthorized("authentication required");
  }
  if (response.status === 404) return null;
  if (!response.ok) {
    const body = await response.json().catch(() => ({}));
    throw new Error(body.error ?? response.statusText);
  }
  return response.json();
}

export async function saveProvisionScript(
  lab: string,
  path: string,
  content: string,
  baseRev: string | null,
): Promise<string> {
  const result = await req(scriptUrl(lab), {
    method: "PUT",
    body: JSON.stringify({ path, content, base_rev: baseRev }),
  }).catch((error) => {
    if (error instanceof Error && error.message.includes("changed on disk")) {
      throw new ScriptStale();
    }
    throw error;
  });
  return result.rev;
}
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
export const removeStoreTemplate = (template: StoreTemplate): Promise<{ removed: string }> =>
  del(
    `/api/catalog/templates/${encodeURIComponent(template.arch)}/${encodeURIComponent(template.name)}/${encodeURIComponent(template.version)}`,
  );
export const listProfiles = (): Promise<string[]> => req("/api/catalog/profiles");
export const catalogMeta = (): Promise<CatalogMeta> => req("/api/catalog/meta");

export interface OciCatalogEntry {
  name: string;
  repo: string;
  arches: string[];
  version: string;
  reference: string;
}

export const searchOciCatalog = (
  registry: string,
  query: string,
  arch: string,
  kind: "vm" | "container",
): Promise<OciCatalogEntry[]> => {
  const params = new URLSearchParams({ registry, q: query, arch, kind });
  return req(`/api/catalog/oci?${params}`);
};

export interface RegistrySetting {
  namespace: string;
  vms: boolean;
  containers: boolean;
  authenticated: boolean;
}

export interface RegistrySettings {
  entries: RegistrySetting[];
  removed: string[];
}

export const listRegistries = (): Promise<RegistrySettings> => req("/api/registries");
export const addRegistrySetting = (
  namespace: string,
  useFor: "vms" | "containers" | "both",
): Promise<unknown> => post("/api/registries", { namespace, use_for: useFor });
export const removeRegistrySetting = (namespace: string): Promise<unknown> =>
  req("/api/registries", {
    method: "DELETE",
    body: JSON.stringify({ namespace }),
  });
export const loginRegistry = (
  namespace: string,
  username: string,
  password: string,
): Promise<{ authenticated: boolean }> =>
  post("/api/registries/login", { namespace, username, password });

/** Host capacity and virtualization acceleration (GET /api/host). */
export interface HostInfo {
  cpus: number;
  memory: number; // bytes
  acceleration: "kvm" | "tcg";
  arch: string;
  /** Suffix guest names register under (host config, default vmlab.internal). */
  dns_suffix: string;
}

export const hostInfo = (): Promise<HostInfo> => req("/api/host");

/** The daemon's network fast-path tier (GET /api/fastpath): which
 *  acceleration tier the supervisor probed into use, and why the skipped
 *  kernel tiers were unavailable (keyed by tier name). */
export interface FastpathInfo {
  tier: "afxdp" | "sockmap" | "userspace";
  mode: string;
  reasons: Record<string, string>;
}

export const fastpathInfo = (): Promise<FastpathInfo> => req("/api/fastpath");

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

// --- guest web pages ------------------------------------------------------

/** Mint the path-scoped `vmlab_web` cookie the iframe proxy rides (iframe
 *  subresources can't carry the bearer token). Call after login. */
export const mintWebSession = (): Promise<unknown> => post("/api/web/session", {});

/** The same-origin proxy URL for a declared page, opened in an iframe. */
export function webPageUrl(
  lab: string,
  kind: "vms" | "containers",
  machine: string,
  page: WebPage,
): string {
  const base = `/web/${encodeURIComponent(lab)}/${kind}/${encodeURIComponent(machine)}/${encodeURIComponent(page.name)}`;
  const path = page.path.startsWith("/") ? page.path : `/${page.path}`;
  return base + path;
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
