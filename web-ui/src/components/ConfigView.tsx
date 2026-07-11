import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { Alert, Button, Empty } from "@forge/ui";
import { CodeEditor } from "@forge/code";
import type { CodeAnnotation } from "@forge/code";
import {
  state,
  showToast,
  anyVmRunning,
  reloadLab as reloadCurrentLab,
} from "../store";
import * as api from "../api";
import { ValidationError, type ConfigIssue } from "../api";
import { wclLanguage } from "../wcl-language";

export default function ConfigView() {
  const [text, setText] = createSignal("");
  const [baseline, setBaseline] = createSignal(""); // last loaded/saved content
  const [loaded, setLoaded] = createSignal(false);
  const [busy, setBusy] = createSignal<string | null>(null);
  const [issues, setIssues] = createSignal<ConfigIssue[]>([]);
  const [path, setPath] = createSignal("");

  const dirty = () => text() !== baseline();

  // Issues → whole-line lint annotations (the huge col clamps to line end).
  const annotations = createMemo<CodeAnnotation[]>(() =>
    issues()
      .filter((i) => i.line != null)
      .map((i) => ({
        from: { line: i.line!, col: 0 },
        to: { line: i.line!, col: 100000 },
        severity: "error" as const,
        message: i.message,
        source: "wcl",
      })),
  );

  // (Re)load the file whenever the current lab changes.
  createEffect(() => {
    const lab = state.currentLab;
    if (!lab) return;
    void load(lab);
  });

  async function load(lab: string) {
    setLoaded(false);
    try {
      const doc = await api.getConfig(lab);
      setBaseline(doc.content);
      setText(doc.content);
      setPath(doc.path);
      setIssues([]);
      setLoaded(true);
    } catch (e) {
      showToast(`Failed to load config: ${msg(e)}`, "danger");
    }
  }

  function onError(e: unknown) {
    if (e instanceof ValidationError) {
      setIssues(e.issues);
      showToast(`${e.issues.length} validation issue(s)`, "danger");
    } else {
      showToast(`Error: ${msg(e)}`, "danger");
    }
  }

  async function doSave(): Promise<boolean> {
    try {
      await api.saveConfig(state.currentLab!, text());
      setIssues([]);
      setBaseline(text());
      return true;
    } catch (e) {
      onError(e);
      return false;
    }
  }

  async function validate() {
    setBusy("validate");
    try {
      await api.validateConfig(state.currentLab!, text());
      setIssues([]);
      showToast("Config is valid");
    } catch (e) {
      onError(e);
    } finally {
      setBusy(null);
    }
  }

  async function save() {
    setBusy("save");
    try {
      if (await doSave()) showToast("Config saved");
    } finally {
      setBusy(null);
    }
  }

  async function saveReload() {
    setBusy("reload");
    try {
      if (!(await doSave())) return;
      showToast("Saved — reloading lab…", "info");
      await reloadCurrentLab();
      showToast("Lab reloaded");
    } catch (e) {
      // A 409 (VMs still running) or daemon error surfaces here.
      showToast(`Reload failed: ${msg(e)}`, "danger");
    } finally {
      setBusy(null);
    }
  }

  function revert() {
    const lab = state.currentLab;
    if (lab) void load(lab);
  }

  const disabled = () => !loaded() || busy() !== null;
  const readOnly = () => anyVmRunning();

  return (
    <Show when={state.currentLab} fallback={<Empty title="No lab selected" />}>
      <div class="config-modal">
        <div class="config-modal-head">
          <span class="config-modal-path">
            {path() || "—"}
            {dirty() ? " · unsaved changes" : ""}
          </span>
          <div class="config-modal-actions">
            <Button
              size="sm"
              variant="ghost"
              onClick={revert}
              disabled={disabled()}
              title="Discard edits and reload from disk"
            >
              Revert
            </Button>
            <Button
              size="sm"
              onClick={validate}
              disabled={disabled() || readOnly()}
              title={
                readOnly() ? "Stop all machines before editing config" : "Validate without saving"
              }
            >
              Validate
            </Button>
            <Button size="sm" onClick={save} disabled={disabled() || readOnly() || !dirty()}>
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
        <div class="stack">
          <CodeEditor
            value={text()}
            onChange={setText}
            readOnly={readOnly()}
            language={wclLanguage}
            annotations={annotations()}
            height="min(66vh, 720px)"
          />
          <Show when={anyVmRunning()}>
            <Alert tone="warning">
              Config is read-only while any VM or container is up. Stop all machines to edit it.
            </Alert>
          </Show>
          <Show when={issues().length}>
            <Alert tone="danger" title={`${issues().length} validation issue(s)`}>
              <For each={issues()}>
                {(i) => (
                  <div class="cfg-issue">
                    <span class="cfg-issue-line">{i.line != null ? `line ${i.line}` : ""}</span>
                    <span>{i.message}</span>
                  </div>
                )}
              </For>
            </Alert>
          </Show>
        </div>
      </div>
    </Show>
  );
}

function msg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
