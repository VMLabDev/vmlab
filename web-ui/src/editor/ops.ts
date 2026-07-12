// The diff engine: compare the baseline model (as fetched, spans intact)
// against the edited draft and emit the surgical op batch the backend
// applies to vmlab.wcl. Identity is the block span — a draft block that
// kept its span is "the same block, maybe edited"; `span: null` is a new
// block; a baseline span missing from the draft is a removal.
//
// Op order: field edits first, then adds, then removes — all addresses are
// baseline spans, which stay valid for the whole batch because the server
// mutates the AST without recomputing spans.

import type {
  BlockRuleModel,
  BlockSpec,
  ConnectModel,
  ContainerModel,
  DiskModel,
  DnsModel,
  EnvVarModel,
  ForwardModel,
  GpuModel,
  HealthcheckModel,
  LabModel,
  MediaModel,
  ModelOp,
  NicModel,
  OpValue,
  PortMapModel,
  ProvisionModel,
  HandlerModel,
  RecordModel,
  RedirectModel,
  RouteModel,
  SegmentModel,
  ShareModel,
  SinkholeModel,
  Span,
  VmModel,
  VolumeModel,
} from "./model";
import { HEALTHCHECK_DEFAULTS } from "./model";
import { toUnitValue } from "./bytesize";

// A field value with enough typing to encode + compare. `flag` is a bool
// the schema defaults (never removed, skipped in add-specs at its default);
// `strdef`/`intdef`/`secs` are the string / int / duration-in-seconds
// equivalents (e.g. forward proto defaulting to tcp, healthcheck timings).
type FV =
  | { k: "str"; v: string | null }
  | { k: "int"; v: number | null }
  | { k: "bool"; v: boolean | null }
  | { k: "flag"; v: boolean; def: boolean }
  | { k: "strdef"; v: string; def: string }
  | { k: "intdef"; v: number; def: number }
  | { k: "secs"; v: number; def: number }
  | { k: "bytes"; v: number | null }
  | { k: "list"; v: string[] };

const str = (v: string | null): FV => ({ k: "str", v: v?.trim() ? v : null });
const int = (v: number | null): FV => ({ k: "int", v });
const bool = (v: boolean | null): FV => ({ k: "bool", v });
const flag = (v: boolean, def: boolean): FV => ({ k: "flag", v, def });
const strdef = (v: string, def: string): FV => ({ k: "strdef", v, def });
const intdef = (v: number, def: number): FV => ({ k: "intdef", v, def });
const secs = (v: number, def: number): FV => ({ k: "secs", v, def });
const bytes = (v: number | null): FV => ({ k: "bytes", v });
const list = (v: string[]): FV => ({ k: "list", v });

/** Encode for the wire; null = "unset this field". */
function encode(f: FV): OpValue | null {
  switch (f.k) {
    case "str":
      return f.v;
    case "int":
      return f.v;
    case "bool":
      return f.v;
    case "flag":
      return f.v;
    case "strdef":
      return f.v;
    case "intdef":
      return f.v;
    case "secs":
      // std.Duration: `{num, unit}` → a unit literal (`interval = 10s`).
      return { num: f.v, unit: "s" };
    case "bytes":
      return f.v === null ? null : toUnitValue(f.v);
    case "list":
      return f.v.length ? f.v : null;
  }
}

function eq(a: FV, b: FV): boolean {
  return JSON.stringify(encode(a)) === JSON.stringify(encode(b));
}

type Pair = [string, FV, FV];

class Ops {
  sets: ModelOp[] = [];
  adds: ModelOp[] = [];
  removes: ModelOp[] = [];
  all(): ModelOp[] {
    return [...this.sets, ...this.adds, ...this.removes];
  }
}

function diffFields(ops: Ops, span: Span, pairs: Pair[]) {
  for (const [name, base, draft] of pairs) {
    if (eq(base, draft)) continue;
    const value = encode(draft);
    if (value === null) ops.removes.push({ op: "remove_field", block: span, name });
    else ops.sets.push({ op: "set_field", block: span, name, value });
  }
}

function diffLabel(ops: Ops, span: Span, base: string, draft: string) {
  if (base !== draft) ops.sets.push({ op: "set_label", block: span, slot: 0, value: draft });
}

