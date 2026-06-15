//! Lab-scoped CLI verbs (PRD §12): up/down/destroy/status, per-VM power
//! ops, snapshots, exec, logs. The CLI resolves the lab from cwd (or an
//! explicit `lab/vm` reference), starts daemons as needed, and talks to the
//! lab daemon directly.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};

use super::daemon;
use crate::proto::client::Client;

/// Resolve the current lab (name + root) from cwd, like git.
pub fn current_lab() -> Result<(String, std::path::PathBuf)> {
    let cwd = std::env::current_dir()?;
    let root = crate::paths::find_lab_root(&cwd)?;
    let file =
        crate::config::load_lab_root(&root).map_err(|e| anyhow!("{:?}", miette::Report::new(e)))?;
    Ok((file.lab.name, root))
}

/// Resolve a `[lab/]vm` reference (PRD §9.3): with a slash the lab is
/// explicit (daemon must be running); otherwise the cwd's lab.
pub fn split_vm_ref(vm_ref: &str) -> Result<(Option<String>, String)> {
    match vm_ref.split_once('/') {
        Some((lab, vm)) if !lab.is_empty() && !vm.is_empty() => {
            Ok((Some(lab.to_string()), vm.to_string()))
        }
        Some(_) => bail!("malformed reference `{vm_ref}` (expected [lab/]vm)"),
        None => Ok((None, vm_ref.to_string())),
    }
}

async fn lab_client_for(lab: Option<String>) -> Result<(String, Client)> {
    match lab {
        Some(name) => {
            let client = daemon::try_lab_daemon(&name)
                .await
                .ok_or_else(|| anyhow!("lab \"{name}\" is not running"))?;
            Ok((name, client))
        }
        None => {
            let (name, root) = current_lab()?;
            let client = daemon::ensure_lab_daemon(&name, &root).await?;
            Ok((name, client))
        }
    }
}

fn rt() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Runtime::new()?)
}

fn remote(e: crate::proto::ProtoError) -> anyhow::Error {
    anyhow!("{e}")
}

pub fn cmd_up(vms: Vec<String>) -> Result<()> {
    rt()?.block_on(async {
        // Validate before any side effect (PRD §5.1: implicitly every verb).
        super::validate::validate_current()?;
        let (name, root) = current_lab()?;
        let client = daemon::ensure_lab_daemon(&name, &root).await?;
        client
            .call_streaming("up", json!({"vms": vms}), |chunk| print!("{chunk}"))
            .await
            .map_err(remote)?;
        println!("lab \"{name}\" is up");
        Ok(())
    })
}

pub fn cmd_down(vms: Vec<String>, force: bool) -> Result<()> {
    rt()?.block_on(async {
        let (name, _root) = current_lab()?;
        let Some(client) = daemon::try_lab_daemon(&name).await else {
            println!("lab \"{name}\" is not running");
            return Ok(());
        };
        client
            .call("down", json!({"vms": vms, "force": force}))
            .await
            .map_err(remote)?;
        println!("lab \"{name}\" is down (clones retained)");
        Ok(())
    })
}

pub fn cmd_destroy() -> Result<()> {
    rt()?.block_on(async {
        let (name, root) = current_lab()?;
        // Destroy needs a daemon (to stop VMs and delete state) even if one
        // isn't currently running — .vmlab may still hold clones.
        let lab_local = crate::paths::lab_local_dir(&root);
        match daemon::try_lab_daemon(&name).await {
            Some(client) => {
                client.call("destroy", Value::Null).await.map_err(remote)?;
            }
            None if lab_local.exists() => {
                std::fs::remove_dir_all(&lab_local)
                    .with_context(|| format!("removing {}", lab_local.display()))?;
            }
            None => {}
        }
        // Reap the lab daemon.
        if let Ok(sup) = daemon::ensure_supervisor().await {
            let _ = sup.call("lab.release", json!({"name": name})).await;
        }
        println!("lab \"{name}\" destroyed");
        Ok(())
    })
}

pub fn cmd_status() -> Result<()> {
    rt()?.block_on(async {
        let (name, _root) = current_lab()?;
        let Some(client) = daemon::try_lab_daemon(&name).await else {
            println!("lab \"{name}\": not running");
            return Ok(());
        };
        let status = client.call("status", Value::Null).await.map_err(remote)?;
        print_status(&status);
        Ok(())
    })
}

