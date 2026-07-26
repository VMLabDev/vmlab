// Pure helpers over a config-weave playbook document (api.PlaybookDoc): node
// addressing, traversal, the mutations the designer performs, and the checks
// config-weave itself would fail on. Kept free of Solid so the tree, the
// inspector and the tests all reason about the same shapes.

import type {
  CatalogInfo,
  ContainerDoc,
  GatherDoc,
  Kv,
  PackageDoc,
  ParamDoc,
  PlayDoc,
  PlayItemDoc,
  PlaybookDoc,
  ResourceDoc,
  StepDoc,
  Val,
} from "../api";

// --- selection ---------------------------------------------------------------
//
// A step or step folder is addressed by its play plus the chain of item
// indices that reaches it, so `{play: 0, path: [2, 0]}` is "the first item of
// the play's third item". The empty path addresses the play's own item list.

export type Sel =
  | { kind: "playbook" }
  | { kind: "gather"; index: number }
  | { kind: "var"; index: number }
  | { kind: "play"; play: number }
  | { kind: "item"; play: number; path: number[] };

export const selId = (sel: Sel): string => {
  switch (sel.kind) {
    case "playbook":
      return "playbook";
    case "gather":
      return `gather:${sel.index}`;
    case "var":
      return `var:${sel.index}`;
    case "play":
      return `play:${sel.play}`;
    case "item":
      return `item:${sel.play}:${sel.path.join(".")}`;
  }
};

export const sameSel = (a: Sel | null, b: Sel | null): boolean =>
  a !== null && b !== null && selId(a) === selId(b);

// --- item traversal ----------------------------------------------------------

export const isStep = (item: PlayItemDoc): item is { step: StepDoc } => "step" in item;
export const isContainer = (item: PlayItemDoc): item is { container: ContainerDoc } =>
  "container" in item;

/** The item list a path points *into* (its parent's children), or null when
 *  the path doesn't resolve. An empty path is the play's own list. */
export function itemsAt(doc: PlaybookDoc, play: number, path: number[]): PlayItemDoc[] | null {
  let items = doc.plays[play]?.items;
  if (!items) return null;
  for (const index of path) {
    const item = items[index];
    if (!item || !isContainer(item)) return null;
    items = item.container.items;
  }
  return items;
}

/** The item a full path addresses. */
export function itemAt(doc: PlaybookDoc, play: number, path: number[]): PlayItemDoc | null {
  if (!path.length) return null;
  const parent = itemsAt(doc, play, path.slice(0, -1));
  return parent?.[path[path.length - 1]] ?? null;
}

/** Every step in a play, flattened through folders in declaration order —
 *  config-weave's own scope for step names and `requires`. */
export function playSteps(play: PlayDoc): StepDoc[] {
  const out: StepDoc[] = [];
  const walk = (items: PlayItemDoc[]) => {
    for (const item of items) {
      if (isStep(item)) out.push(item.step);
      else walk(item.container.items);
    }
  };
  walk(play.items);
  return out;
}

/** True when `ancestor` is `path` itself or one of its parents — the guard
 *  that stops a folder being dragged into its own subtree. */
export function isAncestorPath(ancestor: number[], path: number[]): boolean {
  return ancestor.length <= path.length && ancestor.every((n, i) => path[i] === n);
}

// --- mutations ---------------------------------------------------------------
//
// All of these mutate in place; callers wrap them in Solid's `produce`.

export function removeItem(doc: PlaybookDoc, play: number, path: number[]): PlayItemDoc | null {
  const parent = itemsAt(doc, play, path.slice(0, -1));
  if (!parent) return null;
  return parent.splice(path[path.length - 1], 1)[0] ?? null;
}

/** Append to a play's items or a folder's; returns the item's path. */
export function appendItem(
  doc: PlaybookDoc,
  play: number,
  parentPath: number[],
  item: PlayItemDoc,
): number[] | null {
  const parent = itemsAt(doc, play, parentPath);
  if (!parent) return null;
  parent.push(item);
  return [...parentPath, parent.length - 1];
}

/** Where a dragged item should land: beside a target item, or inside a folder. */
export interface DropTarget {
  play: number;
  /** The folder receiving the item ([] = the play itself). */
  parentPath: number[];
  index: number;
}

/** A path rewritten for the hole a removal leaves behind: anything under the
 *  same parent list, positioned after the removed item, shifts up one. */
export function adjustForRemoval(
  path: number[],
  fromParent: number[],
  fromIndex: number,
): number[] {
  if (path.length <= fromParent.length) return path;
  if (!fromParent.every((n, i) => path[i] === n)) return path;
  const out = [...path];
  if (out[fromParent.length] > fromIndex) out[fromParent.length] -= 1;
  return out;
}

