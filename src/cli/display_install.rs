//! Explicit installation through the existing agent file and execution channels.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::daemon::remote;
use super::lab::{lab_client_for, lab_status, load_lab_here, push_via_agent, rt};
use crate::config::model::{GpuMode, Vm};
use crate::proto::LabRequest;

const GUEST_PACKAGE: &str = r"C:\ProgramData\vmlab\display";
const REQUIRED: &[&str] = &[
    "vmlab-display.inf",
    "vmlab-display.cat",
    "vmlab-display.dll",
    "vmlab-display.cer",
    "install.exe",
    "install.ps1",
    "ivshmem/ivshmem.inf",
    "ivshmem/ivshmem.cat",
    "ivshmem/ivshmem.sys",
];

fn eligible(vm: &Vm) -> Result<()> {
    let Some(gpu) = &vm.gpu else {
        bail!(
            "{} does not enable gpu.virtual_display; no guest changes made",
            vm.name
        );
    };
    if !gpu.virtual_display || gpu.mode != GpuMode::Passthrough {
        bail!(
            "{} requires gpu {{ mode = \"passthrough\" virtual_display = true }}; no guest changes made",
            vm.name
        );
    }
    Ok(())
}

fn package_path(explicit: Option<&Path>) -> Result<PathBuf> {
    let candidates = match explicit {
        Some(path) => vec![path.to_path_buf()],
        None => crate::agent_asset::candidate_dirs()
            .iter()
            .map(|root| root.join("vdisplay-windows/x86_64"))
            .collect(),
    };
    for path in &candidates {
        if REQUIRED.iter().all(|name| path.join(name).is_file()) {
            return path
                .canonicalize()
                .with_context(|| format!("opening {}", path.display()));
        }
    }
    bail!(
        "signed Windows display package is incomplete or absent in {candidates:?}; build and sign guest/vdisplay-windows first"
    )
}

pub(super) fn install(machine: &str, package: Option<&Path>) -> Result<()> {
    if machine.contains('/') {
        bail!("run install-display from the target lab and use its local VM name");
    }
    let (file, _) = load_lab_here()?;
    let vm = file
        .lab
        .vms
        .iter()
        .find(|vm| vm.name == machine)
        .with_context(|| format!("no VM named {machine}"))?;
    eligible(vm)?;
    let package = package_path(package)?;
    rt()?.block_on(async {
        let (_, client) = lab_client_for(None).await?;
        let status = lab_status(&client).await?;
        if status.config_digest != crate::config::digest(&file) {
            bail!("the running lab has different configuration; run vmlab up before installing display drivers");
        }
        let info = client.send(LabRequest::MachineOsInfo { machine: machine.into(), timeout: 30 })
            .await.map_err(remote)?;
        if info["id"].as_str() != Some("windows") || info["arch"].as_str() != Some("x86_64") {
            bail!("display installation requires a Windows agent; no guest changes made");
        }
        push_via_agent(&client, machine, &package, GUEST_PACKAGE).await?;
        let result = client.send(LabRequest::MachineExec {
            machine: machine.into(),
            cmd: "powershell.exe".into(),
            args: vec!["-NoProfile".into(), "-NonInteractive".into(), "-ExecutionPolicy".into(),
                "Bypass".into(), "-File".into(), format!(r"{GUEST_PACKAGE}\install.ps1"),
                "-VirtualDisplayEnabled".into()],
            timeout: 180,
            user: Some("SYSTEM".into()),
            password: None,
        }).await.map_err(remote)?;
        print!("{}", result["stdout"].as_str().unwrap_or(""));
        let code = result["exit_code"].as_i64().context("agent omitted installer exit code")?;
        if code != 0 {
            bail!("display installation exited {code}: {}", result["stderr"].as_str().unwrap_or(""));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lab(gpu: &str) -> crate::config::LabFile {
        let source = format!(
            "import <vmlab.wcl>\nlab \"test\" {{ vm \"win\" {{ template = \"scratch\" {gpu} }} }}"
        );
        crate::config::load_lab_source(&source, "vmlab.wcl", Path::new("/tmp/test")).unwrap()
    }

    #[test]
    fn installation_requires_explicit_passthrough_opt_in() {
        for gpu in [
            "",
            "gpu { mode = \"passthrough\" }",
            "gpu { mode = \"virgl\" virtual_display = true }",
        ] {
            assert!(eligible(&lab(gpu).lab.vms[0]).is_err());
        }
        assert!(
            eligible(
                &lab("gpu { mode = \"passthrough\" virtual_display = true }")
                    .lab
                    .vms[0]
            )
            .is_ok()
        );
    }

    #[test]
    fn unsigned_or_incomplete_package_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        assert!(package_path(Some(directory.path())).is_err());
        for name in REQUIRED {
            let path = directory.path().join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "package member").unwrap();
        }
        assert_eq!(
            package_path(Some(directory.path())).unwrap(),
            directory.path()
        );
    }
}
