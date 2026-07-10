// The combined lab-editing page: one sidebar entry, two tabs — the visual
// Design canvas and the raw Config (vmlab.wcl) editor — swapped at the top.
//
// The two tabs edit the same file through different doors, so swapping
// re-syncs: Config remounts (fresh disk read) every time it activates, and
// Design re-fetches its model when it isn't holding unsaved edits.

import { Show, createSignal } from "solid-js";
import { Tabs } from "@forge/ui";
import { editorDirty, reloadModel } from "../../editor/store";
import ConfigView from "../ConfigView";
import EditorView from "./EditorView";

const [tab, setTab] = createSignal<"design" | "config">("design");

/** Jump to the raw-WCL tab (used when the model can't be opened visually). */
export function openConfigTab() {
  setTab("config");
}

export default function LabEditorView() {
  function onTab(id: string) {
    const next = id as "design" | "config";
    if (next === "design" && tab() !== "design" && !editorDirty()) {
      // Pick up whatever the raw editor (or anyone else) wrote to disk.
      void reloadModel();
    }
    setTab(next);
  }

  return (
    <div class="stack">
      <Tabs
        tabs={[
          { id: "design", label: "Design" },
          { id: "config", label: "Config" },
        ]}
        active={tab()}
        onChange={onTab}
      />
      <Show when={tab() === "design"}>
        <EditorView />
      </Show>
      <Show when={tab() === "config"}>
        <ConfigView />
      </Show>
    </div>
  );
}
