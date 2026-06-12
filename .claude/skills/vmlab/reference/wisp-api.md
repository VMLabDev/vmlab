# vmlab wisp API reference (the `vmlab` module)

Every script starts with `use vmlab`. Scripts are synchronous; all blocking
calls take timeouts and return `Result[..., string]`. Generate
`vmlab.wispi` (`vmlab wispi`) for LSP support when editing scripts.

## Entry points

```rust
// Provision script (provision "x.wisp" {} in vmlab.wcl) and `vmlab run x.wisp`:
fn main(lab: Lab) { ... }       // an Err propagating out fails the provision run (and `vmlab up`)

// Event handler (on "vm.crashed" { run = "x.wisp" }):
fn handle(event: Event, lab: Lab) { ... }   // failures logged, never fatal
```

Events: `vm.starting`, `vm.ready`, `vm.stopped`, `vm.crashed`, `lab.up`,
`lab.down`, `snapshot.created`, `snapshot.restored`, `template.built`,
`lab.daemon_crashed`, `host.disk_low`.

## Free functions

| Function | Notes |
|---|---|
| `vmlab::sleep_ms(ms: int)` | Sleep; call module-qualified (or `use vmlab::sleep_ms`). Prefer `wait_*` methods over fixed sleeps |

## Lab

| Method | Returns | Notes |
|---|---|---|
| `lab.name()` | `string` | Lab name from vmlab.wcl |
| `lab.log(msg: string)` | `unit` | Lab log + live CLI stream |
| `lab.vm(name: string)` | `Result[Vm, string]` | Err if not defined |
| `lab.vms()` | `List[Vm]` | All VMs |
| `lab.segment(name: string)` | `Result[Segment, string]` | Err if not declared |

## Vm — lifecycle & state

| Method | Returns | Notes |
|---|---|---|
| `vm.name()` | `string` | |
| `vm.start()` / `vm.stop()` / `vm.stop_force()` / `vm.restart()` | `Result[unit, string]` | stop = graceful ladder (agent → ACPI → kill) |
| `vm.state()` | `string` | `"stopped"` \| `"starting"` \| `"running"` \| `"stopping"` |
| `vm.is_ready()` | `bool` | Guest agent responding |
| `vm.wait_ready(timeout_secs: int)` | `Result[unit, string]` | Block until agent responds |
| `vm.wait_shutdown(timeout_secs: int)` | `Result[unit, string]` | Block until powered off |
| `vm.ip()` | `Result[string, string]` | Primary NIC IPv4 (DHCP lease / agent) |
| `vm.ip_nic(nic: int)` | `Result[string, string]` | By NIC index (0-based) |

## Vm — snapshots

| Method | Returns |
|---|---|
| `vm.snapshot(name: string)` | `Result[unit, string]` — online or offline per current state |
| `vm.restore(name: string)` | `Result[unit, string]` — resumes running iff taken online |
| `vm.snapshots()` | `Result[List[string], string]` |
| `vm.delete_snapshot(name: string)` | `Result[unit, string]` |

## Vm — keyboard & mouse

| Method | Returns | Notes |
|---|---|---|
| `vm.send_keys(chord: string)` | `Result[unit, string]` | e.g. `"ctrl-alt-del"`, `"enter"`, `"shift-f5"` |
| `vm.type_text(text: string)` | `Result[unit, string]` | ~35 ms/key; `\n`→enter, `\t`→tab; **US ASCII only** |
| `vm.type_text_paced(text: string, delay_ms: int)` | `Result[unit, string]` | Custom inter-key delay |
| `vm.mouse_move(x: int, y: int)` | `Result[unit, string]` | Absolute, scaled to current screen size |
| `vm.mouse_click(button: string)` | `Result[unit, string]` | `"left"` \| `"right"` \| `"middle"` |
| `vm.mouse_drag(x1, y1, x2, y2: int)` | `Result[unit, string]` | Human-ish 8-step drag |

Chord keys are `-`-separated, case-insensitive QMP names with aliases:
`ctrl alt shift win/super/meta enter/return esc del space tab backspace
up down left right home end pgup/pageup pgdn/pagedown insert menu print
pause caps_lock num_lock scroll_lock f1..f12 a-z 0-9`.

## Vm — screen, image matching, OCR