fn print_status(status: &Value) {
    println!("lab \"{}\"", status["lab"].as_str().unwrap_or("?"));
    if let Some(vms) = status["vms"].as_array() {
        println!(
            "  {:<16} {:<10} {:<7} {:<16} TEMPLATE",
            "VM", "STATE", "READY", "IP"
        );
        for vm in vms {
            println!(
                "  {:<16} {:<10} {:<7} {:<16} {}",
                vm["name"].as_str().unwrap_or("?"),
                vm["state"].as_str().unwrap_or("?"),
                if vm["ready"].as_bool().unwrap_or(false) {
                    "yes"
                } else {
                    "no"
                },
                vm["ip"].as_str().unwrap_or("-"),
                vm["template"].as_str().unwrap_or("?"),
            );
        }
    }
    if let Some(segments) = status["segments"].as_array() {
        println!();
        println!(
            "  {:<16} {:<18} {:<15} NAT/DHCP",
            "SEGMENT", "SUBNET", "GATEWAY"
        );
        for s in segments {
            println!(
                "  {:<16} {:<18} {:<15} {}/{}",
                s["name"].as_str().unwrap_or("?"),
                s["subnet"].as_str().unwrap_or("?"),
                s["gateway"].as_str().unwrap_or("?"),
                if s["nat"].as_bool().unwrap_or(false) {
                    "on"
                } else {
                    "off"
                },
                if s["dhcp"].as_bool().unwrap_or(false) {
                    "on"
                } else {
                    "off"
                },
            );
        }
    }
}

pub fn cmd_vm_power(vm_ref: &str, op: &str, force: bool) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        match op {
            "start" => client
                .call("vm.start", json!({"vm": vm}))
                .await
                .map_err(remote)?,
            "stop" => client
                .call("vm.stop", json!({"vm": vm, "force": force}))
                .await
                .map_err(remote)?,
            "restart" => client
                .call("vm.restart", json!({"vm": vm}))
                .await
                .map_err(remote)?,
            _ => unreachable!(),
        };
        Ok(())
    })
}

pub fn cmd_exec(vm_ref: &str, timeout: u64, cmd: Vec<String>) -> Result<()> {
    if cmd.is_empty() {
        bail!("nothing to execute — usage: vmlab exec <vm> -- <cmd> [args...]");
    }
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let result = client
            .call(
                "vm.exec",
                json!({"vm": vm, "cmd": cmd[0], "args": cmd[1..].to_vec(), "timeout": timeout}),
            )
            .await
            .map_err(remote)?;
        print!("{}", result["stdout"].as_str().unwrap_or(""));
        eprint!("{}", result["stderr"].as_str().unwrap_or(""));
        let code = result["exit_code"].as_i64().unwrap_or(0);
        if code != 0 {
            std::process::exit(code as i32);
        }
        Ok(())
    })
}

/// `vmlab osinfo <vm>` — guest OS identification as one JSON object, fit
/// for machine consumption (config-weave's testlab parses it).
pub fn cmd_osinfo(vm_ref: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let info = client
            .call("vm.osinfo", json!({"vm": vm}))
            .await
            .map_err(remote)?;
        println!("{info}");
        Ok(())
    })
}

/// Per-message payload for `vm.copy_in` (pre-base64). Modest chunks keep
/// each JSON line small and each agent call inside its timeout.
const CP_CHUNK: usize = 1 << 20;

/// `vmlab cp <src> <vm>:<dest>` — copy a host file or directory tree into
/// a guest through the agent, creating parent directories.
pub fn cmd_cp(src: &str, dest: &str) -> Result<()> {
    // Split on the *first* colon: VM names contain none, guest paths may
    // (e.g. box:C:/weave).
    let Some((vm_part, guest_dest)) = dest.split_once(':') else {
        bail!("malformed destination `{dest}` — usage: vmlab cp <src> <vm>:<path>");
    };
    if vm_part.is_empty() || guest_dest.is_empty() {
        bail!("malformed destination `{dest}` — usage: vmlab cp <src> <vm>:<path>");
    }
    let src_path = std::path::Path::new(src);
    if !src_path.exists() {
        bail!("source {src} does not exist");
    }
    let (lab, vm) = split_vm_ref(vm_part)?;
    rt()?.block_on(async {
        let (_name, client) = lab_client_for(lab).await?;
        // The guest OS decides the mkdir command and path separator.
        let osinfo = client
            .call("vm.osinfo", json!({"vm": vm}))
            .await
            .map_err(remote)?;
        let windows = osinfo["id"].as_str() == Some("mswindows");
        if src_path.is_dir() {
            guest_mkdir(&client, &vm, guest_dest, windows).await?;
            copy_tree(&client, &vm, src_path, guest_dest, windows).await
        } else {
            if let Some((parent, _)) = guest_dest.rsplit_once(['/', '\\'])
                && !parent.is_empty()
                && !parent.ends_with(':')
            {
                guest_mkdir(&client, &vm, parent, windows).await?;
            }
            copy_file(&client, &vm, src_path, guest_dest).await
        }
    })
}

