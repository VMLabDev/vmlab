// Editor-scoped state: the baseline model (as fetched, spans intact), the
// editable draft, selection, and the save/validate flow. Module-level so
// the draft survives view switches; it resets on lab switch.

import { createStore, produce } from "solid-js/store";
import * as api from "../api";
import type { CatalogMeta, ConfigIssue, StoreTemplate } from "../api";
import { StaleRev, ValidationError } from "../api";
import { setNavGuard, showToast } from "../store";
import { confirmDialog } from "../components/dialogs";
import type { LabModel, Span, TemplateSummary, VmModel, SegmentModel } from "./model";
import { deepClone, emptySegment, emptyVm, uniqueName } from "./model";
import { buildOps } from "./ops";

export type Selection =
  | { kind: "lab" }
  | { kind: "segment"; index: number }
  | { kind: "vm"; index: number }
  | { kind: "nat" };

interface EditorState {
  lab: string | null;
  loading: boolean;
  /** Set when the file exists but doesn't parse — the editor can't open it. */
  fallback: string | null;
  /** Set when a save hit a 409 — the file changed underneath the draft. */
  conflict: boolean;
  path: string;
  rev: string | null;
  /** Raw vmlab.wcl text matching `rev` — used to map issue lines to blocks. */
  source: string;
  baseline: LabModel | null;
  draft: LabModel | null;
  templatesInFile: TemplateSummary[];
  selection: Selection;
  busy: string | null;
  issues: ConfigIssue[];
  catalog: {
    templates: StoreTemplate[];
    profiles: string[];
    meta: CatalogMeta | null;
  };
}

const [editor, setEditor] = createStore<EditorState>({
  lab: null,
  loading: false,
  fallback: null,
  conflict: false,
  path: "",
  rev: null,
  source: "",
  baseline: null,
  draft: null,
  templatesInFile: [],
  selection: { kind: "lab" },
  busy: null,
  issues: [],
  catalog: { templates: [], profiles: [], meta: null },
});

export { editor, setEditor };

/** The op batch a Save would send right now. */
export function pendingOps() {
  if (!editor.baseline || !editor.draft) return [];
  return buildOps(editor.baseline, editor.draft);
}

export function editorDirty(): boolean {
  return pendingOps().length > 0;
}

// --- loading ----------------------------------------------------------------

export async function openEditor(lab: string) {
  if (editor.lab === lab && editor.draft) return; // keep the live draft
  setEditor({
    lab,
    loading: true,
    fallback: null,
    conflict: false,
    baseline: null,
    draft: null,
    issues: [],
    selection: { kind: "lab" },
  });
  await Promise.all([reloadModel(lab), loadCatalogs()]);
}

/** Discard the draft and re-fetch the model from disk. */
export async function reloadModel(lab = editor.lab): Promise<void> {
  if (!lab) return;
  setEditor({ loading: true, fallback: null, conflict: false, issues: [] });
  try {
    const [doc, raw] = await Promise.all([
      api.getLabModel(lab),
      api.getConfig(lab).catch(() => null),
    ]);
    if (editor.lab !== lab) return;
    setEditor({
      loading: false,
      path: doc.path,
      rev: doc.rev,
      source: raw?.content ?? "",
      baseline: doc.lab,
      draft: deepClone(doc.lab),
      templatesInFile: doc.templates,
    });
  } catch (e) {
    if (editor.lab !== lab) return;
    const msg =
      e instanceof ValidationError
        ? `vmlab.wcl has ${e.issues.length} problem(s) — fix it in the Config page first`
        : e instanceof Error
          ? e.message
          : String(e);
    setEditor({ loading: false, fallback: msg });
  }
}

async function loadCatalogs() {
  const [templates, profiles, meta] = await Promise.all([
    api.listStoreTemplates().catch(() => [] as StoreTemplate[]),
    api.listProfiles().catch(() => [] as string[]),
    api.catalogMeta().catch(() => null),
  ]);
  setEditor("catalog", { templates, profiles, meta });
}

// --- selection ---------------------------------------------------------------

export function select(sel: Selection) {
  setEditor("selection", sel);
}

// --- draft mutations ----------------------------------------------------------

export function addVm(): number {
  const draft = editor.draft;
  if (!draft) return -1;
  const name = uniqueName("vm", draft.vms.map((v) => v.name));
  // Default to the first store template so a fresh VM validates once the
  // user picks nothing else; empty string still forces a choice visually.
  const tpl = editor.catalog.templates[0];
  const template = tpl ? `${tpl.arch}/${tpl.name}` : "";
  setEditor("draft", "vms", draft.vms.length, emptyVm(name, template));
  const index = editor.draft!.vms.length - 1;
  select({ kind: "vm", index });
  return index;
}

export function addSegment(): number {
  const draft = editor.draft;
  if (!draft) return -1;
  const name = uniqueName("segment", draft.segments.map((s) => s.name));
  setEditor("draft", "segments", draft.segments.length, emptySegment(name));
  const index = editor.draft!.segments.length - 1;
  select({ kind: "segment", index });
  return index;
}

export function removeVm(index: number) {
  setEditor(
    "draft",
    "vms",
    produce((vms: VmModel[]) => {
      vms.splice(index, 1);
    }),
  );
  select({ kind: "lab" });
}

