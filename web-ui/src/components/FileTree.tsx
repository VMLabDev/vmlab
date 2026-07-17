// A small recursive file tree for folder editors (playbooks). Server
// supplies the nested entries (dirs first); this renders expand/collapse,
// selection, dirty markers, and an optional per-directory "new file"
// affordance. Non-editable files render muted and unselectable.

import { For, Show, createSignal } from "solid-js";
import { Icon, IconButton } from "@forge/ui";
import { File, Folder, FolderOpen, Plus } from "lucide-solid";
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
}

export default function FileTree(props: FileTreeProps) {
  // Expanded dirs; everything starts open (playbook trees are small).
  const [collapsed, setCollapsed] = createSignal<Set<string>>(new Set());
  const toggle = (path: string) => {
    const next = new Set(collapsed());
    if (next.has(path)) next.delete(path);
    else next.add(path);
    setCollapsed(next);
  };

  const Row = (p: { entry: PlaybookTreeEntry; depth: number }) => {
    const indent = () => `${p.depth * 14 + 6}px`;
    if (p.entry.dir) {
      const open = () => !collapsed().has(p.entry.path);
      return (
        <>
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
          </div>
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
    );
  };

  return (
    <div class="file-tree">
      <Show when={props.onNewFile}>
        <div class="file-tree-row file-tree-root">
          <span>files</span>
          <IconButton icon={Plus} label="New file" onClick={() => props.onNewFile!("")} />
        </div>
      </Show>
      <For each={props.nodes}>{(entry) => <Row entry={entry} depth={0} />}</For>
    </div>
  );
}