/** Spec fields: encode + skip unset values and defaults. */
function specFields(pairs: [string, FV][]): { name: string; value: OpValue }[] {
  const out: { name: string; value: OpValue }[] = [];
  for (const [name, f] of pairs) {
    if (f.k === "flag" && f.v === f.def) continue;
    if (f.k === "strdef" && f.v === f.def) continue;
    if ((f.k === "intdef" || f.k === "secs") && f.v === f.def) continue;
    const value = encode(f);
    if (value !== null) out.push({ name, value });
  }
  return out;
}

const key = (s: Span) => `${s[0]}:${s[1]}`;

interface HasSpan {
  span: Span | null;
}

function diffChildren<T extends HasSpan>(
  ops: Ops,
  parent: Span,
  base: T[],
  draft: T[],
  diffOne: (ops: Ops, base: T, draft: T) => void,
  spec: (d: T) => BlockSpec,
) {
  const draftSpans = new Set(draft.filter((d) => d.span).map((d) => key(d.span!)));
  for (const b of base) {
    if (b.span && !draftSpans.has(key(b.span))) {
      ops.removes.push({ op: "remove_block", block: b.span });
    }
  }
  const baseBySpan = new Map(base.filter((b) => b.span).map((b) => [key(b.span!), b]));
  for (const d of draft) {
    if (d.span) {
      const b = baseBySpan.get(key(d.span));
      if (b) diffOne(ops, b, d);
    } else {
      ops.adds.push({ op: "add_block", parent, block: spec(d) });
    }
  }
}

/** Optional single child block (gpu / dns / connect). */
function diffChild<T extends HasSpan>(
  ops: Ops,
  parent: Span,
  base: T | null,
  draft: T | null,
  diffOne: (ops: Ops, base: T, draft: T) => void,
  spec: (d: T) => BlockSpec,
) {
  if (base?.span && !draft) {
    ops.removes.push({ op: "remove_block", block: base.span });
  } else if (!base && draft) {
    ops.adds.push({ op: "add_block", parent, block: spec(draft) });
  } else if (base && draft) {
    if (draft.span && base.span && key(draft.span) === key(base.span)) {
      diffOne(ops, base, draft);
    } else if (base.span) {
      // Replaced wholesale (removed then re-added in the editor).
      ops.removes.push({ op: "remove_block", block: base.span });
      ops.adds.push({ op: "add_block", parent, block: spec(draft) });
    }
  }
}

// --- per-block field tables --------------------------------------------------

const vmPairs = (v: VmModel): [string, FV][] => [
  ["template", str(v.template)],
  ["arch", str(v.arch)],
  ["profile", str(v.profile)],
  ["cpus", int(v.cpus)],
  ["memory", bytes(v.memory)],
  ["disk", bytes(v.disk)],
  ["cdrom", str(v.cdrom)],
  ["floppy", str(v.floppy)],
  ["depends_on", list(v.depends_on)],
  ["nested", flag(v.nested, false)],
  ["gui", bool(v.gui)],
  ["display", str(v.display)],
  ["firmware", str(v.firmware)],
  ["tpm", bool(v.tpm)],
  ["secure_boot", bool(v.secure_boot)],
  ["qemu_args", list(v.qemu_args)],
];

const nicPairs = (n: NicModel): [string, FV][] => [
  ["segment", str(n.segment)],
  ["nat", flag(n.nat, false)],
  ["ip", str(n.ip)],
  ["gateway", flag(n.gateway, false)],
  ["mac", str(n.mac)],
  ["isolated", flag(n.isolated, false)],
];

const diskPairs = (d: DiskModel): [string, FV][] => [
  ["size", bytes(d.size)],
  ["from", str(d.from)],
];

const sharePairs = (s: ShareModel): [string, FV][] => [
  ["host", str(s.host)],
  ["guest", str(s.guest)],
  ["readonly", flag(s.readonly, false)],
  ["smb1", flag(s.smb1, false)],
  ["name", str(s.name)],
];

const mediaPairs = (m: MediaModel): [string, FV][] => [
  ["kind", str(m.kind)],
  ["from", str(m.from)],
  ["label", str(m.label)],
];

const gpuPairs = (g: GpuModel): [string, FV][] => [
  ["mode", str(g.mode)],
  ["address", str(g.address)],
];

// --- container field tables ----------------------------------------------------

const containerPairs = (c: ContainerModel): [string, FV][] => [
  ["image", str(c.image)],
  ["entrypoint", list(c.entrypoint ?? [])],
  ["command", list(c.command ?? [])],
  ["workdir", str(c.workdir)],
  ["user", str(c.user)],
  ["cpus", int(c.cpus)],
  ["memory", bytes(c.memory)],
  ["depends_on", list(c.depends_on)],
  ["restart", strdef(c.restart || "no", "no")],
];

