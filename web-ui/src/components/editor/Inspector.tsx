// The property panel beside the canvas: renders the selected block's full
// schema surface as descriptor-driven forms, including every child
// collection. All edits mutate the editor draft; nothing touches disk
// until Save builds the op batch.

import { For, Show, createMemo, createSignal } from "solid-js";
import { produce } from "solid-js/store";
import { Button, IconButton, Input, Modal, Tabs, Toggle } from "@forge/ui";
import {
  ChevronDown,
  ChevronRight,
  Disc,
  FolderSearch,
  HardDrive,
  Trash2,
} from "lucide-solid";
import type { LabModel, SegmentModel, ShareModel, VolumeModel } from "../../editor/model";
import { HEALTHCHECK_DEFAULTS, uniqueName } from "../../editor/model";
import * as F from "../../editor/fields";
import { formatByteSize, formatMemory, parseByteSize } from "../../editor/bytesize";
import {
  addMachineNic,
  addNic,
  editor,
  machineNames,
  removeContainer,
  removeRemote,
  removeSegment,
  removeVm,
  renameContainer,
  renameRemote,
  renameSegment,
  renameVm,
  select,
  setEditor,
  setSegmentPeer,
  storeTemplateFor,
} from "../../editor/store";
import { anyVmRunning, containerIsUp, vmIsUp } from "../../store";
import { confirmDialog } from "../dialogs";
import BlockForm from "./BlockForm";
import BlockList from "./BlockList";
import FileBrowserModal from "./FileBrowserModal";
import SliderRow from "./SliderRow";
import TemplatePicker from "./TemplatePicker";
import VmRefList from "./VmRefList";

const mutate = (fn: (d: LabModel) => void) =>
  setEditor(
    "draft",
    produce((d: LabModel | null) => {
      if (d) fn(d);
    }),
  );

export default function Inspector() {
  const sel = () => editor.selection;
  const readOnly = () => {
    const selection = sel();
    if (selection.kind === "vm") {
      const name = editor.draft?.vms[selection.index]?.name;
      return name ? vmIsUp(name) : false;
    }
    if (selection.kind === "container") {
      const name = editor.draft?.containers[selection.index]?.name;
      return name ? containerIsUp(name) : false;
    }
    return (selection.kind === "segment" || selection.kind === "remote") && anyVmRunning();
  };
  const readOnlyMessage = () =>
    sel().kind === "segment" || sel().kind === "remote"
      ? "Networking is read-only while any machine is up."
      : "Properties are read-only while this machine is up.";

  return (
    <div class="inspector">
      <Show when={readOnly()}>
        <div class="inspector-lock" role="status">
          {readOnlyMessage()}
        </div>
      </Show>
      <fieldset class="inspector-fields" disabled={readOnly()}>
        <Show when={sel().kind === "lab"}>
          <LabInspector />
        </Show>
        <Show when={sel().kind === "nat"}>
          <NatInspector />
        </Show>
        <Show when={sel().kind === "remote"}>
          <RemoteInspector host={(sel() as { host: string }).host} />
        </Show>
        <Show when={sel().kind === "vm" && editor.draft?.vms[(sel() as any).index]}>
          <VmInspector index={(sel() as { index: number }).index} />
        </Show>
        <Show
          when={sel().kind === "container" && editor.draft?.containers[(sel() as any).index]}
        >
          <ContainerInspector index={(sel() as { index: number }).index} />
        </Show>
        <Show when={sel().kind === "segment" && editor.draft?.segments[(sel() as any).index]}>
          <SegmentInspector index={(sel() as { index: number }).index} />
        </Show>
      </fieldset>
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
        <span class="inspector-kind">wan</span>
        <span class="inspector-name">NAT · internet uplink</span>
      </div>
      <div class="inspector-note">
        The lab's internet uplink. Plug a VM's NIC into it for direct NAT egress (
        <code>nat = true</code> on the NIC), or cable a switch's side port to it to give that
        whole segment internet access (<code>nat = true</code> on the segment). It isn't
        declared in the config — it appears through those connections.
      </div>
    </>
  );
}

