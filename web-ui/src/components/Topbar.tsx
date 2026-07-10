import { Show } from "solid-js";
import { Badge, DropdownMenu, Icon, IconButton } from "@forge/ui";
import { applyTheme } from "@forge/tokens";
import { Check, LogOut, Moon } from "lucide-solid";
import { doLogout, selectLab, showLab, state } from "../store";

function toggleTheme() {
  const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  const current = document.documentElement.dataset.theme ?? (dark ? "dark" : "light");
  applyTheme(current === "dark" ? "light" : "dark");
}

export default function Topbar() {
  return (
    <>
      <a
        class="ftopbar-brand"
        href="#"
        onClick={(e) => {
          e.preventDefault();
          showLab();
        }}
      >
        <span>
          <span class="brand-vm">vm</span>
          <span class="brand-lab">lab</span>
        </span>
      </a>
      <DropdownMenu
        label={state.currentLab ?? "no lab"}
        size="sm"
        items={state.labs.map((l) => ({
          label: (
            <span style={{ display: "inline-flex", "align-items": "center", gap: "8px" }}>
              {l.name}
              <Show when={l.name === state.currentLab}>
                <Icon of={Check} size={12} />
              </Show>
            </span>
          ),
          onSelect: () => selectLab(l.name),
        }))}
      />
      <div style={{ flex: 1 }} />
      <Badge tone={state.connected ? "success" : "neutral"} dot>
        {state.connected ? "connected" : "offline"}
      </Badge>
      <IconButton icon={Moon} label="Toggle dark/light" onClick={toggleTheme} />
      <Show when={state.authRequired}>
        <IconButton
          icon={LogOut}
          label={`Sign out${state.authUser ? ` (${state.authUser})` : ""}`}
          onClick={doLogout}
        />
      </Show>
    </>
  );
}
