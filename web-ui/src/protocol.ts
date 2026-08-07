// generated from `src/proto/vocab.rs` — run `just proto-generate`
//
// Edit the vocabulary, not this file.

/** Why a request failed. The daemon sends this alongside the message, and the REST
 *  layer maps it to the HTTP status — so branch on the code, never on the prose. */
export type ErrorCode =
  | "unknown_command"
  | "invalid_argument"
  | "not_found"
  | "conflict"
  | "unsupported"
  | "failed"
  | "internal"
;

/** An error body from any `/api` endpoint. */
export interface ApiError {
  error: string;
  code?: ErrorCode;
}

/** The most one guest file transfer may carry inline, in bytes. The console shows
 *  this before a transfer is attempted; the daemon refuses anything over it. */
export const INLINE_FILE_LIMIT = 8388608;

/** Lab-wide actions: `POST /api/labs/{lab}/{action}`. */
export type LabAction =
  /** `up` */
  | "up"
  /** `down` */
  | "down"
  /** `destroy` */
  | "destroy"
  /** `pull` */
  | "pull"
;

/** Per-machine actions: `POST /api/labs/{lab}/machines/{machine}/{action}`. */
export type MachineAction =
  /** `machine.start` */
  | "start"
  /** `machine.stop` */
  | "stop"
  /** `machine.restart` */
  | "restart"
  /** `machine.destroy` */
  | "destroy"
;

/** Every command the lab daemon's socket serves. */
export type LabCommand =
  | "ping"
  | "status"
  | "dns.table"
  | "up"
  | "pull"
  | "pull.cancel"
  | "run"
  | "down"
  | "destroy"
  | "machine.start"
  | "machine.stop"
  | "machine.restart"
  | "machine.destroy"
  | "machine.capabilities"
  | "machine.ip"
  | "machine.screenshot"
  | "machine.sendkeys"
  | "machine.mouse_move"
  | "machine.mouse_click"
  | "machine.mouse_drag"
  | "machine.ocr"
  | "machine.find_image"
  | "machine.exec"
  | "machine.osinfo"
  | "machine.tty_open"
  | "machine.ssh_open"
  | "machine.repair_agent"
  | "machine.tty_resize"
  | "machine.push_file"
  | "machine.pull_file"
  | "machine.tail"
  | "machine.eventlog"
  | "machine.stats"
  | "machine.clipboard_get"
  | "machine.clipboard_set"
  | "machine.logs"
  | "web.forward"
  | "playbook.list"
  | "playbook.check"
  | "playbook.apply"
  | "playbook.op_status"
  | "snapshot.take"
  | "snapshot.restore"
  | "snapshot.delete"
  | "snapshot.list"
  | "workspace.flush"
  | "workspace.resolve"
  | "workspace.diff"
  | "shutdown"
;

/** Every command the supervisor socket serves. */
export type SupervisorCommand =
  | "ping"
  | "version"
  | "fastpath"
  | "status"
  | "lab.ensure"
  | "lab.release"
  | "lab.restart"
  | "global.attach"
  | "global.detach"
  | "global.list"
  | "template.list"
  | "template.remote"
  | "template.build"
  | "template.stop_build"
  | "template.push"
  | "template.op_status"
  | "template.console_path"
  | "store.list"
  | "store.remove"
  | "store.prune"
  | "store.export"
  | "store.import"
  | "store.pull"
  | "store.push"
  | "store.stop_push"
  | "registry.search"
  | "registry.login"
  | "registry.namespaces"
  | "registry.namespace_add"
  | "registry.namespace_remove"
  | "shutdown"
;

