//! The wscript scripting surface (PRD §10): vmlab's host module exposing
//! lab/VM/segment handles to provision scripts, event handlers, and ad-hoc
//! runs. Scripts are daemon-unaware; the wscript VM is synchronous, so scripts
//! execute on blocking threads and host methods bridge into the lab
//! daemon's tokio runtime via `Handle::block_on`.

mod runner;
pub mod terminal;

use crate::sync::LockRecover;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use wscript::{Context, Module, Script};

use crate::labd::display::Display;
use crate::labd::lab::LabRuntime;
use crate::labd::machine::{Machine, MachineKind};
use crate::labd::vm::PowerState;
use crate::vision;

pub use runner::{OutputSink, ScriptOwner, run_event_handler, run_script_file, run_script_source};

/// Convention: reference images resolve relative to the lab root, typically
/// `images/` beside vmlab.wcl (PRD §10.3).
const SCREENSHOT_DIR: &str = "screenshots";

// ---------------------------------------------------------------------------
// Script-visible types
// ---------------------------------------------------------------------------

/// The lab handle every script receives (PRD §10.1).
#[derive(Script)]
#[script(name = "Lab")]
#[script(opaque)]
pub struct LabHandle {
    pub(crate) runtime: Arc<LabRuntime>,
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) output: OutputSink,
    /// Directory the running script lives in. Relative reference-image and
    /// screenshot paths resolve against this, so a provision can ship its
    /// reference crops next to itself (the build runs from a separate work
    /// dir, where `runtime.root` points, so that base would not find them).
    pub(crate) ref_base: Arc<std::path::PathBuf>,
    /// The VM this script belongs to, fetched with `lab.this_vm()`: the
    /// machine whose `provision {}` block declared it, or the VM a template
    /// first-boot provision runs against. `None` for handlers and
    /// `vmlab script`.
    pub(crate) owner: Option<runner::ScriptOwner>,
}

impl LabHandle {
    /// Whether `name` is the VM whose first-boot provision is the running
    /// script — the only case where full readiness is unreachable and
    /// `is_ready`/`wait_ready` must mean agent-level readiness.
    fn owns_first_boot(&self, name: &str) -> bool {
        self.owner
            .as_ref()
            .is_some_and(|o| o.first_boot && o.vm == name)
    }
}

/// A machine handle (PRD §10.3): the entry point to lifecycle, snapshots,
/// input, screen matching and the guest agent, for a VM or a container alike.
///
/// One handle over [`Machine`], not one per kind. What a particular machine
/// cannot do is reported at call time and names the capability — `screenshot`
/// on a machine with no display fails with "machine `api` has no display",
/// never "no such method" and never "containers cannot have displays".
#[derive(Script)]
#[script(name = "Machine")]
#[script(opaque)]
pub struct MachineHandle {
    pub(crate) machine: Arc<dyn Machine>,
    pub(crate) runtime: Arc<LabRuntime>,
    pub(crate) rt: tokio::runtime::Handle,
    /// Last pointer position, for the VNC input transport: RFB PointerEvent
    /// always carries x,y, but the API splits `mouse_move`/`mouse_click`, so
    /// a click reuses the position the preceding move set.
    pub(crate) last_pointer: Arc<std::sync::Mutex<(i64, i64)>>,
    /// Directory the running script lives in (see [`LabHandle::ref_base`]).
    pub(crate) ref_base: Arc<std::path::PathBuf>,
    /// True when this handle targets the machine whose own first-boot
    /// provision is the running script. Full readiness is unreachable until
    /// that script returns (the poller defers the ready flag), so `is_ready` /
    /// `wait_ready` on this handle mean **agent-level** readiness — a
    /// first-boot script that reboots its guest can wait for it to come back.
    /// Everywhere else they mean full readiness.
    pub(crate) first_boot_gated: bool,
}

/// A segment handle (PRD §10.2).
#[derive(Script)]
#[script(name = "Segment")]
#[script(opaque)]
pub struct SegmentHandle {
    pub(crate) segment: String,
    pub(crate) runtime: Arc<LabRuntime>,
    pub(crate) rt: tokio::runtime::Handle,
}

/// Result of `vm.exec` (PRD §10.3).
#[derive(Script, Clone)]
pub struct ExecResult {
    pub exit_code: i64,
    pub stdout: String,
    pub stderr: String,
}

/// One guest metrics sample (`vm.stats()` / `container.stats()`, via the
/// vmlab-agent `metrics` feature).
#[derive(Script, Clone)]
pub struct GuestStats {
    pub cpu_pct: f64,
    pub mem_used: i64,
    pub mem_total: i64,
    pub disks: Vec<DiskStat>,
}

/// One mounted filesystem in [`GuestStats`].
#[derive(Script, Clone)]
pub struct DiskStat {
    pub mount: String,
    pub used: i64,
    pub total: i64,
}

