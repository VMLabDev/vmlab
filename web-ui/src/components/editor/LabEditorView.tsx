// The lab page's tabbed body: the visual Overview, the Files tab (the whole
// lab folder, vmlab.wcl included), and daemon Logs. "Edit config"
// affordances land on the Files tab with vmlab.wcl opened.

import { Show } from "solid-js";
import { Tabs } from "@forge/ui";
import { state } from "../../store";
import FilesView, { filesDirtyCount, openLabFile } from "../FilesView";
import LogPanel from "../LogPanel";
import EditorView from "./EditorView";
import { labTab, setLabTab } from "./labTab";
import type { LabTab } from "./labTab";

export default function LabEditorView() {
  function onTab(id: string) {
    setLabTab(id as LabTab);
  }

  function editConfig() {
    openLabFile("vmlab.wcl");
    setLabTab("files");
  }

  return (
    <div class="stack">
      <Tabs
        tabs={[
          { id: "design", label: "Overview" },
          { id: "files", label: filesDirtyCount() ? `Files (${filesDirtyCount()})` : "Files" },
          { id: "logs", label: "Logs" },
        ]}
        active={labTab()}
        onChange={onTab}
      />
      <Show when={labTab() === "design"}>
        <EditorView onEditConfig={editConfig} />
      </Show>
      <Show when={labTab() === "files"}>
        <FilesView />
      </Show>
      <Show when={labTab() === "logs" && state.currentLab}>
        <LogPanel lab={state.currentLab!} source="lab" />
      </Show>
    </div>
  );
}
