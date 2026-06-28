//! The wscript scripting surface (PRD §10): vmlab's host module exposing
//! lab/VM/segment handles to provision scripts, event handlers, and ad-hoc
//! runs. Scripts are daemon-unaware; the wscript VM is synchronous, so scripts
//! execute on blocking threads and host methods bridge into the lab
//! daemon's tokio runtime via `Handle::block_on`.

pub mod interact;
pub mod keymap;
mod runner;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use wscript::{Context, Module, Script};

use crate::labd::lab::LabRuntime;
use crate::labd::vm::{PowerState, VmInstance};
use crate::vision;

pub use runner::{OutputSink, run_event_handler, run_script_file, run_script_source};

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
    /// For a template first-boot provision: the VM the script targets, fetched
    /// with `lab.this_vm()`. `None` for ordinary provisions/handlers.
    pub(crate) first_boot_vm: Option<String>,
}

/// A VM handle (PRD §10.3).
#[derive(Script)]
#[script(name = "Vm")]
#[script(opaque)]
pub struct VmHandle {
    pub(crate) vm: Arc<VmInstance>,
    pub(crate) runtime: Arc<LabRuntime>,
    pub(crate) rt: tokio::runtime::Handle,
    pub(crate) output: OutputSink,
    /// Last pointer position, for the VNC input transport: RFB PointerEvent
    /// always carries x,y, but the API splits `mouse_move`/`mouse_click`, so
    /// a click reuses the position the preceding move set.
    pub(crate) last_pointer: Arc<std::sync::Mutex<(i64, i64)>>,
    /// Directory the running script lives in (see [`LabHandle::ref_base`]).
    pub(crate) ref_base: Arc<std::path::PathBuf>,
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

impl VmHandle {
    fn block<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.rt.block_on(fut)
    }

    fn resolve_ref(&self, path: &str) -> PathBuf {
        let p = PathBuf::from(path);
        if p.is_absolute() {
            p
        } else {
            self.ref_base.join(p)
        }
    }