impl From<crate::labd::vm_agent::MetricsSnapshot> for GuestStats {
    fn from(m: crate::labd::vm_agent::MetricsSnapshot) -> Self {
        GuestStats {
            cpu_pct: m.cpu_pct as f64,
            mem_used: m.mem_used as i64,
            mem_total: m.mem_total as i64,
            disks: m
                .disks
                .into_iter()
                .map(|d| DiskStat {
                    mount: d.mount,
                    used: d.used as i64,
                    total: d.total as i64,
                })
                .collect(),
        }
    }
}

/// An image/text match: location + score, usable to anchor a relative
/// mouse click (PRD §10.3).
#[derive(Script, Clone)]
#[script(name = "Match")]
pub struct ScriptMatch {
    pub x: i64,
    pub y: i64,
    pub w: i64,
    pub h: i64,
    pub score: f64,
    /// Center point, for `vm.mouse_move(m.cx, m.cy)`.
    pub cx: i64,
    pub cy: i64,
    /// For wait_for_text: the matched text.
    pub text: String,
}

impl From<vision::Match> for ScriptMatch {
    fn from(m: vision::Match) -> Self {
        let (cx, cy) = m.center();
        ScriptMatch {
            x: m.x as i64,
            y: m.y as i64,
            w: m.w as i64,
            h: m.h as i64,
            score: m.score,
            cx: cx as i64,
            cy: cy as i64,
            text: String::new(),
        }
    }
}

/// Event payload for handler scripts (PRD §10.4: handlers receive
/// `(event, lab)`). `data` is the JSON payload as text.
#[derive(Script, Clone)]
#[script(name = "Event")]
pub struct EventData {
    pub name: String,
    pub vm: String,
    pub data: String,
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn estr(e: impl std::fmt::Display) -> String {
    format!("{e:#}")
}

impl MachineHandle {
    fn block<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.rt.block_on(fut)
    }

    fn name(&self) -> &str {
        self.machine.name()
    }

    /// Relative local paths resolve against the running script's directory.
    fn resolve_ref(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            self.ref_base.join(p)
        }
    }

    /// This machine's framebuffer, or an error naming the capability.
    ///
    /// The gate is the capability probe, never the machine kind: the day a
    /// container reports a display, every screen method below works on it
    /// unchanged. The message says what this machine does not offer, not what
    /// its kind could never have.
    fn display(&self) -> Result<Display, String> {
        self.machine
            .clone()
            .display()
            .ok_or_else(|| format!("machine `{}` has no display", self.name()))
    }

    /// `exec` / `exec_timeout` over the vmlab-agent transport (streamed,
    /// captured output).
    fn exec(
        &self,
        cmd: String,
        args: Vec<String>,
        timeout: Duration,
    ) -> Result<ExecResult, String> {
        self.block(async {
            let agent = self.machine.agent().await.map_err(estr)?;
            let mut argv = vec![cmd];
            argv.extend(args);
            let r = agent
                .exec(argv, vec![], None, None, timeout)
                .await
                .map_err(estr)?;
            Ok(ExecResult {
                exit_code: r.exit_code as i64,
                stdout: String::from_utf8_lossy(&r.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&r.stderr).into_owned(),
            })
        })
    }

    fn match_opts(threshold: f64, region: Vec<i64>) -> Result<vision::MatchOptions, String> {
        let region = match region.len() {
            0 => None,
            4 => Some((
                region[0].max(0) as u32,
                region[1].max(0) as u32,
                region[2].max(0) as u32,
                region[3].max(0) as u32,
            )),
            n => return Err(format!("region needs [x, y, w, h], got {n} elements")),
        };
        Ok(vision::MatchOptions { threshold, region })
    }

    fn find_once(
        &self,
        refs: &[String],
        opts: &vision::MatchOptions,
    ) -> Result<Option<ScriptMatch>, String> {
        let display = self.display()?;
        let screen = self.block(display.grab()).map_err(estr)?;
        for r in refs {
            let path = self.resolve_ref(r);
            let template = vision::load_screen(&path)
                .map_err(|e| format!("reference image {}: {e:#}", path.display()))?;
            if let Some(m) = vision::find_template(&screen, &template, opts) {
                return Ok(Some(m.into()));
            }
        }
        Ok(None)
    }

    fn wait_for(
        &self,
        refs: &[String],
        threshold: f64,
        region: Vec<i64>,
        timeout_secs: i64,
        interval_ms: i64,
    ) -> Result<ScriptMatch, String> {
        let opts = Self::match_opts(threshold, region)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(timeout_secs.max(0) as u64);
        loop {
            if let Some(m) = self.find_once(refs, &opts)? {
                return Ok(m);
            }
            if std::time::Instant::now() >= deadline {
                return Err(format!(
                    "timed out after {timeout_secs}s waiting for {:?} on {}",
                    refs,
                    self.name()
                ));
            }
            std::thread::sleep(Duration::from_millis(interval_ms.max(50) as u64));
        }
    }
}

