import { For, Show, createSignal, createEffect, onMount, onCleanup } from "solid-js";
import { basicSetup } from "codemirror";
import { EditorView } from "@codemirror/view";
import { StreamLanguage } from "@codemirror/language";
import { lintGutter, setDiagnostics, type Diagnostic } from "@codemirror/lint";
import {
  state,
  showToast,
  anyVmRunning,
  reloadLab as reloadCurrentLab,
} from "../store";
import * as api from "../api";
import { ValidationError, type ConfigIssue } from "../api";
import { Code } from "./icons";

// A small, best-effort highlighter for WCL (HCL-like): comments, strings,
// import paths (<vmlab.wcl>), numbers, booleans, identifiers. The returned
// token names map to @lezer/highlight tags via CodeMirror's default table.
const wclLang = StreamLanguage.define({
  token(stream) {
    if (stream.eatSpace()) return null;
    if (stream.match("//") || stream.match("#")) {
      stream.skipToEnd();
      return "comment";
    }
    if (stream.match("/*")) {
      while (!stream.eol() && !stream.match("*/")) stream.next();
      return "comment";
    }
    const ch = stream.peek();
    if (ch === '"') {
      stream.next();
      let escaped = false;
      while (!stream.eol()) {
        const c = stream.next();
        if (c === '"' && !escaped) break;
        escaped = c === "\\" && !escaped;
      }
      return "string";
    }
    if (ch === "<" && stream.match(/^<[^>]*>/)) return "string";
    if (stream.match(/^-?\d+(\.\d+)?/)) return "number";
    if (stream.match(/^(true|false|null)\b/)) return "atom";
    if (stream.match(/^[A-Za-z_][\w.-]*/)) return "variableName";
    stream.next();
    return null;
  },
});

// Editor chrome tuned to the app's dark palette (uses the CSS vars from theme.css).
const appTheme = EditorView.theme(
  {
    "&": { backgroundColor: "transparent", color: "var(--fg-0)", height: "100%" },
    "&.cm-focused": { outline: "none" },
    ".cm-scroller": {
      fontFamily: "var(--font-mono)",
      fontSize: "12.5px",
      lineHeight: "1.6",
    },
    ".cm-content": { caretColor: "var(--fg-0)" },
    ".cm-gutters": {
      backgroundColor: "var(--bg-1)",
      color: "var(--fg-3)",
      border: "none",
      borderRight: "1px solid var(--border)",
    },
    ".cm-activeLine": { backgroundColor: "var(--bg-2)" },
    ".cm-activeLineGutter": { backgroundColor: "var(--bg-2)" },
  },
  { dark: true },
);

export default function ConfigView() {
  let host: HTMLDivElement | undefined;
  let view: EditorView | undefined;
  let loadingDoc = false;

  const [loaded, setLoaded] = createSignal(false);
  const [dirty, setDirty] = createSignal(false);
  const [busy, setBusy] = createSignal<string | null>(null);
  const [issues, setIssues] = createSignal<ConfigIssue[]>([]);
  const [path, setPath] = createSignal("");

  onMount(() => {
    view = new EditorView({
      parent: host!,
      extensions: [
        basicSetup,
        wclLang,
        appTheme,
        lintGutter(),
        EditorView.updateListener.of((u) => {
          if (u.docChanged && !loadingDoc) setDirty(true);
        }),
      ],
    });
    onCleanup(() => view?.destroy());
  });

  // (Re)load the file whenever the current lab changes.
  createEffect(() => {
    const lab = state.currentLab;
    if (!lab || !view) return;
    void load(lab);
  });

  function setDoc(text: string) {
    if (!view) return;
    loadingDoc = true;
    view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text } });
    loadingDoc = false;
    setDirty(false);
  }

  async function load(lab: string) {
    setLoaded(false);
    try {
      const doc = await api.getConfig(lab);
      setDoc(doc.content);
      setPath(doc.path);
      clearIssues();
      setLoaded(true);
    } catch (e) {
      showToast(`Failed to load config: ${msg(e)}`);
    }
  }

  const text = () => view?.state.doc.toString() ?? "";

  function clearIssues() {
    setIssues([]);
    if (view) view.dispatch(setDiagnostics(view.state, []));
  }

  function showIssues(list: ConfigIssue[]) {
    setIssues(list);
    if (!view) return;
    const doc = view.state.doc;
    const diags: Diagnostic[] = list
      .filter((i) => i.line != null)
      .map((i) => {
        const line = doc.line(Math.min(Math.max(i.line!, 1), doc.lines));
        return { from: line.from, to: line.to, severity: "error", message: i.message };
      });
    view.dispatch(setDiagnostics(view.state, diags));
  }

  function onError(e: unknown) {
    if (e instanceof ValidationError) {
      showIssues(e.issues);
      showToast(`${e.issues.length} validation issue(s)`);
    } else {
      showToast(`Error: ${msg(e)}`);
    }
  }

  async function doSave(): Promise<boolean> {
    try {
      await api.saveConfig(state.currentLab!, text());
      clearIssues();
      setDirty(false);
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
      clearIssues();
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
      showToast("Saved — reloading lab…");
      await reloadCurrentLab();
      showToast("Lab reloaded");
    } catch (e) {
      // A 409 (VMs still running) or daemon error surfaces here.
      showToast(`Reload failed: ${msg(e)}`);
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
    <>
      <header class="chead">
        <div>
          <div class="eyebrow">// config</div>
          <h1 class="ctitle">
            <span class="niic" style="width:20px;height:20px;display:inline-flex">
              <Code />
            </span>
            vmlab.wcl
          </h1>
          <div class="csub">
            {path() || "—"}
            {dirty() ? " · unsaved changes" : ""}
          </div>
        </div>
        <div class="logctl">
          <button class="logbtn" onClick={revert} disabled={disabled()} title="Discard edits and reload from disk">
            revert
          </button>
          <button class="logbtn" onClick={validate} disabled={disabled()} title="Validate without saving">
            validate
          </button>
          <button class="btn" onClick={save} disabled={disabled() || !dirty()}>
            save
          </button>
          <button
            class="btn btn-primary"
            classList={{ dis: disabled() || anyVmRunning() }}
            onClick={saveReload}
            title={
              anyVmRunning()
                ? "Stop all VMs before reloading"
                : "Save and restart the lab to apply changes"
            }
          >
            save &amp; reload
          </button>
        </div>
      </header>
      <div class="body cfgbody">
        <Show
          when={state.currentLab}
          fallback={<div class="csub">No lab selected.</div>}
        >
          <div class="cfgbox" ref={host} />
          <Show when={anyVmRunning()}>
            <div class="cfgnote">
              Some VMs are running — stop the lab before reloading to apply config changes.
            </div>
          </Show>
          <Show when={issues().length}>
            <div class="cfgissues">
              <h4>{issues().length} validation issue(s)</h4>
              <For each={issues()}>
                {(i) => (
                  <div class="cfgissue">
                    <span class="ln">{i.line != null ? `line ${i.line}` : ""}</span>
                    <span>{i.message}</span>
                  </div>
                )}
              </For>
            </div>
          </Show>
        </Show>
      </div>
    </>
  );
}

function msg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}