// --- remote vmlab ---------------------------------------------------------------

function RemoteInspector(props: { host: string }) {
  const attached = () =>
    (editor.draft?.segments ?? [])
      .map((s, i) => ({ seg: s, index: i }))
      .filter(({ seg }) => seg.connect?.host === props.host);
  const del = async () => {
    if (
      await confirmDialog({
        title: `Delete remote vmlab "${props.host || "(no address)"}"?`,
        body: "Segments cabled to it lose their peer link (they stay global).",
        confirmLabel: "Delete",
        danger: true,
      })
    ) {
      removeRemote(props.host);
    }
  };
  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">remote</span>
        <Input
          value={props.host}
          placeholder="otherhost:13947"
          error={!props.host.trim()}
          onInput={(e: InputEvent) =>
            renameRemote(props.host, (e.currentTarget as HTMLInputElement).value)
          }
        />
        <IconButton icon={Trash2} label="Delete remote vmlab" onClick={del} />
      </div>
      <div class="inspector-section-title">Bridged segments</div>
      <Show
        when={attached().length}
        fallback={
          <div class="inspector-note">
            Not cabled to any segment yet — drag a switch's side port onto this node (or this
            node's port onto a switch) to bridge that segment to the remote vmlab instance.
          </div>
        }
      >
        <div class="remote-attached">
          <For each={attached()}>
            {({ seg, index }) => (
              <button
                type="button"
                class="remote-attached-row"
                onClick={() => select({ kind: "segment", index })}
              >
                {seg.name}
              </button>
            )}
          </For>
        </div>
      </Show>
      <div class="inspector-note">
        Cabling writes <code>connect {"{ host }"}</code> and <code>global = true</code> on the
        segment: both vmlab instances share the segment over a TCP trunk, authenticated by the
        <code> psk</code> in each host's config (<code>~/.config/vmlab/config.wcl</code>). The
        LED and cable animate while the trunk is up.
      </div>
    </>
  );
}

// --- vm -----------------------------------------------------------------------

const GIB = 1024 * 1024 * 1024;
const MIB = 1024 * 1024;

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
          { id: "overrides", label: "Overrides" },
        ]}
        active={tab()}
        onChange={setTab}
      />
      <Show when={tab() === "general"}>
        <TemplatePicker
          value={vm().template}
          onChange={(v) => setField("template", v)}
          arch={vm().arch}
          profile={vm().profile}
          onMeta={setField}
        />
        <VmRefList
          label="Depends on"
          doc="VM/container names to wait for before this one (no cycles)"
          values={vm().depends_on}
          candidates={machineNames().filter((n) => n !== vm().name)}
          onChange={(v) => setField("depends_on", v)}
        />
      </Show>
      <Show when={tab() === "hardware"}>
        <SliderRow
          label="vCPUs"
          doc="vCPU count; inherited from template→profile if not set"
          value={vm().cpus}
          fallback={storeTemplateFor(vm().template)?.cpus ?? null}
          min={1}
          max={editor.catalog.host?.cpus ?? 16}
          step={1}
          fmt={(v) => `${v} vCPU`}
          editText={(v) => String(v)}
          parse={(t) => {
            const n = parseInt(t, 10);
            return Number.isFinite(n) ? n : null;
          }}
          onChange={(v) => setField("cpus", v)}
        />
        <SliderRow
          label="Memory"
          doc="RAM; inherited from template→profile if not set"
          value={vm().memory}
          fallback={storeTemplateFor(vm().template)?.memory ?? null}
          min={256 * MIB}
          max={editor.catalog.host?.memory ?? 16 * GIB}
          step={256 * MIB}
          fmt={formatMemory}
          parse={parseByteSize}
          onChange={(v) => setField("memory", v)}
        />
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
        <VmStorage index={props.index} />
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
              // Picking a segment moves a NAT-attached NIC off the NAT bus.
              if (key === "segment" && v) nic.nat = false;
            })
          }
        />
        <div class="inspector-note">
          No NICs = air-gapped VM. Drag a connection onto the NAT bus in the canvas for
          internet egress.
        </div>
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
      </Show>
      <Show when={tab() === "overrides"}>
        <div class="inspector-note">
          Everything here is normally supplied by the template/profile — set a value only to
          override it for this VM.
        </div>
        <BlockForm fields={F.VM_OVERRIDES} value={vm() as any} onSet={setField} />
      </Show>
    </>
  );
}

