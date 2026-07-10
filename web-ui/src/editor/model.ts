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

export interface HandlerModel {
  span: Span | null;
  event: string;
  run: string;
}

export interface LabModel {
  span: Span | null;
  name: string;
  gui: boolean | null;
  segments: SegmentModel[];
  vms: VmModel[];
  provisions: ProvisionModel[];
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

export type OpValue = string | number | boolean | string[] | UnitValue;

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
  return { span: null, segment, nat: segment === null, ip: null, mac: null, isolated: false };
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
