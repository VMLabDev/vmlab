// GENERATED — do not edit. Run `just schema-gen` after changing
// src/config/schema.wcl or src/config/designer.rs.
//
// The Schema projection (ADR-0005) as the console consumes it: the
// inspector's field descriptors, with help text, option lists, bounds
// and defaults reflected from src/config/schema.wcl. Nothing here is
// hand-maintained, so nothing here can drift from the schema.

export type FieldType =
  | "text" // utf8 → Input
  | "int" // i64 / duration in seconds → Input[number]
  | "bool3" // bool? with an inherited default → default/on/off ToggleGroup
  | "flag" // plain bool → Toggle
  | "bytes" // std.ByteSize → ByteSizeInput
  | "enum" // closed option list → Select
  | "lines" // list<utf8> → Textarea, one per line
  | "segref" // one segment name
  | "segrefs" // several segment names
  | "vmref" // one VM name
  | "vmrefs" // several machine names
  | "event"; // lifecycle event picker

export interface FieldDesc {
  key: string;
  label: string;
  /** The schema field's `@doc` text, verbatim. */
  doc: string;
  type: FieldType;
  options?: string[];
  placeholder?: string;
  /** The schema requires a value or supplies one, so a picker
   *  offers no "(default)" choice. */
  required?: boolean;
  min?: number;
  max?: number;
  /** The schema default, in WCL source form, when it declares one. */
  default?: string;
}

/** Every closed option list in the schema, keyed `block.field`. */
export const SCHEMA_OPTIONS: Record<string, string[]> = {
  "forward.proto": ["tcp", "udp", "both"],
  "block.proto": ["tcp", "udp", "icmp"],
  "redirect.proto": ["tcp", "udp"],
  "sinkhole.mode": ["nxdomain", "zero"],
  "vm.firmware": ["ovmf", "seabios"],
  "container.mode": ["workload", "idle"],
  "port.proto": ["tcp", "udp", "both"],
  "share.transport": ["auto", "virtiofs", "smb"],
  "auth.method": ["basic", "bearer", "header", "ntlm", "form"],
  "auth.login_method": ["POST", "GET"],
  "auth.login_content_type": ["application/x-www-form-urlencoded", "application/json"],
  "media.kind": ["iso", "floppy"],
  "gpu.mode": ["passthrough", "virgl", "vulkan"],
  "template.firmware": ["ovmf", "seabios"],
  "source.kind": ["iso", "qcow2", "template", "scratch"],
};

/** Schema defaults in base units (seconds / bytes / count), keyed `block.field`. */
export const SCHEMA_DEFAULTS: Record<string, number> = {
  "healthcheck.interval": 10,
  "healthcheck.timeout": 5,
  "healthcheck.retries": 3,
  "healthcheck.start_period": 10,
};

/** A `@one_of` rule: at least one of `fields` must be set, and unless
 *  `exclusive` is false, no more than one. */
export interface RequiredGroup {
  fields: string[];
  exclusive: boolean;
}

/** The `@one_of` rules, keyed by block kind. */
export const REQUIRED_GROUPS: Record<string, RequiredGroup[]> = {
  "volume": [{ fields: ["host", "name"], exclusive: true }],
  "disk": [{ fields: ["size", "from"], exclusive: false }],
};

/** A decorator an author may write on a block, with one row per
 *  declared argument. */
export interface DecoratorDesc {
  name: string;
  /** The decorator declaration's doc comment. */
  doc: string;
  /** It may be written more than once on one block. */
  repeatable: boolean;
  fields: FieldDesc[];
}

