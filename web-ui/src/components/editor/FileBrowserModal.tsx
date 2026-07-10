// Server-side file picker: browses the host filesystem through
// GET /api/host/fs (auth-gated). Directories always navigate; only files
// matching `extensions` are selectable — everything else shows greyed for
// context.

import { For, Show, createEffect, createResource, createSignal } from "solid-js";
import { Button, Empty, Modal, Spinner } from "@forge/ui";
import { ArrowUp, File, Folder } from "lucide-solid";
import * as api from "../../api";
import type { FsEntry } from "../../api";

export interface FileBrowserModalProps {
  open: boolean;
  title: string;
  /** Absolute directory to open at (e.g. the lab root). */
  start: string;
  /** Selectable file extensions, lower-case with dot (e.g. [".iso"]). */
  extensions: string[];
  onClose: () => void;
  onPick: (absPath: string) => void;
}

function fmtSize(bytes: number | null): string {
  if (bytes == null) return "";
  const mb = bytes / (1024 * 1024);
  if (mb >= 1024) return `${(mb / 1024).toFixed(1)} GiB`;
  if (mb >= 1) return `${Math.round(mb)} MiB`;
  return `${Math.max(1, Math.round(bytes / 1024))} KiB`;
}

export default function FileBrowserModal(props: FileBrowserModalProps) {
  const [path, setPath] = createSignal(props.start);
  // Re-open starts back at the caller's directory.
  createEffect(() => {
    if (props.open) setPath(props.start);
  });

  const [listing] = createResource(
    () => (props.open ? path() : null),
    (p) => api.browseFs(p),
  );

  const join = (name: string) => {
    const base = listing()?.path ?? path();
    return base === "/" ? `/${name}` : `${base}/${name}`;
  };
  const selectable = (e: FsEntry) =>
    !e.dir && props.extensions.some((ext) => e.name.toLowerCase().endsWith(ext));

  return (
    <Modal
      open={props.open}
      onClose={props.onClose}
      title={props.title}
      footer={
        <Button variant="ghost" onClick={props.onClose}>
          Cancel
        </Button>
      }
    >
      <div class="fs-browser">
        <div class="fs-crumb">
          <Button
            size="sm"
            variant="ghost"
            icon={ArrowUp}
            disabled={!listing()?.parent}
            onClick={() => {
              const parent = listing()?.parent;
              if (parent) setPath(parent);
            }}
          >
            Up
          </Button>
          <span class="fs-path">{listing()?.path ?? path()}</span>
        </div>
        <Show
          when={!listing.loading}
          fallback={
            <div class="fs-loading">
              <Spinner label="Listing directory" /> loading…
            </div>
          }
        >
          <Show
            when={!listing.error}
            fallback={<Empty title="Can't open this directory">{String(listing.error)}</Empty>}
          >
            <Show
              when={(listing()?.entries.length ?? 0) > 0}
              fallback={<Empty title="Empty directory" />}
            >
              <div class="fs-list">
                <For each={listing()?.entries ?? []}>
                  {(e) => (
                    <div
                      class="fs-row"
                      classList={{ pickable: e.dir || selectable(e), muted: !e.dir && !selectable(e) }}
                      onClick={() => {
                        if (e.dir) setPath(join(e.name));
                        else if (selectable(e)) props.onPick(join(e.name));
                      }}
                    >
                      {e.dir ? <Folder size={14} /> : <File size={14} />}
                      <span class="fs-name">{e.name}</span>
                      <span class="fs-size">{e.dir ? "" : fmtSize(e.size)}</span>
                    </div>
                  )}
                </For>
              </div>
            </Show>
          </Show>
        </Show>
      </div>
    </Modal>
  );
}
