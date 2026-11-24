//! Launch the container process.
//!
//! The workload owns a PID and mount namespace.  Exec-style helpers join
//! both, so `/proc` describes the container process tree rather than the
//! surrounding micro-VM while every process still uses the shared overlay.

use std::ffi::CString;
use std::fs::File;
use std::os::fd::AsFd;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use nix::mount::{MntFlags, MsFlags, mount, umount2};
use nix::sched::{CloneFlags, clone, setns};
use nix::sys::signal::Signal;
use nix::unistd::{Gid, Pid, Uid, chdir, chroot, execve, setgid, setuid};

use vmlab_cinit_proto::ContainerSpec;

use crate::mounts::{ROOTFS, rootfs_dir};
use crate::users::ResolvedUser;
use crate::util::{Ctx, Result, in_root};

pub(crate) const DEFAULT_PATH: &str =
    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

const CLONE_STACK_SIZE: usize = 1024 * 1024;

/// Open namespace handles remain valid across workload exec and are shared
/// with shell and healthcheck threads.
#[derive(Clone)]
pub struct Namespaces {
    pid: Arc<File>,
    mount: Arc<File>,
}

impl Namespaces {
    fn open(pid: Pid) -> Result<Self> {
        let base = format!("/proc/{}/ns", pid.as_raw());
        Ok(Self {
            pid: Arc::new(File::open(format!("{base}/pid")).ctx("open container PID namespace")?),
            mount: Arc::new(
                File::open(format!("{base}/mnt")).ctx("open container mount namespace")?,
            ),
        })
    }

    /// Enter the mount namespace immediately. Entering a PID namespace only
    /// affects subsequently-created children, so callers must fork once more.
    pub fn enter_for_child(&self) -> Result<()> {
        setns(self.mount.as_fd(), CloneFlags::CLONE_NEWNS).ctx("join container mount namespace")?;
        setns(self.pid.as_fd(), CloneFlags::CLONE_NEWPID).ctx("join container PID namespace")
    }
}

