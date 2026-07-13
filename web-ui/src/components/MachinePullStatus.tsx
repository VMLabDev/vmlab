import { Icon, Progress } from "@forge/ui";
import { CircleAlert, Download } from "lucide-solid";
import { fmtBytes, type Pull } from "../store";

export default function MachinePullStatus(props: {
  machine: string;
  kind: "template" | "image";
  pull: Pull;
}) {
  const failed = () => props.pull.status === "error";
  const checking = () => props.pull.status === "checking";
  const label = () => (props.kind === "template" ? "VM template" : "container image");
  const percent = () => Math.min(100, Math.max(0, props.pull.percent));

  return (
    <div
      class="console-pull-state"
      classList={{ error: failed() }}
      role="status"
      aria-live="polite"
    >
      <div class="console-pull-panel">
        <div class="console-pull-head">
          <div class="console-pull-icon">
            <Icon of={failed() ? CircleAlert : Download} size={22} />
          </div>
          <div class="console-pull-copy">
            <span class="console-pull-kicker">
              {failed() ? "STARTUP BLOCKED" : "STARTUP PREPARATION"}
            </span>
            <strong>
              {failed()
                ? `${label()} download failed`
                : checking()
                  ? `Resolving ${label().toLowerCase()}`
                  : `Downloading ${label().toLowerCase()}`}
            </strong>
            <span>
              {failed()
                ? props.pull.error ??
                  "The download could not be completed. Retry by starting the machine again."
                : `${props.machine} will start automatically when the download finishes.`}
            </span>
          </div>
        </div>

        <Progress
          label={props.pull.reference || label()}
          value={failed() ? 100 : percent()}
          indeterminate={checking()}
          tone={failed() ? "danger" : "accent"}
          showValue={props.pull.status === "pulling"}
        />

        <div class="console-pull-foot">
          <span>
            {failed()
              ? "Download interrupted"
              : checking()
                ? "Checking the registry and local cache…"
                : `${fmtBytes(props.pull.bytesDone)} of ${fmtBytes(props.pull.bytesTotal)}`}
          </span>
          <span>{failed() ? "Start to retry" : "Waiting to boot"}</span>
        </div>
      </div>
    </div>
  );
}