/** The decorators each block kind accepts, keyed by block kind. */
export const BLOCK_DECORATORS: Record<string, DecoratorDesc[]> = {
  "vm": [
    { name: "dev", doc: "Designates this machine as a development environment (PRD §19.1): vmlab\npublishes it as an SSH endpoint an editor attaches *into*, and syncs a\nworkspace onto it. A decorator rather than a child block because it states\nsomething *about* the machine — nothing it carries is a setting the guest\never sees.\n\nEvery argument is optional and a bare `@dev` is a complete, attachable dev\nmachine; unset arguments resolve `@dev` > profile > vmlab floor, in\n`src/dev.rs`. Any number of machines may carry it and zero is normal.", repeatable: false, fields: [{ key: "default", label: "Default", doc: "Make this the lab's default dev machine; at most one per lab, and the only `@dev` machine is it implicitly", type: "flag" }, { key: "workspace", label: "Workspace", doc: "Host directory whose contents sync into the workspace, relative to the lab root", type: "text" }, { key: "workspace_guest", label: "Workspace guest", doc: "Guest path the workspace lands at; inherited from the profile (`C:\\src` / `/src`) if omitted", type: "text" }] },
  ],
  "container": [
    { name: "dev", doc: "Designates this machine as a development environment (PRD §19.1): vmlab\npublishes it as an SSH endpoint an editor attaches *into*, and syncs a\nworkspace onto it. A decorator rather than a child block because it states\nsomething *about* the machine — nothing it carries is a setting the guest\never sees.\n\nEvery argument is optional and a bare `@dev` is a complete, attachable dev\nmachine; unset arguments resolve `@dev` > profile > vmlab floor, in\n`src/dev.rs`. Any number of machines may carry it and zero is normal.", repeatable: false, fields: [{ key: "default", label: "Default", doc: "Make this the lab's default dev machine; at most one per lab, and the only `@dev` machine is it implicitly", type: "flag" }, { key: "workspace", label: "Workspace", doc: "Host directory whose contents sync into the workspace, relative to the lab root", type: "text" }, { key: "workspace_guest", label: "Workspace guest", doc: "Guest path the workspace lands at; inherited from the profile (`C:\\src` / `/src`) if omitted", type: "text" }] },
  ],
};

/** `vm` block fields. */
export const VM_HARDWARE: FieldDesc[] = [
  { key: "nested", label: "Nested virt", doc: "Enable nested virtualisation (host CPU passthrough)", type: "flag" },
];

/** `vm` block fields. */
export const VM_OVERRIDES: FieldDesc[] = [
  { key: "firmware", label: "Firmware", doc: "Firmware: `ovmf` | `seabios`; inherited from template→profile", type: "enum", options: ["ovmf", "seabios"] },
  { key: "tpm", label: "TPM 2.0", doc: "Enable a TPM 2.0 device; inherited from template→profile", type: "bool3" },
  { key: "secure_boot", label: "Secure boot", doc: "Enable secure boot (OVMF only); inherited from template→profile", type: "bool3" },
  { key: "display", label: "Display", doc: "QEMU display string; inherited from template→profile if omitted", type: "text", placeholder: "e.g. virtio-vga" },
  { key: "disk", label: "Primary disk", doc: "Primary disk size, e.g. `64GiB` — scratch VMs only (rejected on cloned VMs)", type: "bytes", placeholder: "e.g. 64GiB" },
  { key: "floppy", label: "Floppy", doc: "Path to a floppy image to attach (relative to lab root)", type: "text" },
  { key: "qemu_args", label: "QEMU args", doc: "Raw QEMU flags appended last — escape hatch", type: "lines" },
];

/** `nic` block fields. */
export const NIC_FIELDS: FieldDesc[] = [
  { key: "segment", label: "Segment", doc: "Segment name to attach to; required unless `nat = true`", type: "segref" },
  { key: "ip", label: "Static IP", doc: "Static IPv4 (becomes a DHCP reservation); must be in the subnet, unique", type: "text", placeholder: "10.0.0.10" },
  { key: "mac", label: "MAC", doc: "Fixed MAC, e.g. `52:54:00:ab:cd:ef`; generated and persisted otherwise", type: "text", placeholder: "52:54:00:ab:cd:ef" },
];

/** `disk` block fields. */
export const DISK_FIELDS: FieldDesc[] = [
  { key: "name", label: "Name", doc: "Disk identifier; the inline block label", type: "text", required: true },
  { key: "size", label: "Size", doc: "Blank disk size, e.g. `10GiB`; one of `size`/`from` is required", type: "bytes", placeholder: "e.g. 10GiB" },
  { key: "from", label: "From folder", doc: "Folder copied onto a fresh FAT filesystem; one of `size`/`from` is required", type: "text" },
];

/** `share` block fields. */
export const SHARE_FIELDS: FieldDesc[] = [
  { key: "host", label: "Host path", doc: "Host directory to share; must exist (required)", type: "text", required: true },
  { key: "guest", label: "Guest path", doc: "Guest mount path, e.g. `/mnt/src` or `D:\\data` (required)", type: "text", required: true },
  { key: "readonly", label: "Read-only", doc: "Mount read-only (default false)", type: "flag", default: "false" },
  { key: "smb1", label: "SMB1", doc: "Enable the SMB1 dialect + auth relaxation for XP/2003-era guests", type: "flag" },
  { key: "name", label: "Share name", doc: "Share name; derived from the guest path if omitted", type: "text" },
  { key: "transport", label: "Transport", doc: "Transport: auto (default; virtiofs when host + guest support it, else SMB) | virtiofs | smb", type: "enum", options: ["auto", "virtiofs", "smb"], required: true, default: "\"auto\"" },
];

