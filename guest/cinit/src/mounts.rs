//! Filesystem assembly: early pseudo-filesystems, the squashfs + ext4-scratch
//! overlay that becomes the container root, CIFS volume mounts, and the
//! runtime mounts inside the container rootfs.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::process::Command;

use nix::mount::{MsFlags, mount};

use crate::util::{Ctx, Result};

/// Container rootfs mount point (overlay of squashfs + scratch).
pub const ROOTFS: &str = "/rootfs";
const ROOTFS_RO: &str = "/rootfs-ro";
const SCRATCH: &str = "/scratch";

pub fn ensure_dir(path: &str) -> Result<()> {
    fs::create_dir_all(path).ctx(&format!("mkdir -p {path}"))
}

fn do_mount(
    source: &str,
    target: &str,
    fstype: &str,
    flags: MsFlags,
    data: Option<&str>,
) -> Result<()> {
    mount(Some(source), target, Some(fstype), flags, data).ctx(&format!(
        "mount -t {fstype} {source} {target} ({data})",
        data = data.unwrap_or("-")
    ))
}

/// Mount /proc, /sys and devtmpfs /dev in the initramfs root.
pub fn mount_early() -> Result<()> {
    for d in ["/proc", "/sys", "/dev", "/run"] {
        ensure_dir(d)?;
    }
    do_mount("proc", "/proc", "proc", MsFlags::empty(), None)?;
    do_mount("sysfs", "/sys", "sysfs", MsFlags::empty(), None)?;
    do_mount("devtmpfs", "/dev", "devtmpfs", MsFlags::empty(), None)?;
    // devpts in the init namespace too: the interactive-shell PTY (tty.rs)
    // is allocated here, not inside the container root — without this,
    // openpty fails with ENODEV.
    ensure_dir("/dev/pts")?;
    do_mount(
        "devpts",
        "/dev/pts",
        "devpts",
        MsFlags::empty(),
        Some("mode=0620,ptmxmode=0666"),
    )?;
    Ok(())
}

/// Load the kernel modules listed (dependency-first) in /etc/vmlab-modules,
/// written by guest/build-asset.sh alongside the trimmed /lib/modules tree.
pub fn load_modules() -> Result<()> {
    let list = fs::read_to_string("/etc/vmlab-modules").ctx("read /etc/vmlab-modules")?;
    for name in list.lines().map(str::trim) {
        if name.is_empty() || name.starts_with('#') {
            continue;
        }
        let st = Command::new("/bin/busybox")
            .args(["modprobe", name])
            .status()
            .ctx(&format!("run modprobe {name}"))?;
        if !st.success() {
            return Err(format!("modprobe {name} failed ({st})"));
        }
    }
    Ok(())
}

/// True when `dev` carries an ext4 (ext2/3/4 family) superblock: magic
/// 0xEF53, little-endian at byte offset 1080 (superblock at 1024 + 56).
pub fn has_ext4_superblock(dev: &Path) -> Result<bool> {
    let mut f = fs::File::open(dev).ctx(&format!("open {}", dev.display()))?;
    f.seek(SeekFrom::Start(1080))
        .ctx(&format!("seek {}", dev.display()))?;
    let mut magic = [0u8; 2];
    match f.read_exact(&mut magic) {
        Ok(()) => Ok(magic == [0x53, 0xEF]),
        // Shorter than a superblock — certainly not ext4.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(format!("read {}: {e}", dev.display())),
    }
}

/// Assemble the container root: squashfs (RO) + ext4 scratch (formatted on
/// first boot) merged by overlayfs at [`ROOTFS`].
pub fn mount_container_root(root_dev: &str, scratch_dev: &str) -> Result<()> {
    ensure_dir(ROOTFS_RO)?;
    do_mount(root_dev, ROOTFS_RO, "squashfs", MsFlags::MS_RDONLY, None)?;

    if !has_ext4_superblock(Path::new(scratch_dev))? {
        println!("vmlab-cinit: formatting scratch {scratch_dev}");
        let st = Command::new("/sbin/mkfs.ext4")
            .args(["-q", "-F", scratch_dev])
            .status()
            .ctx("run mkfs.ext4")?;
        if !st.success() {
            return Err(format!("mkfs.ext4 {scratch_dev} failed ({st})"));
        }
    }
    ensure_dir(SCRATCH)?;
    do_mount(scratch_dev, SCRATCH, "ext4", MsFlags::empty(), None)?;
    ensure_dir("/scratch/upper")?;
    ensure_dir("/scratch/work")?;

    ensure_dir(ROOTFS)?;
    do_mount(
        "overlay",
        ROOTFS,
        "overlay",
        MsFlags::empty(),
        Some("lowerdir=/rootfs-ro,upperdir=/scratch/upper,workdir=/scratch/work"),
    )
}

/// Mount one virtiofs volume inside the container rootfs, from the
/// vhost-user-fs device the host attached for it (proto v4). Native FUSE
/// over shared memory — no network, no credentials — so these mount before
/// DHCP. Snapshot-safe: virtiofsd migrates its state through QEMU's
/// migration stream (the host runs it with `--migration-mode`), which is
/// exactly what an online snapshot stores.
pub fn mount_virtiofs(tag: &str, target: &str, read_only: bool) -> Result<()> {
    let inside = format!("{ROOTFS}/{}", target.trim_start_matches('/'));
    ensure_dir(&inside)?;
    let flags = if read_only {
        MsFlags::MS_RDONLY
    } else {
        MsFlags::empty()
    };
    do_mount(tag, &inside, "virtiofs", flags, None)
}

