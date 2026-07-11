// The lab page's tabbed body: the visual Overview and daemon Logs. Raw
// vmlab.wcl editing opens from the lab card in a modal so it remains close
// to the object it configures.

import { Show, createSignal } from "solid-js";
import { Modal, Tabs } from "@forge/ui";
import { editorDirty, reloadModel } from "../../editor/store";
import { state } from "../../store";
import ConfigView from "../ConfigView";
import LogPanel from "../LogPanel";
import EditorView from "./EditorView";

const [tab, setTab] = createSignal<"design" | "logs">("design");

export default function LabEditorView() {
  const [configOpen, setConfigOpen] = createSignal(false);

  function onTab(id: string) {
    setTab(id as "design" | "logs");
  }

  function closeConfig() {
    setConfigOpen(false);
    if (!editorDirty()) void reloadModel();
  }

  return (
    <div class="stack">
      <Tabs
        tabs={[
          { id: "design", label: "Overview" },
          { id: "logs", label: "Logs" },
        ]}
        active={tab()}
        onChange={onTab}
      />
      <Show when={tab() === "design"}>
        <EditorView onEditConfig={() => setConfigOpen(true)} />
      </Show>
      <Show when={tab() === "logs" && state.currentLab}>
        <LogPanel lab={state.currentLab!} source="lab" />
      </Show>
      <Show when={configOpen()}>
        <Modal open title="Edit vmlab.wcl" onClose={closeConfig}>
          <ConfigView />
        </Modal>
      </Show>
    </div>
  );
}