/** `gpu` block fields. */
export const GPU_FIELDS: FieldDesc[] = [
  { key: "mode", label: "Mode", doc: "Mode: `passthrough` | `virgl` | `vulkan` (required)", type: "enum", options: ["passthrough", "virgl", "vulkan"], required: true },
  { key: "address", label: "PCI address", doc: "Host PCI address, e.g. `0000:01:00.0` — required for `passthrough`", type: "text", placeholder: "0000:01:00.0" },
];

/** `web` block fields. */
export const WEB_FIELDS: FieldDesc[] = [
  { key: "port", label: "Guest port", doc: "Guest TCP port serving the HTTP UI (1–65535) (required)", type: "int", required: true, min: 1, max: 65535 },
  { key: "path", label: "Initial path", doc: "Initial path opened in the console (default `/`)", type: "text", placeholder: "/", default: "\"/\"" },
];

/** `auth.method` — the selector the other tables key off. */
export const WEB_AUTH_METHOD: FieldDesc = { key: "method", label: "Method", doc: "Method: `:basic` | `:bearer` | `:header` | `:ntlm` (IIS/AD integrated) | `:form` (cookie capture) (required)", type: "enum", options: ["basic", "bearer", "header", "ntlm", "form"], required: true };

/** `container` block fields. */
export const CONTAINER_RUNTIME: FieldDesc[] = [
  { key: "workdir", label: "Working directory", doc: "Working directory inside the container; image default if omitted", type: "text", placeholder: "/srv/app" },
  { key: "user", label: "User", doc: "User to run as: `uid[:gid]` or a name from the image; image default if omitted", type: "text", placeholder: "1000:1000" },
  { key: "entrypoint", label: "Entrypoint", doc: "Override the image entrypoint (exec form, e.g. [\"/bin/sh\", \"-c\"])", type: "lines" },
  { key: "command", label: "Command", doc: "Override the image cmd (exec form)", type: "lines" },
];

/** `env` block fields. */
export const ENV_FIELDS: FieldDesc[] = [
  { key: "name", label: "Name", doc: "Variable name (required)", type: "text", placeholder: "APP_ENV", required: true },
  { key: "value", label: "Value", doc: "Variable value (required)", type: "text", required: true },
];

/** `volume` block fields. */
export const VOLUME_FIELDS: FieldDesc[] = [
  { key: "host", label: "Host path", doc: "Host path to bind-mount, relative to the lab root; one of `host`/`name` is required", type: "text", placeholder: "data/www" },
  { key: "name", label: "Volume name", doc: "Named volume kept under the lab dir, shared by name, retained until lab destroy; one of `host`/`name`", type: "text", placeholder: "dbdata" },
  { key: "target", label: "Target", doc: "Absolute mount path inside the container (required)", type: "text", placeholder: "/var/lib/data", required: true },
  { key: "read_only", label: "Read-only", doc: "Mount read-only (default false)", type: "flag", default: "false" },
];

/** `port` block fields. */
export const PORT_FIELDS: FieldDesc[] = [
  { key: "host", label: "Host port", doc: "Host port to listen on (1–65535); unique across the lab (required)", type: "int", required: true, min: 1, max: 65535 },
  { key: "container", label: "Container port", doc: "Container port to forward to (1–65535) (required)", type: "int", required: true, min: 1, max: 65535 },
  { key: "proto", label: "Protocol", doc: "Protocol: `tcp` (default) | `udp` | `both`", type: "enum", options: ["tcp", "udp", "both"], required: true, default: "\"tcp\"" },
];

/** `healthcheck` block fields. */
export const HEALTHCHECK_FIELDS: FieldDesc[] = [
  { key: "command", label: "Command", doc: "Probe command run inside the container (exec form); exit 0 = healthy (required)", type: "lines", required: true },
  { key: "interval", label: "Interval (s)", doc: "Time between probes, e.g. `10s` (default 10s)", type: "int", min: 1, default: "10s" },
  { key: "timeout", label: "Timeout (s)", doc: "Per-probe timeout (default 5s)", type: "int", min: 1, default: "5s" },
  { key: "retries", label: "Retries", doc: "Consecutive failures before unhealthy (default 3)", type: "int", min: 1, default: "3" },
  { key: "start_period", label: "Start period (s)", doc: "Grace period after start before failures count (default 10s)", type: "int", min: 0, default: "10s" },
];