impl LabHandle {
    fn handle_for(&self, machine: Arc<dyn Machine>) -> MachineHandle {
        let first_boot_gated = self.owns_first_boot(machine.name());
        MachineHandle {
            machine,
            runtime: self.runtime.clone(),
            rt: self.rt.clone(),
            last_pointer: Default::default(),
            ref_base: self.ref_base.clone(),
            first_boot_gated,
        }
    }
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

/// Build the `lab` host module (PRD §10). All state rides inside the opaque
/// handles, so the same module serves compile-checking and live execution.
pub fn lab_module() -> Module {
    let mut m = Module::new("vmlab");
    m.doc("vmlab lab/VM/segment API (PRD §10)");

    m.fn_("sleep_ms", |ms: i64| {
        std::thread::sleep(Duration::from_millis(ms.max(0) as u64));
    });

    // Host environment variable, or "" when unset. Lets build/provision
    // scripts carry operator toggles (e.g. VMLAB_SKIP_UPDATES=1 for fast
    // test template builds) without schema changes.
    m.fn_("env", |name: &str| -> String {
        std::env::var(name).unwrap_or_default()
    });

    // -- Lab (§10.1) ---------------------------------------------------------
    m.ty::<LabHandle>()
        .method("name", |l: &LabHandle| l.runtime.name.clone())
        .method("log", |l: &LabHandle, msg: &str| {
            (l.output)(format!("{msg}\n"));
        })
        .method(
            "machine",
            |l: &LabHandle, name: &str| -> Result<MachineHandle, String> {
                Ok(l.handle_for(l.runtime.machine(name).map_err(estr)?))
            },
        )
        .method("machines", |l: &LabHandle| -> Vec<MachineHandle> {
            l.runtime.machines().map(|m| l.handle_for(m)).collect()
        })
        // `vm` and `container` are `machine` with a kind check, kept because
        // they read well in a script that knows what it declared — and because
        // the error ("x is a container") is a better one than "no such
        // machine". The handle they return is the same.
        .method(
            "vm",
            |l: &LabHandle, name: &str| -> Result<MachineHandle, String> {
                let m = l.runtime.machine(name).map_err(estr)?;
                if m.kind() != MachineKind::Vm {
                    return Err(format!("\"{name}\" is a container — use lab.container()"));
                }
                Ok(l.handle_for(m))
            },
        )
        .method(
            "this_vm",
            |l: &LabHandle| -> Result<MachineHandle, String> {
                let owner = l.owner.as_ref().ok_or(
                    "this_vm() is only available inside a machine's own provision or a template \
                 first-boot script",
                )?;
                Ok(l.handle_for(l.runtime.machine(&owner.vm).map_err(estr)?))
            },
        )
        .method("vms", |l: &LabHandle| -> Vec<MachineHandle> {
            l.runtime
                .machines()
                .filter(|m| m.kind() == MachineKind::Vm)
                .map(|m| l.handle_for(m))
                .collect()
        })
        .method(
            "container",
            |l: &LabHandle, name: &str| -> Result<MachineHandle, String> {
                let m = l.runtime.machine(name).map_err(estr)?;
                if m.kind() != MachineKind::Container {
                    return Err(format!("\"{name}\" is a vm — use lab.vm()"));
                }
                Ok(l.handle_for(m))
            },
        )
        .method("containers", |l: &LabHandle| -> Vec<MachineHandle> {
            l.runtime
                .machines()
                .filter(|m| m.kind() == MachineKind::Container)
                .map(|m| l.handle_for(m))
                .collect()
        })
        .method(
            "segment",
            |l: &LabHandle, name: &str| -> Result<SegmentHandle, String> {
                let exists = l
                    .rt
                    .block_on(async { l.runtime.network.lock().await.segments.contains_key(name) });
                if !exists {
                    return Err(format!(
                        "no segment \"{name}\" in lab \"{}\"",
                        l.runtime.name
                    ));
                }
                Ok(SegmentHandle {
                    segment: name.to_string(),
                    runtime: l.runtime.clone(),
                    rt: l.rt.clone(),
                })
            },
        );

    // -- Segment (§10.2) -----------------------------------------------------
    m.ty::<SegmentHandle>()
        .method("name", |s: &SegmentHandle| s.segment.clone())
        .method(
            "dns_set",
            |s: &SegmentHandle, name: String, ip: String| -> Result<i64, String> {
                let ip: std::net::Ipv4Addr = ip.parse().map_err(|_| format!("bad IP `{ip}`"))?;
                s.with_zone(|z| z.set_static(&name, ip) as i64)
            },
        )
        .method(
            "dns_sinkhole",
            |s: &SegmentHandle, pattern: &str| -> Result<i64, String> {
                s.with_zone(|z| {
                    z.add_sinkhole(pattern, crate::config::model::SinkholeMode::Nxdomain) as i64
                })
            },
        )
        .method(
            "dns_clear",
            |s: &SegmentHandle, rule_id: i64| -> Result<bool, String> {
                s.with_zone(|z| z.remove_rule(rule_id as u64))
            },
        )
        .method(
            "block",
            |s: &SegmentHandle, cidr: &str| -> Result<i64, String> {
                s.rule_block(cidr, None, None)
            },
        )
        .method(
            "block_port",
            |s: &SegmentHandle, cidr: String, proto: String, port: i64| -> Result<i64, String> {
                s.rule_block(&cidr, Some(&proto), Some(port))
            },
        )
        .method(
            "unblock",
            |s: &SegmentHandle, rule_id: i64| -> Result<bool, String> { s.rule_remove(rule_id) },
        )
        .method(
            "redirect",
            |s: &SegmentHandle, from: String, to: String| -> Result<i64, String> {
                s.rule_redirect(&from, &to)
            },
        )
        .method(
            "forward",
            |s: &SegmentHandle,
             host_port: i64,
             vm: String,
             guest_port: i64|
             -> Result<i64, String> { s.add_forward(host_port, &vm, guest_port) },
        )
        .method(
            "route_to",
            |s: &SegmentHandle, other: &str| -> Result<(), String> { s.route_to(other, true) },
        )
        .method(
            "unroute_to",
            |s: &SegmentHandle, other: &str| -> Result<(), String> { s.route_to(other, false) },
        )
        .method("rules", |s: &SegmentHandle| -> Result<String, String> {
            s.rules_json()
        });

    // -- Machine (§10.3, §16, §18) --------------------------------------------
    //
    // One surface for every machine. What a machine cannot do is reported at
    // call time and names the capability, so a script written against a VM
    // runs against a container and fails — if it fails at all — for a reason a
    // lab author can act on.
    m.ty::<MachineHandle>()
        .method("name", |h: &MachineHandle| h.name().to_string())
        .method("kind", |h: &MachineHandle| -> String {
            match h.machine.kind() {
                MachineKind::Vm => "vm".into(),
                MachineKind::Container => "container".into(),
            }
        })
        // Lifecycle / state
        .method("start", |h: &MachineHandle| -> Result<(), String> {
            let runtime = h.runtime.clone();
            let name = h.name().to_string();
            h.block(async move { runtime.start_machine(&name).await })
                .map_err(estr)
        })
        .method("stop", |h: &MachineHandle| -> Result<(), String> {
            h.block(h.machine.stop(false)).map_err(estr)
        })
        .method("stop_force", |h: &MachineHandle| -> Result<(), String> {
            h.block(h.machine.stop(true)).map_err(estr)
        })
        .method("restart", |h: &MachineHandle| -> Result<(), String> {
            let runtime = h.runtime.clone();
            let name = h.name().to_string();
            h.block(async move { runtime.restart_machine(&name, false).await })
                .map_err(estr)
        })
        // Clean QMP `quit`: exits QEMU *gracefully*, flushing block-device
        // caches first (unlike stop_force's SIGKILL). For guests with no ACPI
        // (DOS, Win 3.x) this is the only way to seal a consistent disk — a
        // SIGKILL can drop unflushed qcow2 writes and leave it unbootable.
        .method("poweroff", |h: &MachineHandle| -> Result<(), String> {
            h.block(h.machine.poweroff()).map_err(estr)
        })
        .method("state", |h: &MachineHandle| -> String {
            match h.block(h.machine.state()) {
                PowerState::Stopped => "stopped".into(),
                PowerState::Starting => "starting".into(),
                PowerState::Running => "running".into(),
                PowerState::Stopping => "stopping".into(),
            }
        })
        // Readiness. Inside the machine's own first-boot provision the ready
        // flag is deferred until that script returns, so these mean "does the
        // agent answer right now" there (see `MachineHandle::first_boot_gated`)
        // — a live signal the script can use to watch its own guest reboot —
        // and full readiness everywhere else.
        .method("is_ready", |h: &MachineHandle| -> bool {
            if h.first_boot_gated {
                h.block(h.machine.agent_answering())
            } else {
                h.block(h.machine.is_ready())
            }
        })
        .method(
            "wait_ready",
            |h: &MachineHandle, timeout_secs: i64| -> Result<(), String> {
                let timeout = Duration::from_secs(timeout_secs.max(0) as u64);
                if h.first_boot_gated {
                    h.block(h.machine.wait_agent_answering(timeout))
                        .map_err(estr)
                } else {
                    h.block(h.machine.wait_ready(timeout)).map_err(estr)
                }
            },
        )
        // Healthy = the healthcheck's latest verdict; a machine declaring none
        // counts as healthy once it is ready.
        .method("is_healthy", |h: &MachineHandle| -> bool {
            h.block(h.machine.is_healthy())
        })
        // The live agent probe, ungated: goes false while the guest is down or
        // mid-reboot even though the sticky ready flag stays set. What a build
        // provision needs to watch an in-guest reboot it requested (`is_ready`
        // outside first-boot never drops while QEMU runs).
        .method("agent_answering", |h: &MachineHandle| -> bool {
            h.block(h.machine.agent_answering())
        })
        .method(
            "wait_shutdown",
            |h: &MachineHandle, timeout_secs: i64| -> Result<(), String> {
                h.block(h.machine.wait_state(
                    PowerState::Stopped,
                    Duration::from_secs(timeout_secs.max(0) as u64),
                ))
                .map_err(estr)
            },
        )
        .method("ip", |h: &MachineHandle| -> Result<String, String> {
            h.block(h.machine.guest_ip(None)).map_err(estr)
        })
        .method(
            "ip_nic",
            |h: &MachineHandle, nic: i64| -> Result<String, String> {
                h.block(h.machine.guest_ip(Some(nic.max(0) as usize)))
                    .map_err(estr)
            },
        )
        // Snapshots (§10.3, §18) — routed through the runtime so records,
        // events and pin-guarding stay in one place.
        .method(
            "snapshot",
            |h: &MachineHandle, name: &str| -> Result<(), String> {
                let runtime = h.runtime.clone();
                let machine = h.name().to_string();
                let snap = name.to_string();
                h.block(async move { runtime.snapshot(&machine, &snap).await })
                    .map(|_| ())
                    .map_err(estr)
            },
        )
        .method(
            "restore",
            |h: &MachineHandle, name: &str| -> Result<(), String> {
                let runtime = h.runtime.clone();
                let machine = h.name().to_string();
                let snap = name.to_string();
                h.block(async move { runtime.restore(&machine, &snap).await })
                    .map_err(estr)
            },
        )
        .method(
            "snapshots",
            |h: &MachineHandle| -> Result<Vec<String>, String> {
                let runtime = h.runtime.clone();
                let machine = h.name().to_string();
                let val = h
                    .block(async move { runtime.snapshots(&machine).await })
                    .map_err(estr)?;
                Ok(val
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|s| s["name"].as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default())
            },
        )
        .method(
            "delete_snapshot",
            |h: &MachineHandle, name: &str| -> Result<(), String> {
                let runtime = h.runtime.clone();
                let machine = h.name().to_string();
                let snap = name.to_string();
                h.block(async move { runtime.delete_snapshot(&machine, &snap).await })
                    .map_err(estr)
            },
        )
        // Input (§10.3) — the Display capability.
        .method(
            "send_keys",
            |h: &MachineHandle, chord: &str| -> Result<(), String> {
                h.block(h.display()?.send_keys(chord)).map_err(estr)
            },
        )
        .method(
            "type_text",
            |h: &MachineHandle, text: &str| -> Result<(), String> {
                h.block(h.display()?.type_text(text, 35)).map_err(estr)
            },
        )
        .method(
            "type_text_paced",
            |h: &MachineHandle, text: String, delay_ms: i64| -> Result<(), String> {
                h.block(h.display()?.type_text(&text, delay_ms.max(0) as u64))
                    .map_err(estr)
            },
        )
        .method(
            "mouse_move",
            |h: &MachineHandle, x: i64, y: i64| -> Result<(), String> {
                let display = h.display()?;
                *h.last_pointer.lock_recover() = (x, y);
                h.block(display.mouse_move(x, y)).map_err(estr)
            },
        )
        .method(
            "mouse_click",
            |h: &MachineHandle, button: &str| -> Result<(), String> {
                // A click reuses the position the preceding move set; for QMP
                // this is a no-op (QEMU retains the last absolute position),
                // for VNC it is the click target.
                let display = h.display()?;
                let at = *h.last_pointer.lock_recover();
                h.block(display.mouse_click(button, Some(at))).map_err(estr)
            },
        )
        .method(
            "mouse_drag",
            |h: &MachineHandle, x1: i64, y1: i64, x2: i64, y2: i64| -> Result<(), String> {
                let display = h.display()?;
                *h.last_pointer.lock_recover() = (x2, y2);
                h.block(display.mouse_drag(x1, y1, x2, y2)).map_err(estr)
            },
        )
        // Screen (§10.3) — the Display capability.
        .method(
            "screenshot",
            |h: &MachineHandle, path: &str| -> Result<String, String> {
                let display = h.display()?;
                let out = if path.is_empty() {
                    let dir = h.runtime.lab_local.join(SCREENSHOT_DIR);
                    dir.join(format!(
                        "{}-{}.png",
                        h.name(),
                        chrono::Utc::now().format("%Y%m%dT%H%M%S%.3f")
                    ))
                } else {
                    h.resolve_ref(path)
                };
                h.block(display.screenshot(&out)).map_err(estr)?;
                Ok(out.display().to_string())
            },
        )
        .method(
            "wait_for_image",
            |h: &MachineHandle, image: String, timeout_secs: i64| -> Result<ScriptMatch, String> {
                h.wait_for(&[image], 0.9, vec![], timeout_secs, 1000)
            },
        )
        .method(
            "wait_for_image_opts",
            |h: &MachineHandle,
             image: String,
             timeout_secs: i64,
             threshold: f64,
             region: Vec<i64>|
             -> Result<ScriptMatch, String> {
                h.wait_for(&[image], threshold, region, timeout_secs, 1000)
            },
        )
        .method(
            "wait_for_any",
            |h: &MachineHandle,
             images: Vec<String>,
             timeout_secs: i64|
             -> Result<ScriptMatch, String> {
                h.wait_for(&images, 0.9, vec![], timeout_secs, 1000)
            },
        )
        .method(
            "find_image",
            |h: &MachineHandle, image: &str| -> Result<Option<ScriptMatch>, String> {
                let opts = MachineHandle::match_opts(0.9, vec![])?;
                h.find_once(&[image.to_string()], &opts)
            },
        )
        .method("ocr", |h: &MachineHandle| -> Result<String, String> {
            h.block(h.display()?.ocr(None)).map_err(estr)
        })
        .method(
            "ocr_region",
            |h: &MachineHandle, region: Vec<i64>| -> Result<String, String> {
                let opts = MachineHandle::match_opts(0.9, region)?;
                h.block(h.display()?.ocr(opts.region)).map_err(estr)
            },
        )
        .method(
            "wait_for_text",
            |h: &MachineHandle,
             pattern: String,
             timeout_secs: i64|
             -> Result<ScriptMatch, String> {
                let display = h.display()?;
                let re = regex::Regex::new(&pattern).map_err(|e| format!("bad pattern: {e}"))?;
                let deadline =
                    std::time::Instant::now() + Duration::from_secs(timeout_secs.max(0) as u64);
                loop {
                    let img = h.block(display.grab()).map_err(estr)?;
                    let text = h.block(vision::ocr(&img, None)).map_err(estr)?;
                    if let Some(found) = re.find(&text) {
                        return Ok(ScriptMatch {
                            x: 0,
                            y: 0,
                            w: 0,
                            h: 0,
                            score: 1.0,
                            cx: 0,
                            cy: 0,
                            text: found.as_str().to_string(),
                        });
                    }
                    if std::time::Instant::now() >= deadline {
                        return Err(format!(
                            "timed out after {timeout_secs}s waiting for /{pattern}/ on {}",
                            h.name()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(1000));
                }
            },
        )
        // Guest agent (§10.3). Exec and file transfer ride the vmlab-agent
        // channel (streamed, no polling, no base64); guests from pre-agent
        // templates have no exec transport at all.
        .method(
            "exec",
            |h: &MachineHandle, cmd: String, args: Vec<String>| -> Result<ExecResult, String> {
                h.exec(cmd, args, Duration::from_secs(120))
            },
        )
        .method(
            "exec_timeout",
            |h: &MachineHandle,
             cmd: String,
             args: Vec<String>,
             timeout_secs: i64|
             -> Result<ExecResult, String> {
                h.exec(cmd, args, Duration::from_secs(timeout_secs.max(1) as u64))
            },
        )
        .method(
            "copy_to",
            |h: &MachineHandle, local: String, guest_path: String| -> Result<(), String> {
                let src = h.resolve_ref(&local);
                h.block(async {
                    let agent = h.machine.agent().await.map_err(estr)?;
                    agent
                        .push_file(&src, &guest_path, None)
                        .await
                        .map(|_| ())
                        .map_err(estr)
                })
            },
        )
        .method(
            "copy_from",
            |h: &MachineHandle, guest_path: String, local: String| -> Result<(), String> {
                let out = h.resolve_ref(&local);
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent).map_err(estr)?;
                }
                h.block(async {
                    let agent = h.machine.agent().await.map_err(estr)?;
                    agent
                        .pull_file(&guest_path, &out)
                        .await
                        .map(|_| ())
                        .map_err(estr)
                })
            },
        )
        // Console log: a container's captured stdout/stderr, a VM's serial log.
        .method(
            "logs",
            |h: &MachineHandle, lines: i64| -> Result<String, String> {
                match h.machine.console_log(lines.max(0) as usize) {
                    Some(r) => r.map_err(estr),
                    None => Err(format!("machine `{}` has no console log", h.name())),
                }
            },
        )
        // Interactive terminal (send/expect; vmlab-agent `terminal` feature).
        .method(
            "terminal",
            |h: &MachineHandle| -> Result<terminal::TerminalHandle, String> {
                let session = h.block(async {
                    let agent = h.machine.agent().await.map_err(estr)?;
                    agent
                        .open_terminal(terminal::SCRIPT_COLS, terminal::SCRIPT_ROWS, None)
                        .await
                        .map_err(estr)
                })?;
                Ok(terminal::TerminalHandle::new(
                    h.name().to_string(),
                    h.rt.clone(),
                    session,
                ))
            },
        )
        .method("stats", |h: &MachineHandle| -> Result<GuestStats, String> {
            h.block(async {
                let agent = h.machine.agent().await.map_err(estr)?;
                agent
                    .stats(Duration::from_secs(10))
                    .await
                    .map(GuestStats::from)
                    .map_err(estr)
            })
        });

    // -- Terminal sessions (send/expect) ---------------------------------------
    m.ty::<terminal::TerminalHandle>()
        .method(
            "send",
            |t: &terminal::TerminalHandle, text: String| -> Result<(), String> { t.send(&text) },
        )
        .method(
            "send_line",
            |t: &terminal::TerminalHandle, text: String| -> Result<(), String> {
                t.send_line(&text)
            },
        )
        .method("read", |t: &terminal::TerminalHandle| -> String {
            t.read()
        })
        .method(
            "expect",
            |t: &terminal::TerminalHandle,
             pattern: String,
             timeout_secs: i64|
             -> Result<String, String> { t.expect(&pattern, timeout_secs) },
        )
        .method(
            "resize",
            |t: &terminal::TerminalHandle, cols: i64, rows: i64| -> Result<(), String> {
                t.resize(cols, rows)
            },
        )
        .method("close", |t: &terminal::TerminalHandle| t.close());

    m
}

/// Build the full wscript context for compiling and running lab scripts.
pub fn context() -> Context {
    Context::new()
        .module(lab_module())
        .register_type::<ExecResult>()
        .register_type::<ScriptMatch>()
        .register_type::<EventData>()
        .register_type::<GuestStats>()
        .register_type::<DiskStat>()
}

/// Compile-check a script (used by `vmlab validate`, PRD §5.1).
pub fn check_script_source(source: &str) -> Result<(), String> {
    match context().compile(source) {
        Ok(_) => Ok(()),
        Err(wscript::Error::Compile(diags)) => {
            let msgs: Vec<String> = diags.iter().map(runner::render_diag).collect();
            Err(msgs.join("; "))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Write the `.wscripti` interface file for LSP support (PRD §10).
pub fn write_interface(path: &std::path::Path) -> std::io::Result<()> {
    context().write_interface(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_compiles_against_module() {
        let src = r#"
use vmlab

fn provision_dc(lab: Lab) {
    let Ok(dc) = lab.vm("dc01") else {
        lab.log("no dc01")
        return
    }
    match dc.wait_ready(600) {
        Ok(_) => lab.log("dc01 ready"),
        Err(e) => lab.log("not ready: " + e),
    }
    match dc.exec("ipconfig", ["/all"]) {
        Ok(r) => lab.log(r.stdout),
        Err(e) => lab.log("exec failed: " + e),
    }
    let k0 = dc.send_keys("ctrl-alt-del")
    let k1 = dc.type_text("Password1!\n")
    match dc.wait_for_image("images/login.png", 120) {
        Ok(m) => {
            let mv = dc.mouse_move(m.cx, m.cy)
            let cl = dc.mouse_click("left")
            lab.log("clicked")
        }
        Err(e) => lab.log(e),
    }
}

fn main(lab: Lab) {
    lab.log("lab " + lab.name())
    for vm in lab.vms() {
        lab.log(vm.name() + ": " + vm.state())
    }
    provision_dc(lab)
}
"#;
        check_script_source(src).expect("API surface should type-check");
    }

    /// Every operation is on every machine. Before ADR-0002 the surface was
    /// split by Rust type: a container handle had no `snapshot`, no power
    /// control and no `ip_nic`; a VM handle had no `logs` and no `is_healthy`.
    /// A script asking for one got "no such method", which told a lab author
    /// nothing about *this* machine.
    #[test]
    fn one_surface_for_both_kinds() {
        let src = r#"
use vmlab

fn drive(lab: Lab, m: Machine) {
    let s = m.start()
    match m.wait_ready(120) {
        Ok(_) => lab.log(m.name() + " (" + m.kind() + ") is ready"),
        Err(e) => lab.log("not ready: " + e),
    }
    match m.ip() {
        Ok(ip) => lab.log("ip " + ip),
        Err(e) => lab.log(e),
    }
    let nic0 = m.ip_nic(0)
    if m.is_ready() && m.is_healthy() && m.agent_answering() {
        match m.exec("uname", ["-a"]) {
            Ok(r) => { if r.exit_code == 0 { lab.log(r.stdout) } else { lab.log(r.stderr) } }
            Err(e) => lab.log("exec failed: " + e),
        }
        let t = m.exec_timeout("sleep", ["5"], 10)
    }
    let up = m.copy_to("conf/app.conf", "/etc/app.conf")
    let down = m.copy_from("/var/log/app.log", "logs/app.log")
    match m.logs(50) {
        Ok(text) => lab.log(text),
        Err(e) => lab.log(e),
    }
    let snap = m.snapshot("clean")
    match m.snapshots() {
        Ok(names) => { for n in names { lab.log(n) } }
        Err(e) => lab.log(e),
    }
    let rs = m.restore("clean")
    let ds = m.delete_snapshot("clean")
    let r = m.restart()
    let st = m.stop()
    let sf = m.stop_force()
    let po = m.poweroff()
    let w = m.wait_shutdown(60)
}

fn main(lab: Lab) {
    let Ok(web) = lab.container("web") else { return }
    drive(lab, web)
    let Ok(dc) = lab.vm("dc01") else { return }
    drive(lab, dc)
    let Ok(any) = lab.machine("api") else { return }
    drive(lab, any)
    for m in lab.machines() { lab.log(m.name() + ": " + m.state()) }
    for c in lab.containers() { lab.log(c.name()) }
}
"#;
        check_script_source(src).expect("one machine surface should type-check");
    }

    /// The screen operations are on every machine too — a container calling
    /// one fails at runtime naming the Display capability, not at compile time
    /// naming its kind. That is what keeps the expansion point open: the day a
    /// container reports a display, this script runs unchanged.
    #[test]
    fn screen_operations_are_on_every_machine() {
        let src = r#"
use vmlab

fn main(lab: Lab) {
    let Ok(web) = lab.container("web") else { return }
    match web.screenshot("") {
        Ok(path) => lab.log(path),
        Err(e) => lab.log(e),
    }
    let k = web.send_keys("ctrl-alt-del")
    let o = web.ocr()
}
"#;
        check_script_source(src).expect("screen methods must exist on a container handle");
    }

    #[test]
    fn terminal_api_compiles() {
        // The send/expect terminal handle + metrics, on VMs and containers.
        let src = r#"
use vmlab

fn main(lab: Lab) {
    let Ok(vm) = lab.vm("box") else { return }
    match vm.terminal() {
        Ok(t) => {
            let s = t.send_line("hostname")
            match t.expect("box", 10) {
                Ok(out) => lab.log("saw: " + out),
                Err(e) => lab.log(e),
            }
            let raw = t.send("\u{3}")
            lab.log(t.read())
            let rz = t.resize(200, 50)
            t.close()
        }
        Err(e) => lab.log("no terminal: " + e),
    }
    match vm.stats() {
        Ok(s) => {
            let cpu: float = s.cpu_pct
            let mem: int = s.mem_used
            for d in s.disks {
                let usage: int = d.used
                lab.log(d.mount)
            }
        }
        Err(e) => lab.log(e),
    }
    let Ok(web) = lab.container("web") else { return }
    let ct = web.terminal()
    let cs = web.stats()
}
"#;
        check_script_source(src).expect("terminal API surface should type-check");
    }

    #[test]
    fn bad_scripts_rejected() {
        // Wrong arg type to exec.
        let err = check_script_source(
            "use vmlab\nfn main(lab: Lab) { let v = lab.vm(\"a\") let _ = v.exec(1, []) }",
        )
        .unwrap_err();
        assert!(!err.is_empty());
        // Unknown method.
        assert!(check_script_source("use vmlab\nfn main(lab: Lab) { lab.frobnicate() }").is_err());
    }

    #[test]
    fn first_boot_this_vm_compiles() {
        // A template first-boot provision reaches its VM via lab.this_vm().
        let src = r#"
use vmlab

fn main(lab: Lab) {
    let vm = lab.this_vm().expect("no target vm")
    for i in 0..10 {
        match vm.exec("cmd.exe", ["/c", "if exist C:\\m (exit 0) else (exit 1)"]) {
            Ok(r) => { if r.exit_code == 0 { return } }
            Err(e) => lab.log("waiting: " + e),
        }
        vmlab::sleep_ms(1000)
    }
}
"#;
        check_script_source(src).expect("first-boot this_vm() should type-check");
    }

    #[test]
    fn handler_signature_compiles() {
        let src = r#"
use vmlab

fn handle(event: Event, lab: Lab) {
    lab.log("event " + event.name + " on " + event.vm)
    if event.name == "container.crashed" {
        let Ok(machine) = lab.machine(event.vm) else { return }
        let started = machine.start()
    }
}
"#;
        check_script_source(src).expect("a crash handler can explicitly start the machine");
    }

    #[test]
    fn interface_file_generates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vmlab.wscripti");
        write_interface(&path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("mod vmlab"), "{content}");
        assert!(content.contains("Lab"), "{content}");
    }
}

#[cfg(test)]
mod example_tests {
    use super::check_script_source;

    /// Every shipped example script (provision + handler, all labs and
    /// templates) plus the Docker sample lab's provision must type-check
    /// against the host module (keeps docs honest).
    #[test]
    fn shipped_examples_compile() {
        let mut stack = vec![
            std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/examples")),
            std::path::PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/docker")),
        ];
        let mut checked = 0usize;
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|e| e == "ws") {
                    let src = std::fs::read_to_string(&path)
                        .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
                    check_script_source(&src).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
                    checked += 1;
                }
            }
        }
        assert!(checked >= 7, "expected example scripts, found {checked}");
    }
}
