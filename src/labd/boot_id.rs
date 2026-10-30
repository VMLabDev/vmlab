//! Boot identity: an opaque string that is the same for the whole of one guest
//! boot and different after any reboot (§10.3).
//!
//! The live agent ping cannot see a reboot that finishes between two probes —
//! a fast Windows guest answers the next ping through the new boot's agent —
//! so a script that asked its guest to reboot compares boot identities
//! instead. Read host-side over the agent's plain `exec`, the one feature
//! every agent tier serves, legacy included.
//!
//! What each family reads:
//!
//! - **Linux** (VMs, container micro-VMs, the legacy tier's old Linux):
//!   `/proc/sys/kernel/random/boot_id`, a UUID the kernel draws once per boot.
//! - **Windows**: the kernel's boot counter, `BootId` under the prefetcher's
//!   registry key (Vista onward), read with `reg.exe` — no PowerShell start-up
//!   on the hot path, and unmoved by a clock step, which a restore's
//!   `set_time` makes. Where that value is missing (NT4 through XP/2003) the
//!   last boot time from WMI, `wmic` first and PowerShell where Windows has
//!   dropped `wmic`, normalised to UTC at full precision.
//! - **macOS**: `kern.bootsessionuuid`, a UUID the kernel draws once per
//!   boot, read with `sysctl`.
//! - **Windows 9x/ME, DOS and TempleOS** have nothing a command can read that
//!   survives the boot and changes with the next one, so they refuse by name.

use std::future::Future;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};

use super::machine::Machine;

/// One command that may answer a boot identity, and how to read its output.
#[derive(Debug, Clone, Copy)]
pub struct Probe {
    pub argv: &'static [&'static str],
    pub parse: fn(&str) -> Option<String>,
}

const LINUX: &[Probe] = &[Probe {
    argv: &["cat", "/proc/sys/kernel/random/boot_id"],
    parse: parse_linux,
}];

const MACOS: &[Probe] = &[Probe {
    argv: &["/usr/sbin/sysctl", "-n", "kern.bootsessionuuid"],
    parse: parse_linux,
}];

const PREFETCH_KEY: &str =
    r"HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Memory Management\PrefetchParameters";

const WINDOWS: &[Probe] = &[
    Probe {
        argv: &["reg", "query", PREFETCH_KEY, "/v", "BootId"],
        parse: parse_reg_boot_id,
    },
    Probe {
        argv: &["wmic", "os", "get", "LastBootUpTime", "/value"],
        parse: parse_wmi_boot_time,
    },
    Probe {
        argv: &[
            "powershell.exe",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-WmiObject Win32_OperatingSystem).LastBootUpTime",
        ],
        parse: parse_wmi_boot_time,
    },
];

/// The probes for a guest, tried in order, from the agent handshake's `os`
/// tag and the machine's profile — the tag alone cannot tell 9x from NT, the
/// legacy agent answering `windows` on both.
pub fn probes(os: &str, profile: Option<&str>) -> Result<&'static [Probe], String> {
    match (os, profile) {
        (_, Some("windows-9x")) => Err(no_identity("Windows 9x/ME")),
        ("linux", _) => Ok(LINUX),
        ("windows", _) => Ok(WINDOWS),
        ("macos", _) => Ok(MACOS),
        ("dos", _) => Err(no_identity("DOS")),
        ("templeos", _) => Err(no_identity("TempleOS")),
        (other, _) => Err(no_identity(&format!(
            "a guest whose agent reports `{other}`"
        ))),
    }
}

fn no_identity(family: &str) -> String {
    format!(
        "{family} has no boot identity vmlab can read; watch the reboot another way \
         (agent_answering, or the screen)"
    )
}

/// The kernel's boot UUID, as it prints it — Linux's `boot_id` and macOS's
/// `kern.bootsessionuuid` alike.
pub fn parse_linux(out: &str) -> Option<String> {
    let id = out.trim();
    let ok = id.len() == 36
        && id.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    ok.then(|| id.to_ascii_lowercase())
}

/// `reg query … /v BootId` prints `    BootId    REG_DWORD    0x2a`.
pub fn parse_reg_boot_id(out: &str) -> Option<String> {
    let line = out
        .lines()
        .find(|l| l.split_whitespace().next() == Some("BootId"))?;
    let mut fields = line.split_whitespace().skip(1);
    if fields.next()? != "REG_DWORD" {
        return None;
    }
    let hex = fields.next()?.strip_prefix("0x")?;
    let n = u32::from_str_radix(hex, 16).ok()?;
    Some(format!("win-bootid:{n}"))
}

/// A WMI datetime — `yyyymmddHHMMSS.ffffff±UUU`, local time and its offset
/// from UTC in minutes — wherever it appears in the output (`wmic … /value`
/// prefixes `LastBootUpTime=`), as UTC at full precision: the same instant
/// reads the same whichever time zone the guest is in today.
pub fn parse_wmi_boot_time(out: &str) -> Option<String> {
    out.split(|c: char| c.is_whitespace() || c == '=')
        .find_map(normalise_wmi_datetime)
        .map(|t| format!("win-boot:{t}"))
}

