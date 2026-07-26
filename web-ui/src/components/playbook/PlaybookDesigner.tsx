// The visual editor for a playbook.wcl, shown in the Files tab instead of the
// code editor.
//
// It is a *view over the file buffer*: the text comes in as a prop and edits
// go back out through `onChange`, so the Files tab's dirty marker, Save,
// stale-rev handling and nav guard keep working untouched. The WCL itself is
// config-weave's business — `doc/inspect` turns the buffer into a structural
// document, `doc/render` turns the edited document back into source against
// the buffer it came from, which is what keeps comments alive.

import { Show, createEffect, createMemo, createSignal, on, onCleanup } from "solid-js";
import { createStore, produce, unwrap } from "solid-js/store";
import { Alert, Button, Spinner, SplitPane } from "@forge/ui";
import { For } from "solid-js";
import * as api from "../../api";
import type { CatalogInfo, PlaybookDoc } from "../../api";
import { state } from "../../store";
import { confirmDialog } from "../dialogs";
import type { DropTarget, Sel } from "../../playbook/doc";
import {
  appendItem,
  isContainer,
  itemAt,
  moveItem,
  newContainer,
  newGather,
  newPlay,
  newStep,
  newVar,
  playSteps,
  removeItem,
  warningIndex,
  warningsFor,
} from "../../playbook/doc";
import PlaybookInspector from "./PlaybookInspector";
import PlaybookTree from "./PlaybookTree";

/** Coalesce keystrokes before spending a render round trip. */
const RENDER_DEBOUNCE_MS = 200;

export interface PlaybookDesignerProps {
  /** The playbook.wcl's lab-relative path (its folder locates the packages). */
  path: string;
  content: string;
  readOnly?: boolean;
  onChange: (source: string) => void;
  /** Switch the Files tab to the raw code view. */
  onShowCode: () => void;
  /** Open the Add-package picker for this playbook folder. */
  onAddPackages: (folder: string) => void;
}

