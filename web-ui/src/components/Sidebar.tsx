import { For, Show, createSignal } from "solid-js";
import {
  state,
  selectLab,
  showLab,
  showNetwork,
  showLogs,
  showConfig,
  showTemplates,
  showVm,
  doLogout,
  look,
  osOf,
  archOf,
} from "../store";
import { Chevron, Check, Grid, Network, Logs, Code, Box } from "./icons";

export default function Sidebar() {
  const [menu, setMenu] = createSignal(false);
  const cur = () => state.status;
  const subnet = () => cur()?.segments?.[0]?.subnet ?? "";

  return (
    <aside class="side">
      <div class="sidehd">
        <a
          class="brand"
          href="#"
          onClick={(e) => {
            e.preventDefault();
            showLab();
          }}
        >
          <span>
            <span class="bvm">vm</span>
            <span class="blab">lab</span>
          </span>
        </a>
        <button class="labswitch" onClick={() => setMenu(!menu())}>
          <span class="sdot" style="background:var(--success-fg)" />
          <span class="ln">{state.currentLab ?? "no lab"}</span>
          <span class="lh">{subnet()}</span>
          <span class="lchev">
            <Chevron />
          </span>
        </button>
        <Show when={menu()}>
          <div class="labmenu">
            <div class="labmenuhd">Switch lab</div>
            <For each={state.labs}>
              {(l) => (
                <button
                  class="labopt"
                  classList={{ on: l.name === state.currentLab }}
                  onClick={() => {
                    setMenu(false);
                    selectLab(l.name);
                  }}
                >
                  <div class="laboptmeta">
                    <span class="laboptname">{l.name}</span>
                    <span class="laboptsub">{l.state ?? ""}</span>
                  </div>
                  <span class="laboptcheck">
                    <Check />
                  </span>
                </button>
              )}
            </For>
          </div>
        </Show>
      </div>

      <div class="navscroll">
        <div class="navsec">Lab</div>
        <button
          class="navitem"
          classList={{ on: state.view.kind === "lab" }}
          onClick={showLab}
        >
          <span class="niic">
            <Grid />
          </span>
          <span class="nitext">
            <span class="niname">{state.currentLab ?? "—"}</span>
            <span class="nimeta">{cur() ? `${cur()!.vms.length} machines` : ""}</span>
          </span>
        </button>
        <button
          class="navitem"
          classList={{ on: state.view.kind === "network" }}
          onClick={showNetwork}
        >
          <span class="niic">
            <Network />
          </span>
          <span class="nitext">
            <span class="niname">network</span>
            <span class="nimeta">
              {cur() ? `${cur()!.segments.length} segments` : ""}
            </span>
          </span>
        </button>
        <button
          class="navitem"
          classList={{ on: state.view.kind === "logs" }}
          onClick={showLogs}
        >
          <span class="niic">
            <Logs />
          </span>
          <span class="nitext">
            <span class="niname">logs</span>
            <span class="nimeta">live stream</span>
          </span>
        </button>
        <button
          class="navitem"
          classList={{ on: state.view.kind === "config" }}
          onClick={showConfig}
        >
          <span class="niic">
            <Code />
          </span>
          <span class="nitext">
            <span class="niname">config</span>
            <span class="nimeta">vmlab.wcl</span>
          </span>
        </button>

        <Show when={state.templates.length > 0}>
          <button
            class="navitem"
            classList={{ on: state.view.kind === "templates" }}
            onClick={showTemplates}
          >
            <span class="niic">
              <Box />
            </span>
            <span class="nitext">
              <span class="niname">templates</span>
              <span class="nimeta">{state.templates.length} defined</span>
            </span>
          </button>
        </Show>

        <div class="navsec">Machines</div>
        <For each={cur()?.vms ?? []}>
          {(vm) => {
            const lk = look(vm);
            return (
              <button
                class="navitem"
                classList={{
                  on: state.view.kind === "vm" && state.view.vm === vm.name,
                }}
                onClick={() => showVm(vm.name)}
              >
                <span class="sdot" style={`background:${lk.dot}`} />
                <span class="nitext">
                  <span class="niname">{vm.name}</span>
                  <span class="nimeta">{osOf(vm)}</span>
                </span>
                <span class="niarch">{archOf(vm)}</span>
              </button>
            );
          }}
        </For>
      </div>

      <div class="sidefoot">
        <Show when={state.authRequired} fallback={<span>vmlab</span>}>
          <button
            class="snaprestore"
            onClick={doLogout}
            title={`Signed in as ${state.authUser ?? ""}`}
          >
            Sign out
          </button>
        </Show>
        <span class={state.connected ? "c-ok" : "c-dim"}>
          ● {state.connected ? "connected" : "offline"}
        </span>
      </div>
    </aside>
  );
}
