//! A lost host GPU (win3d C13, `guest/win3d/docs/device-lost.md`).
//!
//! The renderer's host GPU can go away under a running guest: its render
//! server process ends, or its render node goes away with its driver. The
//! device then stops waiting on it and reports the loss, so that the guest
//! sees a removed device and not a VM that has stopped:
//!
//! - Every fence the device holds is answered, and every later command that
//!   needs the host GPU is answered `ERR_UNSPEC` at once. Commands that only
//!   release guest state still answer `OK_NODATA`, so that a guest can tear
//!   its objects down. The display info and the EDID are served as before.
//! - With [`super::ORDERED_FLIP_FEATURE`] acked, `events_read` carries
//!   [`super::EVENT_DEVICE_LOST`] and a configuration interrupt is raised.
//!   A host GPU does not come back, so every later negotiation of the bit
//!   (after a device reset) raises the event again. A driver without the bit
//!   (Linux, QEMU's drivers) sees only the error answers.
//!
//! [`Watch`] is the detection. It runs on its own thread and holds no device
//! lock while it looks, because a command that waits on a dead host GPU can
//! hold the device lock for seconds.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The commands that only release what the guest holds: `RESOURCE_UNREF`,
/// `DETACH_BACKING`, `CTX_DESTROY`, `CTX_DETACH_RESOURCE` and
/// `RESOURCE_UNMAP_BLOB`. After a loss they still answer `OK_NODATA`.
pub(super) fn releases(kind: u32) -> bool {
    matches!(kind, 0x102 | 0x107 | 0x201 | 0x203 | 0x209)
}

/// How often [`Watch`] looks.
pub(super) const INTERVAL: Duration = Duration::from_millis(250);

/// Set once when the host GPU is lost. The renderer reads it before each
/// call, so that no call waits on a host GPU that is gone.
#[derive(Clone, Default)]
pub(super) struct Flag(Arc<AtomicBool>);

impl Flag {
    pub(super) fn set(&self) -> bool {
        !self.0.swap(true, Ordering::SeqCst)
    }

    pub(super) fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// One way to see that the host GPU is gone. `None` while it is there.
pub(super) trait Probe: Send {
    fn lost(&mut self) -> Option<String>;
}

/// The probes of one renderer.
pub(super) struct Watch {
    probes: Vec<Box<dyn Probe>>,
}

impl Watch {
    pub(super) fn new(probes: Vec<Box<dyn Probe>>) -> Self {
        Self { probes }
    }

    /// The probes for the renderer on `node`, and for `servers`, the child
    /// processes its start made: virglrenderer's render server.
    pub(super) fn for_renderer(node: &Path, servers: &[u32]) -> Self {
        let mut probes: Vec<Box<dyn Probe>> = Vec::new();
        match RenderNode::new(node) {
            Some(probe) => probes.push(Box::new(probe)),
            None => log::warn!(
                "virtio-gpu: no device number for {}; a removed render node is not detected",
                node.display()
            ),
        }
        let parent = std::process::id();
        for &pid in servers {
            probes.push(Box::new(RenderServer::watching(
                Path::new("/proc"),
                pid,
                parent,
            )));
        }
        Self::new(probes)
    }

    /// The reason of the first probe that sees the host GPU gone.
    pub(super) fn check(&mut self) -> Option<String> {
        self.probes.iter_mut().find_map(|probe| probe.lost())
    }
}

/// The render node: lost when the kernel no longer has its character
/// device, as after the driver is unbound or the GPU is removed. sysfs is
/// read, not `/dev`, so that the answer does not wait for udev.
pub(super) struct RenderNode {
    node: PathBuf,
    sysfs: PathBuf,
}

impl RenderNode {
    pub(super) fn new(node: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let rdev = std::fs::metadata(node).ok()?.rdev();
        Some(Self::at(node, Path::new("/sys/dev/char"), rdev))
    }

    fn at(node: &Path, sysfs: &Path, rdev: u64) -> Self {
        Self {
            node: node.to_path_buf(),
            sysfs: sysfs.join(format!("{}:{}", libc::major(rdev), libc::minor(rdev))),
        }
    }
}

impl Probe for RenderNode {
    fn lost(&mut self) -> Option<String> {
        (!self.sysfs.exists()).then(|| format!("the render node {} is gone", self.node.display()))
    }
}

/// virglrenderer's render server, a child process that runs every Venus
/// context: lost when it has ended. Nobody reaps it while the renderer
/// lives, so an ended server stays a zombie. virglrenderer forks it without
/// a new program name, so it is known by its number: the child the
/// renderer's start made ([`children`] before and after).
pub(super) struct RenderServer {
    proc: PathBuf,
    pid: u32,
    parent: u32,
}

impl RenderServer {
    pub(super) fn watching(proc: &Path, pid: u32, parent: u32) -> Self {
        Self {
            proc: proc.to_path_buf(),
            pid,
            parent,
        }
    }
}

/// The running child processes of `parent`, in `proc`, in order.
pub(super) fn children(proc: &Path, parent: u32) -> Vec<u32> {
    let Ok(entries) = std::fs::read_dir(proc) else {
        return Vec::new();
    };
    let mut pids: Vec<u32> = entries
        .flatten()
        .filter_map(|entry| {
            let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
            let stat = Stat::read(proc, pid)?;
            (stat.parent == parent && stat.alive()).then_some(pid)
        })
        .collect();
    pids.sort_unstable();
    pids
}

impl Probe for RenderServer {
    fn lost(&mut self) -> Option<String> {
        let running = Stat::read(&self.proc, self.pid)
            .is_some_and(|stat| stat.parent == self.parent && stat.alive());
        (!running).then(|| format!("the render server (pid {}) has ended", self.pid))
    }
}

/// The fields of `/proc/<pid>/stat` the probe needs.
struct Stat {
    #[cfg_attr(not(test), allow(dead_code))]
    name: String,
    state: char,
    parent: u32,
}

impl Stat {
    fn read(proc: &Path, pid: u32) -> Option<Self> {
        Self::parse(&std::fs::read_to_string(proc.join(pid.to_string()).join("stat")).ok()?)
    }