/** Move an item. The destination list is resolved *before* the removal —
 *  taking the item out first shifts every later sibling, which silently
 *  invalidated the drop path (and lost the item) when it was resolved after.
 *  Returns the item's new path, for keeping it selected. */
export function moveItem(
  doc: PlaybookDoc,
  from: { play: number; path: number[] },
  to: DropTarget,
): number[] | null {
  const target = itemsAt(doc, to.play, to.parentPath);
  const source = itemsAt(doc, from.play, from.path.slice(0, -1));
  if (!target || !source) return null;
  const fromParent = from.path.slice(0, -1);
  const fromIndex = from.path[from.path.length - 1];
  const item = source[fromIndex];
  if (!item) return null;

  source.splice(fromIndex, 1);
  // Only a move within one list shifts the insertion point itself; a move
  // between lists doesn't (and neither does a move across plays).
  const index = target === source && to.index > fromIndex ? to.index - 1 : to.index;
  const at = Math.max(0, Math.min(index, target.length));
  target.splice(at, 0, item);

  const parentPath =
    from.play === to.play ? adjustForRemoval(to.parentPath, fromParent, fromIndex) : to.parentPath;
  return [...parentPath, at];
}

// --- factories ---------------------------------------------------------------

const nextName = (base: string, taken: string[]): string => {
  if (!taken.includes(base)) return base;
  for (let n = 2; n < 1000; n++) {
    const name = `${base}-${n}`;
    if (!taken.includes(name)) return name;
  }
  return `${base}-${taken.length}`;
};

export const newStep = (taken: string[]): { step: StepDoc } => ({
  step: {
    name: nextName("step", taken),
    description: "",
    resource: "",
    requires: [],
    properties: [],
  },
});

export const newContainer = (taken: string[]): { container: ContainerDoc } => ({
  container: { name: nextName("folder", taken), description: "", items: [] },
});

export const newPlay = (taken: string[]): PlayDoc => ({
  name: nextName("play", taken),
  description: "",
  items: [],
});

export const newGather = (taken: string[]): GatherDoc => ({
  name: nextName("facts", taken),
  from: "",
  params: [],
});

export const newVar = (taken: string[]): Kv => ({
  key: nextName("value", taken),
  value: { lit: "" },
});

// --- values ------------------------------------------------------------------

export const isExpr = (v: Val | undefined): v is { expr: string } => !!v && "expr" in v;
export const litOf = (v: Val | undefined): string | number | boolean | null =>
  v && "lit" in v ? v.lit : null;

/** The text a value shows in an input: expression source, or the literal. */
export function valText(v: Val | undefined): string {
  if (!v) return "";
  if (isExpr(v)) return v.expr;
  const lit = v.lit;
  return lit === null || lit === undefined ? "" : String(lit);
}

/** Parse form input back into a literal of the parameter's declared type.
 *  Anything that doesn't parse stays a string — config-weave type-checks on
 *  load and says so far better than a silently dropped value would. */
export function litFor(type: string, text: string): Val {
  switch (type) {
    case "int": {
      const n = Number.parseInt(text, 10);
      return { lit: Number.isFinite(n) ? n : text };
    }
    case "float": {
      const n = Number.parseFloat(text);
      return { lit: Number.isFinite(n) ? n : text };
    }
    case "bool":
      return { lit: text === "true" };
    default:
      return { lit: text };
  }
}

export const kvGet = (kvs: Kv[], key: string): Kv | undefined => kvs.find((kv) => kv.key === key);

// --- catalogue ---------------------------------------------------------------

/** `"pkg.resource"` → its declaration, or undefined when the package isn't
 *  installed (or the name is still being typed). */
export function findResource(catalog: CatalogInfo | null, ref: string): ResourceDoc | undefined {
  const [pkg, name] = splitRef(ref);
  return catalog?.packages
    .find((p: PackageDoc) => p.name === pkg)
    ?.resources.find((r) => r.name === name);
}

export function findGatherer(catalog: CatalogInfo | null, ref: string) {
  const [pkg, name] = splitRef(ref);
  return catalog?.packages.find((p) => p.name === pkg)?.gatherers.find((g) => g.name === name);
}

export function splitRef(ref: string): [string, string] {
  const dot = ref.indexOf(".");
  return dot < 0 ? [ref, ""] : [ref.slice(0, dot), ref.slice(dot + 1)];
}

/** Every `pkg.resource` the installed packages offer, for the picker. */
export function resourceRefs(catalog: CatalogInfo | null): string[] {
  return (catalog?.packages ?? []).flatMap((p) => p.resources.map((r) => `${p.name}.${r.name}`));
}