| Method | Returns | Notes |
|---|---|---|
| `vm.screenshot(path: string)` | `Result[string, string]` | Returns saved path. `""` → auto-named in `.vmlab/screenshots/` |
| `vm.wait_for_image(image: string, timeout_secs: int)` | `Result[Match, string]` | Threshold 0.9, polls every 1 s |
| `vm.wait_for_image_opts(image, timeout_secs, threshold: float, region: List[int])` | `Result[Match, string]` | `region` = `[x, y, w, h]` or `[]` |
| `vm.wait_for_any(images: List[string], timeout_secs: int)` | `Result[Match, string]` | First image to appear wins |
| `vm.find_image(image: string)` | `Result[Option[Match], string]` | Single shot, no waiting |
| `vm.ocr()` | `Result[string, string]` | Tesseract over the whole screen |
| `vm.ocr_region(region: List[int])` | `Result[string, string]` | Cropped `[x, y, w, h]` |
| `vm.wait_for_text(pattern: string, timeout_secs: int)` | `Result[Match, string]` | Regex over OCR text; only `m.text`/`m.score` meaningful (coords are 0) |

Reference images resolve relative to the **lab root**; convention is an
`images/` folder beside `vmlab.wcl`. Matching is normalized
cross-correlation on grayscale.

## Vm — guest agent

| Method | Returns | Notes |
|---|---|---|
| `vm.exec(cmd: string, args: List[string])` | `Result[ExecResult, string]` | 120 s timeout |
| `vm.exec_timeout(cmd, args, timeout_secs: int)` | `Result[ExecResult, string]` | Custom timeout |
| `vm.copy_to(local: string, guest_path: string)` | `Result[unit, string]` | local relative to lab root; guest path absolute |
| `vm.copy_from(guest_path: string, local: string)` | `Result[unit, string]` | Parent dirs created on host |

## Segment (runtime network mutation)

| Method | Returns | Notes |
|---|---|---|
| `seg.name()` | `string` | |
| `seg.dns_set(name: string, ip: string)` | `Result[int, string]` | Static DNS entry → rule id |
| `seg.dns_sinkhole(pattern: string)` | `Result[int, string]` | Wildcards OK; always NXDOMAIN |
| `seg.dns_clear(rule_id: int)` | `Result[bool, string]` | |
| `seg.block(cidr: string)` | `Result[int, string]` | CIDR or bare IP |
| `seg.block_port(cidr: string, proto: string, port: int)` | `Result[int, string]` | proto: `"tcp"` \| `"udp"` \| `"icmp"` |
| `seg.unblock(rule_id: int)` | `Result[bool, string]` | |
| `seg.redirect(from: string, to: string)` | `Result[int, string]` | DNAT `"ip[:port]"` → `"ip[:port]"` |
| `seg.forward(host_port: int, vm: string, guest_port: int)` | `Result[int, string]` | TCP only; VM needs a lease already |
| `seg.rules()` | `Result[string, string]` | JSON list of rules |
| `seg.route_to(other)` / `seg.unroute_to(other)` | `Result[unit, string]` | **Always Err — not yet available from scripts** |

## Data types

```rust
struct Match     { x: int, y: int, w: int, h: int, score: float,
                   cx: int, cy: int,      // center — feed to mouse_move
                   text: string }         // set by wait_for_text only
struct ExecResult { exit_code: int, stdout: string, stderr: string }
struct Event      { name: string, vm: string, data: string }   // data = JSON payload as text
```

## Idiomatic patterns (from examples/ad-lab/scripts/)

```rust
use vmlab

fn main(lab: Lab) {
    lab.log("setting up " + lab.name())

    let Ok(dc) = lab.vm("dc01") else {
        lab.log("dc01 is not defined")
        return
    }

    match dc.wait_ready(600) {
        Ok(_)  => lab.log("dc01 agent is responding"),
        Err(e) => { lab.log("dc01 never became ready: " + e); return }
    }

    match dc.exec("ipconfig", ["/all"]) {
        Ok(r)  => lab.log(r.stdout),
        Err(e) => lab.log("ipconfig failed: " + e),
    }

    // Screen-driven step: wait for a UI element, click its center.
    match dc.wait_for_image("images/promote-button.png", 120) {
        Ok(m) => {
            let mv = dc.mouse_move(m.cx, m.cy)   // bind unused Results
            let cl = dc.mouse_click("left")
            lab.log("clicked the promote button")
        }
        Err(e) => lab.log("promote button not found (skipping): " + e),
    }
}
```

```rust
use vmlab

fn handle(event: Event, lab: Lab) {
    lab.log("crash handler fired for " + event.vm + " (" + event.name + ")")
    let Ok(vm) = lab.vm(event.vm) else { return }
    match vm.screenshot("") {
        Ok(path) => lab.log("saved crash screenshot: " + path),
        Err(e)   => lab.log("could not screenshot: " + e),
    }
}
```

Source of truth: PRD §10; `src/scripting/mod.rs` (every registration),
`src/scripting/keymap.rs`, `examples/ad-lab/scripts/`. Snippets here must
type-check — verify with `vmlab validate` or
`crate::scripting::check_script_source` tests.
