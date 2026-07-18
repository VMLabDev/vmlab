// Descriptor tables for the inspector forms — the whole schema surface as
// data, one entry per field, with the schema.wcl @doc text as help. The
// generic FieldRow renders each entry; BlockForm renders a table.
//
// Paired with src/config/schema.wcl — keep in sync when the schema grows.

export type FieldType =
  | "text" // utf8 → Input
  | "int" // i64 → Input[number]
  | "bool3" // bool? with inherited default → default/on/off ToggleGroup
  | "flag" // bool the schema defaults → Toggle
  | "bytes" // std.ByteSize → ByteSizeInput
  | "enum" // fixed options → Select (+ "default" when optional)
  | "lines" // list<utf8> free-form → Textarea, one per line
  | "segref" // one segment name
  | "segrefs" // several segment names
  | "vmref" // one VM name
  | "vmrefs" // several VM names
  | "event"; // lifecycle event picker

export interface FieldDesc {
  key: string;
  label: string;
  doc: string;
  type: FieldType;
  options?: string[];
  placeholder?: string;
  /** Optional enum gets an extra "default" (unset) choice. */
  required?: boolean;
  min?: number;
  max?: number;
}

export interface Section {
  title: string;
  fields: FieldDesc[];
}

// --- vm ---------------------------------------------------------------------
// The General tab's template picker (arch/profile fold into it) and the
// depends-on list are dedicated components, not descriptor rows; the
// Hardware tab's cpu/memory sliders likewise.

export const VM_HARDWARE: FieldDesc[] = [
  {
    key: "nested",
    label: "Nested virt",
    doc: "Enable nested virtualisation (host CPU passthrough)",
    type: "flag",
  },
];

/** Everything normally supplied by the template/profile, plus escape
 *  hatches — the trailing Overrides tab. */
export const VM_OVERRIDES: FieldDesc[] = [
  {
    key: "firmware",
    label: "Firmware",
    doc: "`ovmf` | `seabios`; inherited from template→profile",
    type: "enum",
    options: ["ovmf", "seabios"],
  },
  {
    key: "tpm",
    label: "TPM 2.0",
    doc: "Enable a TPM 2.0 device; inherited from template→profile",
    type: "bool3",
  },
  {
    key: "secure_boot",
    label: "Secure boot",
    doc: "Enable secure boot (OVMF only); inherited from template→profile",
    type: "bool3",
  },
  {
    key: "display",
    label: "Display",
    doc: "QEMU display string; inherited from template→profile if omitted",
    type: "text",
    placeholder: "e.g. virtio-vga",
  },
  {
    key: "disk",
    label: "Primary disk",
    doc: "Primary disk size, e.g. `64GiB` — scratch VMs only",
    type: "bytes",
    placeholder: "e.g. 64GiB",
  },
  {
    key: "floppy",
    label: "Floppy",
    doc: "Path to a floppy image to attach (relative to lab root)",
    type: "text",
  },
  {
    key: "qemu_args",
    label: "QEMU args",
    doc: "Raw QEMU flags appended last — escape hatch (one per line)",
    type: "lines",
  },
];

// --- vm children --------------------------------------------------------------

// NAT attachment (and port isolation) are wired on the canvas / raw config,
// not per-NIC form fields — the form covers segment, address, MAC.
export const NIC_FIELDS: FieldDesc[] = [
  {
    key: "segment",
    label: "Segment",
    doc: "Segment name to attach to; required unless NAT",
    type: "segref",
  },
  {
    key: "ip",
    label: "Static IP",
    doc: "Static IPv4 (a DHCP reservation); must be in the subnet, unique",
    type: "text",
    placeholder: "10.0.0.10",
  },
  {
    key: "mac",
    label: "MAC",
    doc: "Fixed MAC; generated and persisted otherwise",
    type: "text",
    placeholder: "52:54:00:ab:cd:ef",
  },
];

export const DISK_FIELDS: FieldDesc[] = [
  {
    key: "name",
    label: "Name",
    doc: "Disk identifier; the inline block label",
    type: "text",
    required: true,
  },
  {
    key: "size",
    label: "Size",
    doc: "Blank disk size, e.g. `10GiB`; one of size/from is required",
    type: "bytes",
    placeholder: "e.g. 10GiB",
  },
  {
    key: "from",
    label: "From folder",
    doc: "Folder copied onto a fresh FAT filesystem",
    type: "text",
  },
];