    /// `pid (name) state ppid ...`. The name can hold spaces and
    /// parentheses, so it ends at the last `)`.
    fn parse(stat: &str) -> Option<Self> {
        let open = stat.find('(')?;
        let close = stat.rfind(')')?;
        let mut rest = stat.get(close + 1..)?.split_whitespace();
        let state = rest.next()?.chars().next()?;
        let parent = rest.next()?.parse().ok()?;
        Some(Self {
            name: stat.get(open + 1..close)?.to_string(),
            state,
            parent,
        })
    }

    fn alive(&self) -> bool {
        !matches!(self.state, 'Z' | 'X' | 'x')
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc_entry(proc: &Path, pid: u32, stat: &str) {
        let dir = proc.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stat"), stat).unwrap();
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("vmlab-gpu-lost-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_stat_line_parses_with_a_name_that_holds_parentheses() {
        let stat = Stat::parse("42 (a (b) c) S 7 42 42 0 -1").unwrap();
        assert_eq!(stat.name, "a (b) c");
        assert_eq!(stat.state, 'S');
        assert_eq!(stat.parent, 7);
        assert!(stat.alive());
        assert!(!Stat::parse("42 (x) Z 7").unwrap().alive());
        assert!(Stat::parse("42 (x)").is_none());
    }

    #[test]
    fn the_render_server_is_lost_when_it_ends_or_is_a_zombie() {
        let proc = scratch("server");
        proc_entry(&proc, 10, "10 (virgl_render_se) S 99 1 1");
        proc_entry(&proc, 11, "11 (virgl_render_se) S 5 1 1");
        proc_entry(&proc, 12, "12 (vmlab-hv) S 99 1 1");
        std::fs::create_dir_all(proc.join("self")).unwrap();

        // Only a child of this process counts.
        proc_entry(&proc, 13, "13 (gone) Z 99 1 1");
        assert_eq!(children(&proc, 99), [10, 12]);
        assert!(children(&proc, 98).is_empty());
        let mut probe = RenderServer::watching(&proc, 10, 99);
        assert_eq!(probe.lost(), None);

        // Ended, not reaped.
        proc_entry(&proc, 10, "10 (virgl_render_se) Z 99 1 1");
        assert!(probe.lost().unwrap().contains("pid 10"));
        // Reaped, and the number taken by a process of another parent.
        proc_entry(&proc, 10, "10 (virgl_render_se) S 3 1 1");
        assert!(probe.lost().is_some());
        std::fs::remove_dir_all(proc.join("10")).unwrap();
        assert!(probe.lost().is_some());
        std::fs::remove_dir_all(&proc).unwrap();
    }

    #[test]
    fn the_render_node_is_lost_when_its_sysfs_device_goes() {
        let sysfs = scratch("node");
        let rdev = libc::makedev(226, 129);
        std::fs::create_dir_all(sysfs.join("226:129")).unwrap();
        let mut probe = RenderNode::at(Path::new("/dev/dri/renderD129"), &sysfs, rdev);
        assert_eq!(probe.lost(), None);

        std::fs::remove_dir_all(sysfs.join("226:129")).unwrap();
        assert_eq!(
            probe.lost().as_deref(),
            Some("the render node /dev/dri/renderD129 is gone")
        );
        std::fs::remove_dir_all(&sysfs).unwrap();
    }

    #[test]
    fn the_watch_names_the_first_probe_that_sees_a_loss() {
        struct Fixed(Option<&'static str>);
        impl Probe for Fixed {
            fn lost(&mut self) -> Option<String> {
                self.0.map(str::to_string)
            }
        }
        let mut watch = Watch::new(vec![Box::new(Fixed(None)), Box::new(Fixed(Some("b")))]);
        assert_eq!(watch.check().as_deref(), Some("b"));
        assert_eq!(Watch::new(Vec::new()).check(), None);

        let flag = Flag::default();
        assert!(flag.set());
        assert!(!flag.set(), "a second loss is not a new one");
        assert!(flag.get());
    }
}
