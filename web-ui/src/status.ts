// Lab status, as the daemon projects it (ADR-0004).
//
// The types in ./gen/status.ts are generated from the Rust projection
// (src/status.rs) by `just status-types-build`; nothing here re-declares a field
// the daemon owns, and a producer-side rename turns into a `tsc` error rather than
// a table that renders as nothing.
//
// This module adds only what generation cannot: the runtime check that the
// payload is the shape it claims to be, and the by-kind groupings the views
// want — written once, so the console and the CLI cannot disagree about which
// machines are VMs.

import type { LabStatus, MachineStatus } from "./gen/status";

export type {
  ContainerStatus,
  LabStatus,
  LabelState,
  MachineDetail,
  MachineKind,
  MachineLabel,
  MachineStatus,
  NicStatus,
  PowerState,
  PullKind,
  PullStatus,
  SegmentFrames,
  SegmentStatus,
  Severity,
  VmStatus,
  WebPageStatus,
} from "./gen/status";

/** A machine narrowed to its VM variant: `template`, `cpus` and the rest are
 *  reachable only from here. */
export type VmMachine = MachineStatus & { kind: "vm" };
/** A machine narrowed to its container variant. */
export type ContainerMachine = MachineStatus & { kind: "container" };

/** Every VM in a lab. The kind filter lives here, not at each call site. */
export const vms = (status?: LabStatus | null): VmMachine[] =>
  (status?.machines ?? []).filter((m): m is VmMachine => m.kind === "vm");

/** Every container in a lab. */
export const containers = (status?: LabStatus | null): ContainerMachine[] =>
  (status?.machines ?? []).filter((m): m is ContainerMachine => m.kind === "container");

/** One machine by name, whatever its kind. */
export const machine = (
  status: LabStatus | null | undefined,
  name: string | null,
): MachineStatus | undefined => status?.machines.find((m) => m.name === name);

/** Check a `status` payload against the contract before the views trust it.
 *
 *  A daemon speaking a shape this console does not understand must say so.
 *  Rendering it anyway is how a rename once produced an empty machine list that
 *  was indistinguishable from an empty lab — the failure ADR-0004 exists to
 *  make impossible. Only the discriminants are checked: everything below them
 *  is `tsc`'s job, and re-validating each field here would be the hand-written
 *  mirror this design just retired. */
export function parseLabStatus(raw: unknown): LabStatus {
  const status = raw as LabStatus;
  if (
    !status ||
    typeof status !== "object" ||
    typeof status.lab !== "string" ||
    !Array.isArray(status.machines) ||
    !Array.isArray(status.segments)
  ) {
    throw new Error("the lab daemon reported a status this console cannot read");
  }
  for (const m of status.machines) {
    const kind: unknown = m?.kind;
    if (kind !== "vm" && kind !== "container") {
      throw new Error(`the lab daemon reported a machine of unknown kind: ${JSON.stringify(kind)}`);
    }
    if (typeof m.label?.text !== "string") {
      throw new Error(`the lab daemon reported no status label for "${m.name}"`);
    }
  }
  return status;
}