fn normalise_wmi_datetime(s: &str) -> Option<String> {
    use chrono::{Duration as Delta, NaiveDateTime};
    if s.len() != 25 || !s.is_char_boundary(14) || !s.is_char_boundary(21) {
        return None;
    }
    let (stamp, rest) = s.split_at(14);
    let (frac, offset) = rest.split_at(7);
    let frac = frac.strip_prefix('.')?;
    if !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let sign = match offset.as_bytes()[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let minutes: i64 = offset[1..].parse().ok()?;
    let local = NaiveDateTime::parse_from_str(stamp, "%Y%m%d%H%M%S").ok()?;
    let utc = local - Delta::minutes(sign * minutes);
    Some(format!("{}.{frac}Z", utc.format("%Y-%m-%dT%H:%M:%S")))
}

/// How long one probe may take. Long enough for a cold PowerShell on a busy
/// Windows guest, short enough that a probe caught by a shutdown does not eat
/// a reboot's whole budget.
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);

/// Read the machine's boot identity over its agent.
pub async fn read(m: &dyn Machine) -> Result<String> {
    let agent = m.agent().await?;
    let info = agent.info();
    let probes =
        probes(&info.os, m.profile().as_deref()).map_err(|e| anyhow!("{}: {e}", m.name()))?;
    let mut tried = Vec::new();
    for probe in probes {
        let argv: Vec<String> = probe.argv.iter().map(|s| s.to_string()).collect();
        match agent
            .exec(argv, vec![], None, None, PROBE_TIMEOUT, None)
            .await
        {
            Ok(out) => {
                let stdout = String::from_utf8_lossy(&out.stdout);
                if out.exit_code == 0
                    && let Some(id) = (probe.parse)(&stdout)
                {
                    return Ok(id);
                }
                tried.push(format!("`{}` exited {}", probe.argv[0], out.exit_code));
            }
            Err(e) => tried.push(format!("`{}`: {e}", probe.argv[0])),
        }
    }
    bail!(
        "{}: no boot identity readable ({})",
        m.name(),
        tried.join("; ")
    )
}

/// How often [`wait_changed`] asks.
pub const POLL: Duration = Duration::from_secs(1);

