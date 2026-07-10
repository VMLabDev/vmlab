import { For, Show } from "solid-js";
import { NavLink, NavSection, StatusDot } from "@forge/ui";
import { Code, LayoutGrid, Network, Package, ScrollText } from "lucide-solid";
import {
  archOf,
  look,
  showConfig,
  showLab,
  showLogs,
  showNetwork,
  showTemplates,
  showVm,
  state,
} from "../store";

/** Wrap a view-switch action as an anchor click handler. */
const nav = (go: () => void) => (e: MouseEvent) => {
  e.preventDefault();
  go();
};

export default function SidebarNav() {
  const s = () => state.status;
  return (
    <>
      <NavSection>Lab</NavSection>
      <NavLink
        href="#"
        icon={LayoutGrid}
        active={state.view.kind === "lab"}
        count={s()?.vms.length}
        onClick={nav(showLab)}
      >
        {state.currentLab ?? "—"}
      </NavLink>
      <NavLink
        href="#"
        icon={Network}
        active={state.view.kind === "network"}
        count={s()?.segments.length}
        onClick={nav(showNetwork)}
      >
        network
      </NavLink>
      <NavLink
        href="#"
        icon={ScrollText}
        active={state.view.kind === "logs"}
        onClick={nav(showLogs)}
      >
        logs
      </NavLink>
      <NavLink
        href="#"
        icon={Code}
        active={state.view.kind === "config"}
        onClick={nav(showConfig)}
      >
        config
      </NavLink>
      <Show when={state.templates.length > 0}>
        <NavLink
          href="#"
          icon={Package}
          active={state.view.kind === "templates"}
          count={state.templates.length}
          onClick={nav(showTemplates)}
        >
          templates
        </NavLink>
      </Show>

      <NavSection>Machines</NavSection>
      <For each={s()?.vms ?? []}>
        {(vm) => (
          <NavLink
            href="#"
            active={state.view.kind === "vm" && state.view.vm === vm.name}
            count={archOf(vm)}
            onClick={nav(() => showVm(vm.name))}
          >
            <StatusDot tone={look(vm).tone} />
            {vm.name}
          </NavLink>
        )}
      </For>
    </>
  );
}