export const SHARE_FIELDS: FieldDesc[] = [
  {
    key: "host",
    label: "Host path",
    doc: "Host directory to share; must exist (required)",
    type: "text",
    required: true,
  },
  {
    key: "guest",
    label: "Guest path",
    doc: "Guest mount path, e.g. `/mnt/src` or `D:\\data` (required)",
    type: "text",
    required: true,
  },
  {
    key: "readonly",
    label: "Read-only",
    doc: "Mount read-only (default false)",
    type: "flag",
  },
  {
    key: "smb1",
    label: "SMB1",
    doc: "Enable the SMB1 dialect for XP/2003-era guests",
    type: "flag",
  },
  {
    key: "name",
    label: "Share name",
    doc: "SMB share name; derived from the guest path if omitted",
    type: "text",
  },
];

export const GPU_FIELDS: FieldDesc[] = [
  {
    key: "mode",
    label: "Mode",
    doc: "`passthrough` | `virgl` | `vulkan` (required)",
    type: "enum",
    options: ["passthrough", "virgl", "vulkan"],
    required: true,
  },
  {
    key: "address",
    label: "PCI address",
    doc: "Host PCI address, e.g. `0000:01:00.0` — required for passthrough",
    type: "text",
    placeholder: "0000:01:00.0",
  },
];

export const ENV_FIELDS: FieldDesc[] = [
  {
    key: "name",
    label: "Name",
    doc: "Variable name (required)",
    type: "text",
    required: true,
    placeholder: "APP_ENV",
  },
  {
    key: "value",
    label: "Value",
    doc: "Variable value (required)",
    type: "text",
    required: true,
  },
];

export const VOLUME_FIELDS: FieldDesc[] = [
  {
    key: "host",
    label: "Host path",
    doc: "Host path to bind-mount, relative to the lab root; exactly one of host path / volume name",
    type: "text",
    placeholder: "data/www",
  },
  {
    key: "name",
    label: "Volume name",
    doc: "Named volume kept under the lab dir, shared by name; exactly one of host path / volume name",
    type: "text",
    placeholder: "dbdata",
  },
  {
    key: "target",
    label: "Target",
    doc: "Absolute mount path inside the container (required)",
    type: "text",
    required: true,
    placeholder: "/var/lib/data",
  },
  {
    key: "read_only",
    label: "Read-only",
    doc: "Mount read-only (default false)",
    type: "flag",
  },
];

export const PORT_FIELDS: FieldDesc[] = [
  {
    key: "host",
    label: "Host port",
    doc: "Host port to listen on (1–65535); unique across the lab (required)",
    type: "int",
    required: true,
    min: 1,
    max: 65535,
  },
  {
    key: "container",
    label: "Container port",
    doc: "Container port to forward to (1–65535) (required)",
    type: "int",
    required: true,
    min: 1,
    max: 65535,
  },
  {
    key: "proto",
    label: "Protocol",
    doc: "`tcp` (default) | `udp` | `both`",
    type: "enum",
    options: ["tcp", "udp", "both"],
    required: true,
  },
];

export const WEB_FIELDS: FieldDesc[] = [
  {
    key: "port",
    label: "Guest port",
    doc: "Guest TCP port serving the HTTP UI (1–65535) (required)",
    type: "int",
    required: true,
    min: 1,
    max: 65535,
  },
  {
    key: "path",
    label: "Initial path",
    doc: "Initial path opened in the console (default `/`)",
    type: "text",
    placeholder: "/",
  },
];

/** The auth `method` selector (drives which other fields show). */
export const WEB_AUTH_METHOD: FieldDesc = {
  key: "method",
  label: "Method",
  doc: "How the proxy authenticates to the guest app so its login never prompts",
  type: "enum",
  options: ["basic", "bearer", "header", "ntlm", "form"],
  required: true,
};

