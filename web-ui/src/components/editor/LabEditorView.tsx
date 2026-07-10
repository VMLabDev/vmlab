// The lab page's tabbed body: the visual Design canvas, the raw Config
// (vmlab.wcl) editor, and the lab daemon's Logs — swapped at the top.
//
// Design and Config edit the same file through different doors, so
// swapping re-syncs: Config remounts (fresh disk read) every time it
// activates, and Design re-fetches its model when it isn't holding
// unsaved edits. Logs remounts too (fresh backlog from the stream).

import { Show, createSignal } from "solid-js";
import { Tabs } from "@forge/ui";
import { editorDirty, reloadModel } from "../../editor/store";
import { state } from "../../store";
import ConfigView from "../ConfigView";
import LogPanel from "../LogPanel";
import EditorView from "./EditorView";

const [tab, setTab] = createSignal<"design" | "config" | "logs">("design");

/** Jump to the raw-WCL tab (used when the model can't be opened visually). */
export function openConfigTab() {
  setTab("config");
}

export default function LabEditorView() {
  function onTab(id: string) {
    const next = id as "design" | "config" | "logs";
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
          { id: "logs", label: "Logs" },
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
      <Show when={tab() === "logs" && state.currentLab}>
        <LogPanel lab={state.currentLab!} source="lab" />
      </Show>
    </div>
  );
}