/// Ask `probe` until it answers something other than `since`, tolerating
/// failures — the guest is down for part of any reboot — and return the new
/// identity. Times out naming the last thing it saw.
pub async fn wait_changed<F, Fut>(
    name: &str,
    since: &str,
    timeout: Duration,
    poll: Duration,
    mut probe: F,
) -> Result<String>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last = format!("boot id still `{since}`");
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            bail!(
                "{name}: no reboot seen within {}s ({last})",
                timeout.as_secs()
            );
        }
        match tokio::time::timeout(left, probe()).await {
            Ok(Ok(id)) if id != since => return Ok(id),
            Ok(Ok(id)) => last = format!("boot id still `{id}`"),
            Ok(Err(e)) => last = format!("last probe failed: {e:#}"),
            Err(_) => last = "last probe timed out".into(),
        }
        tokio::time::sleep(
            poll.min(deadline.saturating_duration_since(tokio::time::Instant::now())),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn each_family_gets_its_probes() {
        assert_eq!(
            probes("linux", Some("linux-modern")).unwrap()[0].argv,
            ["cat", "/proc/sys/kernel/random/boot_id"]
        );
        assert_eq!(probes("linux", Some("container")).unwrap().len(), 1);
        let win = probes("windows", Some("windows-11")).unwrap();
        assert_eq!(win[0].argv[0], "reg");
        assert_eq!(win[0].argv[4], "BootId");
        assert_eq!(win[1].argv[0], "wmic");
        assert_eq!(win[2].argv[0], "powershell.exe");
        assert_eq!(
            probes("macos", Some("macos")).unwrap()[0].argv,
            ["/usr/sbin/sysctl", "-n", "kern.bootsessionuuid"]
        );
        assert_eq!(
            parse_linux("9F1D3C2A-6B1E-4C7D-8E2F-0123456789AB\n").as_deref(),
            Some("9f1d3c2a-6b1e-4c7d-8e2f-0123456789ab")
        );
        // The legacy agent says `windows` on NT and 9x alike; NT gets the
        // probes and falls through to WMI, 9x is refused by its profile.
        assert_eq!(probes("windows", Some("windows-xp")).unwrap().len(), 3);
        for (os, profile, family) in [
            ("windows", Some("windows-9x"), "Windows 9x/ME"),
            ("dos", Some("windows-9x"), "Windows 9x/ME"),
            ("dos", Some("custom"), "DOS"),
            ("templeos", Some("templeos"), "TempleOS"),
        ] {
            let err = probes(os, profile).unwrap_err();
            assert!(err.starts_with(family), "{err}");
            assert!(err.contains("no boot identity"), "{err}");
        }
        assert!(probes("plan9", None).unwrap_err().contains("`plan9`"));
    }

    #[test]
    fn linux_boot_id_is_the_uuid_alone() {
        assert_eq!(
            parse_linux("0F6C3D1E-9b2a-4c55-8e1d-2a3b4c5d6e7f\n").as_deref(),
            Some("0f6c3d1e-9b2a-4c55-8e1d-2a3b4c5d6e7f")
        );
        assert_eq!(parse_linux(""), None);
        assert_eq!(parse_linux("cat: can't open"), None);
        assert_eq!(parse_linux("0f6c3d1e-9b2a-4c55-8e1d-2a3b4c5d6e7"), None);
    }

    #[test]
    fn reg_boot_id_is_the_dword() {
        let out = "\r\nHKEY_LOCAL_MACHINE\\SYSTEM\\CurrentControlSet\\Control\\Session Manager\\\
                   Memory Management\\PrefetchParameters\r\n    BootId    REG_DWORD    0x2a\r\n\r\n";
        assert_eq!(parse_reg_boot_id(out).as_deref(), Some("win-bootid:42"));
        assert_eq!(
            parse_reg_boot_id(
                "ERROR: The system was unable to find the specified registry key or value."
            ),
            None
        );
        assert_eq!(parse_reg_boot_id("    BootId    REG_SZ    x"), None);
    }

    #[test]
    fn wmi_boot_time_is_normalised_to_utc_at_full_precision() {
        // wmic's `/value` form, a guest east of UTC.
        assert_eq!(
            parse_wmi_boot_time("\r\r\nLastBootUpTime=20261018093012.500000+600\r\r\n").as_deref(),
            Some("win-boot:2026-10-17T23:30:12.500000Z")
        );
        // PowerShell's bare string, west of UTC across a month boundary.
        assert_eq!(
            parse_wmi_boot_time("20261031223000.123456-300\r\n").as_deref(),
            Some("win-boot:2026-11-01T03:30:00.123456Z")
        );
        // The same instant from two time zones is one identity.
        assert_eq!(
            parse_wmi_boot_time("20261018000000.000001+000"),
            parse_wmi_boot_time("20261018053000.000001+330")
        );
        assert_eq!(parse_wmi_boot_time("LastBootUpTime="), None);
        assert_eq!(parse_wmi_boot_time("20261018093012.5+600"), None);
        assert_eq!(parse_wmi_boot_time("20261318093012.500000+600"), None);
    }

    /// A machine's boot-id answers in order; the last one repeats.
    fn scripted(
        answers: Vec<Result<&'static str, &'static str>>,
    ) -> impl FnMut() -> std::future::Ready<Result<String>> {
        let answers = Mutex::new(answers);
        move || {
            let mut a = answers.lock().unwrap();
            let next = if a.len() > 1 { a.remove(0) } else { a[0] };
            std::future::ready(next.map(String::from).map_err(|e| anyhow!(e)))
        }
    }

    #[tokio::test(start_paused = true)]
    async fn waits_through_unchanged_and_failing_probes_for_a_new_id() {
        let id = wait_changed(
            "box",
            "old",
            Duration::from_secs(60),
            POLL,
            scripted(vec![
                Ok("old"),
                Ok("old"),
                Err("agent: connection reset"),
                Err("agent: not connected"),
                Ok("new"),
            ]),
        )
        .await
        .unwrap();
        assert_eq!(id, "new");
    }

    #[tokio::test(start_paused = true)]
    async fn a_reboot_finished_before_the_first_probe_is_still_seen() {
        let id = wait_changed(
            "box",
            "old",
            Duration::from_secs(5),
            POLL,
            scripted(vec![Ok("new")]),
        )
        .await
        .unwrap();
        assert_eq!(id, "new");
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_naming_the_last_id_seen() {
        let err = wait_changed(
            "box",
            "old",
            Duration::from_secs(10),
            POLL,
            scripted(vec![Ok("old")]),
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(err, "box: no reboot seen within 10s (boot id still `old`)");
    }

    #[tokio::test(start_paused = true)]
    async fn times_out_naming_the_last_failure() {
        let err = wait_changed(
            "box",
            "old",
            Duration::from_secs(10),
            POLL,
            scripted(vec![Ok("old"), Err("agent: not connected")]),
        )
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(
            err,
            "box: no reboot seen within 10s (last probe failed: agent: not connected)"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_hung_probe_cannot_outlive_the_budget() {
        let err = wait_changed("box", "old", Duration::from_secs(10), POLL, || {
            std::future::pending::<Result<String>>()
        })
        .await
        .unwrap_err()
        .to_string();
        assert_eq!(err, "box: no reboot seen within 10s (last probe timed out)");
    }
}
