import { Show, onMount } from "solid-js";
import { state, init } from "./store";
import Login from "./components/Login";
import Sidebar from "./components/Sidebar";
import LabView from "./components/LabView";
import NetworkView from "./components/NetworkView";
import LogsView from "./components/LogsView";
import ConfigView from "./components/ConfigView";
import MachineView from "./components/MachineView";
import Toast from "./components/Toast";

export default function App() {
  onMount(init);
  return (
    <Show
      when={state.ready}
      fallback={
        <div class="offscreen" style="height:100vh">
          <div class="offt">loading…</div>
        </div>
      }
    >
      <Show when={state.loggedIn} fallback={<Login />}>
        <div class="app">
          <Sidebar />
          <main class="content">
            <Show when={state.view.kind === "lab"}>
              <LabView />
            </Show>
            <Show when={state.view.kind === "network"}>
              <NetworkView />
            </Show>
            <Show when={state.view.kind === "logs"}>
              <LogsView />
            </Show>
            <Show when={state.view.kind === "config"}>
              <ConfigView />
            </Show>
            <Show when={state.view.kind === "vm"}>
              <MachineView />
            </Show>
          </main>
        </div>
      </Show>
      <Toast />
    </Show>
  );
}
