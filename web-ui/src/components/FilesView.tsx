// The lab page's Files tab: a tree of everything in the lab directory with
// create / edit / rename / delete, so provision scripts and config-weave
// playbook folders are workable in place. Buffers, tree, and selection live
// at module scope so switching tabs (or peeking at Logs) never drops edits;
// they reset when a different lab loads.
//
// vmlab.wcl is special-cased: its saves route through the validating
// /config endpoint (422 issues render as CodeMirror annotations), it gains
// Validate / Save & reload actions, and it goes read-only while machines
// run. Every other file stays editable while the lab is up — editing
// playbook files against a live guest is a core workflow.

import { For, Show, batch, createEffect, createSignal } from "solid-js";
import { createStore, produce } from "solid-js/store";
import { CodeEditor } from "@forge/code";
import type { CodeAnnotation } from "@forge/code";
import { Alert, Button, Empty, IconButton, Spinner, SplitPane } from "@forge/ui";
import {
  FilePlus2,
  FolderPlus,
  Library,
  PackageMinus,
  PackagePlus,
  RefreshCw,
  Save,
} from "lucide-solid";
import * as api from "../api";
import type { ConfigIssue, PlaybookTreeEntry } from "../api";
import {
  anyVmRunning,
  registerNavGuard,
  reloadLab as reloadCurrentLab,
  showLab,
  showToast,
  state,
} from "../store";
import { editorDirty, reloadModel } from "../editor/store";
import { wclLanguage } from "../wcl-language";
import { wscriptLanguage } from "../wscript-language";
import { confirmDialog, promptDialog } from "./dialogs";
import { setLabTab } from "./editor/labTab";
import FileTree from "./FileTree";
import { openPkgRepos } from "./PkgReposModal";
import { openPkgSearch } from "./PkgSearchModal";

const LAB_FILE = "vmlab.wcl";
const extOf = (path: string) => path.split(".").pop() ?? "";

interface OpenFile {
  content: string;
  savedContent: string;
  rev: string | null;
  /** Set when a save 409'd — the file changed on disk underneath us. */
  stale: boolean;
  binary?: boolean;
  tooLarge?: boolean;
  size?: number;
}

type TreeState =
  | { kind: "loading" }
  | { kind: "ok"; entries: PlaybookTreeEntry[] }
  | { kind: "error"; message: string };

// --- module-scoped state (survives tab switches; reset on lab change) -------

const [tree, setTree] = createSignal<TreeState>({ kind: "loading" });
const [files, setFiles] = createStore<Record<string, OpenFile>>({});
const [active, setActive] = createSignal<string | null>(null);
const [pendingOpen, setPendingOpen] = createSignal<string | null>(null);
const [issues, setIssues] = createSignal<ConfigIssue[]>([]);
let loadedLab: string | null = null;

function dirtyPaths(): Set<string> {
  const dirty = new Set<string>();
  for (const [path, file] of Object.entries(files)) {
    if (file && !file.binary && !file.tooLarge && file.content !== file.savedContent) {
      dirty.add(path);
    }
  }
  return dirty;
}

/** Unsaved-edit count, for the lab page's Files tab label. */
export function filesDirtyCount(): number {
  return dirtyPaths().size;
}

/** Ask FilesView to open (and load) a path — LabEditorView switches to the
 *  Files tab and calls this for "edit vmlab.wcl" affordances. */
export function openLabFile(path: string) {
  setPendingOpen(path);
}

/** Refresh the lab file tree (safe from anywhere; no-op without a lab). */
export async function reloadFilesTree() {
  const current = state.currentLab;
  if (!current) return;
  try {
    const result = await api.labFilesTree(current);
    if (state.currentLab !== current) return;
    setTree({ kind: "ok", entries: result.entries });
  } catch (cause) {
    if (state.currentLab !== current) return;
    setTree({ kind: "error", message: msg(cause) });
  }
}

/** A declared playbook folder is editable here only when it lies inside
 *  the lab dir — outside ones run fine but the web sandbox can't touch
 *  them (matching the backend's playbook_dir check). */
export const canEditPlaybook = (path: string) =>
  !path.startsWith("/") && !path.startsWith("../") && path !== "..";

/** "Edit playbook" affordances land here: scaffold the declared folder if
 *  it doesn't exist yet (idempotent), then open its playbook.wcl in the
 *  Files tab. */