/** The Storage tab: one unified disk list — extra HDDs (`disk {}` blocks)
 *  plus at most one CD-ROM (the `cdrom` attr), added through a kind picker;
 *  the CD-ROM's ISO is chosen with the server-side file browser. */
function VmStorage(props: { index: number }) {
  const vm = () => editor.draft!.vms[props.index];
  // Add-disk flow: pick the kind, then (for HDDs) lock in a size.
  const [addPhase, setAddPhase] = createSignal<null | "kind" | "hdd">(null);
  const [hddSize, setHddSize] = createSignal("10GiB");
  const [browseOpen, setBrowseOpen] = createSignal(false);
  const [openDisk, setOpenDisk] = createSignal<number | null>(null);

  // The lab root (dirname of vmlab.wcl) anchors browsing, and ISO paths
  // inside it are stored lab-relative.
  const labRoot = createMemo(() => {
    const p = editor.path;
    const cut = p.lastIndexOf("/");
    return cut > 0 ? p.slice(0, cut) : "/";
  });

  const setCdrom = (v: string | null) =>
    mutate((d) => (d.vms[props.index].cdrom = v));

  function pickIso(abs: string) {
    const root = labRoot();
    setCdrom(abs.startsWith(`${root}/`) ? abs.slice(root.length + 1) : abs);
    setBrowseOpen(false);
  }

  function addHdd() {
    const size = parseByteSize(hddSize());
    if (size == null) return;
    setAddPhase(null);
    mutate((d) => {
      const disks = d.vms[props.index].extra_disks;
      disks.push({
        span: null,
        name: uniqueName("data", disks.map((x) => x.name)),
        size,
        from: null,
      });
    });
  }

  return (
    <>
      <div class="block-list">
        <div class="block-list-head">
          <span class="block-list-title">Disks</span>
          <Button size="sm" variant="ghost" icon={HardDrive} onClick={() => setAddPhase("kind")}>
            Add disk
          </Button>
        </div>
        <Show
          when={vm().cdrom || vm().extra_disks.length}
          fallback={<div class="block-list-empty">none — the template supplies the boot disk</div>}
        >
          <Show when={vm().cdrom}>
            <div class="block-list-item">
              <div class="block-list-row">
                <Disc size={14} />
                <span class="block-list-summary">CD-ROM · {vm().cdrom}</span>
                <IconButton
                  icon={FolderSearch}
                  label="Choose another ISO"
                  onClick={() => setBrowseOpen(true)}
                />
                <IconButton icon={Trash2} label="Remove CD-ROM" onClick={() => setCdrom(null)} />
              </div>
            </div>
          </Show>
          <For each={vm().extra_disks}>
            {(disk, i) => (
              <div class="block-list-item">
                <div
                  class="block-list-row"
                  onClick={() => setOpenDisk(openDisk() === i() ? null : i())}
                >
                  {openDisk() === i() ? <ChevronDown size={14} /> : <ChevronRight size={14} />}
                  <span class="block-list-summary">
                    {disk.name || "(disk)"}
                    {disk.size != null ? ` · ${formatByteSize(disk.size)}` : ""}
                  </span>
                  <IconButton
                    icon={Trash2}
                    label="Remove disk"
                    onClick={(e: MouseEvent) => {
                      e.stopPropagation();
                      if (openDisk() === i()) setOpenDisk(null);
                      mutate((d) => void d.vms[props.index].extra_disks.splice(i(), 1));
                    }}
                  />
                </div>
                <Show when={openDisk() === i()}>
                  <div class="block-list-body">
                    <BlockForm
                      fields={F.DISK_FIELDS}
                      value={disk as unknown as Record<string, unknown>}
                      onSet={(key, v) =>
                        mutate((d) => ((d.vms[props.index].extra_disks[i()] as any)[key] = v))
                      }
                    />
                  </div>
                </Show>
              </div>
            )}
          </For>
        </Show>
      </div>

      <Modal
        open={addPhase() !== null}
        onClose={() => setAddPhase(null)}
        title={addPhase() === "hdd" ? "New hard disk" : "Add disk"}
        footer={
          <>
            <Button variant="ghost" onClick={() => setAddPhase(null)}>
              Cancel
            </Button>
            <Show when={addPhase() === "hdd"}>
              <Button
                variant="primary"
                disabled={parseByteSize(hddSize()) == null}
                onClick={addHdd}
              >
                Add disk
              </Button>
            </Show>
          </>
        }
      >
        <Show when={addPhase() === "kind"}>
          <div class="disk-kind-choices">
            <Button
              icon={HardDrive}
              onClick={() => {
                setHddSize("10GiB");
                setAddPhase("hdd");
              }}
            >
              Hard disk (blank)
            </Button>
            <Button
              icon={Disc}
              disabled={vm().cdrom !== null}
              title={vm().cdrom !== null ? "Only one CD-ROM per VM" : undefined}
              onClick={() => {
                setAddPhase(null);
                setBrowseOpen(true);
              }}
            >
              CD-ROM (browse for an ISO)
            </Button>
          </div>
        </Show>
        <Show when={addPhase() === "hdd"}>
          <div class="disk-kind-choices">
            <Input
              label="Disk size"
              help="Blank disk created in the VM's directory on first boot, e.g. `10GiB`"
              value={hddSize()}
              error={parseByteSize(hddSize()) == null}
              onInput={(e) => setHddSize(e.currentTarget.value)}
            />
          </div>
        </Show>
      </Modal>

      <FileBrowserModal
        open={browseOpen()}
        title="Choose an ISO on the server"
        start={labRoot()}
        extensions={[".iso"]}
        onClose={() => setBrowseOpen(false)}
        onPick={pickIso}
      />
    </>
  );
}

