// TypeScript mirror of the backend's lab-model DTO (src/config/dto.rs) plus
// the surgical-op wire types (src/config/edit_ops.rs). Every block carries
// its source span as the edit address; blocks created in the editor carry
// `span: null` until the first save returns the refreshed model.

export type Span = [number, number];

// --- blocks -----------------------------------------------------------------

export interface NicModel {
  span: Span | null;
  segment: string | null;
  nat: boolean;
  ip: string | null;
  gateway: boolean;
  mac: string | null;
  isolated: boolean;
}

export interface DiskModel {
  span: Span | null;
  name: string;
  /** Bytes. */
  size: number | null;
  from: string | null;
}

export interface ShareModel {
  span: Span | null;
  host: string;
  guest: string;
  readonly: boolean;
  smb1: boolean;
  /** Derived from the guest path when not declared — treated as optional. */
  name: string;
}

export interface MediaModel {
  span: Span | null;
  kind: string; // iso | floppy
  from: string;
  label: string | null;
}

export interface GpuModel {
  span: Span | null;
  mode: string; // passthrough | virgl | vulkan
  address: string | null;
}

/** Upstream credentials the proxy injects (flat form of `auth {}`; unused
 *  fields stay null per method). */
export interface WebAuthModel {
  span: Span | null;
  method: string; // basic | bearer | header | ntlm | form
  username: string | null;
  password: string | null;
  domain: string | null;
  token: string | null;
  header: string | null;
  value: string | null;
  login_path: string | null;
  login_method: string | null;
  login_body: string | null;
  login_content_type: string | null;
  fail_redirect: string | null;
}

export interface WebPageModel {
  span: Span | null;
  name: string;
  port: number;
  path: string;
  auth: WebAuthModel | null;
  auth_span?: Span | null;
}

export interface VmModel {
  span: Span | null;
  name: string;
  template: string;
  arch: string | null;
  profile: string | null;
  cpus: number | null;
  /** Bytes. */
  memory: number | null;
  /** Bytes (scratch VMs only). */
  disk: number | null;
  cdrom: string | null;
  floppy: string | null;
  depends_on: string[];
  nested: boolean;
  gui: boolean | null;
  display: string | null;
  firmware: string | null; // ovmf | seabios
  tpm: boolean | null;
  secure_boot: boolean | null;
  qemu_args: string[];
  gpu: GpuModel | null;
  nics: NicModel[];
  extra_disks: DiskModel[];
  shares: ShareModel[];
  media: MediaModel[];
  web: WebPageModel[];
}

// --- container children -------------------------------------------------------

export interface EnvVarModel {
  span: Span | null;
  name: string;
  value: string;
}

export interface VolumeModel {
  span: Span | null;
  /** Host bind path; exactly one of `host`/`name` is set. */
  host: string | null;
  /** Named volume (kept under the lab dir, shared by name). */
  name: string | null;
  target: string;
  read_only: boolean;
}

export interface PortMapModel {
  span: Span | null;
  host: number;
  container: number;
  proto: string; // tcp | udp | both
}

export interface HealthcheckModel {
  span: Span | null;
  command: string[];
  /** Seconds. */
  interval: number;
  /** Seconds. */
  timeout: number;
  retries: number;
  /** Seconds. */
  start_period: number;
}

/** Schema defaults for `healthcheck {}` (mirrors src/config extraction). */
export const HEALTHCHECK_DEFAULTS = {
  interval: 10,
  timeout: 5,
  retries: 3,
  start_period: 10,
} as const;

export interface ContainerModel {
  span: Span | null;
  name: string;
  /** OCI image reference exactly as written. */
  image: string;
  image_span: Span | null;
  mode: "workload" | "idle";
  entrypoint: string[] | null;
  command: string[] | null;
  workdir: string | null;
  user: string | null;
  cpus: number | null;
  /** Bytes. */
  memory: number | null;
  depends_on: string[];
  restart: string; // no | on-failure | always
  nics: NicModel[];
  env: EnvVarModel[];
  volumes: VolumeModel[];
  ports: PortMapModel[];
  healthcheck: HealthcheckModel | null;
  web: WebPageModel[];
}

export interface DnsModel {
  declared: boolean;
  span: Span | null;
  server: string | null;
  enabled: boolean;
}

export interface ConnectModel {
  span: Span | null;
  host: string;
}

export interface RouteModel {
  span: Span | null;
  dest: string;
  via: string;
}

export interface RecordModel {
  span: Span | null;
  name: string;
  ip: string;
}

export interface ForwardModel {
  span: Span | null;
  host_port: number;
  vm: string;
  guest_port: number;
  proto: string; // tcp | udp | both
}

export interface BlockRuleModel {
  span: Span | null;
  cidr: string;
  proto: string | null; // tcp | udp | icmp
  port: number | null;
}

export interface RedirectModel {
  span: Span | null;
  from: string;
  to: string;
  proto: string | null;
}

export interface SinkholeModel {
  span: Span | null;
  pattern: string;
  mode: string; // nxdomain | zero
}

