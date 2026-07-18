// The lab page's tabbed body: the visual Overview, the Files tab (the whole
// lab folder, vmlab.wcl included), and daemon Logs. "Edit config"
// affordances land on the Files tab with vmlab.wcl opened.

import { Show, createSignal } from "solid-js";
import { Tabs } from "@forge/ui";
import { state } from "../../store";
import FilesView, { filesDirtyCount, openLabFile } from "../FilesView";
import LogPanel from "../LogPanel";
import EditorView from "./EditorView";

const [tab, setTab] = createSignal<"design" | "files" | "logs">("design");

export default function LabEditorView() {
  function onTab(id: string) {
    setTab(id as "design" | "files" | "logs");
  }

  function editConfig() {
    openLabFile("vmlab.wcl");
    setTab("files");
  }

  return (
    <div class="stack">
      <Tabs
        tabs={[
          { id: "design", label: "Overview" },
          { id: "files", label: filesDirtyCount() ? `Files (${filesDirtyCount()})` : "Files" },
          { id: "logs", label: "Logs" },
        ]}
        active={tab()}
        onChange={onTab}
      />
      <Show when={tab() === "design"}>
        <EditorView onEditConfig={editConfig} />
      </Show>
      <Show when={tab() === "files"}>
        <FilesView />
      </Show>
      <Show when={tab() === "logs" && state.currentLab}>
        <LogPanel lab={state.currentLab!} source="lab" />
      </Show>
    </div>
  );
}
