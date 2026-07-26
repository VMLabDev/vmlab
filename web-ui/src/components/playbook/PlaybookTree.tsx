// The playbook designer's left pane: playbook → gathers, vars, plays → steps
// and step folders, nested to any depth.
//
// Rows are HTML5 drag sources and drop targets. A drop lands *before*, *after*
// or *inside* the row under the pointer depending on where in its height the
// pointer sits — "inside" only for folders and plays, which are the two things
// that hold items.

import { For, Show, createSignal } from "solid-js";
import { Badge, IconButton } from "@forge/ui";
import {
  ChevronDown,
  ChevronRight,
  FolderPlus,
  Plus,
  ScrollText,
  Variable,
} from "lucide-solid";
import type { PlayItemDoc, PlaybookDoc } from "../../api";
import type { DropTarget, Sel } from "../../playbook/doc";
import { isAncestorPath, isContainer, sameSel, selId } from "../../playbook/doc";

/** Where a drag would land relative to the row under the pointer. */
type DropSpot = "before" | "after" | "inside";

interface DragSource {
  play: number;
  path: number[];
}

export interface PlaybookTreeProps {
  doc: PlaybookDoc;
  selection: Sel | null;
  /** Node id → the problems reported for it. */
  warnings: Map<string, string[]>;
  disabled?: boolean;
  onSelect: (sel: Sel) => void;
  onAddPlay: () => void;
  onAddGather: () => void;
  onAddVar: () => void;
  onAddStep: (play: number, parentPath: number[]) => void;
  onAddFolder: (play: number, parentPath: number[]) => void;
  onMoveItem: (from: DragSource, to: DropTarget) => void;
  onMovePlay: (from: number, to: number) => void;
}

