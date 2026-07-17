// Full-screen playbook folder editor: file tree on the left, CodeMirror on
// the right, per-file revision tracking with save / save-all. Deliberately
// NOT read-only while machines run — editing playbook files and re-running
// check/apply against a live guest IS the workflow this page exists for.

import { Show, batch, createEffect, createMemo, createSignal, onCleanup, untrack } from "solid-js";
import { createStore, produce } from "solid-js/store";
import { CodeEditor } from "@forge/code";
import { Alert, Button, Empty, PageHead, Spinner, SplitPane } from "@forge/ui";
import { ArrowLeft, FilePlus2, Save } from "lucide-solid";
import * as api from "../api";
import type { PlaybookTreeEntry } from "../api";
import { showLab, showToast, state } from "../store";
import { wclLanguage } from "../wcl-language";
import { wscriptLanguage } from "../wscript-language";
import { confirmDialog, promptDialog } from "./dialogs";
import FileTree from "./FileTree";

const EDITABLE_EXTS = ["wcl", "wscript", "ws"];
const extOf = (path: string) => path.split(".").pop() ?? "";
const editable = (path: string) => EDITABLE_EXTS.includes(extOf(path));

interface OpenFile {
  content: string;
  savedContent: string;
  rev: string | null;
  /** Set when a save 409'd — the file changed on disk underneath us. */
  stale: boolean;
}

type TreeState =
  | { kind: "loading" }
  | { kind: "ok"; entries: PlaybookTreeEntry[] }
  | { kind: "missing" } // declared, folder absent → offer scaffold
  | { kind: "undeclared"; message: string }
  | { kind: "error"; message: string };