/// Create a directory (and parents) inside the guest via agent exec.
async fn guest_mkdir(client: &Client, vm: &str, guest_dir: &str, windows: bool) -> Result<()> {
    let (cmd, args) = if windows {
        // cmd's mkdir creates intermediate directories but errors when the
        // target exists; guard with `if not exist`. Backslashes only — cmd
        // reads forward slashes as switches. The trailing backslash in the
        // existence check makes it a directory test.
        let d = guest_dir.replace('/', "\\");
        (
            "cmd.exe".to_string(),
            vec![
                "/C".to_string(),
                "if".to_string(),
                "not".to_string(),
                "exist".to_string(),
                format!("{d}\\"),
                "mkdir".to_string(),
                d,
            ],
        )
    } else {
        (
            "mkdir".to_string(),
            vec!["-p".to_string(), guest_dir.to_string()],
        )
    };
    let result = client
        .call("vm.exec", json!({"vm": vm, "cmd": cmd, "args": args}))
        .await
        .map_err(remote)?;
    let code = result["exit_code"].as_i64().unwrap_or(0);
    if code != 0 {
        bail!(
            "cannot create {guest_dir} in {vm} (exit {code}): {}",
            result["stderr"].as_str().unwrap_or("").trim()
        );
    }
    Ok(())
}

/// Copy one host file into the guest in chunked `vm.copy_in` calls.
async fn copy_file(
    client: &Client,
    vm: &str,
    src: &std::path::Path,
    guest_dest: &str,
) -> Result<()> {
    use base64::Engine as _;
    let data = std::fs::read(src).with_context(|| format!("reading {}", src.display()))?;
    // An empty file still needs one write to be created.
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(CP_CHUNK).collect()
    };
    for (i, chunk) in chunks.into_iter().enumerate() {
        client
            .call(
                "vm.copy_in",
                json!({
                    "vm": vm,
                    "dest": guest_dest,
                    "data": base64::engine::general_purpose::STANDARD.encode(chunk),
                    "append": i > 0,
                }),
            )
            .await
            .map_err(|e| anyhow!("copying {} to {vm}:{guest_dest}: {e}", src.display()))?;
    }
    Ok(())
}

/// Recursively copy a host directory's contents into `guest_dir` (which
/// already exists).
async fn copy_tree(
    client: &Client,
    vm: &str,
    dir: &std::path::Path,
    guest_dir: &str,
    windows: bool,
) -> Result<()> {
    let sep = if windows { '\\' } else { '/' };
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            bail!("non-UTF-8 file name under {}", dir.display());
        };
        let guest_path = format!("{guest_dir}{sep}{name}");
        if entry.path().is_dir() {
            guest_mkdir(client, vm, &guest_path, windows).await?;
            Box::pin(copy_tree(client, vm, &entry.path(), &guest_path, windows)).await?;
        } else {
            copy_file(client, vm, &entry.path(), &guest_path).await?;
        }
    }
    Ok(())
}

pub fn cmd_run(script: &str) -> Result<()> {
    rt()?.block_on(async {
        let (name, root) = current_lab()?;
        if !root.join(script).is_file() {
            bail!("script {script} not found under {}", root.display());
        }
        let client = daemon::ensure_lab_daemon(&name, &root).await?;
        client
            .call_streaming("run", json!({"script": script}), |chunk| print!("{chunk}"))
            .await
            .map_err(remote)?;
        Ok(())
    })
}

pub fn cmd_snapshot(vm_ref: Option<String>, name: String) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = match &vm_ref {
            Some(r) => {
                let (l, v) = split_vm_ref(r)?;
                (l, Some(v))
            }
            None => (None, None),
        };
        let (_lab_name, client) = lab_client_for(lab).await?;
        let mut args = json!({"name": name});
        if let Some(v) = vm {
            args["vm"] = json!(v);
        }
        client.call("snapshot.take", args).await.map_err(remote)?;
        println!("snapshot \"{name}\" created");
        Ok(())
    })
}

