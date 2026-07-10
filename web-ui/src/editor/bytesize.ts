// std.ByteSize helpers: the model stores byte counts; users type/see
// strings like "8GiB"; the op layer wants {num, unit} so the WCL stays a
// readable unit literal.

import type { UnitValue } from "./model";

const UNITS: [string, number][] = [
  ["TiB", 1024 ** 4],
  ["GiB", 1024 ** 3],
  ["MiB", 1024 ** 2],
  ["KiB", 1024],
  ["TB", 1000 ** 4],
  ["GB", 1000 ** 3],
  ["MB", 1000 ** 2],
  ["KB", 1000],
  ["B", 1],
];

/** Parse "8GiB" / "512 MiB" / "1.5GiB" / "1073741824" → bytes, or null on
 *  garbage. Fractional units round to whole bytes. */
export function parseByteSize(text: string): number | null {
  const m = /^\s*(\d+(?:\.\d+)?)\s*([KMGT]i?B|B)?\s*$/i.exec(text);
  if (!m) return null;
  const num = Number(m[1]);
  if (!Number.isFinite(num)) return null;
  if (!m[2]) return Math.round(num);
  const unit = UNITS.find(([u]) => u.toLowerCase() === m[2].toLowerCase());
  return unit ? Math.round(num * unit[1]) : null;
}

/** Format bytes with the largest binary unit that divides evenly. */
export function formatByteSize(bytes: number): string {
  const { num, unit } = toUnitValue(bytes);
  return unit === "B" ? `${num}` : `${num}${unit}`;
}

/** RAM display: MiB below a GiB, (fractional) GiB from there up. */
export function formatMemory(bytes: number): string {
  const GIB = 1024 ** 3;
  const MIB = 1024 ** 2;
  return bytes >= GIB
    ? `${parseFloat((bytes / GIB).toFixed(2))}GiB`
    : `${Math.round(bytes / MIB)}MiB`;
}

/** Bytes → {num, unit} for the op layer (binary units preferred). */
export function toUnitValue(bytes: number): UnitValue {
  for (const [unit, factor] of UNITS) {
    if (bytes >= factor && bytes % factor === 0) return { num: bytes / factor, unit };
  }
  return { num: bytes, unit: "B" };
}
