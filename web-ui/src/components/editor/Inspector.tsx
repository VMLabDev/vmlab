// The property panel beside the canvas: renders the selected block's full
// schema surface as descriptor-driven forms, including every child
// collection. All edits mutate the editor draft; nothing touches disk
// until Save builds the op batch.

import { Show, createSignal } from "solid-js";
import { produce } from "solid-js/store";
import { Button, Input, Tabs, Toggle } from "@forge/ui";
import { Trash2 } from "lucide-solid";
import type {
  DiskModel,
  LabModel,
  MediaModel,
  SegmentModel,
  ShareModel,
  VmModel,
} from "../../editor/model";
import { uniqueName } from "../../editor/model";
import * as F from "../../editor/fields";
import {
  addNic,
  editor,
  removeSegment,
  removeVm,
  renameSegment,
  renameVm,
  setEditor,
} from "../../editor/store";
import { confirmDialog } from "../dialogs";
import BlockForm from "./BlockForm";
import BlockList from "./BlockList";

const mutate = (fn: (d: LabModel) => void) =>
  setEditor(
    "draft",
    produce((d: LabModel | null) => {
      if (d) fn(d);
    }),
  );

export default function Inspector() {
  const sel = () => editor.selection;
  return (
    <div class="inspector">
      <Show when={sel().kind === "lab"}>
        <LabInspector />
      </Show>
      <Show when={sel().kind === "nat"}>
        <NatInspector />
      </Show>
      <Show when={sel().kind === "vm" && editor.draft?.vms[(sel() as any).index]}>
        <VmInspector index={(sel() as { index: number }).index} />
      </Show>
      <Show when={sel().kind === "segment" && editor.draft?.segments[(sel() as any).index]}>
        <SegmentInspector index={(sel() as { index: number }).index} />
      </Show>
    </div>
  );
}

// --- lab ----------------------------------------------------------------------

function LabInspector() {
  const lab = () => editor.draft!;
  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">lab</span>
        <span class="inspector-name">{lab().name}</span>
      </div>
      <BlockForm
        fields={F.LAB_FIELDS}
        value={lab() as unknown as Record<string, unknown>}
        onSet={(key, v) => mutate((d) => ((d as any)[key] = v))}
      />
      <BlockList
        title="Provision scripts"
        items={lab().provisions as any}
        fields={F.PROVISION_FIELDS}
        summary={(p) => p.script || "(script path)"}
        onAdd={() => mutate((d) => d.provisions.push({ span: null, script: "", vms: [] }))}
        onRemove={(i) => mutate((d) => void d.provisions.splice(i, 1))}
        onSet={(i, key, v) => mutate((d) => ((d.provisions[i] as any)[key] = v))}
      />
      <BlockList
        title="Event handlers"
        items={lab().handlers as any}
        fields={F.HANDLER_FIELDS}
        summary={(h) => `${h.event || "(event)"} → ${h.run || "(script)"}`}
        onAdd={() => mutate((d) => d.handlers.push({ span: null, event: "vm.ready", run: "" }))}
        onRemove={(i) => mutate((d) => void d.handlers.splice(i, 1))}
        onSet={(i, key, v) => mutate((d) => ((d.handlers[i] as any)[key] = v))}
      />
      <BlockList
        title="DNS records (lab-wide)"
        items={lab().records as any}
        fields={F.RECORD_FIELDS}
        summary={(r) => `${r.name || "(name)"} → ${r.ip || "?"}`}
        onAdd={() => mutate((d) => d.records.push({ span: null, name: "", ip: "" }))}
        onRemove={(i) => mutate((d) => void d.records.splice(i, 1))}
        onSet={(i, key, v) => mutate((d) => ((d.records[i] as any)[key] = v))}
      />
      <BlockList
        title="DNS sinkholes (lab-wide)"
        items={lab().sinkholes as any}
        fields={F.SINKHOLE_FIELDS}
        summary={(s) => s.pattern || "(pattern)"}
        onAdd={() =>
          mutate((d) => d.sinkholes.push({ span: null, pattern: "", mode: "nxdomain" }))
        }
        onRemove={(i) => mutate((d) => void d.sinkholes.splice(i, 1))}
        onSet={(i, key, v) => mutate((d) => ((d.sinkholes[i] as any)[key] = v))}
      />
      <Show when={editor.templatesInFile.length}>
        <div class="inspector-note">
          This file also defines {editor.templatesInFile.length} template block(s) — edit those
          in the Config page.
        </div>
      </Show>
    </>
  );
}