export async function editPlaybook(path: string) {
  const lab = state.currentLab;
  if (!lab) return;
  try {
    await api.scaffoldPlaybook(lab, path);
  } catch (cause) {
    showToast(msg(cause), "danger");
    return;
  }
  showLab();
  setLabTab("files");
  openLabFile(`${path}/playbook.wcl`);
  void reloadFilesTree();
}

// --- config-weave package actions -------------------------------------------
// Buttons live on the file tree rows of declared playbook folders: Add +
// Update-all on the pkgs/ dir (Add falls back to the playbook root while
// pkgs/ doesn't exist yet), Remove on each package dir under pkgs/.

const [pkgBusy, setPkgBusy] = createSignal(false);

type PkgCtx =
  | { kind: "root"; playbook: string }
  | { kind: "pkgs"; playbook: string }
  | { kind: "pkg"; playbook: string; pkg: string };

function pkgContext(entry: PlaybookTreeEntry): PkgCtx | null {
  if (!entry.dir) return null;
  for (const decl of state.playbooks) {
    const root = decl.path;
    if (!canEditPlaybook(root)) continue;
    if (entry.path === root) {
      const t = tree();
      const hasPkgs = t.kind === "ok" && !!findEntry(t.entries, `${root}/pkgs`)?.dir;
      return hasPkgs ? null : { kind: "root", playbook: root };
    }
    if (entry.path === `${root}/pkgs`) return { kind: "pkgs", playbook: root };
    const prefix = `${root}/pkgs/`;
    if (entry.path.startsWith(prefix) && !entry.path.slice(prefix.length).includes("/")) {
      return { kind: "pkg", playbook: root, pkg: entry.path.slice(prefix.length) };
    }
  }
  return null;
}

/** After the CLI changed files under `<playbook>/pkgs/`: refresh the tree
 *  and drop clean open buffers there so they reload from disk. Dirty ones
 *  stay — their next save 409s into the existing stale banner. */
async function afterPkgChange(playbook: string) {
  await reloadFilesTree();
  const prefix = `${playbook}/pkgs/`;
  const current = active();
  batch(() => {
    for (const key of Object.keys(files)) {
      if (!key.startsWith(prefix)) continue;
      const file = files[key];
      if (!file || file.content !== file.savedContent) continue;
      setFiles(key, undefined as unknown as OpenFile);
    }
  });
  if (current?.startsWith(prefix) && !files[current]) {
    const t = tree();
    if (t.kind === "ok" && findEntry(t.entries, current)) setPendingOpen(current);
    else setActive(null);
  }
}

async function runPkgAction(
  playbook: string,
  action: "remove" | "update",
  pkg: string | undefined,
  doneToast: string,
) {
  const lab = state.currentLab;
  if (!lab) return;
  setPkgBusy(true);
  try {
    await api.pkgAction(lab, playbook, action, pkg);
    showToast(doneToast);
    await afterPkgChange(playbook);
  } catch (cause) {
    showToast(msg(cause), "danger");
  } finally {
    setPkgBusy(false);
  }
}

async function removePkg(playbook: string, pkg: string) {
  const prefix = `${playbook}/pkgs/${pkg}/`;
  const dirtyUnder = [...dirtyPaths()].filter((p) => p.startsWith(prefix)).length;
  const ok = await confirmDialog({
    title: `Remove package ${pkg}?`,
    body: [
      `${playbook}/pkgs/${pkg} and its repo.wcl entry will be deleted.`,
      dirtyUnder ? `${dirtyUnder} open file(s) with unsaved edits will be discarded.` : "",
    ]
      .filter(Boolean)
      .join(" "),
    confirmLabel: "Remove",
    danger: true,
  });
  if (!ok) return;
  await runPkgAction(playbook, "remove", pkg, `Removed package ${pkg}`);
}

async function updateAllPkgs(playbook: string) {
  const prefix = `${playbook}/pkgs/`;
  const dirtyUnder = [...dirtyPaths()].filter((p) => p.startsWith(prefix)).length;
  const ok = await confirmDialog({
    title: "Update all packages?",
    body: [
      "config-weave re-syncs the repos and re-copies every installed package — local edits inside package folders are lost.",
      dirtyUnder ? `${dirtyUnder} open file(s) with unsaved edits under pkgs/ will go stale.` : "",
    ]
      .filter(Boolean)
      .join(" "),
    confirmLabel: "Update all",
    danger: true,
  });
  if (!ok) return;
  await runPkgAction(playbook, "update", undefined, "Packages updated");
}

