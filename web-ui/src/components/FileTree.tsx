// A small recursive file tree for folder editors (playbooks, the lab Files
// tab). Server supplies the nested entries (dirs first); this renders
// expand/collapse, selection, dirty markers, an optional per-directory
// "new file" affordance, and — when mutation handlers are provided — a
// right-click context menu with new/rename/delete. Non-editable files
// render muted and unselectable.

import { For, Show, createSignal } from "solid-js";
import type { JSX } from "solid-js";
import { ContextMenu, Icon, IconButton } from "@forge/ui";
import type { MenuItem } from "@forge/ui";
import { File, Folder, FolderOpen, FolderPlus, Plus } from "lucide-solid";
import type { PlaybookTreeEntry } from "../api";

export interface FileTreeProps {
  nodes: PlaybookTreeEntry[];
  /** Selected file path (relative, `/`-separated). */
  selected: string | null;
  /** Paths with unsaved edits — rendered with a dot marker. */
  dirty: ReadonlySet<string>;
  editable: (path: string) => boolean;
  onSelect: (path: string) => void;
  /** "+" on directory rows (and the root, via dirPath "") — new file. */
  onNewFile?: (dirPath: string) => void;
  /** The following enable the right-click context menu. */
  onNewFolder?: (dirPath: string) => void;
  onRename?: (path: string) => void;
  onDelete?: (path: string) => void;
  /** Paths the menu may not rename/delete (e.g. vmlab.wcl). */
  canMutate?: (path: string) => boolean;
  /** Extra per-row action buttons on directory rows (rendered after the
   *  "+" button) — e.g. the Files tab's package actions. */
  rowActions?: (entry: PlaybookTreeEntry) => JSX.Element | null;
}

export default function FileTree(props: FileTreeProps) {
  // Expanded dirs; everything starts open (lab trees are small).
  const [collapsed, setCollapsed] = createSignal<Set<string>>(new Set());
  const toggle = (path: string) => {
    const next = new Set(collapsed());
    if (next.has(path)) next.delete(path);
    else next.add(path);
    setCollapsed(next);
  };

  const hasMenu = () => !!(props.onNewFolder || props.onRename || props.onDelete);

  const menuFor = (entry: PlaybookTreeEntry): MenuItem[] => {
    const items: MenuItem[] = [];
    if (entry.dir) {
      if (props.onNewFile)
        items.push({ label: "New file…", onSelect: () => props.onNewFile!(entry.path) });
      if (props.onNewFolder)
        items.push({ label: "New folder…", onSelect: () => props.onNewFolder!(entry.path) });
    }
    if (props.canMutate?.(entry.path) ?? true) {
      if (items.length && (props.onRename || props.onDelete)) items.push({ separator: true });
      if (props.onRename)
        items.push({ label: "Rename…", onSelect: () => props.onRename!(entry.path) });
      if (props.onDelete)
        items.push({
          label: "Delete",
          danger: true,
          onSelect: () => props.onDelete!(entry.path),
        });
    }
    return items;
  };

  const MaybeMenu = (p: { entry: PlaybookTreeEntry; children: JSX.Element }) => (
    <Show when={hasMenu() && menuFor(p.entry).length} fallback={p.children}>
      <ContextMenu items={menuFor(p.entry)}>{p.children}</ContextMenu>
    </Show>
  );

  const Row = (p: { entry: PlaybookTreeEntry; depth: number }) => {
    const indent = () => `${p.depth * 14 + 6}px`;
    if (p.entry.dir) {
      const open = () => !collapsed().has(p.entry.path);
      return (
        <>
          <MaybeMenu entry={p.entry}>
            <div class="file-tree-row" style={{ "padding-left": indent() }}>
              <button class="file-tree-dir" onClick={() => toggle(p.entry.path)}>
                <Icon of={open() ? FolderOpen : Folder} size={14} />
                <span>{p.entry.name}</span>
              </button>
              <Show when={props.onNewFile}>
                <IconButton
                  icon={Plus}
                  label={`New file in ${p.entry.path}`}
                  onClick={() => props.onNewFile!(p.entry.path)}
                />
              </Show>
              {props.rowActions?.(p.entry)}
            </div>
          </MaybeMenu>
          <Show when={open()}>
            <For each={p.entry.children ?? []}>
              {(child) => <Row entry={child} depth={p.depth + 1} />}
            </For>
          </Show>
        </>
      );
    }
    const ok = () => props.editable(p.entry.path);
    return (
      <MaybeMenu entry={p.entry}>
        <div class="file-tree-row" style={{ "padding-left": indent() }}>
          <button
            class="file-tree-file"
            classList={{
              selected: props.selected === p.entry.path,
              muted: !ok(),
            }}
            disabled={!ok()}
            title={ok() ? p.entry.path : `${p.entry.path} — not editable here`}
            onClick={() => ok() && props.onSelect(p.entry.path)}
          >
            <Icon of={File} size={14} />
            <span>{p.entry.name}</span>
            <Show when={props.dirty.has(p.entry.path)}>
              <span class="file-tree-dirty" title="Unsaved changes" />
            </Show>
          </button>
        </div>
      </MaybeMenu>
    );
  };

  return (
    <div class="file-tree">
      <Show when={props.onNewFile}>
        <div class="file-tree-row file-tree-root">
          <span>files</span>
          <Show when={props.onNewFolder}>
            <IconButton
              icon={FolderPlus}
              label="New folder"
              onClick={() => props.onNewFolder!("")}
            />
          </Show>
          <IconButton icon={Plus} label="New file" onClick={() => props.onNewFile!("")} />
        </div>
      </Show>
      <For each={props.nodes}>{(entry) => <Row entry={entry} depth={0} />}</For>
    </div>
  );
}