pub struct ContainerProcess {
    pub pid: Pid,
    pub namespaces: Namespaces,
}

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
///
/// Candidates are resolved inside `rootfs` (F56), as the chrooted exec will
/// see them, so an image symlink never probes the micro-VM's own files.
pub fn resolve_exe(rootfs: &str, argv0: &str, env: &[(String, String)]) -> Result<String> {
    let is_executable_file = |inside: &str| {
        in_root(rootfs, inside)
            .ok()
            .and_then(|p| p.metadata().ok())
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if argv0.contains('/') {
        if is_executable_file(argv0) {
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
        if is_executable_file(&inside) {
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

/// Who a workload process runs as and where, computed before fork: the
/// workdir to chdir to inside the chroot and the identity to drop to. The
/// container and its healthcheck (F58) both go through it, so a check runs
/// exactly as the workload does — v1: primary gid only, no supplementary
/// group list beyond it.
pub struct ExecIdentity {
    pub workdir: CString,
    pub ids: Option<(Uid, Gid)>,
}

impl ExecIdentity {
    pub fn new(workdir: Option<&str>, user: Option<&ResolvedUser>) -> Result<Self> {
        Ok(Self {
            workdir: cstring(workdir.unwrap_or("/"))?,
            ids: user.map(|u| (Uid::from_raw(u.uid), Gid::from_raw(u.gid))),
        })
    }

    /// Call after chroot, in the forked child: chdir to the workdir, then
    /// drop groups, gid and uid. Allocation-free (async-signal-safe); the
    /// error names the step that failed.
    pub fn apply(&self) -> std::result::Result<(), &'static str> {
        if chdir(self.workdir.as_c_str()).is_err() {
            return Err("chdir to workdir");
        }
        if let Some((uid, gid)) = self.ids {
            if nix::unistd::setgroups(&[gid]).is_err() {
                return Err("setgroups");
            }
            if setgid(gid).is_err() {
                return Err("setgid");
            }
            if setuid(uid).is_err() {
                return Err("setuid");
            }
        }
        Ok(())
    }
}

/// Fork the container: the child chroots into the overlay rootfs, chdirs to
/// the workdir, drops to the resolved user ([`ExecIdentity`]) and execs. stdout/stderr are inherited, so container
/// output lands on the VM console.
pub fn spawn(
    spec: &ContainerSpec,
    user: Option<&ResolvedUser>,
    env: &[(String, String)],
) -> Result<ContainerProcess> {
    let argv = build_argv(spec)?;
    let exe = resolve_exe(ROOTFS, &argv[0], env)?;
    // The container's /proc as the runtime mounts resolved it (F56).
    let proc_dir = in_root(ROOTFS, "/proc")?;

    // Everything the child needs, allocated before fork (the child must not
    // allocate: another thread could hold the allocator lock at fork time).
    let c_exe = cstring(&exe)?;
    let c_argv: Vec<CString> = argv.iter().map(|a| cstring(a)).collect::<Result<_>>()?;
    let c_env: Vec<CString> = env
        .iter()
        .map(|(k, v)| cstring(&format!("{k}={v}")))
        .collect::<Result<_>>()?;
    let c_proc = CString::new(proc_dir.into_os_string().into_vec())
        .ctx("NUL byte in the container /proc path")?;
    let identity = ExecIdentity::new(spec.workdir.as_deref(), user)?;
    // Prepare the directory in the shared overlay so the workload and its
    // healthcheck can both enter it after chroot.
    if let Some(workdir) = &spec.workdir {
        rootfs_dir(workdir).ctx(&format!("prepare workdir {workdir:?}"))?;
    }
    let mut ready_fds = [0_i32; 2];
    if unsafe { libc::pipe2(ready_fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(format!(
            "create container readiness pipe: {}",
            nix::errno::Errno::last()
        ));
    }
    let ready_read = ready_fds[0];
    let ready_write = ready_fds[1];

    let mut stack = vec![0_u8; CLONE_STACK_SIZE];
    let callback = Box::new(move || -> isize {
        let die = |what: &str| -> ! {
            eprintln!("vmlab-cinit: container launch failed: {what}");
            unsafe { libc::_exit(127) }
        };
        unsafe { libc::close(ready_read) };

        // Stop mount changes in this namespace propagating back to cinit,
        // then replace the inherited host-namespace procfs with one owned by
        // the new PID namespace.
        if mount::<str, str, str, str>(None, "/", None, MsFlags::MS_REC | MsFlags::MS_PRIVATE, None)
            .is_err()
        {
            die("make mounts private");
        }
        if umount2(c_proc.as_c_str(), MntFlags::MNT_DETACH).is_err() {
            die("unmount inherited /proc");
        }
        if mount(
            Some("proc"),
            c_proc.as_c_str(),
            Some("proc"),
            MsFlags::empty(),
            None::<&str>,
        )
        .is_err()
        {
            die("mount container /proc");
        }
        // The parent must not publish namespace handles until their procfs is
        // ready; otherwise a fast shell attach can observe the inherited
        // micro-VM process view.
        let ready = [1_u8];
        if unsafe { libc::write(ready_write, ready.as_ptr().cast(), 1) } != 1 {
            die("signal namespace readiness");
        }
        unsafe { libc::close(ready_write) };
        if chroot(ROOTFS).is_err() {
            die("chroot");
        }
        if let Err(what) = identity.apply() {
            die(what);
        }
        let _ = execve(&c_exe, &c_argv, &c_env);
        die("execve");
    });

    // SAFETY: the callback follows the same post-fork restrictions as the
    // previous fork path and receives a dedicated stack. CLONE_VM is absent.
    let pid_result = unsafe {
        clone(
            callback,
            &mut stack,
            CloneFlags::CLONE_NEWPID | CloneFlags::CLONE_NEWNS,
            Some(Signal::SIGCHLD as i32),
        )
    }
    .ctx("clone container namespaces");
    unsafe { libc::close(ready_write) };
    let pid = match pid_result {
        Ok(pid) => pid,
        Err(e) => {
            unsafe { libc::close(ready_read) };
            return Err(e);
        }
    };
    let mut ready = [0_u8];
    let n = unsafe { libc::read(ready_read, ready.as_mut_ptr().cast(), 1) };
    unsafe { libc::close(ready_read) };
    if n != 1 {
        return Err("container namespace setup failed before readiness".into());
    }
    let namespaces = Namespaces::open(pid)?;
    Ok(ContainerProcess { pid, namespaces })
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

    /// F58: the healthcheck runs through the same identity as the
    /// workload — its uid, gid and workdir — never root in `/`.
    #[test]
    fn exec_identity_carries_the_workload_user_and_workdir() {
        let user = ResolvedUser {
            uid: 101,
            gid: 102,
            home: None,
        };
        let id = ExecIdentity::new(Some("/srv/app"), Some(&user)).unwrap();
        assert_eq!(id.workdir.as_c_str().to_str().unwrap(), "/srv/app");
        assert_eq!(id.ids, Some((Uid::from_raw(101), Gid::from_raw(102))));

        // No user and no workdir: root in `/`, as the image asks.
        let id = ExecIdentity::new(None, None).unwrap();
        assert_eq!(id.workdir.as_c_str().to_str().unwrap(), "/");
        assert_eq!(id.ids, None);
    }

    /// F56: an absolute link on PATH resolves inside the rootfs.
    #[test]
    fn exe_lookup_follows_links_inside_the_rootfs() {
        let dir = tempfile::tempdir().unwrap();
        let rootfs = dir.path().join("rootfs");
        fs::create_dir_all(rootfs.join("bin")).unwrap();
        fs::create_dir_all(dir.path().join("bin")).unwrap();
        // Executable only beside the rootfs, where the link would land if
        // it were followed on cinit's side.
        let outside = dir.path().join("bin/outside");
        fs::write(&outside, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink(&outside, rootfs.join("bin/tool")).unwrap();
        let rootfs = rootfs.to_str().unwrap().to_string();
        let env = vec![("PATH".to_string(), "/bin".to_string())];
        assert!(resolve_exe(&rootfs, "tool", &env).is_err());
        assert!(resolve_exe(&rootfs, "/bin/tool", &env).is_err());
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
