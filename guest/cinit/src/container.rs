//! Launch the container process.
//!
//! Isolation note (v1): the child confines itself with plain `chroot` into
//! the overlay rootfs and stays in init's namespaces. `pivot_root` + mount
//! namespace would be tidier, but the whole VM already *is* the isolation
//! boundary — chroot only has to make the container see its own filesystem
//! layout, not defend against escape.

use std::ffi::CString;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use nix::unistd::{ForkResult, Gid, Pid, Uid, chdir, chroot, execve, fork, setgid, setuid};

use vmlab_cinit_proto::ContainerSpec;

use crate::mounts::ROOTFS;
use crate::users::ResolvedUser;
use crate::util::{Ctx, Result};

const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// The container environment: the spec's env verbatim (the host pre-merges
/// image env with lab overrides), plus a sane PATH if absent and HOME from
/// passwd when the user resolved to one.
pub fn build_env(spec: &ContainerSpec, user: Option<&ResolvedUser>) -> Vec<(String, String)> {
    let mut env = spec.env.clone();
    if !env.iter().any(|(k, _)| k == "PATH") {
        env.push(("PATH".into(), DEFAULT_PATH.into()));
    }
    if let Some(home) = user.and_then(|u| u.home.as_deref())
        && !env.iter().any(|(k, _)| k == "HOME")
    {
        env.push(("HOME".into(), home.to_string()));
    }
    env
}

/// Resolve the executable path *inside* the rootfs before forking: an argv[0]
/// with a slash is taken as-is, otherwise each PATH entry is searched under
/// `rootfs`. Doing this pre-fork keeps the post-fork child async-signal-safe
/// (no allocation) and gives a proper error instead of a bare ENOENT.
pub fn resolve_exe(rootfs: &str, argv0: &str, env: &[(String, String)]) -> Result<String> {
    let is_executable_file = |p: &Path| {
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if argv0.contains('/') {
        let full = format!("{rootfs}/{}", argv0.trim_start_matches('/'));
        if is_executable_file(Path::new(&full)) {
            return Ok(argv0.to_string());
        }
        return Err(format!("executable {argv0} not found in container rootfs"));
    }
    let path_var = env
        .iter()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str())
        .unwrap_or(DEFAULT_PATH);
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let inside = format!("{}/{argv0}", dir.trim_end_matches('/'));
        let full = format!("{rootfs}/{}", inside.trim_start_matches('/'));
        if is_executable_file(Path::new(&full)) {
            return Ok(inside);
        }
    }
    Err(format!(
        "executable {argv0:?} not found on PATH ({path_var}) in container rootfs"
    ))
}

/// The container argv: `entrypoint ++ cmd`. Error when both are empty.
pub fn build_argv(spec: &ContainerSpec) -> Result<Vec<String>> {
    let argv: Vec<String> = spec
        .entrypoint
        .iter()
        .chain(spec.cmd.iter())
        .cloned()
        .collect();
    if argv.is_empty() {
        return Err("container has neither entrypoint nor cmd".into());
    }
    Ok(argv)
}

fn cstring(s: &str) -> Result<CString> {
    CString::new(s).ctx(&format!("NUL byte in {s:?}"))
}