export function removeSegment(index: number) {
  setEditor(
    "draft",
    "segments",
    produce((segments: SegmentModel[]) => {
      segments.splice(index, 1);
    }),
  );
  select({ kind: "lab" });
}

/** Attach a new NIC on VM `vmIndex` (segment name, or null = NAT). */
export function addNic(vmIndex: number, segment: string | null) {
  const vm = editor.draft?.vms[vmIndex];
  if (!vm) return;
  setEditor("draft", "vms", vmIndex, "nics", vm.nics.length, {
    span: null as Span | null,
    segment,
    nat: segment === null,
    ip: null,
    mac: null,
    isolated: false,
  });
}

/** Rename a VM and rewrite every reference to it in the draft. */
export function renameVm(index: number, to: string) {
  const draft = editor.draft;
  if (!draft) return;
  const from = draft.vms[index].name;
  if (from === to) return;
  setEditor(
    "draft",
    produce((d: LabModel | null) => {
      if (!d) return;
      d.vms[index].name = to;
      for (const vm of d.vms) {
        vm.depends_on = vm.depends_on.map((n) => (n === from ? to : n));
      }
      for (const p of d.provisions) {
        p.vms = p.vms.map((n) => (n === from ? to : n));
      }
      for (const s of d.segments) {
        for (const f of s.forwards) {
          if (f.vm === from) f.vm = to;
        }
      }
    }),
  );
}

/** Rename a segment and rewrite every reference to it in the draft. */
export function renameSegment(index: number, to: string) {
  const draft = editor.draft;
  if (!draft) return;
  const from = draft.segments[index].name;
  if (from === to) return;
  setEditor(
    "draft",
    produce((d: LabModel | null) => {
      if (!d) return;
      d.segments[index].name = to;
      for (const s of d.segments) {
        s.routes_to = s.routes_to.map((n) => (n === from ? to : n));
      }
      for (const vm of d.vms) {
        for (const n of vm.nics) {
          if (n.segment === from) n.segment = to;
        }
      }
    }),
  );
}

// --- save / validate ----------------------------------------------------------

export function revertDraft() {
  if (!editor.baseline) return;
  setEditor({ draft: deepClone(editor.baseline), issues: [], conflict: false });
}

function onSaveError(e: unknown) {
  if (e instanceof ValidationError) {
    setEditor("issues", e.issues);
    showToast(`${e.issues.length} validation issue(s)`, "danger");
  } else if (e instanceof StaleRev) {
    setEditor("conflict", true);
    showToast("Config changed on disk — reload the editor", "danger");
  } else {
    showToast(`Error: ${e instanceof Error ? e.message : String(e)}`, "danger");
  }
}

export async function validateDraft() {
  const lab = editor.lab;
  if (!lab || !editor.rev) return;
  setEditor("busy", "validate");
  try {
    await api.editLabModel(lab, editor.rev, pendingOps(), true);
    setEditor("issues", []);
    showToast("Config is valid");
  } catch (e) {
    onSaveError(e);
  } finally {
    setEditor("busy", null);
  }
}

export async function saveDraft(): Promise<boolean> {
  const lab = editor.lab;
  if (!lab || !editor.rev) return false;
  setEditor("busy", "save");
  try {
    const res = await api.editLabModel(lab, editor.rev, pendingOps(), false);
    if (res.lab && res.rev) {
      setEditor({
        rev: res.rev,
        source: res.source ?? editor.source,
        baseline: res.lab,
        draft: deepClone(res.lab),
        templatesInFile: res.templates ?? editor.templatesInFile,
        issues: [],
        conflict: false,
      });
    }
    showToast("Config saved");
    return true;
  } catch (e) {
    onSaveError(e);
    return false;
  } finally {
    setEditor("busy", null);
  }
}

/** Names usable as references in the draft (pickers). */
export const vmNames = () => editor.draft?.vms.map((v) => v.name) ?? [];
export const segmentNames = () => editor.draft?.segments.map((s) => s.name) ?? [];

// Lab switches consult this guard so an unsaved draft is never silently
// dropped (the draft itself survives view switches — it dies only when a
// different lab's model loads over it).
setNavGuard(async () => {
  if (!editorDirty()) return true;
  return confirmDialog({
    title: "Discard unsaved lab changes?",
    body: "The designer has edits that haven't been saved.",
    confirmLabel: "Discard",
    danger: true,
  });
});

/** Map a validation issue's 1-based line to the VM/segment whose span
 *  contains it (best effort — the canvas rings that node, clicking the
 *  issue selects it). Issues in edited-but-unsaved text may drift a little;
 *  the panel still shows the message either way. */
export function selectionForLine(line: number | null): Selection | null {
  const base = editor.baseline;
  if (line == null || !base || !editor.source) return null;
  // Byte offset of the line start (source is the rev the spans bind to).
  let offset = 0;
  const lines = editor.source.split("\n");
  for (let i = 0; i < Math.min(line - 1, lines.length); i++) {
    offset += lines[i].length + 1;
  }
  const contains = (span: Span | null) => span && offset >= span[0] && offset < span[1];
  for (let i = 0; i < base.vms.length; i++) {
    if (contains(base.vms[i].span)) return { kind: "vm", index: i };
  }
  for (let i = 0; i < base.segments.length; i++) {
    if (contains(base.segments[i].span)) return { kind: "segment", index: i };
  }
  return null;
}
