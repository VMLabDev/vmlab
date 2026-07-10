import { For, Show } from "solid-js";
import { Badge, Card, Empty, PageHead, StatusDot, Table } from "@forge/ui";
import { state, showVm, look, osOf } from "../store";
import type { Vm } from "../api";

interface Row {
  vm: Vm;
  mac: string | null;
  ip: string | null;
}

export default function NetworkView() {
  const s = () => state.status;

  const rowsFor = (segName: string): Row[] => {
    const rows: Row[] = [];
    for (const vm of s()!.vms) {
      const nic = vm.nics.find((n) => n.segment === segName);
      if (nic) rows.push({ vm, mac: nic.mac, ip: nic.static_ip ?? vm.ip });
    }
    return rows;
  };

  return (
    <Show when={s()} fallback={<Empty title="No lab selected" />}>
      <PageHead title="network" sub={`${s()!.segments.length} segments`} />
      <div class="stack">
        <For each={s()!.segments}>
          {(seg) => {
            const rows = rowsFor(seg.name);
            return (
              <Card
                padded={false}
                title={
                  <span class="tpl-title">
                    {seg.name}
                    <span class="tpl-meta">
                      {seg.subnet} · gateway {seg.gateway}
                    </span>
                  </span>
                }
                action={
                  <span style={{ display: "inline-flex", gap: "6px", "align-items": "center" }}>
                    <Badge tone={seg.dhcp ? "success" : "neutral"}>
                      {seg.dhcp ? "dhcp" : "static"}
                    </Badge>
                    <Show when={seg.nat}>
                      <Badge tone="success">nat</Badge>
                    </Show>
                    <span class="tpl-meta">{rows.length} hosts</span>
                  </span>
                }
              >
                <Table>
                  <thead>
                    <tr>
                      <th>Machine</th>
                      <th>OS</th>
                      <th>IP address</th>
                      <th>MAC</th>
                      <th>State</th>
                    </tr>
                  </thead>
                  <tbody>
                    <Show
                      when={rows.length}
                      fallback={
                        <tr>
                          <td colspan="5">No machines on this segment.</td>
                        </tr>
                      }
                    >
                      <For each={rows}>
                        {(r) => (
                          <tr class="row-link" onClick={() => showVm(r.vm.name)}>
                            <td>{r.vm.name}</td>
                            <td>{osOf(r.vm)}</td>
                            <td style={{ "font-family": "var(--font-mono)" }}>{r.ip ?? "—"}</td>
                            <td style={{ "font-family": "var(--font-mono)" }}>{r.mac ?? "—"}</td>
                            <td>
                              <span class="cell-state">
                                <StatusDot tone={look(r.vm).tone} />
                                {look(r.vm).label}
                              </span>
                            </td>
                          </tr>
                        )}
                      </For>
                    </Show>
                  </tbody>
                </Table>
              </Card>
            );
          }}
        </For>
      </div>
    </Show>
  );
}