/// Mount one volume share inside the container rootfs over CIFS, from the
/// lab's SMB server at the segment gateway (PRD §18: volumes are network
/// mounts precisely so no filesystem device state lands in snapshots).
/// Requires the network to be up. Options mirror the VM shared-folder mount
/// (§7.5, `vers=3.0`); `ip=` skips in-kernel name resolution — the source is
/// already the gateway address. `echo_interval=5` makes the client notice a
/// dead session in ~10s instead of the 2-minute default: after an online
/// snapshot restore the rewound TCP session is stale, and the first volume
/// access stalls until the client re-establishes it (same semantics as VM
/// shares across restore, §7.5).
pub fn mount_volume(
    smb: &vmlab_cinit_proto::SmbInfo,
    share: &str,
    target: &str,
    read_only: bool,
) -> Result<()> {
    let inside = format!("{ROOTFS}/{}", target.trim_start_matches('/'));
    ensure_dir(&inside)?;
    let source = format!("//{}/{}", smb.gateway, share);
    let opts = format!(
        "username={},password={},vers=3.0,ip={},echo_interval=5",
        smb.username, smb.password, smb.gateway
    );
    let flags = if read_only {
        MsFlags::MS_RDONLY
    } else {
        MsFlags::empty()
    };
    // Not do_mount: its error context echoes the option string, which here
    // carries the SMB credential — keep that out of the console log.
    mount(
        Some(source.as_str()),
        inside.as_str(),
        Some("cifs"),
        flags,
        Some(opts.as_str()),
    )
    .ctx(&format!("mount -t cifs {source} {inside}"))
}

/// The runtime mounts every container expects: /proc, /sys, /dev, /dev/pts,
/// /dev/shm and a tmpfs /tmp under the container rootfs.
pub fn mount_rootfs_runtime() -> Result<()> {
    for d in [
        "/rootfs/proc",
        "/rootfs/sys",
        "/rootfs/dev",
        "/rootfs/tmp",
        "/rootfs/etc",
    ] {
        ensure_dir(d)?;
    }
    do_mount("proc", "/rootfs/proc", "proc", MsFlags::empty(), None)?;
    do_mount("sysfs", "/rootfs/sys", "sysfs", MsFlags::empty(), None)?;
    do_mount(
        "devtmpfs",
        "/rootfs/dev",
        "devtmpfs",
        MsFlags::empty(),
        None,
    )?;
    ensure_dir("/rootfs/dev/pts")?;
    ensure_dir("/rootfs/dev/shm")?;
    do_mount(
        "devpts",
        "/rootfs/dev/pts",
        "devpts",
        MsFlags::empty(),
        Some("gid=5,mode=620,ptmxmode=666"),
    )?;
    do_mount(
        "tmpfs",
        "/rootfs/dev/shm",
        "tmpfs",
        MsFlags::empty(),
        Some("mode=1777"),
    )?;
    do_mount(
        "tmpfs",
        "/rootfs/tmp",
        "tmpfs",
        MsFlags::empty(),
        Some("mode=1777"),
    )?;
    Ok(())
}

/// Best-effort: copy the initramfs' static busybox into the overlay at
/// [`vmlab_agent_proto::BUSYBOX_FALLBACK`] so the agent's interactive shell
/// has something to exec even in a distroless image (tiny, and it lands in
/// the scratch upper layer, never the read-only image). Failures only cost
/// the fallback.
pub fn install_shell_fallback() {
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::fs::symlink;
    let dir = format!("{ROOTFS}/.vmlab");
    let dest = format!("{dir}/busybox");
    let bin_dir = format!("{ROOTFS}{}", vmlab_agent_proto::BUSYBOX_BIN_DIR);
    let result = fs::create_dir_all(&dir)
        .and_then(|()| fs::copy("/bin/busybox", &dest).map(|_| ()))
        .and_then(|()| fs::set_permissions(&dest, fs::Permissions::from_mode(0o755)))
        .and_then(|()| fs::create_dir_all(&bin_dir))
        .and_then(|()| {
            let output = Command::new("/bin/busybox").arg("--list").output()?;
            if !output.status.success() {
                return Err(std::io::Error::other("busybox --list failed"));
            }
            for applet in String::from_utf8_lossy(&output.stdout).lines() {
                let link = format!("{bin_dir}/{applet}");
                match symlink("../busybox", &link) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        });
    if let Err(e) = result {
        eprintln!("vmlab-cinit: warning: busybox shell fallback not installed: {e}");
    }
}

/// Write /etc/hostname and /etc/hosts inside the rootfs and set the (shared —
/// v1 keeps the container in the init UTS namespace) kernel hostname.
pub fn write_identity(hostname: &str) -> Result<()> {
    nix::unistd::sethostname(hostname).ctx("sethostname")?;
    fs::write("/rootfs/etc/hostname", format!("{hostname}\n")).ctx("write /etc/hostname")?;
    fs::write(
        "/rootfs/etc/hosts",
        format!("127.0.0.1\tlocalhost\n127.0.1.1\t{hostname}\n"),
    )
    .ctx("write /etc/hosts")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn ext4_probe_detects_magic() {
        let dir = tempfile::tempdir().unwrap();
        let dev = dir.path().join("disk");
        let mut f = fs::File::create(&dev).unwrap();
        f.write_all(&[0u8; 1080]).unwrap();
        f.write_all(&[0x53, 0xEF]).unwrap();
        drop(f);
        assert!(has_ext4_superblock(&dev).unwrap());
    }

    #[test]
    fn ext4_probe_rejects_blank_and_short() {
        let dir = tempfile::tempdir().unwrap();
        let blank = dir.path().join("blank");
        fs::write(&blank, vec![0u8; 4096]).unwrap();
        assert!(!has_ext4_superblock(&blank).unwrap());

        let short = dir.path().join("short");
        fs::write(&short, b"tiny").unwrap();
        assert!(!has_ext4_superblock(&short).unwrap());
    }
}