const envPairs = (e: EnvVarModel): [string, FV][] => [
  ["name", str(e.name)],
  ["value", str(e.value)],
];

const volumePairs = (v: VolumeModel): [string, FV][] => [
  ["host", str(v.host)],
  ["name", str(v.name)],
  ["target", str(v.target)],
  ["read_only", flag(v.read_only, false)],
];

const portPairs = (p: PortMapModel): [string, FV][] => [
  ["host", int(p.host)],
  ["container", int(p.container)],
  ["proto", strdef(p.proto || "tcp", "tcp")],
];

const healthcheckPairs = (h: HealthcheckModel): [string, FV][] => [
  ["command", list(h.command)],
  ["interval", secs(h.interval, HEALTHCHECK_DEFAULTS.interval)],
  ["timeout", secs(h.timeout, HEALTHCHECK_DEFAULTS.timeout)],
  ["retries", intdef(h.retries, HEALTHCHECK_DEFAULTS.retries)],
  ["start_period", secs(h.start_period, HEALTHCHECK_DEFAULTS.start_period)],
];

const segmentPairs = (s: SegmentModel): [string, FV][] => [
  ["subnet", str(s.subnet)],
  ["global", flag(s.global, false)],
  ["dhcp", flag(s.dhcp, true)],
  ["nat", flag(s.nat, false)],
  ["mtu", int(s.mtu)],
  ["routes_to", list(s.routes_to)],
];

const dnsPairs = (d: DnsModel): [string, FV][] => [
  ["server", str(d.server)],
  ["enabled", flag(d.enabled, true)],
];

const connectPairs = (c: ConnectModel): [string, FV][] => [["host", str(c.host)]];

const routePairs = (r: RouteModel): [string, FV][] => [
  ["dest", str(r.dest)],
  ["via", str(r.via)],
];

const recordPairs = (r: RecordModel): [string, FV][] => [
  ["name", str(r.name)],
  ["ip", str(r.ip)],
];

const forwardPairs = (f: ForwardModel): [string, FV][] => [
  ["host_port", int(f.host_port)],
  ["to", str(`${f.vm}:${f.guest_port}`)],
  ["proto", strdef(f.proto || "tcp", "tcp")],
];

const blockRulePairs = (b: BlockRuleModel): [string, FV][] => [
  ["cidr", str(b.cidr)],
  ["proto", str(b.proto)],
  ["port", int(b.port)],
];

const redirectPairs = (r: RedirectModel): [string, FV][] => [
  ["from", str(r.from)],
  ["to", str(r.to)],
  ["proto", str(r.proto)],
];

const sinkholePairs = (s: SinkholeModel): [string, FV][] => [
  ["pattern", str(s.pattern)],
  ["mode", strdef(s.mode || "nxdomain", "nxdomain")],
];

const provisionPairs = (p: ProvisionModel): [string, FV][] => [["vms", list(p.vms)]];

const handlerPairs = (h: HandlerModel): [string, FV][] => [
  ["run", str(h.run)],
  ["targets", list(h.targets)],
];

// --- add-spec builders --------------------------------------------------------

const nicSpec = (n: NicModel): BlockSpec => ({ kind: "nic", fields: specFields(nicPairs(n)) });
const diskSpec = (d: DiskModel): BlockSpec => ({
  kind: "disk",
  labels: [d.name],
  fields: specFields(diskPairs(d)),
});
const shareSpec = (s: ShareModel): BlockSpec => ({
  kind: "share",
  fields: specFields(sharePairs(s)),
});
const mediaSpec = (m: MediaModel): BlockSpec => ({
  kind: "media",
  fields: specFields(mediaPairs(m)),
});
const gpuSpec = (g: GpuModel): BlockSpec => ({ kind: "gpu", fields: specFields(gpuPairs(g)) });
const dnsSpec = (d: DnsModel): BlockSpec => ({ kind: "dns", fields: specFields(dnsPairs(d)) });
const connectSpec = (c: ConnectModel): BlockSpec => ({
  kind: "connect",
  fields: specFields(connectPairs(c)),
});
const routeSpec = (r: RouteModel): BlockSpec => ({
  kind: "route",
  fields: specFields(routePairs(r)),
});
const recordSpec = (r: RecordModel): BlockSpec => ({
  kind: "record",
  fields: specFields(recordPairs(r)),
});
const forwardSpec = (f: ForwardModel): BlockSpec => ({
  kind: "forward",
  fields: specFields(forwardPairs(f)),
});
const blockRuleSpec = (b: BlockRuleModel): BlockSpec => ({
  kind: "block",
  fields: specFields(blockRulePairs(b)),
});
const redirectSpec = (r: RedirectModel): BlockSpec => ({
  kind: "redirect",
  fields: specFields(redirectPairs(r)),
});
const sinkholeSpec = (s: SinkholeModel): BlockSpec => ({
  kind: "sinkhole",
  fields: specFields(sinkholePairs(s)),
});
const provisionSpec = (p: ProvisionModel): BlockSpec => ({
  kind: "provision",
  labels: [p.script],
  fields: specFields(provisionPairs(p)),
});
const handlerSpec = (h: HandlerModel): BlockSpec => ({
  kind: "on",
  labels: [h.event],
  fields: specFields(handlerPairs(h)),
});

