import { Show, onMount } from "solid-js";
import { AppShell, Empty, Toaster } from "@forge/ui";
import { state, init } from "./store";
import { Dialogs } from "./components/dialogs";
import Login from "./components/Login";
import Topbar from "./components/Topbar";
import SidebarNav from "./components/SidebarNav";
import LabView from "./components/LabView";
import NetworkView from "./components/NetworkView";
import LogsView from "./components/LogsView";
import TemplatesView from "./components/TemplatesView";
import MachineView from "./components/MachineView";
import LabEditorView from "./components/editor/LabEditorView";
import NewLabModal from "./components/NewLabModal";

export default function App() {
  onMount(init);
  return (
    <Show
      when={state.ready}
      fallback={
        <div class="login-wrap">
          <Empty title="loading…" />
        </div>
      }
    >
      <Show when={state.loggedIn} fallback={<Login />}>
        <AppShell topbar={<Topbar />} sidebar={<SidebarNav />}>
          <Show when={state.view.kind === "lab"}>
            <LabView />
          </Show>
          <Show when={state.view.kind === "network"}>
            <NetworkView />
          </Show>
          <Show when={state.view.kind === "logs"}>
            <LogsView />
          </Show>
          <Show when={state.view.kind === "templates"}>
            <TemplatesView />
          </Show>
          <Show when={state.view.kind === "vm"}>
            <MachineView />
          </Show>
          <Show when={state.view.kind === "editor"}>
            <LabEditorView />
          </Show>
        </AppShell>
      </Show>
      <Toaster />
      <Dialogs />
      <NewLabModal />
    </Show>
  );
}