registerNavGuard(async () => {
  if (dirtyPaths().size === 0) return true;
  return confirmDialog({
    title: "Discard unsaved file changes?",
    body: `${dirtyPaths().size} file(s) in the Files tab have edits that have not been saved.`,
    confirmLabel: "Discard",
    danger: true,
  });
});

// A hard refresh mid-edit would silently lose work.
window.addEventListener("beforeunload", (event) => {
  if (dirtyPaths().size > 0) event.preventDefault();
});

function msg(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function fmtSize(bytes: number | undefined): string {
  if (bytes == null) return "";
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KiB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

function findEntry(entries: PlaybookTreeEntry[], path: string): PlaybookTreeEntry | null {
  for (const entry of entries) {
    if (entry.path === path) return entry;
    if (entry.dir && path.startsWith(`${entry.path}/`)) {
      return findEntry(entry.children ?? [], path);
    }
  }
  return null;
}

/** Client-side mirror of the backend's path rules, for early feedback. */
function cleanPath(raw: string): string | null {
  const clean = raw.trim().replace(/^\/+/, "").replace(/\/+$/, "");
  if (!clean) return null;
  const parts = clean.split("/");
  if (parts.some((part) => !part || part === ".." || part.startsWith("."))) return null;
  return clean;
}

export default function FilesView() {
  const lab = () => state.currentLab!;
  const [loadingFile, setLoadingFile] = createSignal(false);
  const [busy, setBusy] = createSignal<string | null>(null);

  const activeFile = () => (active() ? files[active()!] : undefined);
  const activeIsConfig = () => active() === LAB_FILE;
  const configReadOnly = () => activeIsConfig() && anyVmRunning();

  // Issues → whole-line lint annotations (the huge col clamps to line end).
  const annotations = (): CodeAnnotation[] =>
    activeIsConfig()
      ? issues()
          .filter((i) => i.line != null)
          .map((i) => ({
            from: { line: i.line!, col: 0 },
            to: { line: i.line!, col: 100000 },
            severity: "error" as const,
            message: i.message,
            source: "wcl",
          }))
      : [];

  // Reset the module state when a different lab loads.
  createEffect(() => {
    const current = state.currentLab;
    if (!current || current === loadedLab) return;
    loadedLab = current;
    batch(() => {
      setActive(null);
      setIssues([]);
      for (const key of Object.keys(files)) {
        setFiles(key, undefined as unknown as OpenFile);
      }
      setTree({ kind: "loading" });
    });
    void reloadFilesTree();
  });

  // Deferred opens from openLabFile (Edit-config affordances).
  createEffect(() => {
    const want = pendingOpen();
    if (!want || !state.currentLab) return;
    setPendingOpen(null);
    void openFile(want);
  });

  async function openFile(path: string) {
    setActive(path);
    if (files[path]) return;
    setLoadingFile(true);
    try {
      const doc = await api.getLabFile(lab(), path);
      if (doc.binary || doc.tooLarge) {
        setFiles(path, {
          content: "",
          savedContent: "",
          rev: null,
          stale: false,
          binary: !!doc.binary,
          tooLarge: !!doc.tooLarge,
          size: doc.size,
        });
      } else {
        const content = doc.content ?? "";
        setFiles(path, { content, savedContent: content, rev: doc.rev ?? null, stale: false });
      }
    } catch (cause) {
      showToast(msg(cause), "danger");
      setActive(null);
    } finally {
      setLoadingFile(false);
    }
  }

  /** vmlab.wcl saves go through the validating config endpoint. */
  async function saveConfigFile(file: OpenFile): Promise<boolean> {
    try {
      await api.saveConfig(lab(), file.content);
      setIssues([]);
      setFiles(
        LAB_FILE,
        produce((f: OpenFile) => {
          f.savedContent = f.content;
          f.stale = false;
        }),
      );
      // Refresh the rev quietly; the designer resyncs unless it has a draft.
      void api
        .getLabFile(lab(), LAB_FILE)
        .then((doc) => doc.rev && setFiles(LAB_FILE, "rev", doc.rev))
        .catch(() => {});
      if (!editorDirty()) void reloadModel();
      return true;
    } catch (cause) {
      if (cause instanceof api.ValidationError) {
        setIssues(cause.issues);
        showToast(`${cause.issues.length} validation issue(s)`, "danger");
      } else {
        showToast(msg(cause), "danger");
      }
      return false;
    }
  }

  async function saveOne(path: string): Promise<boolean> {
    const file = files[path];
    if (!file || file.binary || file.tooLarge) return true;
    if (path === LAB_FILE) return saveConfigFile(file);
    try {
      const rev = await api.saveLabFile(lab(), path, file.content, file.rev);
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
        showToast(msg(cause), "danger");
      }
      return false;
    }
  }

  async function saveActive() {
    const path = active();
    if (!path) return;
    setBusy("save");
    try {
      if (await saveOne(path)) showToast(`Saved ${path}`);
    } finally {
      setBusy(null);
    }
  }

  async function saveAll() {
    setBusy("save");
    try {
      for (const path of dirtyPaths()) {
        if (!(await saveOne(path))) return;
      }
      showToast("Saved all files");
    } finally {
      setBusy(null);
    }
  }

  async function validateConfigNow() {
    const file = files[LAB_FILE];
    if (!file) return;
    setBusy("validate");
    try {
      await api.validateConfig(lab(), file.content);
      setIssues([]);
      showToast("Config is valid");
    } catch (cause) {
      if (cause instanceof api.ValidationError) {
        setIssues(cause.issues);
        showToast(`${cause.issues.length} validation issue(s)`, "danger");
      } else {
        showToast(msg(cause), "danger");
      }
    } finally {
      setBusy(null);
    }
  }

  async function saveConfigAndReload() {
    setBusy("reload");
    try {
      if (!(await saveOne(LAB_FILE))) return;
      showToast("Saved — reloading lab…", "info");
      await reloadCurrentLab();
      showToast("Lab reloaded");
    } catch (cause) {
      // A 409 (machines still running) or daemon error surfaces here.
      showToast(`Reload failed: ${msg(cause)}`, "danger");
    } finally {
      setBusy(null);
    }
  }

  async function reloadFromDisk(path: string) {
    setFiles(path, undefined as unknown as OpenFile);
    await openFile(path);
  }

  async function newFile(dirPath: string) {
    const path = await promptDialog({
      title: "New file",
      label: "Path inside the lab folder",
      initial: dirPath ? `${dirPath}/` : "",
      confirmLabel: "Create",
    });
    if (!path) return;
    const clean = cleanPath(path);
    if (!clean) {
      showToast("The path must be relative, without .. or dot-prefixed parts", "danger");
      return;
    }
    if (clean === LAB_FILE) {
      showToast("vmlab.wcl already exists", "danger");
      return;
    }
    try {
      // Created empty on disk right away (noclobber), so the tree shows it
      // and revision tracking starts from a real file.
      const rev = await api.saveLabFile(lab(), clean, "", null);
      setFiles(clean, { content: "", savedContent: "", rev, stale: false });
      setActive(clean);
      await reloadFilesTree();
    } catch (cause) {
      showToast(msg(cause), "danger");
    }
  }

  async function newFolder(dirPath: string) {
    const path = await promptDialog({
      title: "New folder",
      label: "Folder path inside the lab folder",
      initial: dirPath ? `${dirPath}/` : "",
      confirmLabel: "Create",
    });
    if (!path) return;
    const clean = cleanPath(path);
    if (!clean) {
      showToast("The path must be relative, without .. or dot-prefixed parts", "danger");
      return;
    }
    try {
      await api.mkdirLab(lab(), clean);
      await reloadFilesTree();
    } catch (cause) {
      showToast(msg(cause), "danger");
    }
  }

  async function renamePath(path: string) {
    const to = await promptDialog({
      title: `Rename ${path}`,
      label: "New path (moving between folders is fine)",
      initial: path,
      confirmLabel: "Rename",
    });
    if (!to || to === path) return;
    const clean = cleanPath(to);
    if (!clean) {
      showToast("The path must be relative, without .. or dot-prefixed parts", "danger");
      return;
    }
    try {
      await api.renameLabPath(lab(), path, clean);
      // Open buffers (dirty edits included) follow the rename.
      batch(() => {
        const prefix = `${path}/`;
        for (const key of Object.keys(files)) {
          const next =
            key === path ? clean : key.startsWith(prefix) ? clean + key.slice(path.length) : null;
          if (next) {
            setFiles(next, { ...files[key] });
            setFiles(key, undefined as unknown as OpenFile);
          }
        }
        const current = active();
        if (current === path) setActive(clean);
        else if (current?.startsWith(prefix)) setActive(clean + current.slice(path.length));
      });
      await reloadFilesTree();
    } catch (cause) {
      showToast(msg(cause), "danger");
    }
  }

  async function deletePath(path: string) {
    const entry =
      tree().kind === "ok" ? findEntry((tree() as { entries: PlaybookTreeEntry[] }).entries, path) : null;
    const isDir = entry?.dir ?? false;
    const prefix = `${path}/`;
    const dirtyUnder = [...dirtyPaths()].filter((p) => p === path || p.startsWith(prefix)).length;
    const ok = await confirmDialog({
      title: `Delete ${path}?`,
      body: [
        isDir ? "The folder and everything in it will be deleted." : "The file will be deleted.",
        dirtyUnder ? `${dirtyUnder} open file(s) with unsaved edits will be discarded.` : "",
      ]
        .filter(Boolean)
        .join(" "),
      confirmLabel: "Delete",
      danger: true,
    });
    if (!ok) return;
    try {
      await api.deleteLabPath(lab(), path, isDir);
      batch(() => {
        for (const key of Object.keys(files)) {
          if (key === path || key.startsWith(prefix)) {
            setFiles(key, undefined as unknown as OpenFile);
          }
        }
        const current = active();
        if (current && (current === path || current.startsWith(prefix))) setActive(null);
      });
      await reloadFilesTree();
    } catch (cause) {
      showToast(msg(cause), "danger");
    }
  }

  const language = () => {
    const ext = extOf(active() ?? "");
    if (ext === "wcl") return wclLanguage;
    if (ext === "ws" || ext === "wscript") return wscriptLanguage;
    return undefined;
  };

  /** The declared playbook root whose playbook.wcl is active — shows the
   *  Repos… toolbar button. */
  const activePlaybookRoot = () => {
    const current = active();
    if (!current?.endsWith("/playbook.wcl")) return null;
    const root = current.slice(0, -"/playbook.wcl".length);
    const declared = state.playbooks.some((p) => p.path === root);
    return declared && canEditPlaybook(root) ? root : null;
  };

  const pkgRowActions = (entry: PlaybookTreeEntry) => {
    const ctx = pkgContext(entry);
    if (!ctx) return null;
    if (ctx.kind === "pkg") {
      return (
        <span class="file-tree-actions">
          <IconButton
            icon={PackageMinus}
            label={`Remove package ${ctx.pkg}`}
            disabled={pkgBusy()}
            onClick={() => void removePkg(ctx.playbook, ctx.pkg)}
          />
        </span>
      );
    }
    return (
      <span class="file-tree-actions">
        <IconButton
          icon={PackagePlus}
          label={`Add package to ${ctx.playbook}`}
          disabled={pkgBusy()}
          onClick={() => openPkgSearch(ctx.playbook, () => void afterPkgChange(ctx.playbook))}
        />
        <Show when={ctx.kind === "pkgs"}>
          <IconButton
            icon={RefreshCw}
            label="Update all packages"
            disabled={pkgBusy()}
            onClick={() => void updateAllPkgs(ctx.playbook)}
          />
        </Show>
      </span>
    );
  };

  return (
    <div class="stack">
      <div class="config-modal-head">
        <span class="config-modal-path">
          {active() ?? "no file selected"}
          {active() && dirtyPaths().has(active()!) ? " · unsaved changes" : ""}
        </span>
        <div class="config-modal-actions">
          <Show when={activeIsConfig()}>
            <Button
              size="sm"
              onClick={() => void validateConfigNow()}
              disabled={busy() !== null || configReadOnly()}
              title={
                configReadOnly()
                  ? "Stop all machines before editing config"
                  : "Validate without saving"
              }
            >
              Validate
            </Button>
            <Button
              size="sm"
              variant="primary"
              onClick={() => void saveConfigAndReload()}
              disabled={busy() !== null || anyVmRunning()}
              title={
                anyVmRunning()
                  ? "Stop all VMs and containers before reloading"
                  : "Save and restart the lab to apply changes"
              }
            >
              Save & reload
            </Button>
          </Show>
          <Show when={activePlaybookRoot()}>
            {(root) => (
              <Button
                size="sm"
                icon={Library}
                onClick={() => openPkgRepos(root(), () => void afterPkgChange(root()))}
                title="Manage the playbook's package repositories"
              >
                Repos…
              </Button>
            )}
          </Show>
          <Button size="sm" icon={FilePlus2} onClick={() => void newFile("")}>
            New file
          </Button>
          <Button size="sm" icon={FolderPlus} onClick={() => void newFolder("")}>
            New folder
          </Button>
          <Button
            size="sm"
            icon={Save}
            variant={activeIsConfig() ? undefined : "primary"}
            disabled={busy() !== null || !active() || !dirtyPaths().has(active()!)}
            onClick={() => void saveActive()}
          >
            {busy() === "save" ? "Saving…" : "Save"}
          </Button>
          <Show when={dirtyPaths().size > 1}>
            <Button size="sm" icon={Save} disabled={busy() !== null} onClick={() => void saveAll()}>
              Save all ({dirtyPaths().size})
            </Button>
          </Show>
        </div>
      </div>

      <Show when={tree().kind === "loading"}>
        <div class="editor-loading">
          <Spinner /> loading files…
        </div>
      </Show>

      <Show when={tree().kind === "error"}>
        <Alert tone="danger">{(tree() as { message: string }).message}</Alert>
      </Show>

      <Show when={tree().kind === "ok"}>
        <SplitPane
          class="pb-editor-split"
          first={
            <FileTree
              nodes={(tree() as { entries: PlaybookTreeEntry[] }).entries}
              selected={active()}
              dirty={dirtyPaths()}
              editable={() => true}
              onSelect={(path) => void openFile(path)}
              onNewFile={(dir) => void newFile(dir)}
              onNewFolder={(dir) => void newFolder(dir)}
              onRename={(path) => void renamePath(path)}
              onDelete={(path) => void deletePath(path)}
              canMutate={(path) => path !== LAB_FILE}
              rowActions={pkgRowActions}
            />
          }
          second={
            <div class="pb-editor-pane">
              <Show
                when={active()}
                fallback={
                  <Empty title="Pick a file">
                    Select a file from the tree — right-click for rename, delete, and new
                    file/folder.
                  </Empty>
                }
              >
                <Show
                  when={!loadingFile()}
                  fallback={
                    <div class="editor-loading">
                      <Spinner /> loading file…
                    </div>
                  }
                >
                  <Show
                    when={!(activeFile()?.binary || activeFile()?.tooLarge)}
                    fallback={
                      <Empty
                        title={
                          activeFile()?.tooLarge
                            ? "Too large to edit here"
                            : "Binary file"
                        }
                      >
                        {active()} · {fmtSize(activeFile()?.size)}
                      </Empty>
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
                        // may touch the store.
                        const path = active();
                        if (path && files[path] && files[path].content !== value) {
                          setFiles(path, "content", value);
                        }
                      }}
                      readOnly={configReadOnly()}
                      language={language()}
                      annotations={annotations()}
                      height="max(calc(100vh - 380px), 320px)"
                    />
                    <Show when={configReadOnly()}>
                      <Alert tone="warning">
                        Config is read-only while any VM or container is up. Stop all machines to
                        edit it.
                      </Alert>
                    </Show>
                    <Show when={activeIsConfig() && issues().length}>
                      <Alert tone="danger" title={`${issues().length} validation issue(s)`}>
                        <For each={issues()}>
                          {(i) => (
                            <div class="cfg-issue">
                              <span class="cfg-issue-line">
                                {i.line != null ? `line ${i.line}` : ""}
                              </span>
                              <span>{i.message}</span>
                            </div>
                          )}
                        </For>
                      </Alert>
                    </Show>
                  </Show>
                </Show>
              </Show>
            </div>
          }
          initial={260}
          min={180}
        />
      </Show>
    </div>
  );
}