export default function PlaybookDesigner(props: PlaybookDesignerProps) {
  const [doc, setDoc] = createStore<{ value: PlaybookDoc | null }>({ value: null });
  const [selection, setSelection] = createSignal<Sel | null>({ kind: "playbook" });
  const [diags, setDiags] = createSignal<string[]>([]);
  const [error, setError] = createSignal<string | null>(null);
  const [loading, setLoading] = createSignal(true);
  const [catalog, setCatalog] = createSignal<CatalogInfo | null>(null);

  /** The text the current document was read from — `render`'s base, and the
   *  marker for "did this change under us?". */
  let base = "";
  let timer: number | undefined;
  onCleanup(() => clearTimeout(timer));

  const folder = () => props.path.replace(/\/playbook\.wcl$/, "");

  // Re-read whenever the buffer differs from what we last rendered: mount,
  // code-view edits, reload-from-disk, file switches.
  createEffect(
    on(
      () => props.content,
      (content) => {
        if (content === base) return;
        void inspect(content);
      },
    ),
  );

  createEffect(
    on(folder, (dir) => {
      const lab = state.currentLab;
      if (!lab || !dir) return;
      void api
        .playbookCatalog(lab, dir)
        .then((info) => setCatalog(info))
        .catch(() => setCatalog(null));
    }),
  );

  async function inspect(content: string) {
    const lab = state.currentLab;
    if (!lab) return;
    setLoading(true);
    setError(null);
    try {
      const result = await api.inspectPlaybookDoc(lab, content);
      if (result.ok) {
        base = content;
        setDoc("value", result.doc);
        setDiags([]);
      } else {
        setDoc("value", null);
        setDiags((result.diags ?? []).map(diagText));
      }
    } catch (cause) {
      setDoc("value", null);
      setError(cause instanceof Error ? cause.message : String(cause));
    } finally {
      setLoading(false);
    }
  }

  /** Apply an edit, then push the rendered source back to the buffer. */
  function mutate(fn: (doc: PlaybookDoc) => void) {
    if (props.readOnly) return;
    setDoc(
      "value",
      produce((current: PlaybookDoc | null) => {
        if (current) fn(current);
      }),
    );
    scheduleRender();
  }

  function scheduleRender() {
    clearTimeout(timer);
    timer = window.setTimeout(() => void renderNow(), RENDER_DEBOUNCE_MS);
  }

  async function renderNow() {
    const lab = state.currentLab;
    const current = doc.value;
    if (!lab || !current) return;
    try {
      const result = await api.renderPlaybookDoc(lab, base, unwrap(current));
      if (result.ok) {
        base = result.source;
        props.onChange(result.source);
        setError(null);
      } else {
        setError(
          (result.diags ?? []).map(diagText).join("; ") ||
            "config-weave could not write this document",
        );
      }
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause));
    }
  }

  const warnings = createMemo(() =>
    doc.value ? warningsFor(doc.value, catalog()) : [],
  );
  const warningsById = createMemo(() => warningIndex(warnings()));

  // --- tree actions ----------------------------------------------------------

  const stepNamesIn = (play: number): string[] => {
    const current = doc.value;
    if (!current?.plays[play]) return [];
    const names: string[] = [];
    const walk = (items: api.PlayItemDoc[]) => {
      for (const item of items) {
        if (isContainer(item)) {
          names.push(item.container.name);
          walk(item.container.items);
        } else names.push(item.step.name);
      }
    };
    walk(current.plays[play].items);
    return names;
  };

  function add(play: number, parentPath: number[], item: api.PlayItemDoc) {
    let landed: number[] | null = null;
    mutate((d) => {
      landed = appendItem(d, play, parentPath, item);
    });
    if (landed) setSelection({ kind: "item", play, path: landed });
  }

  const addStep = (play: number, parentPath: number[]) =>
    add(play, parentPath, newStep(stepNamesIn(play)));

  const addFolder = (play: number, parentPath: number[]) =>
    add(play, parentPath, newContainer(stepNamesIn(play)));

  function addPlay() {
    mutate((d) => d.plays.push(newPlay(d.plays.map((p) => p.name))));
    setSelection({ kind: "play", play: (doc.value?.plays.length ?? 1) - 1 });
  }

  function addGather() {
    mutate((d) => d.gathers.push(newGather(d.gathers.map((g) => g.name))));
    setSelection({ kind: "gather", index: (doc.value?.gathers.length ?? 1) - 1 });
  }

  function addVar() {
    mutate((d) => d.vars.push(newVar(d.vars.map((v) => v.key))));
    setSelection({ kind: "var", index: (doc.value?.vars.length ?? 1) - 1 });
  }

  function onMoveItem(from: { play: number; path: number[] }, to: DropTarget) {
    let landed: number[] | null = null;
    mutate((d) => {
      landed = moveItem(d, from, to);
    });
    if (landed) setSelection({ kind: "item", play: to.play, path: landed });
  }

  function onMovePlay(from: number, to: number) {
    mutate((d) => {
      const [play] = d.plays.splice(from, 1);
      if (play) d.plays.splice(to, 0, play);
    });
    setSelection({ kind: "play", play: to });
  }

  async function onDelete(sel: Sel) {
    const current = doc.value;
    if (!current) return;
    const what =
      sel.kind === "play"
        ? `play "${current.plays[sel.play]?.name ?? ""}"`
        : sel.kind === "gather"
          ? `gatherer "${current.gathers[sel.index]?.name ?? ""}"`
          : sel.kind === "var"
            ? `variable "${current.vars[sel.index]?.key ?? ""}"`
            : sel.kind === "item"
              ? describeItem(current, sel)
              : "";
    // The playbook block itself has nothing to delete.
    if (!what) return;
    const nested =
      sel.kind === "item" &&
      (() => {
        const item = itemAt(current, sel.play, sel.path);
        return item && isContainer(item) ? item.container.items.length : 0;
      })();
    if (
      !(await confirmDialog({
        title: `Delete ${what}?`,
        body: nested ? `Its ${nested} nested item(s) go with it.` : undefined,
        confirmLabel: "Delete",
        danger: true,
      }))
    ) {
      return;
    }
    mutate((d) => {
      switch (sel.kind) {
        case "play":
          d.plays.splice(sel.play, 1);
          break;
        case "gather":
          d.gathers.splice(sel.index, 1);
          break;
        case "var":
          d.vars.splice(sel.index, 1);
          break;
        case "item":
          removeItem(d, sel.play, sel.path);
          break;
      }
    });
    setSelection({ kind: "playbook" });
  }

  return (
    <div class="pb-designer">
      <Show when={error()}>
        <Alert tone="danger">{error()}</Alert>
      </Show>
      <Show
        when={!loading()}
        fallback={
          <div class="editor-loading">
            <Spinner /> reading playbook…
          </div>
        }
      >
        <Show
          when={doc.value}
          fallback={
            <Alert tone="warning" title="This playbook can't be opened in the designer">
              <For each={diags()}>{(d) => <div class="cfg-issue">{d}</div>}</For>
              <Button size="sm" onClick={() => props.onShowCode()}>
                Edit as code
              </Button>
            </Alert>
          }
        >
          {(current) => (
            <SplitPane
              class="pb-designer-split"
              first={
                <PlaybookTree
                  doc={current()}
                  selection={selection()}
                  warnings={warningsById()}
                  disabled={props.readOnly}
                  onSelect={setSelection}
                  onAddPlay={addPlay}
                  onAddGather={addGather}
                  onAddVar={addVar}
                  onAddStep={addStep}
                  onAddFolder={addFolder}
                  onMoveItem={onMoveItem}
                  onMovePlay={onMovePlay}
                />
              }
              second={
                <PlaybookInspector
                  doc={current()}
                  selection={selection()}
                  catalog={catalog()}
                  warnings={warningsById()}
                  disabled={props.readOnly}
                  mutate={mutate}
                  onDelete={(sel) => void onDelete(sel)}
                  onAddPackages={() => props.onAddPackages(folder())}
                />
              }
              initial={320}
              min={220}
            />
          )}
        </Show>
      </Show>
      <Show when={doc.value && catalog()?.errors.length}>
        <Alert tone="warning">
          <For each={catalog()!.errors}>
            {(e) => <div>package {e.package} could not be read</div>}
          </For>
        </Alert>
      </Show>
    </div>
  );
}

const diagText = (diag: api.DocDiag): string =>
  typeof diag === "string" ? diag : (diag.rendered ?? diag.message ?? "unknown problem");

function describeItem(doc: PlaybookDoc, sel: { kind: "item"; play: number; path: number[] }): string {
  const item = itemAt(doc, sel.play, sel.path);
  if (!item) return "this item";
  return isContainer(item) ? `folder "${item.container.name}"` : `step "${item.step.name}"`;
}

/** Re-exported for tests and the tree's requires picker. */
export { playSteps };
