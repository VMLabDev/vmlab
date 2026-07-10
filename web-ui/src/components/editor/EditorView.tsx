// The visual lab designer page: topology canvas + inspector in a split
// pane, with the ConfigView-style Revert/Validate/Save/Save & reload
// toolbar and validation-issue surfacing.

import { For, Show, createEffect, createMemo } from "solid-js";
import { Alert, Button, Empty, PageHead, Spinner, SplitPane } from "@forge/ui";
import {
  anyVmRunning,
  reloadLab as reloadCurrentLab,
  showToast,
  state,
} from "../../store";
import { openConfigTab } from "./LabEditorView";
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

export default function EditorView() {
  createEffect(() => {
    const lab = state.currentLab;
    if (lab && state.view.kind === "editor") void openEditor(lab);
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
      <PageHead
        title="designer"
        sub={`${editor.path || "—"}${dirty() ? ` · ${ops().length} pending change(s)` : ""}`}
        actions={
          <>
            <Button
              variant="ghost"
              onClick={revertDraft}
              disabled={disabled() || !dirty()}
              title="Discard edits (back to the last saved state)"
            >
              Revert
            </Button>
            <Button
              onClick={validateDraft}
              disabled={disabled() || !dirty()}
              title="Validate the pending changes without saving"
            >
              Validate
            </Button>
            <Button onClick={saveDraft} disabled={disabled() || !dirty()}>
              Save
            </Button>
            <Button
              variant="primary"
              onClick={saveReload}
              disabled={disabled() || anyVmRunning()}
              title={
                anyVmRunning()
                  ? "Stop all VMs before reloading"
                  : "Save and restart the lab to apply changes"
              }
            >
              Save & reload
            </Button>
          </>
        }
      />
      <div class="stack">
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
            <Button size="sm" onClick={openConfigTab}>
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
            first={<TopologyCanvas />}
            second={<Inspector />}
            initial={Math.max(480, window.innerWidth - 720)}
            min={320}
          />
          <Show when={anyVmRunning()}>
            <Alert tone="warning">
              Some VMs are running — stop the lab before reloading to apply config changes.
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