const envSpec = (e: EnvVarModel): BlockSpec => ({ kind: "env", fields: specFields(envPairs(e)) });
const volumeSpec = (v: VolumeModel): BlockSpec => ({
  kind: "volume",
  fields: specFields(volumePairs(v)),
});
const portSpec = (p: PortMapModel): BlockSpec => ({
  kind: "port",
  fields: specFields(portPairs(p)),
});
const healthcheckSpec = (h: HealthcheckModel): BlockSpec => ({
  kind: "healthcheck",
  fields: specFields(healthcheckPairs(h)),
});

function containerSpec(c: ContainerModel): BlockSpec {
  const children: BlockSpec[] = [];
  children.push(...c.nics.map(nicSpec));
  children.push(...c.env.map(envSpec));
  children.push(...c.volumes.map(volumeSpec));
  children.push(...c.ports.map(portSpec));
  if (c.healthcheck) children.push(healthcheckSpec(c.healthcheck));
  return {
    kind: "container",
    labels: [c.name],
    fields: specFields(containerPairs(c)),
    children,
  };
}

function vmSpec(v: VmModel): BlockSpec {
  const children: BlockSpec[] = [];
  if (v.gpu) children.push(gpuSpec(v.gpu));
  children.push(...v.nics.map(nicSpec));
  children.push(...v.extra_disks.map(diskSpec));
  children.push(...v.shares.map(shareSpec));
  children.push(...v.media.map(mediaSpec));
  return { kind: "vm", labels: [v.name], fields: specFields(vmPairs(v)), children };
}

function segmentSpec(s: SegmentModel): BlockSpec {
  const children: BlockSpec[] = [];
  if (s.dns.declared) children.push(dnsSpec(s.dns));
  if (s.connect) children.push(connectSpec(s.connect));
  children.push(...s.routes.map(routeSpec));
  children.push(...s.records.map(recordSpec));
  children.push(...s.forwards.map(forwardSpec));
  children.push(...s.block_rules.map(blockRuleSpec));
  children.push(...s.redirect_rules.map(redirectSpec));
  children.push(...s.sinkholes.map(sinkholeSpec));
  return { kind: "segment", labels: [s.name], fields: specFields(segmentPairs(s)), children };
}

// --- per-block diffs ----------------------------------------------------------

function fieldDiffer<T extends HasSpan>(pairs: (x: T) => [string, FV][]) {
  return (ops: Ops, base: T, draft: T) => {
    const span = base.span!;
    const bp = pairs(base);
    const dp = pairs(draft);
    diffFields(
      ops,
      span,
      bp.map(([name, bv], i) => [name, bv, dp[i][1]] as Pair),
    );
  };
}

const diffNic = fieldDiffer(nicPairs);
const diffShare = fieldDiffer(sharePairs);
const diffMedia = fieldDiffer(mediaPairs);
const diffGpu = fieldDiffer(gpuPairs);
const diffDnsFields = fieldDiffer(dnsPairs);
const diffConnect = fieldDiffer(connectPairs);
const diffRoute = fieldDiffer(routePairs);
const diffRecord = fieldDiffer(recordPairs);
const diffForward = fieldDiffer(forwardPairs);
const diffBlockRule = fieldDiffer(blockRulePairs);
const diffRedirect = fieldDiffer(redirectPairs);
const diffSinkhole = fieldDiffer(sinkholePairs);