// --- container ------------------------------------------------------------------

/** Full-schema inspector for a `container {}` block: image + runtime knobs,
 *  the micro-VM's hardware sliders, NICs + port forwards, volumes, env vars,
 *  and the healthcheck. */
function ContainerInspector(props: { index: number }) {
  const ctr = () => editor.draft!.containers[props.index];
  const [tab, setTab] = createSignal("general");
  const setField = (key: string, v: unknown) =>
    mutate((d) => ((d.containers[props.index] as any)[key] = v));

  async function remove() {
    if (
      await confirmDialog({
        title: `Delete container "${ctr().name}"?`,
        body: "Removed from the config on the next save.",
        danger: true,
      })
    ) {
      removeContainer(props.index);
    }
  }

  const setHealth = (key: string, v: unknown) =>
    mutate((d) => {
      const h = d.containers[props.index].healthcheck;
      if (!h) return;
      // Clearing a timing/count field falls back to the schema default —
      // the model always carries concrete values (matching the DTO).
      const value =
        v == null && key in HEALTHCHECK_DEFAULTS
          ? HEALTHCHECK_DEFAULTS[key as keyof typeof HEALTHCHECK_DEFAULTS]
          : v;
      (h as any)[key] = value;
    });

  return (
    <>
      <div class="inspector-head">
        <span class="inspector-kind">container</span>
        <Input
          value={ctr().name}
          onInput={(e) => renameContainer(props.index, e.currentTarget.value)}
          title="Container name (DNS label, shared namespace with VMs); references update automatically"
        />
        <Button variant="danger" size="sm" icon={Trash2} onClick={remove}>
          Delete
        </Button>
      </div>
      <Tabs
        tabs={[
          { id: "general", label: "General" },
          { id: "hardware", label: "Hardware" },
          { id: "network", label: "Network", count: ctr().nics.length || undefined },
          { id: "storage", label: "Volumes", count: ctr().volumes.length || undefined },
          { id: "env", label: "Env", count: ctr().env.length || undefined },
          { id: "health", label: "Health" },
        ]}
        active={tab()}
        onChange={setTab}
      />
      <Show when={tab() === "general"}>
        <Input
          label="Image"
          help="OCI image reference, e.g. `nginx:1.27` or `ghcr.io/owner/app@sha256:…` (required)"
          placeholder="nginx:1.27"
          value={ctr().image}
          error={!ctr().image.trim()}
          onInput={(e) => setField("image", e.currentTarget.value)}
        />
        <BlockForm fields={F.CONTAINER_GENERAL} value={ctr() as any} onSet={setField} />
        <VmRefList
          label="Depends on"
          doc="VM/container names to wait for before this one (no cycles)"
          values={ctr().depends_on}
          candidates={machineNames().filter((n) => n !== ctr().name)}
          onChange={(v) => setField("depends_on", v)}
        />
      </Show>
      <Show when={tab() === "hardware"}>
        <div class="inspector-note">
          The container runs in its own micro-VM — these size that VM (defaults: 1 vCPU,
          256MiB).
        </div>
        <SliderRow
          label="vCPUs"
          doc="vCPU count for the micro-VM (> 0); default 1"
          value={ctr().cpus}
          fallback={1}
          min={1}
          max={editor.catalog.host?.cpus ?? 16}
          step={1}
          fmt={(v) => `${v} vCPU`}
          editText={(v) => String(v)}
          parse={(t) => {
            const n = parseInt(t, 10);
            return Number.isFinite(n) ? n : null;
          }}
          unsetLabel="default"
          onChange={(v) => setField("cpus", v)}
        />
        <SliderRow
          label="Memory"
          doc="RAM for the micro-VM, e.g. `512MiB`; default 256MiB"
          value={ctr().memory}
          fallback={256 * MIB}
          min={128 * MIB}
          max={editor.catalog.host?.memory ?? 16 * GIB}
          step={128 * MIB}
          fmt={formatMemory}
          parse={parseByteSize}
          unsetLabel="default"
          onChange={(v) => setField("memory", v)}
        />
      </Show>
      <Show when={tab() === "network"}>
        <BlockList
          title="NICs"
          items={ctr().nics as any}
          fields={F.NIC_FIELDS}
          summary={(n) =>
            n.nat
              ? `NAT${n.ip ? ` · ${n.ip}` : ""}`
              : `${n.segment ?? "(no segment)"}${n.ip ? ` · ${n.ip}` : ""}`
          }
          addLabel="Add NIC"
          onAdd={() =>
            addMachineNic("container", props.index, editor.draft!.segments[0]?.name ?? null)
          }
          onRemove={(i) => mutate((d) => void d.containers[props.index].nics.splice(i, 1))}
          onSet={(i, key, v) =>
            mutate((d) => {
              const nic = d.containers[props.index].nics[i] as any;
              nic[key] = v;
              // Picking a segment moves a NAT-attached NIC off the NAT bus.
              if (key === "segment" && v) nic.nat = false;
            })
          }
        />
        <div class="inspector-note">
          No NICs = air-gapped container (exec/copy/logs still work via the agent channel).
        </div>
        <BlockList
          title="Port forwards (host → container)"
          items={ctr().ports as any}
          fields={F.PORT_FIELDS}
          summary={(p) => `:${p.host || "?"} → :${p.container || "?"}${p.proto && p.proto !== "tcp" ? ` ${p.proto}` : ""}`}
          addLabel="Add port"
          onAdd={() =>
            mutate((d) =>
              d.containers[props.index].ports.push({
                span: null,
                host: 8080,
                container: 80,
                proto: "tcp",
              }),
            )
          }
          onRemove={(i) => mutate((d) => void d.containers[props.index].ports.splice(i, 1))}
          onSet={(i, key, v) =>
            mutate((d) => ((d.containers[props.index].ports[i] as any)[key] = v))
          }
        />
      </Show>
      <Show when={tab() === "storage"}>
        <BlockList
          title="Volumes"
          items={ctr().volumes as any}
          fields={F.VOLUME_FIELDS}
          summary={(v: VolumeModel) =>
            `${v.host ?? v.name ?? "(source)"} → ${v.target || "(target)"}${v.read_only ? " · ro" : ""}`
          }
          addLabel="Add volume"
          onAdd={() =>
            mutate((d) =>
              d.containers[props.index].volumes.push({
                span: null,
                host: null,
                name: null,
                target: "",
                read_only: false,
              }),
            )
          }
          onRemove={(i) => mutate((d) => void d.containers[props.index].volumes.splice(i, 1))}
          onSet={(i, key, v) =>
            mutate((d) => {
              const vol = d.containers[props.index].volumes[i] as any;
              vol[key] = v;
              // Exactly one of host / name: setting one clears the other.
              if (key === "host" && v) vol.name = null;
              if (key === "name" && v) vol.host = null;
            })
          }
        />
        <div class="inspector-note">
          A host path bind-mounts a lab-root-relative directory; a named volume is kept under
          the lab dir and shared by name — set exactly one of the two.
        </div>
      </Show>
      <Show when={tab() === "env"}>
        <BlockList
          title="Environment variables"
          items={ctr().env as any}
          fields={F.ENV_FIELDS}
          summary={(e) => (e.name ? `${e.name}=${e.value}` : "(env var)")}
          addLabel="Add variable"
          onAdd={() =>
            mutate((d) =>
              d.containers[props.index].env.push({ span: null, name: "", value: "" }),
            )
          }
          onRemove={(i) => mutate((d) => void d.containers[props.index].env.splice(i, 1))}
          onSet={(i, key, v) =>
            mutate((d) => ((d.containers[props.index].env[i] as any)[key] = v ?? ""))
          }
        />
      </Show>
      <Show when={tab() === "health"}>
        <div class="field-row">
          <div
            class="field-row-label"
            title="Health probe gating readiness; without one the container is ready once its process starts"
          >
            Healthcheck
          </div>
          <div class="field-row-control">
            <Toggle
              checked={ctr().healthcheck !== null}
              onChange={(on) =>
                setField(
                  "healthcheck",
                  on ? { span: null, command: [], ...HEALTHCHECK_DEFAULTS } : null,
                )
              }
            />
          </div>
        </div>
        <Show when={ctr().healthcheck}>
          <BlockForm
            fields={F.HEALTHCHECK_FIELDS}
            value={ctr().healthcheck as any}
            onSet={setHealth}
          />
        </Show>
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
        <SliderRow
          label="MTU"
          doc="Link MTU (576–65535); default jumbo (9000) on NAT segments, else 1500"
          value={seg().mtu}
          fallback={seg().nat ? 9000 : 1500}
          min={576}
          max={65535}
          step={1}
          fmt={(v) => String(v)}
          parse={(t) => {
            const n = parseInt(t, 10);
            return Number.isFinite(n) ? n : null;
          }}
          unsetLabel="default"
          onChange={(v) => setField("mtu", v)}
        />
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
              // Attach also sets global = true (peering rides the shared
              // switch); detach keeps global — same semantics as the canvas.
              onChange={(on) => setSegmentPeer(props.index, on ? "" : null)}
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
