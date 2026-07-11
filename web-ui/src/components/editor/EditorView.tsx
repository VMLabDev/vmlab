// The visual lab designer: topology canvas + inspector in a split pane,
// with a compact Revert/Validate/Save/Save & reload toolbar and
// validation-issue surfacing. Embedded in the lab page's Overview tab.

import { For, Show, createEffect, createMemo } from "solid-js";
import { Alert, Button, Empty, Spinner, SplitPane } from "@forge/ui";
import {
  anyVmRunning,
  reloadLab as reloadCurrentLab,
  showToast,
  state,
} from "../../store";
import {
  editor,
  openEditor,
  pendingOps,
  reloadModel,
  revertDraft,
  saveDraft,
  select,
  selectionForLine,
  validateDraft,
} from "../../editor/store";
import Inspector from "./Inspector";
import TopologyCanvas from "./TopologyCanvas";

export default function EditorView(props: { onEditConfig: () => void }) {
  createEffect(() => {
    const lab = state.currentLab;
    if (lab && state.view.kind === "lab") void openEditor(lab);
  });

  const ops = createMemo(() => (editor.draft && editor.baseline ? pendingOps() : []));
  const dirty = () => ops().length > 0;
  const disabled = () => editor.busy !== null || !editor.draft;

  async function saveReload() {
    if (!(await saveDraft())) return;
    try {
      showToast("Saved — reloading lab…", "info");
      await reloadCurrentLab();
      showToast("Lab reloaded");
    } catch (e) {
      showToast(`Reload failed: ${e instanceof Error ? e.message : e}`, "danger");
    }
  }

  return (
    <Show when={state.currentLab} fallback={<Empty title="No lab selected" />}>
      <div class="stack">
        <div class="editor-toolbar">
          <span class="editor-path">
            {editor.path || "—"}
            {dirty() ? ` · ${ops().length} pending change(s)` : ""}
          </span>
          <div class="editor-actions">
            <Button
              size="sm"
              variant="ghost"
              onClick={revertDraft}
              disabled={disabled() || !dirty()}
              title="Discard edits (back to the last saved state)"
            >
              Revert
            </Button>
            <Button
              size="sm"
              onClick={validateDraft}
              disabled={disabled() || !dirty()}
              title="Validate the pending changes without saving"
            >
              Validate
            </Button>
            <Button size="sm" onClick={saveDraft} disabled={disabled() || !dirty()}>
              Save
            </Button>
            <Button
              size="sm"
              variant="primary"
              onClick={saveReload}
              disabled={disabled() || anyVmRunning()}
              title={
                anyVmRunning()
                  ? "Stop all VMs and containers before reloading"
                  : "Save and restart the lab to apply changes"
              }
            >
              Save & reload
            </Button>
          </div>
        </div>
        <Show when={editor.conflict}>
          <Alert tone="danger" title="vmlab.wcl changed on disk">
            Someone edited the config underneath this draft (Config page or another session).{" "}
            <Button size="sm" onClick={() => void reloadModel()}>
              Reload editor (discard draft)
            </Button>
          </Alert>
        </Show>
        <Show when={editor.fallback}>
          <Alert tone="warning" title="Can't open the visual editor">
            {editor.fallback}{" "}
            <Button size="sm" onClick={props.onEditConfig}>
              Open raw config
            </Button>
          </Alert>
        </Show>
        <Show when={editor.loading}>
          <div class="editor-loading">
            <Spinner /> loading model…
          </div>
        </Show>
        <Show when={editor.draft && !editor.fallback}>
          <SplitPane
            class="editor-split"
            first={<TopologyCanvas onEditConfig={props.onEditConfig} />}
            second={<Inspector />}
            initial={Math.max(480, window.innerWidth - 720)}
            min={320}
          />
          <Show when={anyVmRunning()}>
            <Alert tone="warning">
              Some machines are up — running machines and networking are read-only until they stop.
            </Alert>
          </Show>
          <Show when={editor.issues.length}>
            <Alert tone="danger" title={`${editor.issues.length} validation issue(s)`}>
              <For each={editor.issues}>
                {(i) => (
                  <div
                    class="cfg-issue editor-issue"
                    onClick={() => {
                      const sel = selectionForLine(i.line);
                      if (sel) select(sel);
                    }}
                  >
                    <span class="cfg-issue-line">{i.line != null ? `line ${i.line}` : ""}</span>
                    <span>{i.message}</span>
                  </div>
                )}
              </For>
            </Alert>
          </Show>
        </Show>
      </div>
    </Show>
  );
}