function NatInspector() {
  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">nat</span>
        <span class="inspector-name">built-in NAT</span>
      </div>
      <div class="inspector-note">
        The per-lab built-in NAT segment: NICs with <code>nat = true</code> attach here and get
        DHCP + internet egress. It isn't declared in the config — drag a connection from a VM
        onto this bus (or enable NAT on a NIC) to use it.
      </div>
    </>
  );
}

// --- vm -----------------------------------------------------------------------

function VmInspector(props: { index: number }) {
  const vm = () => editor.draft!.vms[props.index];
  const [tab, setTab] = createSignal("general");
  const setField = (key: string, v: unknown) =>
    mutate((d) => ((d.vms[props.index] as any)[key] = v));

  async function remove() {
    if (
      await confirmDialog({
        title: `Delete VM "${vm().name}"?`,
        body: "Removed from the config on the next save.",
        danger: true,
      })
    ) {
      removeVm(props.index);
    }
  }

  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">vm</span>
        <Input
          value={vm().name}
          onInput={(e) => renameVm(props.index, e.currentTarget.value)}
          title="VM name (DNS label); references update automatically"
        />
        <Button variant="danger" size="sm" icon={Trash2} onClick={remove}>
          Delete
        </Button>
      </div>
      <Tabs
        tabs={[
          { id: "general", label: "General" },
          { id: "hardware", label: "Hardware" },
          { id: "storage", label: "Storage" },
          { id: "network", label: "Network", count: vm().nics.length || undefined },
          { id: "sharing", label: "Shares" },
          { id: "advanced", label: "Advanced" },
        ]}
        active={tab()}
        onChange={setTab}
      />
      <Show when={tab() === "general"}>
        <BlockForm fields={F.VM_GENERAL} value={vm() as any} onSet={setField} />
      </Show>
      <Show when={tab() === "hardware"}>
        <BlockForm fields={F.VM_HARDWARE} value={vm() as any} onSet={setField} />
        <div class="field-row">
          <div class="field-row-label" title="GPU acceleration (passthrough / virgl / vulkan)">
            GPU
          </div>
          <div class="field-row-control">
            <Toggle
              checked={vm().gpu !== null}
              onChange={(on) =>
                setField("gpu", on ? { span: null, mode: "virgl", address: null } : null)
              }
            />
          </div>
        </div>
        <Show when={vm().gpu}>
          <BlockForm
            fields={F.GPU_FIELDS}
            value={vm().gpu as any}
            onSet={(key, v) => mutate((d) => ((d.vms[props.index].gpu as any)[key] = v))}
          />
        </Show>
      </Show>
      <Show when={tab() === "storage"}>
        <BlockForm fields={F.VM_STORAGE} value={vm() as any} onSet={setField} />
        <BlockList
          title="Extra disks"
          items={vm().extra_disks as any}
          fields={F.DISK_FIELDS}
          summary={(d: DiskModel) => d.name || "(disk)"}
          addLabel="Add disk"
          onAdd={() =>
            mutate((d) =>
              d.vms[props.index].extra_disks.push({
                span: null,
                name: uniqueName("data", d.vms[props.index].extra_disks.map((x) => x.name)),
                size: null,
                from: null,
              }),
            )
          }
          onRemove={(i) => mutate((d) => void d.vms[props.index].extra_disks.splice(i, 1))}
          onSet={(i, key, v) =>
            mutate((d) => ((d.vms[props.index].extra_disks[i] as any)[key] = v))
          }
        />
      </Show>
      <Show when={tab() === "network"}>
        <BlockList
          title="NICs"
          items={vm().nics as any}
          fields={F.NIC_FIELDS}
          summary={(n) =>
            n.nat ? `NAT${n.ip ? ` · ${n.ip}` : ""}` : `${n.segment ?? "(no segment)"}${n.ip ? ` · ${n.ip}` : ""}`
          }
          addLabel="Add NIC"
          onAdd={() => addNic(props.index, editor.draft!.segments[0]?.name ?? null)}
          onRemove={(i) => mutate((d) => void d.vms[props.index].nics.splice(i, 1))}
          onSet={(i, key, v) =>
            mutate((d) => {
              const nic = d.vms[props.index].nics[i] as any;
              nic[key] = v;
              // NAT and segment attachment are mutually exclusive.
              if (key === "nat" && v === true) nic.segment = null;
              if (key === "segment" && v) nic.nat = false;
            })
          }
        />
        <div class="inspector-note">No NICs = air-gapped VM.</div>
      </Show>
      <Show when={tab() === "sharing"}>
        <BlockList
          title="SMB shares"
          items={vm().shares as any}
          fields={F.SHARE_FIELDS}
          summary={(s: ShareModel) => (s.guest ? `${s.host || "?"} → ${s.guest}` : "(share)")}
          addLabel="Add share"
          onAdd={() =>
            mutate((d) =>
              d.vms[props.index].shares.push({
                span: null,
                host: "",
                guest: "",
                readonly: false,
                smb1: false,
                name: "",
              }),
            )
          }
          onRemove={(i) => mutate((d) => void d.vms[props.index].shares.splice(i, 1))}
          onSet={(i, key, v) => mutate((d) => ((d.vms[props.index].shares[i] as any)[key] = v))}
        />
        <BlockList
          title="Built media"
          items={vm().media as any}
          fields={F.MEDIA_FIELDS}
          summary={(m: MediaModel) => `${m.kind} ← ${m.from || "(folder)"}`}
          addLabel="Add media"
          onAdd={() =>
            mutate((d) =>
              d.vms[props.index].media.push({ span: null, kind: "iso", from: "", label: null }),
            )
          }
          onRemove={(i) => mutate((d) => void d.vms[props.index].media.splice(i, 1))}
          onSet={(i, key, v) => mutate((d) => ((d.vms[props.index].media[i] as any)[key] = v))}
        />
      </Show>
      <Show when={tab() === "advanced"}>
        <BlockForm fields={F.VM_ADVANCED} value={vm() as any} onSet={setField} />
      </Show>
    </>
  );
}