pub fn cmd_restore(vm_ref: Option<String>, name: String) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = match &vm_ref {
            Some(r) => {
                let (l, v) = split_vm_ref(r)?;
                (l, Some(v))
            }
            None => (None, None),
        };
        let (_lab_name, client) = lab_client_for(lab).await?;
        let mut args = json!({"name": name});
        if let Some(v) = vm {
            args["vm"] = json!(v);
        }
        client
            .call("snapshot.restore", args)
            .await
            .map_err(remote)?;
        println!("snapshot \"{name}\" restored");
        Ok(())
    })
}

pub fn cmd_snapshots(vm_ref: &str) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_name, client) = lab_client_for(lab).await?;
        let snaps = client
            .call("snapshot.list", json!({"vm": vm}))
            .await
            .map_err(remote)?;
        let list = snaps.as_array().cloned().unwrap_or_default();
        if list.is_empty() {
            println!("no snapshots for {vm}");
            return Ok(());
        }
        println!("{:<24} {:<8} TAKEN", "NAME", "KIND");
        for s in list {
            println!(
                "{:<24} {:<8} {}",
                s["name"].as_str().unwrap_or("?"),
                if s["online"].as_bool().unwrap_or(false) {
                    "online"
                } else {
                    "offline"
                },
                s["taken_at"].as_str().unwrap_or("?"),
            );
        }
        Ok(())
    })
}

pub fn cmd_snapshot_delete(vm_ref: &str, name: String) -> Result<()> {
    rt()?.block_on(async {
        let (lab, vm) = split_vm_ref(vm_ref)?;
        let (_lab_name, client) = lab_client_for(lab).await?;
        client
            .call("snapshot.delete", json!({"vm": vm, "name": name}))
            .await
            .map_err(remote)?;
        println!("snapshot \"{name}\" deleted");
        Ok(())
    })
}

/// `vmlab logs [lab/][vm]` — tail or dump JSON-line logs (PRD §8.3). Reads
/// the state-dir files directly so it works with no daemon running.
pub fn cmd_logs(target: Option<String>, follow: bool, lines: usize) -> Result<()> {
    let (lab, vm) = match &target {
        None => (current_lab()?.0, None),
        Some(t) => match split_vm_ref(t)? {
            (Some(lab), vm) => (lab, Some(vm)),
            (None, maybe_vm) => {
                // Bare name: it's a VM in the cwd lab if that lab defines
                // it, otherwise a lab name.
                match current_lab() {
                    Ok((lab_name, root)) => {
                        let file = crate::config::load_lab_root(&root)
                            .map_err(|e| anyhow!("{:?}", miette::Report::new(e)))?;
                        if file.lab.vms.iter().any(|v| v.name == maybe_vm) {
                            (lab_name, Some(maybe_vm))
                        } else {
                            (maybe_vm, None)
                        }
                    }
                    Err(_) => (maybe_vm, None),
                }
            }
        },
    };

    let base = crate::paths::state_dir().join("labs").join(&lab);
    let paths: Vec<std::path::PathBuf> = match &vm {
        Some(vm) => {
            let d = base.join("vms").join(vm);
            vec![d.join("qemu.log"), d.join("serial.log")]
        }
        None => vec![base.join("events.jsonl")],
    };
    let existing: Vec<_> = paths.into_iter().filter(|p| p.exists()).collect();
    if existing.is_empty() {
        bail!(
            "no logs found for {}{}",
            lab,
            vm.map(|v| format!("/{v}")).unwrap_or_default()
        );
    }

    for path in &existing {
        let content = std::fs::read_to_string(path).unwrap_or_default();
        let all: Vec<&str> = content.lines().collect();
        let start = all.len().saturating_sub(lines);
        if existing.len() > 1 {
            println!("==> {} <==", path.display());
        }
        for line in &all[start..] {
            println!("{line}");
        }
    }

    if follow {
        // Poll-based tail on the first file (simple, portable).
        let path = existing[0].clone();
        let mut offset = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        loop {
            std::thread::sleep(std::time::Duration::from_millis(500));
            let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if len > offset {
                use std::io::{Read, Seek};
                let mut f = std::fs::File::open(&path)?;
                f.seek(std::io::SeekFrom::Start(offset))?;
                let mut buf = String::new();
                f.read_to_string(&mut buf)?;
                print!("{buf}");
                offset = len;
            }
        }
    }
    Ok(())
}
