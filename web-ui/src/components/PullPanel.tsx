// Registry downloads running for the current lab: one progress row per
// machine, each cancellable. Downloads start from `up`/`start` (or `vmlab
// pull`), so this panel only reports them — it appears on the lab overview and
// on the Templates page, and hides itself when nothing is downloading.

import { For, Show } from "solid-js";
import { Button, Card, Progress } from "@forge/ui";
import { X } from "lucide-solid";
import { cancelPull, currentPulls, fmtBytes, type Pull } from "../store";

export default function PullPanel() {
  const pulls = () => currentPulls();
  const settled = (pull: Pull) => pull.status === "error" || pull.status === "cancelled";
  const tone = (pull: Pull) =>
    pull.status === "error" ? "danger" : pull.status === "cancelled" ? "warning" : "accent";
  const sub = (pull: Pull) => {
    switch (pull.status) {
      case "error":
        return pull.error;
      case "cancelled":
        return "Cancelled — starting the machine downloads it again";
      case "checking":
        return "Resolving reference…";
      default:
        return `${fmtBytes(pull.bytesDone)} / ${fmtBytes(pull.bytesTotal)}`;
    }
  };

  return (
    <Show when={pulls().length}>
      <Card title="Downloading images">
        <div class="pull-list">
          <For each={pulls()}>
            {(pull) => (
              <div class="pull-row">
                <div class="pull-row-bar">
                  <Progress
                    label={`${pull.vm} — ${pull.reference}`}
                    value={settled(pull) ? 100 : pull.percent}
                    indeterminate={pull.status === "checking"}
                    tone={tone(pull)}
                    showValue={pull.status === "pulling"}
                  />
                  <div
                    class="pull-sub"
                    classList={{ "is-error": pull.status === "error" }}
                  >
                    {sub(pull)}
                  </div>
                </div>
                <Show when={!settled(pull)}>
                  <Button
                    size="sm"
                    variant="ghost"
                    icon={X}
                    title={`Stop downloading ${pull.reference}`}
                    onClick={() => void cancelPull(pull.vm)}
                  >
                    Cancel
                  </Button>
                </Show>
              </div>
            )}
          </For>
        </div>
      </Card>
    </Show>
  );
}