function diffDisk(ops: Ops, base: DiskModel, draft: DiskModel) {
  diffLabel(ops, base.span!, base.name, draft.name);
  fieldDiffer(diskPairs)(ops, base, draft);
}

function diffProvision(ops: Ops, base: ProvisionModel, draft: ProvisionModel) {
  diffLabel(ops, base.span!, base.script, draft.script);
  fieldDiffer(provisionPairs)(ops, base, draft);
}

function diffHandler(ops: Ops, base: HandlerModel, draft: HandlerModel) {
  diffLabel(ops, base.span!, base.event, draft.event);
  fieldDiffer(handlerPairs)(ops, base, draft);
}

const diffEnv = fieldDiffer(envPairs);
const diffVolume = fieldDiffer(volumePairs);
const diffPort = fieldDiffer(portPairs);
const diffHealthcheck = fieldDiffer(healthcheckPairs);

function diffContainer(ops: Ops, base: ContainerModel, draft: ContainerModel) {
  const span = base.span!;
  diffLabel(ops, span, base.name, draft.name);
  fieldDiffer(containerPairs)(ops, base, draft);
  diffChildren(ops, span, base.nics, draft.nics, diffNic, nicSpec);
  diffChildren(ops, span, base.env, draft.env, diffEnv, envSpec);
  diffChildren(ops, span, base.volumes, draft.volumes, diffVolume, volumeSpec);
  diffChildren(ops, span, base.ports, draft.ports, diffPort, portSpec);
  diffChild(ops, span, base.healthcheck, draft.healthcheck, diffHealthcheck, healthcheckSpec);
}

function diffVm(ops: Ops, base: VmModel, draft: VmModel) {
  const span = base.span!;
  diffLabel(ops, span, base.name, draft.name);
  fieldDiffer(vmPairs)(ops, base, draft);
  diffChild(ops, span, base.gpu, draft.gpu, diffGpu, gpuSpec);
  diffChildren(ops, span, base.nics, draft.nics, diffNic, nicSpec);
  diffChildren(ops, span, base.extra_disks, draft.extra_disks, diffDisk, diskSpec);
  diffChildren(ops, span, base.shares, draft.shares, diffShare, shareSpec);
  diffChildren(ops, span, base.media, draft.media, diffMedia, mediaSpec);
}

function diffSegment(ops: Ops, base: SegmentModel, draft: SegmentModel) {
  const span = base.span!;
  diffLabel(ops, span, base.name, draft.name);
  fieldDiffer(segmentPairs)(ops, base, draft);
  diffChild(
    ops,
    span,
    base.dns.declared ? base.dns : null,
    draft.dns.declared ? draft.dns : null,
    diffDnsFields,
    dnsSpec,
  );
  diffChild(ops, span, base.connect, draft.connect, diffConnect, connectSpec);
  diffChildren(ops, span, base.routes, draft.routes, diffRoute, routeSpec);
  diffChildren(ops, span, base.records, draft.records, diffRecord, recordSpec);
  diffChildren(ops, span, base.forwards, draft.forwards, diffForward, forwardSpec);
  diffChildren(ops, span, base.block_rules, draft.block_rules, diffBlockRule, blockRuleSpec);
  diffChildren(ops, span, base.redirect_rules, draft.redirect_rules, diffRedirect, redirectSpec);
  diffChildren(ops, span, base.sinkholes, draft.sinkholes, diffSinkhole, sinkholeSpec);
}

/** The op batch that turns `base` (on disk) into `draft` (in the editor). */
export function buildOps(base: LabModel, draft: LabModel): ModelOp[] {
  const ops = new Ops();
  const labSpan = base.span!;
  diffFields(ops, labSpan, [["gui", bool(base.gui), bool(draft.gui)]]);
  diffChildren(ops, labSpan, base.segments, draft.segments, diffSegment, segmentSpec);
  diffChildren(ops, labSpan, base.vms, draft.vms, diffVm, vmSpec);
  diffChildren(ops, labSpan, base.containers, draft.containers, diffContainer, containerSpec);
  diffChildren(ops, labSpan, base.provisions, draft.provisions, diffProvision, provisionSpec);
  diffChildren(ops, labSpan, base.handlers, draft.handlers, diffHandler, handlerSpec);
  diffChildren(ops, labSpan, base.records, draft.records, diffRecord, recordSpec);
  diffChildren(ops, labSpan, base.sinkholes, draft.sinkholes, diffSinkhole, sinkholeSpec);
  return ops.all();
}
