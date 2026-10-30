//! What a macOS guest's own answers mean, as values: portable so the tests
//! run on any build host, compiled into the agent only for macOS, where
//! `unix/macos.rs` feeds it what the Mach host, `sysctl` and `getmntinfo`
//! said.

use vmlab_agent_proto::{OsInfo, ShutdownMode};

/// The three facts `SystemVersion.plist` carries that a person reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemVersion {
    /// `macOS`.
    pub name: String,
    /// `26.7.2`.
    pub version: String,
    /// `25G332`.
    pub build: String,
}

/// Read the product name, version and build out of `SystemVersion.plist`'s
/// XML. A plist dictionary is `<key>K</key>` followed by its value, and every
/// value wanted here is a `<string>`, so this reads exactly that pair and
/// nothing else of the format.
pub fn parse_system_version(plist: &str) -> Option<SystemVersion> {
    let string_after = |key: &str| -> Option<String> {
        let at = plist.find(&format!("<key>{key}</key>"))?;
        let rest = &plist[at..];
        let open = rest.find("<string>")? + "<string>".len();
        let close = rest[open..].find("</string>")?;
        Some(rest[open..open + close].trim().to_string())
    };
    Some(SystemVersion {
        name: string_after("ProductName")?,
        version: string_after("ProductVersion")?,
        build: string_after("ProductBuildVersion").unwrap_or_default(),
    })
}

/// The handshake's [`OsInfo`] for a macOS guest: the product as Apple names
/// it, and the Darwin kernel `uname` reports. A guest whose plist could not
/// be read still answers, with the kernel's view.
pub fn os_info(version: Option<SystemVersion>, uname: (String, String, String)) -> OsInfo {
    let (kernel, arch, hostname) = uname;
    let (name, version) = match version {
        Some(v) if v.build.is_empty() => (format!("{} {}", v.name, v.version), v.version),
        Some(v) => (format!("{} {} ({})", v.name, v.version, v.build), v.version),
        None => ("macOS".to_string(), String::new()),
    };
    OsInfo {
        id: "macos".to_string(),
        name,
        version,
        kernel,
        arch,
        hostname,
    }
}

/// The Mach host's CPU tick counters, in `host_cpu_load_info`'s order:
/// user, system, idle, nice.
pub type CpuTicks = [u32; 4];

/// Cumulative (busy, total) ticks: everything but idle is busy.
pub fn cpu_sample(ticks: CpuTicks) -> (u64, u64) {
    let [user, system, idle, nice] = ticks.map(u64::from);
    (user + system + nice, user + system + idle + nice)
}

/// The page counts `HOST_VM_INFO64` answers that "used" is made of.
#[derive(Debug, Clone, Copy, Default)]
pub struct PageCounts {
    pub wired: u64,
    /// Anonymous memory: what processes allocated.
    pub internal: u64,
    /// The part of `internal` the kernel may drop on its own.
    pub purgeable: u64,
    /// Pages held by the compressor.
    pub compressed: u64,
}

/// Bytes in use, as Activity Monitor's "Memory Used" counts it: app memory
/// (anonymous pages less the purgeable ones), wired memory and the
/// compressor. The file cache is not in it, which makes it the same
/// measure as Linux's `MemTotal - MemAvailable`: what a new allocation
/// cannot simply take.
pub fn memory_used(pages: PageCounts, page_size: u64) -> u64 {
    let app = pages.internal.saturating_sub(pages.purgeable);
    (app + pages.wired + pages.compressed) * page_size
}

/// Whether a mounted filesystem is one a person means by "the guest's
/// disks": local, backed by a device, and either browsable or the Data
/// volume. On an APFS system `/` is the sealed, read-only system snapshot
/// and the files a person writes live on `/System/Volumes/Data`, which is
/// mounted `nobrowse` like every other system volume (Preboot, VM, Update…)
/// — so it is let through by name, and they are not.
pub fn reported_mount(from: &str, on: &str, local: bool, nobrowse: bool) -> bool {
    local && from.starts_with("/dev/") && (!nobrowse || on == "/System/Volumes/Data")
}

/// The command that brings a macOS guest down in `mode`. macOS has no halt
/// that leaves the machine powered: `shutdown -h` powers a Mac off, so a halt
/// is a power-down here.
pub fn shutdown_argv(mode: ShutdownMode) -> [&'static str; 3] {
    match mode {
        ShutdownMode::Powerdown | ShutdownMode::Halt => ["/sbin/shutdown", "-h", "now"],
        ShutdownMode::Reboot => ["/sbin/shutdown", "-r", "now"],
    }
}

