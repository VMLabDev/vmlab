import { For, Show } from "solid-js";
import { NavLink, NavSection, StatusDot } from "@forge/ui";
import { Globe, LayoutGrid, Package } from "lucide-solid";
import {
  archOf,
  showContainer,
  showLab,
  showTemplates,
  showVm,
  showWeb,
  state,
} from "../store";
import { containers, vms } from "../status";
import { openWebCount } from "./WebView";

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
        count={vms(s()).length}
        onClick={nav(showLab)}
      >
        {state.currentLab ?? "—"}
      </NavLink>
      <NavLink
        href="#"
        icon={Package}
        active={state.view.kind === "templates"}
        count={state.templates.length}
        onClick={nav(showTemplates)}
      >
        templates
      </NavLink>
      <Show when={openWebCount() > 0}>
        <NavLink
          href="#"
          icon={Globe}
          active={state.view.kind === "web"}
          count={openWebCount()}
          onClick={nav(showWeb)}
        >
          Web
        </NavLink>
      </Show>

      <NavSection>Machines</NavSection>
      <For each={vms(s())}>
        {(vm) => (
          <NavLink
            href="#"
            active={state.view.kind === "vm" && state.view.vm === vm.name}
            count={archOf(vm)}
            onClick={nav(() => showVm(vm.name))}
          >
            <StatusDot tone={vm.label.severity} />
            {vm.name}
          </NavLink>
        )}
      </For>

      <Show when={containers(s()).length > 0}>
        <NavSection>Containers</NavSection>
        <For each={containers(s())}>
          {(c) => (
            <NavLink
              href="#"
              active={state.view.kind === "container" && state.view.vm === c.name}
              count="oci"
              onClick={nav(() => showContainer(c.name))}
            >
              <StatusDot tone={c.label.severity} />
              {c.name}
            </NavLink>
          )}
        </For>
      </Show>
    </>
  );
}