export default function PlaybookEditorView() {
  const lab = () => state.currentLab!;
  const playbook = () => state.view.playbook!;
  const decl = () => state.playbooks.find((p) => p.path === state.view.playbook);

  const [tree, setTree] = createSignal<TreeState>({ kind: "loading" });
  const [files, setFiles] = createStore<Record<string, OpenFile>>({});
  const [active, setActive] = createSignal<string | null>(null);
  const [loadingFile, setLoadingFile] = createSignal(false);
  const [saving, setSaving] = createSignal(false);
  let loadGeneration = 0;

  const dirtyPaths = createMemo(() => {
    const dirty = new Set<string>();
    for (const [path, file] of Object.entries(files)) {
      if (file && file.content !== file.savedContent) dirty.add(path);
    }
    return dirty;
  });
  const activeFile = () => (active() ? files[active()!] : undefined);

  async function loadTree(selectFirst = false) {
    const currentLab = lab();
    const currentPb = playbook();
    setTree({ kind: "loading" });
    try {
      const result = await api.playbookTree(currentLab, currentPb);
      if (state.view.playbook !== currentPb) return;
      setTree({ kind: "ok", entries: result.entries });
      if (selectFirst && !active()) {
        const first = firstEditable(result.entries);
        if (first) void openFile(first);
      }
    } catch (cause) {
      if (state.view.playbook !== currentPb) return;
      if (cause instanceof api.PlaybookTreeError && cause.status === 404) {
        setTree({ kind: "missing" });
      } else if (cause instanceof api.PlaybookTreeError && cause.status === 403) {
        setTree({ kind: "undeclared", message: cause.message });
      } else {
        setTree({ kind: "error", message: cause instanceof Error ? cause.message : String(cause) });
      }
    }
  }

  function firstEditable(entries: PlaybookTreeEntry[]): string | null {
    // The playbook's entry point beats tree order (dirs sort first, which
    // would otherwise land on some package resource file).
    if (entries.some((entry) => !entry.dir && entry.path === "playbook.wcl")) {
      return "playbook.wcl";
    }
    for (const entry of entries) {
      if (!entry.dir && editable(entry.path)) return entry.path;
      if (entry.dir) {
        const nested = firstEditable(entry.children ?? []);
        if (nested) return nested;
      }
    }
    return null;
  }

  // (Re)load when the playbook path changes; reset per-folder state. Only
  // `playbook()` may be tracked here — reading `files` untracked, or every
  // cache write would re-trigger the reset (an infinite reload loop).
  createEffect(() => {
    void playbook();
    const generation = ++loadGeneration;
    untrack(() => {
      batch(() => {
        setActive(null);
        for (const key of Object.keys(files)) {
          setFiles(key, undefined as unknown as OpenFile);
        }
      });
      void loadTree(true).then(() => void generation);
    });
  });

  async function openFile(path: string) {
    setActive(path);
    if (files[path]) return;
    setLoadingFile(true);
    try {
      const doc = await api.getPlaybookFile(lab(), playbook(), path);
      const content = doc?.content ?? "";
      setFiles(path, {
        content,
        savedContent: content,
        rev: doc?.rev ?? null,
        stale: false,
      });
    } catch (cause) {
      showToast(cause instanceof Error ? cause.message : String(cause), "danger");
      setActive(null);
    } finally {
      setLoadingFile(false);
    }
  }

  async function saveOne(path: string): Promise<boolean> {
    const file = files[path];
    if (!file) return true;
    try {
      const rev = await api.savePlaybookFile(lab(), playbook(), path, file.content, file.rev);
      setFiles(
        path,
        produce((f: OpenFile) => {
          f.rev = rev;
          f.savedContent = f.content;
          f.stale = false;
        }),
      );
      return true;
    } catch (cause) {
      if (cause instanceof api.ScriptStale) {
        setFiles(path, "stale", true);
      } else {
        showToast(cause instanceof Error ? cause.message : String(cause), "danger");
      }
      return false;
    }
  }

  async function saveActive() {
    const path = active();
    if (!path) return;
    setSaving(true);
    try {
      if (await saveOne(path)) showToast(`Saved ${path}`);
    } finally {
      setSaving(false);
    }
  }

  async function saveAll() {
    setSaving(true);
    try {
      for (const path of dirtyPaths()) {
        if (!(await saveOne(path))) return;
      }
      showToast("Saved all files");
    } finally {
      setSaving(false);
    }
  }

  async function reloadFromDisk(path: string) {
    setFiles(path, undefined as unknown as OpenFile);
    await openFile(path);
  }

  async function newFile(dirPath: string) {
    const suggestion = dirPath ? `${dirPath}/` : "";
    const path = await promptDialog({
      title: "New playbook file",
      label: `Path inside ${playbook()} (.wcl / .wscript)`,
      initial: suggestion,
      confirmLabel: "Create",
    });
    if (!path) return;
    const clean = path.trim().replace(/^\/+/, "");
    if (!clean || clean.split("/").some((part) => !part || part === "..")) {
      showToast("The path must be relative, without ..", "danger");
      return;
    }
    if (!editable(clean)) {
      showToast(`Only ${EDITABLE_EXTS.join("/")} files can be created here`, "danger");
      return;
    }
    try {
      // Created empty on disk right away (noclobber), so the tree shows it
      // and revision tracking starts from a real file.
      const rev = await api.savePlaybookFile(lab(), playbook(), clean, "", null);
      setFiles(clean, { content: "", savedContent: "", rev, stale: false });
      setActive(clean);
      await loadTree();
    } catch (cause) {
      showToast(cause instanceof Error ? cause.message : String(cause), "danger");
    }
  }

  async function scaffold() {
    try {
      await api.scaffoldPlaybook(lab(), playbook());
      showToast("Created playbook.wcl skeleton");
      await loadTree(true);
    } catch (cause) {
      showToast(cause instanceof Error ? cause.message : String(cause), "danger");
    }
  }

  async function back() {
    if (
      dirtyPaths().size > 0 &&
      !(await confirmDialog({
        title: "Discard unsaved playbook changes?",
        body: `${dirtyPaths().size} file(s) have edits that have not been saved.`,
        confirmLabel: "Discard",
        danger: true,
      }))
    ) {
      return;
    }
    showLab();
  }

  // A hard refresh mid-edit would silently lose work.
  const beforeUnload = (event: BeforeUnloadEvent) => {
    if (dirtyPaths().size > 0) event.preventDefault();
  };
  window.addEventListener("beforeunload", beforeUnload);
  onCleanup(() => window.removeEventListener("beforeunload", beforeUnload));

  const language = () => (extOf(active() ?? "") === "wcl" ? wclLanguage : wscriptLanguage);

  return (
    <>
      <PageHead
        title={`Playbook · ${playbook()}`}
        sub={
          decl()
            ? `play ${decl()!.play} · ${decl()!.vms.length ? `${decl()!.vms.length} target(s)` : "all machines"}`
            : "not declared in the current lab config"
        }
        actions={
          <>
            <Button variant="ghost" icon={ArrowLeft} onClick={() => void back()}>
              Back to lab
            </Button>
            <Show when={tree().kind === "ok"}>
              <Button icon={FilePlus2} onClick={() => void newFile("")}>
                New file
              </Button>
              <Button
                icon={Save}
                variant="primary"
                disabled={saving() || !active() || !dirtyPaths().has(active()!)}
                onClick={() => void saveActive()}
              >
                {saving() ? "Saving…" : "Save"}
              </Button>
              <Show when={dirtyPaths().size > 1}>
                <Button icon={Save} disabled={saving()} onClick={() => void saveAll()}>
                  Save all ({dirtyPaths().size})
                </Button>
              </Show>
            </Show>
          </>
        }
      />

      <Show when={tree().kind === "loading"}>
        <div class="editor-loading">
          <Spinner /> loading playbook…
        </div>
      </Show>

      <Show when={tree().kind === "undeclared"}>
        <Empty title="Not a declared playbook">
          {(tree() as { message: string }).message} — declare it with a playbook block in the
          designer and save the lab config first.
        </Empty>
      </Show>

      <Show when={tree().kind === "error"}>
        <Alert tone="danger">{(tree() as { message: string }).message}</Alert>
      </Show>

      <Show when={tree().kind === "missing"}>
        <Empty title="This playbook folder doesn't exist yet">
          <Button variant="primary" onClick={() => void scaffold()}>
            Create playbook.wcl skeleton
          </Button>
        </Empty>
      </Show>

      <Show when={tree().kind === "ok"}>
        <SplitPane
          class="pb-editor-split"
          first={
            <FileTree
              nodes={(tree() as { entries: PlaybookTreeEntry[] }).entries}
              selected={active()}
              dirty={dirtyPaths()}
              editable={editable}
              onSelect={(path) => void openFile(path)}
              onNewFile={(dir) => void newFile(dir)}
            />
          }
          second={
            <div class="pb-editor-pane">
              <Show
                when={active()}
                fallback={<Empty title="Pick a file">Select a file from the tree to edit.</Empty>}
              >
                <Show
                  when={!loadingFile()}
                  fallback={
                    <div class="editor-loading">
                      <Spinner /> loading file…
                    </div>
                  }
                >
                  <Show when={activeFile()?.stale}>
                    <Alert tone="danger">
                      <span
                        style={{ display: "inline-flex", "align-items": "center", gap: "10px" }}
                      >
                        {active()} changed on disk — reload it before saving.
                        <Button size="sm" onClick={() => void reloadFromDisk(active()!)}>
                          Reload from disk
                        </Button>
                      </span>
                    </Alert>
                  </Show>
                  <CodeEditor
                    value={activeFile()?.content ?? ""}
                    onChange={(value) => {
                      // CodeMirror also reports programmatic value swaps
                      // (file switches) — only user edits on a loaded file
                      // may touch the store, or the write lands on a
                      // not-yet-cached entry and throws.
                      const path = active();
                      if (path && files[path] && files[path].content !== value) {
                        setFiles(path, "content", value);
                      }
                    }}
                    language={language()}
                    height="calc(100vh - 220px)"
                  />
                </Show>
              </Show>
            </div>
          }
          initial={280}
          min={200}
        />
      </Show>
    </>
  );
}