/// Run `tool` (`pbcopy` or `pbpaste`) as the console user, inside that
/// user's launchd session — the only place their pasteboard is. `launchctl
/// asuser` puts the command in the session; it keeps root's uid, so `sudo
/// -u` drops to the user's own.
pub fn pasteboard_argv(console_uid: u32, tool: &str) -> Vec<String> {
    vec![
        "/bin/launchctl".to_string(),
        "asuser".to_string(),
        console_uid.to_string(),
        "/usr/bin/sudo".to_string(),
        "-u".to_string(),
        format!("#{console_uid}"),
        tool.to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAHOE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>BuildID</key>
	<string>8A0C1C5E-0000-11F0-0000-000000000000</string>
	<key>ProductBuildVersion</key>
	<string>25G332</string>
	<key>ProductCopyright</key>
	<string>1983-2026 Apple Inc.</string>
	<key>ProductName</key>
	<string>macOS</string>
	<key>ProductUserVisibleVersion</key>
	<string>26.7.2</string>
	<key>ProductVersion</key>
	<string>26.7.2</string>
	<key>iOSSupportVersion</key>
	<string>26.7</string>
</dict>
</plist>
"#;

    fn uname() -> (String, String, String) {
        ("25.7.0".into(), "x86_64".into(), "mac01".into())
    }

    #[test]
    fn the_plist_names_the_product_its_version_and_build() {
        let v = parse_system_version(TAHOE).unwrap();
        assert_eq!(v.name, "macOS");
        assert_eq!(v.version, "26.7.2");
        assert_eq!(v.build, "25G332");
        // A key whose name another key ends with is not mistaken for it.
        assert_ne!(v.version, "26.7");
    }

    #[test]
    fn os_info_is_the_product_and_the_darwin_kernel() {
        let info = os_info(parse_system_version(TAHOE), uname());
        assert_eq!(info.id, "macos");
        assert_eq!(info.name, "macOS 26.7.2 (25G332)");
        assert_eq!(info.version, "26.7.2");
        assert_eq!(info.kernel, "25.7.0");
        assert_eq!(info.arch, "x86_64");
        assert_eq!(info.hostname, "mac01");
    }

    #[test]
    fn an_unreadable_plist_still_answers_as_macos() {
        assert_eq!(parse_system_version("<plist><dict></dict></plist>"), None);
        let info = os_info(None, uname());
        assert_eq!(info.id, "macos");
        assert_eq!(info.name, "macOS");
        assert_eq!(info.kernel, "25.7.0");
    }

    #[test]
    fn idle_is_the_only_tick_that_is_not_busy() {
        assert_eq!(cpu_sample([10, 20, 70, 5]), (35, 105));
        assert_eq!(cpu_sample([0; 4]), (0, 0));
        // Counters near the top of a u32 add up without overflowing.
        let (busy, total) = cpu_sample([u32::MAX; 4]);
        assert_eq!(total, 4 * u64::from(u32::MAX));
        assert_eq!(busy, 3 * u64::from(u32::MAX));
    }

    #[test]
    fn used_memory_leaves_out_purgeable_pages_and_the_file_cache() {
        let pages = PageCounts {
            wired: 100,
            internal: 1000,
            purgeable: 200,
            compressed: 50,
        };
        assert_eq!(memory_used(pages, 4096), (800 + 100 + 50) * 4096);
        // More purgeable than internal (a racy sample) is not negative.
        let odd = PageCounts {
            internal: 1,
            purgeable: 5,
            ..PageCounts::default()
        };
        assert_eq!(memory_used(odd, 4096), 0);
    }

    #[test]
    fn the_root_and_the_data_volume_are_reported_and_the_rest_are_not() {
        assert!(reported_mount("/dev/disk3s1s1", "/", true, false));
        assert!(reported_mount(
            "/dev/disk3s5",
            "/System/Volumes/Data",
            true,
            true
        ));
        assert!(!reported_mount(
            "/dev/disk3s2",
            "/System/Volumes/Preboot",
            true,
            true
        ));
        assert!(!reported_mount(
            "/dev/disk3s6",
            "/System/Volumes/VM",
            true,
            true
        ));
        assert!(!reported_mount("devfs", "/dev", true, false));
        assert!(!reported_mount(
            "map auto_home",
            "/System/Volumes/Data/home",
            false,
            true
        ));
        // A second disk the user mounted is reported like any other.
        assert!(reported_mount("/dev/disk4s1", "/Volumes/Work", true, false));
        // Not local: a share.
        assert!(!reported_mount(
            "/dev/disk9",
            "/Volumes/share",
            false,
            false
        ));
    }

    #[test]
    fn a_halt_powers_a_mac_down() {
        assert_eq!(
            shutdown_argv(ShutdownMode::Halt),
            shutdown_argv(ShutdownMode::Powerdown)
        );
        assert_eq!(
            shutdown_argv(ShutdownMode::Reboot),
            ["/sbin/shutdown", "-r", "now"]
        );
    }

    #[test]
    fn the_pasteboard_is_reached_inside_the_console_users_session_as_them() {
        assert_eq!(
            pasteboard_argv(501, "/usr/bin/pbpaste"),
            [
                "/bin/launchctl",
                "asuser",
                "501",
                "/usr/bin/sudo",
                "-u",
                "#501",
                "/usr/bin/pbpaste"
            ]
        );
    }
}