export function gathererRefs(catalog: CatalogInfo | null): string[] {
  return (catalog?.packages ?? []).flatMap((p) => p.gatherers.map((g) => `${p.name}.${g.name}`));
}

/** Names a property or param expression can reference: declared vars and the
 *  variables gather blocks bind. */
export function scopeNames(doc: PlaybookDoc): string[] {
  return [...doc.vars.map((v) => v.key), ...doc.gathers.map((g) => g.name)];
}

// --- warnings ----------------------------------------------------------------
//
// The subset of config-weave's load-time errors the designer can see without
// running anything, reported per node so the tree can flag them.

export interface Warning {
  sel: Sel;
  message: string;
}

const CONCURRENCY_RANK: Record<string, number> = { parallel: 0, exclusive: 1, global: 2 };

/** The two checks config-weave applies to any `properties`/`params` block:
 *  every required parameter present, nothing it doesn't declare. */
function paramWarnings(sel: Sel, given: Kv[], declared: ParamDoc[], ref: string): Warning[] {
  const out: Warning[] = [];
  for (const param of declared) {
    if (param.required && param.default === undefined && !kvGet(given, param.name)) {
      out.push({ sel, message: `"${param.name}" is required` });
    }
  }
  const known = new Set(declared.map((p) => p.name));
  for (const kv of given) {
    if (!known.has(kv.key)) {
      out.push({ sel, message: `"${kv.key}" is not a parameter of ${ref}` });
    }
  }
  return out;
}

export function warningsFor(doc: PlaybookDoc, catalog: CatalogInfo | null): Warning[] {
  const out: Warning[] = [];
  if (!doc.name.trim()) out.push({ sel: { kind: "playbook" }, message: "The playbook needs a name" });

  doc.gathers.forEach((gather, index) => {
    const sel: Sel = { kind: "gather", index };
    if (!gather.name.trim()) out.push({ sel, message: "The gatherer needs a variable name" });
    if (!gather.from.trim()) out.push({ sel, message: "Pick a gatherer" });
    const decl = findGatherer(catalog, gather.from);
    if (decl) out.push(...paramWarnings(sel, gather.params, decl.params, gather.from));
  });

  doc.plays.forEach((play, playIndex) => {
    const names = new Map<string, number>();
    for (const step of playSteps(play)) names.set(step.name, (names.get(step.name) ?? 0) + 1);
    const known = new Set(names.keys());

    const walk = (items: PlayItemDoc[], path: number[]) => {
      items.forEach((item, index) => {
        const here = [...path, index];
        const sel: Sel = { kind: "item", play: playIndex, path: here };
        if (isContainer(item)) {
          if (!item.container.name.trim()) out.push({ sel, message: "The folder needs a name" });
          walk(item.container.items, here);
          return;
        }
        const step = item.step;
        if (!step.name.trim()) out.push({ sel, message: "The step needs a name" });
        else if ((names.get(step.name) ?? 0) > 1) {
          out.push({ sel, message: `Another step in this play is also called "${step.name}"` });
        }
        if (!step.resource.trim()) out.push({ sel, message: "Pick a resource" });
        for (const dep of step.requires) {
          if (dep === step.name) out.push({ sel, message: "A step cannot require itself" });
          else if (!known.has(dep)) out.push({ sel, message: `Requires unknown step "${dep}"` });
        }
        const decl = findResource(catalog, step.resource);
        if (decl) {
          out.push(...paramWarnings(sel, step.properties, decl.params, step.resource));
          const floor = CONCURRENCY_RANK[decl.concurrency ?? "parallel"] ?? 0;
          const chosen = step.concurrency ? (CONCURRENCY_RANK[step.concurrency] ?? 0) : floor;
          if (chosen < floor) {
            out.push({
              sel,
              message: `${step.resource} declares ${decl.concurrency}; a step may only tighten it`,
            });
          }
        }
        // config-weave's documented trap: inside a properties block the key
        // shadows the outer variable, so `url = url` is a self-reference.
        for (const kv of step.properties) {
          if (isExpr(kv.value) && kv.value.expr.trim() === kv.key) {
            out.push({
              sel,
              message: `"${kv.key} = ${kv.key}" is a self-reference — rename the variable`,
            });
          }
        }
      });
    };
    walk(play.items, []);
  });
  return out;
}

/** Warnings grouped by the node they belong to, for tree markers. */
export function warningIndex(warnings: Warning[]): Map<string, string[]> {
  const map = new Map<string, string[]>();
  for (const w of warnings) {
    const id = selId(w.sel);
    map.set(id, [...(map.get(id) ?? []), w.message]);
  }
  return map;
}

export type { ParamDoc };
