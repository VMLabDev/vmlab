// The VM console: forge's DesktopViewer speaking the desktop-widget protocol
// against vmlab-web's /api/desktop/vnc/{lab}/{vm} (server-side RFB decode →
// RGBA rects). The connect frame's host/port are omitted — the URL fixes the
// target. DesktopViewer has no auto-reconnect, so the viewer is remounted
// (keyed) per lab/vm and a reconnect button covers closed/error states.

import { Show, createEffect, createSignal } from "solid-js";
import { Button, Empty, IconButton, StatusDot } from "@forge/ui";
import type { StatusTone } from "@forge/ui";
import { DesktopViewer } from "@forge/desktop";
import type { DesktopApi, DesktopStatus } from "@forge/desktop";
import { ClipboardCopy, ClipboardPaste, Maximize, RotateCcw } from "lucide-solid";
import { vmClipboardGet, vmClipboardSet, wsUrl } from "../api";
import type { Pull } from "../store";
import { showToast } from "../store";
import MachinePullStatus from "./MachinePullStatus";

export default function ConsoleScreen(props: {
  lab: string;
  vm: string;
  powered: boolean;
  endpoint?: string;
  pull?: Pull;
}) {
  const [status, setStatus] = createSignal<DesktopStatus>("disconnected");
  let api: DesktopApi | undefined;
  let frame: HTMLDivElement | undefined;

  createEffect(() => {
    if (!props.powered) setStatus("disconnected");
  });

  const tone = (): StatusTone => {
    if (props.pull) return props.pull.status === "error" ? "danger" : "warning";
    if (status() === "ready") return "success";
    if (status() === "connecting") return "warning";
    if (status() === "error") return "danger";
    return "neutral";
  };

  const statusLabel = () => {
    if (props.pull) return props.pull.status === "error" ? "download failed" : "preparing image";
    return props.powered ? status() : "powered off";
  };

  // Clipboard sync via the guest agent (needs a logged-on desktop session;
  // errors explain themselves — e.g. headless guests have no clipboard).
  const copyFromGuest = async () => {
    try {
      const { text } = await vmClipboardGet(props.lab, props.vm);
      await navigator.clipboard.writeText(text);
      showToast("Guest clipboard copied");
    } catch (e) {
      showToast(`Clipboard: ${e instanceof Error ? e.message : e}`, "danger");
    }
  };
  const pasteToGuest = async () => {
    try {
      const text = await navigator.clipboard.readText();
      await vmClipboardSet(props.lab, props.vm, text);
      showToast("Sent to guest clipboard");
    } catch (e) {
      showToast(`Clipboard: ${e instanceof Error ? e.message : e}`, "danger");
    }
  };

  return (
    <div>
      <div class="console-strip">
        <StatusDot tone={tone()} />
        <span>{statusLabel()}</span>
        <div class="spacer" />
        <Show when={props.powered && (status() === "closed" || status() === "error")}>
          <Button size="sm" variant="ghost" icon={RotateCcw} onClick={() => api?.connect()}>
            Reconnect
          </Button>
        </Show>
        <Show when={props.powered && !props.endpoint}>
          <IconButton
            icon={ClipboardCopy}
            label="Copy guest clipboard"
            onClick={copyFromGuest}
          />
          <IconButton
            icon={ClipboardPaste}
            label="Paste to guest clipboard"
            onClick={pasteToGuest}
          />
        </Show>
        <IconButton
          icon={Maximize}
          label="Fullscreen"
          onClick={() => frame?.requestFullscreen?.()}
        />
      </div>
      <div ref={frame}>
        <Show
          when={props.powered ? (props.endpoint ?? `${props.lab}/${props.vm}`) : null}
          keyed
          fallback={
            <Show
              when={props.pull}
              fallback={<Empty title={`${props.vm} is powered off`}>No framebuffer.</Empty>}
            >
              {(pull) => (
                <MachinePullStatus machine={props.vm} kind="template" pull={pull()} />
              )}
            </Show>
          }
        >
          <DesktopViewer
            url={wsUrl(
              props.endpoint ??
                `/api/desktop/vnc/${encodeURIComponent(props.lab)}/${encodeURIComponent(props.vm)}`,
            )}
            autoConnect
            scale="fit"
            height="60vh"
            onStatus={setStatus}
            ref={(a) => (api = a)}
          />
        </Show>
      </div>
    </div>
  );
}
