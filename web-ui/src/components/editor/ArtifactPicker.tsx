import { For, Show, createMemo, createSignal, onMount } from "solid-js";
import { Alert, Badge, Button, Empty, Input, Modal, Select, Spinner, Table } from "@forge/ui";
import { Search } from "lucide-solid";
import { searchOciCatalog, type OciCatalogEntry } from "../../api";
import { editor } from "../../editor/store";
import {
  containerRegistry,
  registryEntries,
  refreshRegistries,
  vmRegistry,
  type RegistryEntry,
} from "../../registries";

interface Result extends OciCatalogEntry {
  source: "local" | "registry";
}

export interface ArtifactPickerProps {
  kind: "vm" | "container";
  value: string;
  onSelect: (reference: string, arch: string, source: Result["source"]) => void;
}

export default function ArtifactPicker(props: ArtifactPickerProps) {
  onMount(() => void refreshRegistries().catch(() => undefined));
  const [open, setOpen] = createSignal(false);
  const [arch, setArch] = createSignal(editor.catalog.meta?.arches[0] ?? "x86_64");
  const [query, setQuery] = createSignal("");
  const [registry, setRegistry] = createSignal("all");
  const [results, setResults] = createSignal<Result[]>([]);
  const [searching, setSearching] = createSignal(false);
  const [error, setError] = createSignal<string | null>(null);
  const [searched, setSearched] = createSignal(false);

  const discovered = createMemo<RegistryEntry[]>(() => [
    ...(editor.draft?.vms.map((vm) => vmRegistry(vm.template)).filter(Boolean) ?? []),
    ...(editor.draft?.containers.map((c) => containerRegistry(c.image)).filter(Boolean) ?? []),
  ] as RegistryEntry[]);
  const registries = createMemo(() =>
    registryEntries(discovered()).filter((r) => (props.kind === "vm" ? r.vms : r.containers)),
  );
  const registryOptions = createMemo(() => [
    { value: "all", label: `All ${props.kind === "vm" ? "VM" : "container"} registries` },
    ...registries().map((r) => ({ value: r.namespace, label: r.namespace })),
  ]);

  async function runSearch() {
    setSearching(true);
    setError(null);
    setSearched(true);
    const q = query().trim().toLowerCase();
    const local: Result[] =
      props.kind === "vm"
        ? [
            ...(!q || "scratch blank disk".includes(q)
              ? [
                  {
                    name: "Blank disk (scratch)",
                    repo: "Built into vmlab",
                    arches: [arch()],
                    version: "—",
                    reference: "scratch",
                    source: "local" as const,
                  },
                ]
              : []),
            ...editor.catalog.templates
              .filter((t) => t.arch === arch() && (!q || t.name.toLowerCase().includes(q)))
              .map((t) => ({
                name: t.name,
                repo: "Local template store",
                arches: [t.arch],
                version: t.version,
                reference: `${t.arch}/${t.name}@${t.version}`,
                source: "local" as const,
              })),
          ]
        : [];
    const targets =
      registry() === "all"
        ? registries()
        : registries().filter((r) => r.namespace === registry());
    const settled = await Promise.allSettled(
      targets.map((r) => searchOciCatalog(r.namespace, query().trim(), arch(), props.kind)),
    );
    const remote = settled.flatMap((outcome) =>
      outcome.status === "fulfilled"
        ? outcome.value.map((row) => ({ ...row, source: "registry" as const }))
        : [],
    );
    const failures = settled.filter((outcome) => outcome.status === "rejected");
    if (failures.length && failures.length === settled.length && !local.length) {
      setError(failures.map((f) => String((f as PromiseRejectedResult).reason)).join("\n"));
    } else if (failures.length) {
      setError(
        `${failures.length} registr${failures.length === 1 ? "y" : "ies"} could not be searched; showing the available results.`,
      );
    }
    const unique = new Map<string, Result>();
    for (const result of [...local, ...remote]) unique.set(result.reference, result);
    setResults([...unique.values()].sort((a, b) => a.name.localeCompare(b.name)));
    setSearching(false);
  }

  function choose(result: Result) {
    props.onSelect(result.reference, arch(), result.source);
    setOpen(false);
  }

  return (
    <div class="artifact-picker">
      <div class="artifact-current" classList={{ invalid: !props.value.trim() }}>
        <div>
          <span class="artifact-label">{props.kind === "vm" ? "Template" : "Image"}</span>
          <span class="artifact-reference">{props.value || "Nothing selected"}</span>
        </div>
        <Button icon={Search} onClick={() => setOpen(true)}>
          Select…
        </Button>
      </div>

      <Modal
        open={open()}
        title={props.kind === "vm" ? "Select a VM template" : "Select a container image"}
        onClose={() => setOpen(false)}
        footer={
          <Button variant="ghost" onClick={() => setOpen(false)}>
            Cancel
          </Button>
        }
      >
        <div class="artifact-search">
          <div class="artifact-search-controls">
            <Select
              label="Architecture"
              value={arch()}
              options={(editor.catalog.meta?.arches ?? ["x86_64"]).map((value) => ({
                value,
                label: value,
              }))}
              onChange={setArch}
            />
            <Select
              label="Registry"
              value={registry()}
              options={registryOptions()}
              onChange={setRegistry}
            />
          </div>
          <form
            class="artifact-query"
            onSubmit={(e) => {
              e.preventDefault();
              void runSearch();
            }}
          >
            <Input
              label="Search"
              placeholder={
                props.kind === "vm"
                  ? "alpine, windows, router…"
                  : "nginx, postgres, alpine…"
              }
              value={query()}
              onInput={(e) => setQuery(e.currentTarget.value)}
            />
            <Button variant="primary" icon={Search} disabled={searching()} type="submit">
              Search
            </Button>
          </form>

          <Show when={error()}>{(message) => <Alert tone="warning">{message()}</Alert>}</Show>
          <Show when={searching()}>
            <div class="artifact-loading">
              <Spinner /> Searching registries…
            </div>
          </Show>
          <Show when={!searching() && searched()}>
            <Show
              when={results().length}
              fallback={
                <Empty title="No matches">Try another name, architecture, or registry.</Empty>
              }
            >
              <div class="artifact-results">
                <Table
                  aria-label={props.kind === "vm" ? "VM template search results" : "Container image search results"}
                >
                  <thead>
                    <tr>
                      <th>Name</th>
                      <th>Architecture</th>
                      <th>Version</th>
                      <th aria-label="Actions" />
                    </tr>
                  </thead>
                  <tbody>
                    <For each={results()}>
                      {(result) => (
                        <tr>
                          <td>
                            <span class="artifact-result-name">{result.name}</span>
                            <span class="artifact-result-source">{result.repo}</span>
                          </td>
                          <td>{result.arches.map((a) => <Badge>{a}</Badge>)}</td>
                          <td class="artifact-version">{result.version}</td>
                          <td>
                            <Button size="sm" variant="primary" onClick={() => choose(result)}>
                              Select
                            </Button>
                          </td>
                        </tr>
                      )}
                    </For>
                  </tbody>
                </Table>
              </div>
            </Show>
          </Show>
        </div>
      </Modal>
    </div>
  );
}
