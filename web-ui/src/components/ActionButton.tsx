// A button whose action shows an in-flight spinner, with an optional
// attached dropdown for action variants (the split-button shape: the main
// part runs the default action, the caret opens the variants — e.g. Stop /
// Force stop). Store actions resolve only once the daemon finished and
// toast failures rather than throwing, so promise-based pending is accurate.

import { Show, createSignal } from "solid-js";
import { Button, DropdownMenu, Spinner } from "@forge/ui";
import type { IconComponent } from "@forge/ui";
import { ChevronDown } from "lucide-solid";

export default function ActionButton(p: {
  label: string;
  /** Spinner label while the action (main or menu) runs. */
  busyLabel: string;
  icon?: IconComponent;
  variant?: "primary" | "secondary" | "ghost" | "danger";
  disabled?: boolean;
  onClick: () => Promise<unknown>;
  /** Dropdown variants; present = render as a split button. */
  menu?: { label: string; danger?: boolean; onClick: () => Promise<unknown> }[];
}) {
  const [pending, setPending] = createSignal(false);

  const act = async (fn: () => Promise<unknown>) => {
    setPending(true);
    try {
      await fn();
    } finally {
      setPending(false);
    }
  };

  return (
    <Show
      when={!pending()}
      fallback={
        <Button disabled class="power-busy">
          <Spinner size={14} />
          {p.busyLabel}
        </Button>
      }
    >
      <span class="split-btn">
        <Button
          variant={p.variant}
          icon={p.icon}
          disabled={p.disabled}
          onClick={() => act(p.onClick)}
        >
          {p.label}
        </Button>
        <Show when={p.menu?.length}>
          <DropdownMenu
            variant={p.variant ?? "secondary"}
            icon={ChevronDown}
            align="end"
            items={p.menu!.map((m) => ({
              label: m.label,
              danger: m.danger,
              disabled: p.disabled,
              onSelect: () => void act(m.onClick),
            }))}
          />
        </Show>
      </span>
    </Show>
  );
}