export interface SegmentModel {
  span: Span | null;
  name: string;
  subnet: string | null;
  global: boolean;
  dhcp: boolean;
  nat: boolean;
  mtu: number | null;
  routes_to: string[];
  dns: DnsModel;
  connect: ConnectModel | null;
  routes: RouteModel[];
  records: RecordModel[];
  forwards: ForwardModel[];
  block_rules: BlockRuleModel[];
  redirect_rules: RedirectModel[];
  sinkholes: SinkholeModel[];
}

export interface ProvisionModel {
  span: Span | null;
  script: string;
  vms: string[];
}

export interface PlaybookModel {
  span: Span | null;
  /** Playbook folder, relative to the lab root; the inline block label. */
  path: string;
  play: string;
  vms: string[];
}

export interface HandlerModel {
  span: Span | null;
  event: string;
  run: string;
  targets: string[];
}

export interface LabModel {
  span: Span | null;
  name: string;
  gui: boolean | null;
  segments: SegmentModel[];
  vms: VmModel[];
  containers: ContainerModel[];
  provisions: ProvisionModel[];
  playbooks: PlaybookModel[];
  handlers: HandlerModel[];
  records: RecordModel[];
  sinkholes: SinkholeModel[];
}

/** `template {}` blocks in the same file — shown, never edited here. */
export interface TemplateSummary {
  span: Span;
  name: string;
  arch: string;
  version: string;
}

export interface LabDocument {
  path: string;
  rev: string;
  lab: LabModel;
  templates: TemplateSummary[];
}

// --- surgical ops (mirror src/config/edit_ops.rs) ---------------------------

/** A WCL unit literal, e.g. `{num: 8, unit: "GiB"}` → `8GiB`. */
export interface UnitValue {
  num: number;
  unit: string;
}

/** A WCL symbol literal, e.g. `{symbol: "basic"}` → `:basic`. */
export interface SymbolValue {
  symbol: string;
}

export type OpValue = string | number | boolean | string[] | UnitValue | SymbolValue;

export interface BlockSpec {
  kind: string;
  labels?: string[];
  fields?: { name: string; value: OpValue }[];
  children?: BlockSpec[];
}

export type ModelOp =
  | { op: "set_field"; block: Span; name: string; value: OpValue }
  | { op: "remove_field"; block: Span; name: string }
  | { op: "set_label"; block: Span; slot: number; value: string }
  | { op: "add_block"; parent?: Span; after?: Span; block: BlockSpec }
  | { op: "remove_block"; block: Span }
  | { op: "move_block"; block: Span; down: boolean };

// --- factories --------------------------------------------------------------

export function emptyNic(segment: string | null): NicModel {
  return {
    span: null,
    segment,
    nat: segment === null,
    ip: null,
    gateway: false,
    mac: null,
    isolated: false,
  };
}

export function emptyVm(name: string, template: string): VmModel {
  return {
    span: null,
    name,
    template,
    arch: null,
    profile: null,
    cpus: null,
    memory: null,
    disk: null,
    cdrom: null,
    floppy: null,
    depends_on: [],
    nested: false,
    gui: null,
    display: null,
    firmware: null,
    tpm: null,
    secure_boot: null,
    qemu_args: [],
    gpu: null,
    nics: [],
    extra_disks: [],
    shares: [],
    media: [],
    web: [],
  };
}

export function emptyContainer(name: string, image: string): ContainerModel {
  return {
    span: null,
    name,
    image,
    image_span: null,
    mode: "workload",
    entrypoint: null,
    command: null,
    workdir: null,
    user: null,
    cpus: null,
    memory: null,
    depends_on: [],
    restart: "no",
    nics: [],
    env: [],
    volumes: [],
    ports: [],
    healthcheck: null,
    web: [],
  };
}

/** A new web-page block with sensible port default. */
export function emptyWebPage(name: string): WebPageModel {
  return { span: null, name, port: 80, path: "/", auth: null };
}

/** A new `auth {}` block for a page (basic by default). */
export function emptyWebAuth(): WebAuthModel {
  return {
    span: null,
    method: "basic",
    username: null,
    password: null,
    domain: null,
    token: null,
    header: null,
    value: null,
    login_path: null,
    login_method: null,
    login_body: null,
    login_content_type: null,
    fail_redirect: null,
  };
}

export function emptySegment(name: string): SegmentModel {
  return {
    span: null,
    name,
    subnet: null,
    global: false,
    dhcp: true,
    nat: false,
    mtu: null,
    routes_to: [],
    dns: { declared: false, span: null, server: null, enabled: true },
    connect: null,
    routes: [],
    records: [],
    forwards: [],
    block_rules: [],
    redirect_rules: [],
    sinkholes: [],
  };
}

/** A unique DNS-label name: `base`, `base2`, `base3`, … */
export function uniqueName(base: string, taken: Iterable<string>): string {
  const set = new Set(taken);
  if (!set.has(base)) return base;
  for (let i = 2; ; i++) {
    const candidate = `${base}${i}`;
    if (!set.has(candidate)) return candidate;
  }
}

export function deepClone<T>(v: T): T {
  return JSON.parse(JSON.stringify(v)) as T;
}