export default function PlaybookTree(props: PlaybookTreeProps) {
  const [collapsed, setCollapsed] = createSignal<Set<string>>(new Set());
  const [drag, setDrag] = createSignal<DragSource | null>(null);
  const [over, setOver] = createSignal<{ id: string; spot: DropSpot } | null>(null);
  const [playDrag, setPlayDrag] = createSignal<number | null>(null);

  const isOpen = (id: string) => !collapsed().has(id);
  const toggle = (id: string) =>
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const warn = (sel: Sel) => props.warnings.get(selId(sel));

  /** Which third of the row the pointer is in; "inside" collapses to
   *  before/after for rows that can't hold children. */
  function spotFor(event: DragEvent, holdsItems: boolean): DropSpot {
    const box = (event.currentTarget as HTMLElement).getBoundingClientRect();
    const ratio = (event.clientY - box.top) / Math.max(1, box.height);
    if (!holdsItems) return ratio < 0.5 ? "before" : "after";
    if (ratio < 0.3) return "before";
    if (ratio > 0.7) return "after";
    return "inside";
  }

  function dropItem(play: number, path: number[], spot: DropSpot) {
    const from = drag();
    setDrag(null);
    setOver(null);
    if (!from) return;
    // A folder cannot be dropped into its own subtree.
    if (from.play === play && isAncestorPath(from.path, path)) return;
    const to: DropTarget =
      spot === "inside"
        ? { play, parentPath: path, index: 0 }
        : {
            play,
            parentPath: path.slice(0, -1),
            index: path[path.length - 1] + (spot === "after" ? 1 : 0),
          };
    props.onMoveItem(from, to);
  }

  function Item(itemProps: { item: PlayItemDoc; play: number; path: number[]; depth: number }) {
    const sel = (): Sel => ({ kind: "item", play: itemProps.play, path: itemProps.path });
    const id = () => selId(sel());
    const folder = () => isContainer(itemProps.item);
    const name = () =>
      isContainer(itemProps.item) ? itemProps.item.container.name : itemProps.item.step.name;
    const detail = () =>
      isContainer(itemProps.item) ? "" : (itemProps.item.step.resource ?? "");
    const dropState = () => (over()?.id === id() ? over()!.spot : null);

    return (
      <>
        <div
          class="pb-tree-row"
          classList={{
            selected: sameSel(props.selection, sel()),
            "pb-drop-before": dropState() === "before",
            "pb-drop-after": dropState() === "after",
            "pb-drop-inside": dropState() === "inside",
          }}
          style={{ "padding-left": `${itemProps.depth * 14 + 6}px` }}
          draggable={!props.disabled}
          onClick={() => props.onSelect(sel())}
          onDragStart={(e: DragEvent) => {
            e.stopPropagation();
            setDrag({ play: itemProps.play, path: itemProps.path });
            e.dataTransfer?.setData("text/plain", id());
            if (e.dataTransfer) e.dataTransfer.effectAllowed = "move";
          }}
          onDragEnd={() => {
            setDrag(null);
            setOver(null);
          }}
          onDragOver={(e: DragEvent) => {
            if (!drag()) return;
            e.preventDefault();
            e.stopPropagation();
            setOver({ id: id(), spot: spotFor(e, folder()) });
          }}
          onDragLeave={() => {
            if (over()?.id === id()) setOver(null);
          }}
          onDrop={(e: DragEvent) => {
            e.preventDefault();
            e.stopPropagation();
            dropItem(itemProps.play, itemProps.path, spotFor(e, folder()));
          }}
        >
          <Show
            when={folder()}
            fallback={<span class="pb-tree-bullet">{warn(sel()) ? "▲" : "•"}</span>}
          >
            <button
              class="pb-tree-caret"
              onClick={(e) => {
                e.stopPropagation();
                toggle(id());
              }}
            >
              {isOpen(id()) ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
            </button>
          </Show>
          <span class="pb-tree-name">{name() || "(unnamed)"}</span>
          <Show when={detail()}>
            <span class="pb-tree-detail">{detail()}</span>
          </Show>
          <Show when={warn(sel())}>
            <Badge tone="warning">{warn(sel())!.length}</Badge>
          </Show>
          <Show when={folder() && !props.disabled}>
            <span class="pb-tree-actions">
              <IconButton
                icon={Plus}
                label={`Add a step to ${name()}`}
                onClick={(e: MouseEvent) => {
                  e.stopPropagation();
                  props.onAddStep(itemProps.play, itemProps.path);
                }}
              />
              <IconButton
                icon={FolderPlus}
                label={`Add a folder to ${name()}`}
                onClick={(e: MouseEvent) => {
                  e.stopPropagation();
                  props.onAddFolder(itemProps.play, itemProps.path);
                }}
              />
            </span>
          </Show>
        </div>
        <Show when={isContainer(itemProps.item) && isOpen(id())}>
          <For each={(itemProps.item as { container: { items: PlayItemDoc[] } }).container.items}>
            {(child, index) => (
              <Item
                item={child}
                play={itemProps.play}
                path={[...itemProps.path, index()]}
                depth={itemProps.depth + 1}
              />
            )}
          </For>
        </Show>
      </>
    );
  }

  return (
    <div class="pb-tree">
      <div
        class="pb-tree-row pb-tree-head"
        classList={{ selected: props.selection?.kind === "playbook" }}
        onClick={() => props.onSelect({ kind: "playbook" })}
      >
        <ScrollText size={13} />
        <span class="pb-tree-name">{props.doc.name || "(unnamed playbook)"}</span>
      </div>

      <div class="pb-tree-group">
        <span class="pb-tree-group-label">Gatherers</span>
        <IconButton
          icon={Plus}
          label="Add a gatherer"
          disabled={props.disabled}
          onClick={() => props.onAddGather()}
        />
      </div>
      <For each={props.doc.gathers}>
        {(gather, index) => {
          const sel = (): Sel => ({ kind: "gather", index: index() });
          return (
            <div
              class="pb-tree-row"
              classList={{ selected: sameSel(props.selection, sel()) }}
              style={{ "padding-left": "20px" }}
              onClick={() => props.onSelect(sel())}
            >
              <span class="pb-tree-bullet">{warn(sel()) ? "▲" : "•"}</span>
              <span class="pb-tree-name">{gather.name || "(unnamed)"}</span>
              <span class="pb-tree-detail">{gather.from}</span>
            </div>
          );
        }}
      </For>

      <div class="pb-tree-group">
        <span class="pb-tree-group-label">Variables</span>
        <IconButton
          icon={Plus}
          label="Add a variable"
          disabled={props.disabled}
          onClick={() => props.onAddVar()}
        />
      </div>
      <For each={props.doc.vars}>
        {(entry, index) => {
          const sel = (): Sel => ({ kind: "var", index: index() });
          return (
            <div
              class="pb-tree-row"
              classList={{ selected: sameSel(props.selection, sel()) }}
              style={{ "padding-left": "20px" }}
              onClick={() => props.onSelect(sel())}
            >
              <Variable size={12} />
              <span class="pb-tree-name">{entry.key || "(unnamed)"}</span>
            </div>
          );
        }}
      </For>

      <div class="pb-tree-group">
        <span class="pb-tree-group-label">Plays</span>
        <IconButton
          icon={Plus}
          label="Add a play"
          disabled={props.disabled}
          onClick={() => props.onAddPlay()}
        />
      </div>
      <For each={props.doc.plays}>
        {(play, playIndex) => {
          const sel = (): Sel => ({ kind: "play", play: playIndex() });
          const id = () => selId(sel());
          const dropState = () => (over()?.id === id() ? over()!.spot : null);
          return (
            <>
              <div
                class="pb-tree-row pb-tree-play"
                classList={{
                  selected: sameSel(props.selection, sel()),
                  "pb-drop-before": dropState() === "before",
                  "pb-drop-after": dropState() === "after",
                  "pb-drop-inside": dropState() === "inside",
                }}
                draggable={!props.disabled}
                onClick={() => props.onSelect(sel())}
                onDragStart={(e: DragEvent) => {
                  setPlayDrag(playIndex());
                  e.dataTransfer?.setData("text/plain", id());
                }}
                onDragEnd={() => {
                  setPlayDrag(null);
                  setOver(null);
                }}
                onDragOver={(e: DragEvent) => {
                  if (!drag() && playDrag() === null) return;
                  e.preventDefault();
                  setOver({ id: id(), spot: drag() ? spotFor(e, true) : "before" });
                }}
                onDragLeave={() => {
                  if (over()?.id === id()) setOver(null);
                }}
                onDrop={(e: DragEvent) => {
                  e.preventDefault();
                  const from = drag();
                  const movingPlay = playDrag();
                  setOver(null);
                  if (from) {
                    setDrag(null);
                    props.onMoveItem(from, {
                      play: playIndex(),
                      parentPath: [],
                      index: from.play === playIndex() ? 0 : props.doc.plays[playIndex()].items.length,
                    });
                  } else if (movingPlay !== null && movingPlay !== playIndex()) {
                    setPlayDrag(null);
                    props.onMovePlay(movingPlay, playIndex());
                  }
                }}
              >
                <button
                  class="pb-tree-caret"
                  onClick={(e) => {
                    e.stopPropagation();
                    toggle(id());
                  }}
                >
                  {isOpen(id()) ? <ChevronDown size={12} /> : <ChevronRight size={12} />}
                </button>
                <span class="pb-tree-name">{play.name || "(unnamed play)"}</span>
                <Badge tone="neutral">{play.items.length}</Badge>
                <Show when={!props.disabled}>
                  <span class="pb-tree-actions">
                    <IconButton
                      icon={Plus}
                      label={`Add a step to ${play.name}`}
                      onClick={(e: MouseEvent) => {
                        e.stopPropagation();
                        props.onAddStep(playIndex(), []);
                      }}
                    />
                    <IconButton
                      icon={FolderPlus}
                      label={`Add a folder to ${play.name}`}
                      onClick={(e: MouseEvent) => {
                        e.stopPropagation();
                        props.onAddFolder(playIndex(), []);
                      }}
                    />
                  </span>
                </Show>
              </div>
              <Show when={isOpen(id())}>
                <For each={play.items}>
                  {(item, index) => (
                    <Item item={item} play={playIndex()} path={[index()]} depth={1} />
                  )}
                </For>
                <Show when={!play.items.length}>
                  <div class="pb-tree-empty" style={{ "padding-left": "20px" }}>
                    no steps yet
                  </div>
                </Show>
              </Show>
            </>
          );
        }}
      </For>
    </div>
  );
}