/// Fork the container: the child chroots into the overlay rootfs, drops to
/// the resolved user (v1: primary gid only, no supplementary group list),
/// chdirs to the workdir and execs. stdout/stderr are inherited, so container
/// output lands on the VM console.
pub fn spawn(
    spec: &ContainerSpec,
    user: Option<&ResolvedUser>,
    env: &[(String, String)],
) -> Result<Pid> {
    let argv = build_argv(spec)?;
    let exe = resolve_exe(ROOTFS, &argv[0], env)?;
    let workdir = spec.workdir.as_deref().unwrap_or("/");

    // Everything the child needs, allocated before fork (the child must not
    // allocate: another thread could hold the allocator lock at fork time).
    let c_exe = cstring(&exe)?;
    let c_argv: Vec<CString> = argv.iter().map(|a| cstring(a)).collect::<Result<_>>()?;
    let c_env: Vec<CString> = env
        .iter()
        .map(|(k, v)| cstring(&format!("{k}={v}")))
        .collect::<Result<_>>()?;
    let c_workdir = cstring(workdir)?;
    let ids = user.map(|u| (Uid::from_raw(u.uid), Gid::from_raw(u.gid)));

    // SAFETY: multithreaded fork; the child only performs async-signal-safe
    // operations (raw syscalls via nix + _exit) before execve.
    match unsafe { fork() }.ctx("fork container")? {
        ForkResult::Parent { child } => Ok(child),
        ForkResult::Child => {
            let die = |what: &str| -> ! {
                // eprintln! allocates, but we are already on the error path —
                // a hang here is no worse than a lost message.
                eprintln!("vmlab-cinit: container launch failed: {what}");
                unsafe { libc::_exit(127) }
            };
            if chroot(ROOTFS).is_err() {
                die("chroot");
            }
            if chdir(c_workdir.as_c_str()).is_err() {
                die("chdir to workdir");
            }
            if let Some((uid, gid)) = ids {
                if nix::unistd::setgroups(&[gid]).is_err() {
                    die("setgroups");
                }
                if setgid(gid).is_err() {
                    die("setgid");
                }
                if setuid(uid).is_err() {
                    die("setuid");
                }
            }
            let _ = execve(&c_exe, &c_argv, &c_env);
            die("execve");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn spec_with(entrypoint: &[&str], cmd: &[&str]) -> ContainerSpec {
        serde_json::from_str::<ContainerSpec>(r#"{ "hostname": "t" }"#)
            .map(|mut s| {
                s.entrypoint = entrypoint.iter().map(|s| s.to_string()).collect();
                s.cmd = cmd.iter().map(|s| s.to_string()).collect();
                s
            })
            .unwrap()
    }

    #[test]
    fn argv_is_entrypoint_plus_cmd() {
        let spec = spec_with(&["/entry.sh"], &["-a", "b"]);
        assert_eq!(build_argv(&spec).unwrap(), vec!["/entry.sh", "-a", "b"]);
        assert!(build_argv(&spec_with(&[], &[])).is_err());
    }

    #[test]
    fn env_gets_path_and_home_defaults() {
        let mut spec = spec_with(&["/e"], &[]);
        spec.env = vec![("FOO".into(), "bar".into())];
        let user = ResolvedUser {
            uid: 1,
            gid: 1,
            home: Some("/home/x".into()),
        };
        let env = build_env(&spec, Some(&user));
        assert!(env.contains(&("PATH".into(), DEFAULT_PATH.into())));
        assert!(env.contains(&("HOME".into(), "/home/x".into())));

        // Explicit values win.
        spec.env = vec![
            ("PATH".into(), "/only".into()),
            ("HOME".into(), "/h".into()),
        ];
        let env = build_env(&spec, Some(&user));
        assert_eq!(env.iter().filter(|(k, _)| k == "PATH").count(), 1);
        assert!(env.contains(&("PATH".into(), "/only".into())));
        assert!(env.contains(&("HOME".into(), "/h".into())));
    }

    #[test]
    fn resolves_exe_on_path_inside_rootfs() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().to_str().unwrap().to_string();
        fs::create_dir_all(format!("{rootfs}/usr/bin")).unwrap();
        fs::write(format!("{rootfs}/usr/bin/tool"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(
            format!("{rootfs}/usr/bin/tool"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();

        let env = vec![("PATH".to_string(), "/nope:/usr/bin".to_string())];
        assert_eq!(resolve_exe(&rootfs, "tool", &env).unwrap(), "/usr/bin/tool");
        // Absolute paths pass through unchanged (but are verified).
        assert_eq!(
            resolve_exe(&rootfs, "/usr/bin/tool", &env).unwrap(),
            "/usr/bin/tool"
        );
        assert!(resolve_exe(&rootfs, "missing", &env).is_err());
        assert!(resolve_exe(&rootfs, "/missing", &env).is_err());
    }

    #[test]
    fn non_executable_files_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().to_str().unwrap().to_string();
        fs::create_dir_all(format!("{rootfs}/bin")).unwrap();
        fs::write(format!("{rootfs}/bin/data"), "x").unwrap();
        fs::set_permissions(
            format!("{rootfs}/bin/data"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        let env = vec![("PATH".to_string(), "/bin".to_string())];
        assert!(resolve_exe(&rootfs, "data", &env).is_err());
    }
}
