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
import { Maximize, RotateCcw } from "lucide-solid";
import { wsUrl } from "../api";

export default function ConsoleScreen(props: {
  lab: string;
  vm: string;
  powered: boolean;
  endpoint?: string;
}) {
  const [status, setStatus] = createSignal<DesktopStatus>("disconnected");
  let api: DesktopApi | undefined;
  let frame: HTMLDivElement | undefined;

  createEffect(() => {
    if (!props.powered) setStatus("disconnected");
  });

  const tone = (): StatusTone =>
    status() === "ready"
      ? "success"
      : status() === "connecting"
        ? "warning"
        : status() === "error"
          ? "danger"
          : "neutral";

  return (
    <div>
      <div class="console-strip">
        <StatusDot tone={tone()} />
        <span>{props.powered ? status() : "powered off"}</span>
        <div class="spacer" />
        <Show when={props.powered && (status() === "closed" || status() === "error")}>
          <Button size="sm" variant="ghost" icon={RotateCcw} onClick={() => api?.connect()}>
            Reconnect
          </Button>
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
          fallback={<Empty title={`${props.vm} is powered off`}>No framebuffer.</Empty>}
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