/** `segment` block fields. */
export const SEGMENT_GENERAL: FieldDesc[] = [
  { key: "subnet", label: "Subnet", doc: "CIDR; auto-allocated as a /24 from the host pool if omitted", type: "text", placeholder: "10.50.0.0/24" },
];

/** `segment` block fields. */
export const SEGMENT_SERVICES: FieldDesc[] = [
  { key: "dhcp", label: "DHCP", doc: "Enable DHCP (default true)", type: "flag", default: "true" },
];

/** `record` block fields. */
export const RECORD_FIELDS: FieldDesc[] = [
  { key: "name", label: "Name", doc: "DNS name to resolve; wildcards allowed, e.g. `*.internal` (required)", type: "text", required: true },
  { key: "ip", label: "IP", doc: "IPv4 address the name resolves to (required)", type: "text", required: true },
];

/** `sinkhole` block fields. */
export const SINKHOLE_FIELDS: FieldDesc[] = [
  { key: "pattern", label: "Pattern", doc: "DNS name pattern to sink; wildcards allowed (required)", type: "text", placeholder: "*.telemetry.example.com", required: true },
  { key: "mode", label: "Mode", doc: "Response: `nxdomain` (default) | `zero` (resolve to 0.0.0.0)", type: "enum", options: ["nxdomain", "zero"], required: true, default: "\"nxdomain\"" },
];

/** `on` block fields. */
export const HANDLER_FIELDS: FieldDesc[] = [
  { key: "event", label: "Event", doc: "Event name to handle, e.g. `vm.crashed`; the inline block label", type: "event", required: true },
  { key: "run", label: "Handler script", doc: "Path to the handler `.ws` file; must exist and compile (required)", type: "text", placeholder: "scripts/on-crash.ws", required: true },
  { key: "targets", label: "Target machines", doc: "Optional VM/container names; empty handles every occurrence of the event", type: "vmrefs" },
];

/** `auth` block fields, grouped by the value that selects them. */
export const WEB_AUTH_FIELDS: Record<string, FieldDesc[]> = {
  basic: [
    { key: "username", label: "Username", doc: "Username — `:basic`, `:ntlm`, `:form`", type: "text" },
    { key: "password", label: "Password", doc: "Password — `:basic`, `:ntlm`, `:form`", type: "text" },
  ],
  bearer: [
    { key: "token", label: "Token", doc: "Static bearer token — `:bearer`", type: "text" },
  ],
  header: [
    { key: "header", label: "Header name", doc: "Header name, e.g. `X-Api-Key` — `:header`", type: "text" },
    { key: "value", label: "Header value", doc: "Header value — `:header`", type: "text" },
  ],
  ntlm: [
    { key: "username", label: "Username", doc: "Username — `:basic`, `:ntlm`, `:form`", type: "text" },
    { key: "password", label: "Password", doc: "Password — `:basic`, `:ntlm`, `:form`", type: "text" },
    { key: "domain", label: "Domain", doc: "NTLM domain, e.g. `CORP` — `:ntlm` (optional)", type: "text" },
  ],
  form: [
    { key: "username", label: "Username", doc: "Username — `:basic`, `:ntlm`, `:form`", type: "text" },
    { key: "password", label: "Password", doc: "Password — `:basic`, `:ntlm`, `:form`", type: "text" },
    { key: "login_path", label: "Login path", doc: "Login request path, e.g. `/login` — `:form` (required)", type: "text", placeholder: "/login" },
    { key: "login_method", label: "Login method", doc: "Login HTTP method: `POST` (default) | `GET` — `:form`", type: "enum", options: ["POST", "GET"], required: true, default: "\"POST\"" },
    { key: "login_body", label: "Login body", doc: "Login body template; `{user}`/`{pass}` are substituted and escaped — `:form` (required)", type: "text", placeholder: "user={user}&password={pass}" },
    { key: "login_content_type", label: "Content type", doc: "Login body content type: `application/x-www-form-urlencoded` (default) | `application/json` — `:form`", type: "enum", options: ["application/x-www-form-urlencoded", "application/json"], required: true, default: "\"application/x-www-form-urlencoded\"" },
    { key: "fail_redirect", label: "Fail redirect", doc: "Redirect-Location substring that means 'not logged in' (401/403 always retrigger) — `:form`", type: "text" },
  ],
};