    /// QMP screendump → decoded image.
    fn grab_screen(&self) -> Result<image::RgbImage, String> {
        self.block(interact::grab_screen(&self.vm)).map_err(estr)
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
        let screen = self.grab_screen()?;
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
                    refs, self.vm.cfg.name
                ));
            }
            std::thread::sleep(Duration::from_millis(interval_ms.max(50) as u64));
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

    // -- Lab (§10.1) ---------------------------------------------------------
    m.ty::<LabHandle>()
        .method("name", |l: &LabHandle| l.runtime.name.clone())
        .method("log", |l: &LabHandle, msg: &str| {
            (l.output)(format!("{msg}\n"));
        })
        .method(
            "vm",
            |l: &LabHandle, name: &str| -> Result<VmHandle, String> {
                let vm = l.runtime.vm(name).map_err(estr)?.clone();
                Ok(VmHandle {
                    vm,
                    runtime: l.runtime.clone(),
                    rt: l.rt.clone(),
                    output: l.output.clone(),
                    last_pointer: Default::default(),
                    ref_base: l.ref_base.clone(),
                })
            },
        )
        .method("this_vm", |l: &LabHandle| -> Result<VmHandle, String> {
            let name = l
                .first_boot_vm
                .as_deref()
                .ok_or("this_vm() is only available inside a template first-boot provision")?;
            let vm = l.runtime.vm(name).map_err(estr)?.clone();
            Ok(VmHandle {
                vm,
                runtime: l.runtime.clone(),
                rt: l.rt.clone(),
                output: l.output.clone(),
                last_pointer: Default::default(),
                ref_base: l.ref_base.clone(),
            })
        })
        .method("vms", |l: &LabHandle| -> Vec<VmHandle> {
            l.runtime
                .vms
                .values()
                .map(|vm| VmHandle {
                    vm: vm.clone(),
                    runtime: l.runtime.clone(),
                    rt: l.rt.clone(),
                    output: l.output.clone(),
                    last_pointer: Default::default(),
                    ref_base: l.ref_base.clone(),
                })
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

    // -- VM (§10.3) ----------------------------------------------------------
    m.ty::<VmHandle>()
        .method("name", |v: &VmHandle| v.vm.cfg.name.clone())
        // Lifecycle / state
        .method("start", |v: &VmHandle| -> Result<(), String> {
            let runtime = v.runtime.clone();
            let name = v.vm.cfg.name.clone();
            v.block(async move { runtime.start_vm(&name).await })
                .map_err(estr)
        })
        .method("stop", |v: &VmHandle| -> Result<(), String> {
            v.block(v.vm.stop(false)).map_err(estr)
        })
        .method("stop_force", |v: &VmHandle| -> Result<(), String> {
            v.block(v.vm.stop(true)).map_err(estr)
        })
        // Clean QMP `quit`: exits QEMU *gracefully*, flushing block-device
        // caches first (unlike stop_force's SIGKILL). For guests with no ACPI
        // (DOS, Win 3.x) this is the only way to seal a consistent disk — a
        // SIGKILL can drop unflushed qcow2 writes and leave it unbootable.
        .method("poweroff", |v: &VmHandle| -> Result<(), String> {
            v.block(async {
                if let Ok(qmp) = v.vm.qmp().await {
                    // QEMU exits, so the QMP connection drops — that's expected.
                    let _ = qmp.quit().await;
                }
                v.vm.wait_state(PowerState::Stopped, Duration::from_secs(30))
                    .await
                    .map_err(estr)
            })
        })
        .method("restart", |v: &VmHandle| -> Result<(), String> {
            v.block(async {
                v.vm.stop(false).await.map_err(estr)?;
                v.vm.wait_state(PowerState::Stopped, Duration::from_secs(60))
                    .await
                    .map_err(estr)?;
                v.runtime.start_vm(&v.vm.cfg.name).await.map_err(estr)
            })
        })
        .method("state", |v: &VmHandle| -> String {
            match v.block(v.vm.state()) {
                PowerState::Stopped => "stopped".into(),
                PowerState::Starting => "starting".into(),
                PowerState::Running => "running".into(),
                PowerState::Stopping => "stopping".into(),
            }
        })
        .method("is_ready", |v: &VmHandle| -> bool {
            v.block(v.vm.is_ready())
        })
        .method(
            "wait_ready",
            |v: &VmHandle, timeout_secs: i64| -> Result<(), String> {
                v.block(v.vm.wait_ready(Duration::from_secs(timeout_secs.max(0) as u64)))
                    .map_err(estr)
            },
        )
        .method(
            "wait_shutdown",
            |v: &VmHandle, timeout_secs: i64| -> Result<(), String> {
                v.block(v.vm.wait_state(
                    PowerState::Stopped,
                    Duration::from_secs(timeout_secs.max(0) as u64),
                ))
                .map_err(estr)
            },
        )
        .method("ip", |v: &VmHandle| -> Result<String, String> {
            v.block(v.vm.guest_ip(None)).map_err(estr)
        })
        .method(
            "ip_nic",
            |v: &VmHandle, nic: i64| -> Result<String, String> {
                v.block(v.vm.guest_ip(Some(nic.max(0) as usize)))
                    .map_err(estr)
            },
        )
        // Snapshots (§10.3)
        .method(
            "snapshot",
            |v: &VmHandle, name: &str| -> Result<(), String> {
                let runtime = v.runtime.clone();
                let vm_name = v.vm.cfg.name.clone();
                let snap = name.to_string();
                v.block(async move { runtime.snapshot(&vm_name, &snap).await })
                    .map(|_| ())
                    .map_err(estr)
            },
        )
        .method(
            "restore",
            |v: &VmHandle, name: &str| -> Result<(), String> {
                let runtime = v.runtime.clone();
                let vm_name = v.vm.cfg.name.clone();
                let snap = name.to_string();
                v.block(async move { runtime.restore(&vm_name, &snap).await })
                    .map_err(estr)
            },
        )
        .method("snapshots", |v: &VmHandle| -> Result<Vec<String>, String> {
            let runtime = v.runtime.clone();
            let vm_name = v.vm.cfg.name.clone();
            let val = v
                .block(async move { runtime.snapshots(&vm_name).await })
                .map_err(estr)?;
            Ok(val
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|s| s["name"].as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default())
        })
        .method(
            "delete_snapshot",
            |v: &VmHandle, name: &str| -> Result<(), String> {
                let runtime = v.runtime.clone();
                let vm_name = v.vm.cfg.name.clone();
                let snap = name.to_string();
                v.block(async move { runtime.delete_snapshot(&vm_name, &snap).await })
                    .map_err(estr)
            },
        )
        // Input (§10.3)
        .method(
            "send_keys",
            |v: &VmHandle, chord: &str| -> Result<(), String> {
                v.block(interact::send_keys(&v.vm, chord)).map_err(estr)
            },
        )
        .method(
            "type_text",
            |v: &VmHandle, text: &str| -> Result<(), String> {
                v.block(interact::type_text(&v.vm, text, 35)).map_err(estr)
            },
        )
        .method(
            "type_text_paced",
            |v: &VmHandle, text: String, delay_ms: i64| -> Result<(), String> {
                v.block(interact::type_text(&v.vm, &text, delay_ms.max(0) as u64))
                    .map_err(estr)
            },
        )
        .method(
            "mouse_move",
            |v: &VmHandle, x: i64, y: i64| -> Result<(), String> {
                *v.last_pointer.lock().unwrap() = (x, y);
                v.block(interact::mouse_move(&v.vm, x, y)).map_err(estr)
            },
        )
        .method(
            "mouse_click",
            |v: &VmHandle, button: &str| -> Result<(), String> {
                // A click reuses the position the preceding move set; for QMP
                // this is a no-op (QEMU retains the last absolute position),
                // for VNC it is the click target.
                let at = *v.last_pointer.lock().unwrap();
                v.block(interact::mouse_click(&v.vm, button, Some(at)))
                    .map_err(estr)
            },
        )
        .method(
            "mouse_drag",
            |v: &VmHandle, x1: i64, y1: i64, x2: i64, y2: i64| -> Result<(), String> {
                *v.last_pointer.lock().unwrap() = (x2, y2);
                v.block(interact::mouse_drag(&v.vm, x1, y1, x2, y2))
                    .map_err(estr)
            },
        )
        // Screen (§10.3)
        .method(
            "screenshot",
            |v: &VmHandle, path: &str| -> Result<String, String> {
                let out = if path.is_empty() {
                    let dir = v.runtime.lab_local.join(SCREENSHOT_DIR);
                    dir.join(format!(
                        "{}-{}.png",
                        v.vm.cfg.name,
                        chrono::Utc::now().format("%Y%m%dT%H%M%S%.3f")
                    ))
                } else {
                    v.resolve_ref(path)
                };
                v.block(interact::screenshot(&v.vm, &out)).map_err(estr)?;
                Ok(out.display().to_string())
            },
        )
        .method(
            "wait_for_image",
            |v: &VmHandle, image: String, timeout_secs: i64| -> Result<ScriptMatch, String> {
                v.wait_for(&[image], 0.9, vec![], timeout_secs, 1000)
            },
        )
        .method(
            "wait_for_image_opts",
            |v: &VmHandle,
             image: String,
             timeout_secs: i64,
             threshold: f64,
             region: Vec<i64>|
             -> Result<ScriptMatch, String> {
                v.wait_for(&[image], threshold, region, timeout_secs, 1000)
            },
        )
        .method(
            "wait_for_any",
            |v: &VmHandle, images: Vec<String>, timeout_secs: i64| -> Result<ScriptMatch, String> {
                v.wait_for(&images, 0.9, vec![], timeout_secs, 1000)
            },
        )
        .method(
            "find_image",
            |v: &VmHandle, image: &str| -> Result<Option<ScriptMatch>, String> {
                let opts = VmHandle::match_opts(0.9, vec![])?;
                v.find_once(&[image.to_string()], &opts)
            },
        )
        .method("ocr", |v: &VmHandle| -> Result<String, String> {
            v.block(interact::ocr(&v.vm, None)).map_err(estr)
        })
        .method(
            "ocr_region",
            |v: &VmHandle, region: Vec<i64>| -> Result<String, String> {
                let opts = VmHandle::match_opts(0.9, region)?;
                v.block(interact::ocr(&v.vm, opts.region)).map_err(estr)
            },
        )
        .method(
            "wait_for_text",
            |v: &VmHandle, pattern: String, timeout_secs: i64| -> Result<ScriptMatch, String> {
                let re = regex::Regex::new(&pattern).map_err(|e| format!("bad pattern: {e}"))?;
                let deadline =
                    std::time::Instant::now() + Duration::from_secs(timeout_secs.max(0) as u64);
                loop {
                    let img = v.grab_screen()?;
                    let text = v.block(vision::ocr(&img, None)).map_err(estr)?;
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
                            v.vm.cfg.name
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(1000));
                }
            },
        )
        // Guest agent (§10.3)
        .method(
            "exec",
            |v: &VmHandle, cmd: String, args: Vec<String>| -> Result<ExecResult, String> {
                v.block(async {
                    let qga = v.vm.qga().await.map_err(estr)?;
                    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                    let r = qga
                        .exec(&cmd, &arg_refs, true, Duration::from_secs(120))
                        .await
                        .map_err(estr)?;
                    Ok(ExecResult {
                        exit_code: r.exit_code as i64,
                        stdout: String::from_utf8_lossy(&r.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&r.stderr).into_owned(),
                    })
                })
            },
        )
        .method(
            "exec_timeout",
            |v: &VmHandle,
             cmd: String,
             args: Vec<String>,
             timeout_secs: i64|
             -> Result<ExecResult, String> {
                v.block(async {
                    let qga = v.vm.qga().await.map_err(estr)?;
                    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
                    let r = qga
                        .exec(
                            &cmd,
                            &arg_refs,
                            true,
                            Duration::from_secs(timeout_secs.max(1) as u64),
                        )
                        .await
                        .map_err(estr)?;
                    Ok(ExecResult {
                        exit_code: r.exit_code as i64,
                        stdout: String::from_utf8_lossy(&r.stdout).into_owned(),
                        stderr: String::from_utf8_lossy(&r.stderr).into_owned(),
                    })
                })
            },
        )
        .method(
            "copy_to",
            |v: &VmHandle, local: String, guest_path: String| -> Result<(), String> {
                let data = std::fs::read(v.resolve_ref(&local)).map_err(estr)?;
                v.block(async {
                    let qga = v.vm.qga().await.map_err(estr)?;
                    qga.file_write(&guest_path, &data, Duration::from_secs(60))
                        .await
                        .map_err(estr)
                })
            },
        )
        .method(
            "copy_from",
            |v: &VmHandle, guest_path: String, local: String| -> Result<(), String> {
                let data = v.block(async {
                    let qga = v.vm.qga().await.map_err(estr)?;
                    qga.file_read(&guest_path, Duration::from_secs(60))
                        .await
                        .map_err(estr)
                })?;
                let out = v.resolve_ref(&local);
                if let Some(parent) = out.parent() {
                    std::fs::create_dir_all(parent).map_err(estr)?;
                }
                std::fs::write(out, data).map_err(estr)
            },
        );

    m
}

/// Build the full wscript context for compiling and running lab scripts.
pub fn context() -> Context {
    Context::new()
        .module(lab_module())
        .register_type::<ExecResult>()
        .register_type::<ScriptMatch>()
        .register_type::<EventData>()
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
}
"#;
        check_script_source(src).expect("handler signature should type-check");
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
    /// templates) must type-check against the host module (keeps docs
    /// honest).
    #[test]
    fn shipped_examples_compile() {
        let root = concat!(env!("CARGO_MANIFEST_DIR"), "/examples");
        let mut stack = vec![std::path::PathBuf::from(root)];
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