/** Per-method credential fields (keyed by `method`). */
export const WEB_AUTH_FIELDS: Record<string, FieldDesc[]> = {
  basic: [
    { key: "username", label: "Username", doc: "Basic-auth username", type: "text" },
    { key: "password", label: "Password", doc: "Basic-auth password", type: "text" },
  ],
  bearer: [
    { key: "token", label: "Token", doc: "Static bearer token", type: "text" },
  ],
  header: [
    { key: "header", label: "Header name", doc: "e.g. `X-Api-Key`", type: "text" },
    { key: "value", label: "Header value", doc: "The injected header value", type: "text" },
  ],
  ntlm: [
    { key: "username", label: "Username", doc: "AD/IIS username", type: "text" },
    { key: "password", label: "Password", doc: "AD/IIS password", type: "text" },
    { key: "domain", label: "Domain", doc: "NTLM domain, e.g. `CORP` (optional)", type: "text" },
  ],
  form: [
    { key: "username", label: "Username", doc: "Form-login username", type: "text" },
    { key: "password", label: "Password", doc: "Form-login password", type: "text" },
    {
      key: "login_path",
      label: "Login path",
      doc: "Login request path, e.g. `/login` (required)",
      type: "text",
      placeholder: "/login",
    },
    {
      key: "login_method",
      label: "Login method",
      doc: "`POST` (default) | `GET`",
      type: "enum",
      options: ["POST", "GET"],
    },
    {
      key: "login_body",
      label: "Login body",
      doc: "Template; `{user}`/`{pass}` are substituted and escaped",
      type: "text",
      placeholder: "user={user}&password={pass}",
    },
    {
      key: "login_content_type",
      label: "Content type",
      doc: "`application/x-www-form-urlencoded` (default) | `application/json`",
      type: "text",
    },
    {
      key: "fail_redirect",
      label: "Fail redirect",
      doc: "Redirect-Location substring meaning 'not logged in' (401/403 always retrigger)",
      type: "text",
    },
  ],
};

export const HEALTHCHECK_FIELDS: FieldDesc[] = [
  {
    key: "command",
    label: "Command",
    doc: "Probe command run inside the container (exec form, one argument per line); exit 0 = healthy",
    type: "lines",
  },
  {
    key: "interval",
    label: "Interval (s)",
    doc: "Seconds between probes (default 10)",
    type: "int",
    min: 1,
  },
  {
    key: "timeout",
    label: "Timeout (s)",
    doc: "Per-probe timeout in seconds (default 5)",
    type: "int",
    min: 1,
  },
  {
    key: "retries",
    label: "Retries",
    doc: "Consecutive failures before unhealthy (default 3)",
    type: "int",
    min: 1,
  },
  {
    key: "start_period",
    label: "Start period (s)",
    doc: "Grace period in seconds after start before failures count (default 10)",
    type: "int",
    min: 0,
  },
];

// --- segment -------------------------------------------------------------------

export const SEGMENT_GENERAL: FieldDesc[] = [
  {
    key: "subnet",
    label: "Subnet",
    doc: "CIDR; auto-allocated as a /24 from the host pool if omitted",
    type: "text",
    placeholder: "10.50.0.0/24",
  },
  // mtu renders as a dedicated SliderRow in the segment inspector.
];

export const SEGMENT_SERVICES: FieldDesc[] = [
  {
    key: "dhcp",
    label: "DHCP",
    doc: "Enable DHCP (default true)",
    type: "flag",
  },
];

export const RECORD_FIELDS: FieldDesc[] = [
  {
    key: "name",
    label: "Name",
    doc: "DNS name to resolve; wildcards allowed, e.g. `*.internal`",
    type: "text",
    required: true,
  },
  {
    key: "ip",
    label: "IP",
    doc: "IPv4 address the name resolves to (required)",
    type: "text",
    required: true,
  },
];

export const SINKHOLE_FIELDS: FieldDesc[] = [
  {
    key: "pattern",
    label: "Pattern",
    doc: "DNS name pattern to sink; wildcards allowed (required)",
    type: "text",
    required: true,
    placeholder: "*.telemetry.example.com",
  },
  {
    key: "mode",
    label: "Mode",
    doc: "`nxdomain` (default) | `zero` (resolve to 0.0.0.0)",
    type: "enum",
    options: ["nxdomain", "zero"],
    required: true,
  },
];

// --- lab children ----------------------------------------------------------------

export const PROVISION_FIELDS: FieldDesc[] = [
  {
    key: "script",
    label: "Script",
    doc: "Path to the `.ws` file; must exist and compile",
    type: "text",
    required: true,
    placeholder: "scripts/setup.ws",
  },
  {
    key: "vms",
    label: "Scoped to VMs",
    doc: "VM names this script is scoped to (gates their `depends_on`)",
    type: "vmrefs",
  },
];

export const HANDLER_FIELDS: FieldDesc[] = [
  {
    key: "event",
    label: "Event",
    doc: "Event name to handle, e.g. `vm.crashed`",
    type: "event",
    required: true,
  },
  {
    key: "run",
    label: "Handler script",
    doc: "Path to the handler `.ws` file; must exist and compile",
    type: "text",
    required: true,
    placeholder: "scripts/on-crash.ws",
  },
  {
    key: "targets",
    label: "Target machines",
    doc: "Optional VM/container names; empty handles every occurrence of the event",
    type: "vmrefs",
  },
];

// The lab block has no form fields in the UI (the `gui` VNC-viewer default
// stays raw-config-only); the lab inspector is its child collections.