// --- segment --------------------------------------------------------------------

function SegmentInspector(props: { index: number }) {
  const seg = () => editor.draft!.segments[props.index];
  const [tab, setTab] = createSignal("general");
  const setField = (key: string, v: unknown) =>
    mutate((d) => ((d.segments[props.index] as any)[key] = v));

  async function remove() {
    if (
      await confirmDialog({
        title: `Delete segment "${seg().name}"?`,
        body: "NICs attached to it will need a new segment.",
        danger: true,
      })
    ) {
      removeSegment(props.index);
    }
  }

  const setSeg = (fn: (s: SegmentModel) => void) =>
    mutate((d) => fn(d.segments[props.index]));

  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">segment</span>
        <Input
          value={seg().name}
          onInput={(e) => renameSegment(props.index, e.currentTarget.value)}
          title="Segment name (DNS label); references update automatically"
        />
        <Button variant="danger" size="sm" icon={Trash2} onClick={remove}>
          Delete
        </Button>
      </div>
      <Tabs
        tabs={[
          { id: "general", label: "General" },
          { id: "services", label: "DHCP & NAT" },
          { id: "dns", label: "DNS" },
          { id: "rules", label: "Rules" },
        ]}
        active={tab()}
        onChange={setTab}
      />
      <Show when={tab() === "general"}>
        <BlockForm fields={F.SEGMENT_GENERAL} value={seg() as any} onSet={setField} />
      </Show>
      <Show when={tab() === "services"}>
        <BlockForm fields={F.SEGMENT_SERVICES} value={seg() as any} onSet={setField} />
        <div class="field-row">
          <div class="field-row-label" title="DNS service override: hand out another server, or opt out">
            DNS override
          </div>
          <div class="field-row-control">
            <Toggle
              checked={seg().dns.declared}
              onChange={(on) => setSeg((s) => (s.dns.declared = on))}
            />
          </div>
        </div>
        <Show when={seg().dns.declared}>
          <BlockForm
            fields={F.DNS_FIELDS}
            value={seg().dns as any}
            onSet={(key, v) => setSeg((s) => ((s.dns as any)[key] = v))}
          />
        </Show>
        <div class="field-row">
          <div class="field-row-label" title="Cross-host segment peer over TCP (PSK from host config)">
            Cross-host peer
          </div>
          <div class="field-row-control">
            <Toggle
              checked={seg().connect !== null}
              onChange={(on) => setSeg((s) => (s.connect = on ? { span: null, host: "" } : null))}
            />
          </div>
        </div>
        <Show when={seg().connect}>
          <BlockForm
            fields={F.CONNECT_FIELDS}
            value={seg().connect as any}
            onSet={(key, v) => setSeg((s) => ((s.connect as any)[key] = v))}
          />
        </Show>
        <BlockList
          title="Guest routes (DHCP option 121)"
          items={seg().routes as any}
          fields={F.ROUTE_FIELDS}
          summary={(r) => `${r.dest || "(dest)"} via ${r.via || "?"}`}
          addLabel="Add route"
          onAdd={() => setSeg((s) => s.routes.push({ span: null, dest: "", via: "" }))}
          onRemove={(i) => setSeg((s) => void s.routes.splice(i, 1))}
          onSet={(i, key, v) => setSeg((s) => ((s.routes[i] as any)[key] = v))}
        />
      </Show>
      <Show when={tab() === "dns"}>
        <BlockList
          title="Static DNS records"
          items={seg().records as any}
          fields={F.RECORD_FIELDS}
          summary={(r) => `${r.name || "(name)"} → ${r.ip || "?"}`}
          addLabel="Add record"
          onAdd={() => setSeg((s) => s.records.push({ span: null, name: "", ip: "" }))}
          onRemove={(i) => setSeg((s) => void s.records.splice(i, 1))}
          onSet={(i, key, v) => setSeg((s) => ((s.records[i] as any)[key] = v))}
        />
        <BlockList
          title="DNS sinkholes"
          items={seg().sinkholes as any}
          fields={F.SINKHOLE_FIELDS}
          summary={(s) => s.pattern || "(pattern)"}
          addLabel="Add sinkhole"
          onAdd={() =>
            setSeg((s) => s.sinkholes.push({ span: null, pattern: "", mode: "nxdomain" }))
          }
          onRemove={(i) => setSeg((s) => void s.sinkholes.splice(i, 1))}
          onSet={(i, key, v) => setSeg((s) => ((s.sinkholes[i] as any)[key] = v))}
        />
      </Show>
      <Show when={tab() === "rules"}>
        <BlockList
          title="Port forwards (host → guest)"
          items={seg().forwards as any}
          fields={F.FORWARD_FIELDS}
          summary={(f) => `:${f.host_port || "?"} → ${f.vm || "?"}:${f.guest_port || "?"}`}
          addLabel="Add forward"
          onAdd={() =>
            setSeg((s) =>
              s.forwards.push({
                span: null,
                host_port: 8080,
                vm: editor.draft!.vms[0]?.name ?? "",
                guest_port: 80,
                proto: "tcp",
              }),
            )
          }
          onRemove={(i) => setSeg((s) => void s.forwards.splice(i, 1))}
          onSet={(i, key, v) => setSeg((s) => ((s.forwards[i] as any)[key] = v))}
        />
        <BlockList
          title="Block rules"
          items={seg().block_rules as any}
          fields={F.BLOCK_RULE_FIELDS}
          summary={(b) => `${b.cidr || "(cidr)"}${b.proto ? ` ${b.proto}` : ""}${b.port ? `:${b.port}` : ""}`}
          addLabel="Add rule"
          onAdd={() =>
            setSeg((s) => s.block_rules.push({ span: null, cidr: "", proto: null, port: null }))
          }
          onRemove={(i) => setSeg((s) => void s.block_rules.splice(i, 1))}
          onSet={(i, key, v) => setSeg((s) => ((s.block_rules[i] as any)[key] = v))}
        />
        <BlockList
          title="Redirect rules (DNAT)"
          items={seg().redirect_rules as any}
          fields={F.REDIRECT_FIELDS}
          summary={(r) => `${r.from || "(from)"} → ${r.to || "(to)"}`}
          addLabel="Add redirect"
          onAdd={() =>
            setSeg((s) =>
              s.redirect_rules.push({ span: null, from: "", to: "", proto: null }),
            )
          }
          onRemove={(i) => setSeg((s) => void s.redirect_rules.splice(i, 1))}
          onSet={(i, key, v) => setSeg((s) => ((s.redirect_rules[i] as any)[key] = v))}
        />
      </Show>
    </>
  );
}
