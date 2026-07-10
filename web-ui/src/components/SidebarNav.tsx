import { For, Show } from "solid-js";
import { NavLink, NavSection, StatusDot } from "@forge/ui";
import { LayoutGrid, Package } from "lucide-solid";
import {
  archOf,
  containerLook,
  look,
  showContainer,
  showLab,
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

      <Show when={(s()?.containers ?? []).length > 0}>
        <NavSection>Containers</NavSection>
        <For each={s()?.containers ?? []}>
          {(c) => (
            <NavLink
              href="#"
              active={state.view.kind === "container" && state.view.vm === c.name}
              count="oci"
              onClick={nav(() => showContainer(c.name))}
            >
              <StatusDot tone={containerLook(c).tone} />
              {c.name}
            </NavLink>
          )}
        </For>
      </Show>
    </>
  );
}
