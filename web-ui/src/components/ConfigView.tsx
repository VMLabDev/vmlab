import { For, Show, createEffect, createMemo, createSignal } from "solid-js";
import { Alert, Button, Empty, PageHead } from "@forge/ui";
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

  return (
    <Show when={state.currentLab} fallback={<Empty title="No lab selected" />}>
      <PageHead
        title="vmlab.wcl"
        sub={`${path() || "—"}${dirty() ? " · unsaved changes" : ""}`}
        actions={
          <>
            <Button
              variant="ghost"
              onClick={revert}
              disabled={disabled()}
              title="Discard edits and reload from disk"
            >
              Revert
            </Button>
            <Button onClick={validate} disabled={disabled()} title="Validate without saving">
              Validate
            </Button>
            <Button onClick={save} disabled={disabled() || !dirty()}>
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
        <CodeEditor
          value={text()}
          onChange={setText}
          language={wclLanguage}
          annotations={annotations()}
          height="calc(100vh - 240px)"
        />
        <Show when={anyVmRunning()}>
          <Alert tone="warning">
            Some VMs are running — stop the lab before reloading to apply config changes.
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
    </Show>
  );
}

function msg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
