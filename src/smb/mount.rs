//! Guest-side mount command generation (PRD §7.5 "Guest mounting").
//!
//! These functions only *build* command strings. The daemon executes them in
//! the guest via the vmlab guest agent, or — for XP/2003 guests with no
//! agent — drives them through the screen-automation keystroke surface
//! (§10.3). Nothing here touches a guest.
//!
//! Each builder returns `(program, args)` so callers can hand them straight to
//! an exec-style agent without re-quoting.

use std::net::Ipv4Addr;

/// Build a Linux `mount -t cifs` invocation for a share.
///
/// Returns `("mount", [args...])`:
/// `mount -t cifs //<gw>/<share> <guest_path> -o username=..,password=..,vers=..[,ro]`
///
/// `vers` is `1.0` for smb1 (NT1/CIFS) guests, else `3.0` (the modern SMB2/3
/// dialect every supported guest negotiates).
///
/// Note on uid/gid: kernel CIFS maps all files to the mounting uid/gid by
/// default unless the server advertises Unix extensions (which we do not, to
/// stay Windows-compatible). Callers that need a specific in-guest owner should
/// append `uid=<n>,gid=<n>` (and optionally `file_mode=`/`dir_mode=`) to the
/// `-o` list; we keep the baseline minimal and let the provision layer extend
/// it if a guest needs non-root ownership.
pub fn linux_mount_cmd(
    gateway: Ipv4Addr,
    share: &str,
    guest_path: &str,
    user: &str,
    pass: &str,
    readonly: bool,
    smb1: bool,
) -> (String, Vec<String>) {
    let vers = if smb1 { "1.0" } else { "3.0" };
    let mut opts = format!("username={user},password={pass},vers={vers}");
    if readonly {
        opts.push_str(",ro");
    }
    let args = vec![
        "-t".to_string(),
        "cifs".to_string(),
        format!("//{gateway}/{share}"),
        guest_path.to_string(),
        "-o".to_string(),
        opts,
    ];
    ("mount".to_string(), args)
}

/// Whether a Windows `guest` target is a bare drive letter (`X:` or `X:\`)
/// rather than a folder path. Matches `^[A-Za-z]:\\?$`.
pub fn is_drive_letter(target: &str) -> bool {
    let b = target.as_bytes();
    match b.len() {
        2 => b[0].is_ascii_alphabetic() && b[1] == b':',
        3 => b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\',
        _ => false,
    }
}

/// One Windows guest command, and the exit code (if any) that means retrying
/// it cannot help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsMountCmd {
    pub program: String,
    pub args: Vec<String>,
    /// The exit code with which this command refuses rather than fails.
    pub refused_exit: Option<i32>,
}

/// The exit code the folder-link step refuses with: the folder path exists
/// and is not a link vmlab made, so replacing it could destroy user data.
pub const LINK_REFUSED_EXIT: i32 = 64;

/// Build the Windows mount command(s) for a share.
///
/// - **Drive-letter target** (`X:`): a single `net use X: \\<gw>\<share>
///   /user:<u> <p> /persistent:yes`. This maps directly (PRD §7.5).
/// - **Folder-path target** (e.g. `C:\mnt\data`): realised as a directory
///   symbolic link to the UNC path. Windows permits UNC targets for `/D`;
///   `/J` junctions cannot target UNC paths. We additionally prime
///   credentials with a credential-only `net use \\<gw>\<share> /user:<u>
///   <p>` so the symbolic link resolves authenticated. The link step is
///   [`windows_link_script`], run every `up`: it keeps a link that already
///   points at this gateway and replaces one that points at another.
pub fn windows_mount_cmds(
    gateway: Ipv4Addr,
    share: &str,
    guest_path: &str,
    user: &str,
    pass: &str,
) -> Vec<WindowsMountCmd> {
    let unc = format!("\\\\{gateway}\\{share}");
    let cmd = |program: &str, args: Vec<String>| WindowsMountCmd {
        program: program.to_string(),
        args,
        refused_exit: None,
    };
    if is_drive_letter(guest_path) {
        // Normalise `X:\` to `X:` for net use.
        let letter = &guest_path[..2];
        // A stale remembered mapping on the letter (e.g. from a previous
        // lab run, possibly to another gateway) blocks the fresh `net use` —
        // clear it first. `exit /b 0` because "nothing to delete" exits 2
        // and must not count as a failed mount step.
        let cleanup = cmd(
            "cmd",
            vec![
                "/c".to_string(),
                format!("net use {letter} /delete /y & exit /b 0"),
            ],
        );
        let map = cmd(
            "net",
            vec![
                "use".to_string(),
                letter.to_string(),
                unc,
                format!("/user:{user}"),
                pass.to_string(),
                "/persistent:yes".to_string(),
            ],
        );
        vec![cleanup, map]
    } else {
        // Folder-path target: authenticate, then link the folder to the UNC.
        let auth = cmd(
            "net",
            vec![
                "use".to_string(),
                unc,
                format!("/user:{user}"),
                pass.to_string(),
                "/persistent:yes".to_string(),
            ],
        );
        // Encoded, so no layer between here and PowerShell (the agent's
        // command line, `cmd`, PowerShell's own parser) can re-read a
        // backslash or a quote in the path.
        use base64::Engine as _;
        let script = windows_link_script(gateway, share, guest_path);
        let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
        let link = WindowsMountCmd {
            program: "powershell".to_string(),
            args: vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-EncodedCommand".to_string(),
                base64::engine::general_purpose::STANDARD.encode(utf16),
            ],
            refused_exit: Some(LINK_REFUSED_EXIT),
        };
        vec![auth, link]
    }
}

/// The PowerShell that makes `guest_path` a directory symbolic link to
/// `\\<gateway>\<share>`, whatever the guest already has there.
///
/// - Nothing there: create the link.
/// - A link to this gateway's share: leave it, and succeed.
/// - A link to the same share on another gateway (a link vmlab made before
///   the VM's segment moved to another subnet): remove the link, which
///   leaves the share's files alone, and create it again.
/// - Anything else: exit [`LINK_REFUSED_EXIT`] naming the path. A real
///   directory there may hold the user's data, so it is never removed.
///
/// Windows reports a UNC link's target as `UNC\<host>\<share>`; it is
/// compared as `\\<host>\<share>`, case-insensitively. Progress records are
/// silenced because PowerShell writes them to a redirected stderr as CLIXML,
/// which would bury the refusal message in the `share.unmountable` event.
pub fn windows_link_script(gateway: Ipv4Addr, share: &str, guest_path: &str) -> String {
    let quote = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let path = quote(guest_path);
    let target = quote(&format!("\\\\{gateway}\\{share}"));
    let share = quote(share);
    format!(
        r#"$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$path = {path}
$target = {target}
$share = {share}
$item = Get-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue
if ($item) {{
  $current = ''
  if ($item.LinkType -eq 'SymbolicLink') {{ $current = [string]@($item.Target)[0] }}
  if ($current -like 'UNC\*') {{ $current = '\' + $current.Substring(3) }}
  if ($current -eq $target) {{ exit 0 }}
  if ($current -notmatch ('^\\\\[^\\]+\\' + [regex]::Escape($share) + '\\?$')) {{
    [Console]::Error.WriteLine("$path already exists and is not a link vmlab made to share $share, so vmlab will not replace it. Move it aside and run vmlab up again.")
    exit {LINK_REFUSED_EXIT}
  }}
  $item.Delete()
}}
& cmd.exe /c mklink /D $path $target
exit $LASTEXITCODE
"#
    )
}

/// Path of the connect script dropped on the shared desktop (visible to
/// every interactive user).
pub const DESKTOP_SCRIPT: &str = "C:\\Users\\Public\\Desktop\\vmlab-shares.cmd";

/// Build the command that (re)writes a double-clickable script on the
/// guest's shared desktop authenticating the lab shares for the user who
/// runs it.
///
/// The agent's mounts run as SYSTEM, whose drive mappings land in the
/// GLOBAL DOS-device namespace: every session *sees* the letters, but each
/// logon authenticates separately — interactive users hit "user name or
/// password is incorrect" (and `net use X:` says "already in use", error
/// 85). A Credential Manager entry for the gateway makes the existing
/// letters work, so the script is one `cmdkey /add` per lab, not `net use`.
pub fn windows_desktop_script_cmd(
    gateway: Ipv4Addr,
    shares: &[(&str, &str)], // (share name, drive letter "X:") — for the message
    user: &str,
    pass: &str,
) -> (String, Vec<String>) {
    let letters: Vec<&str> = shares.iter().map(|(_, l)| *l).collect();
    let lines = vec![
        "@echo off".to_string(),
        format!("cmdkey /delete:{gateway}"),
        format!("cmdkey /add:{gateway} /user:{user} /pass:{pass}"),
        format!(
            "echo Lab shares authenticated - {} now open in Explorer.",
            letters.join(" ")
        ),
        "pause".to_string(),
    ];
    let echoes: Vec<String> = lines.into_iter().map(|l| format!("echo {l}")).collect();
    (
        "cmd".to_string(),
        vec![
            "/c".to_string(),
            format!("({}) > {DESKTOP_SCRIPT}", echoes.join("& ")),
        ],
    )
}

/// Build the command registering a machine-wide logon hook (HKLM Run) that
/// stores the lab credential in each interactive user's session at logon —
/// so the globally-visible drive letters authenticate without anyone
/// running the desktop script. Re-registered on every mount, so a rotated
/// credential (after `destroy`) heals on the next logon.
///
/// **The guest agent reads this value back.** A `Run` key needs a desktop
/// session, and the logon the agent mints for an attached developer is not
/// one (PRD §19.2's correction to §7.5) — so `windows/logon.rs` fetches this
/// value inside each logon it mints and, because a network logon cannot hold
/// a `cmdkey` credential, parses it into a `net use \\<gw>\IPC$` session
/// instead (`share_session_command` in the agent's `logon.rs`). Changing the
/// value's shape means the agent no longer recognises it; changing its name
/// or location means the agent finds nothing. Either way an attached
/// developer's share silently stops opening.
pub fn windows_logon_cred_cmd(gateway: Ipv4Addr, user: &str, pass: &str) -> (String, Vec<String>) {
    (
        "reg".to_string(),
        vec![
            "add".to_string(),
            "HKLM\\Software\\Microsoft\\Windows\\CurrentVersion\\Run".to_string(),
            "/v".to_string(),
            "vmlab-shares".to_string(),
            "/t".to_string(),
            "REG_SZ".to_string(),
            "/d".to_string(),
            format!("cmdkey /add:{gateway} /user:{user} /pass:{pass}"),
            "/f".to_string(),
        ],
    )
}

/// XP/2003-era `net use` string for screen-automation driving (PRD §7.5 XP-era
/// caveat). These guests lack a guest agent, so the provision script types this
/// at the console via the keystroke surface (§10.3). We always target a drive
/// letter for these guests (mklink predates nothing useful on XP). Returns the
/// full command as a single string ready to be typed.
pub fn xp_net_use_string(
    gateway: Ipv4Addr,
    share: &str,
    drive_letter: &str,
    user: &str,
    pass: &str,
) -> String {
    let letter = if drive_letter.len() >= 2 {
        &drive_letter[..2]
    } else {
        drive_letter
    };
    format!("net use {letter} \\\\{gateway}\\{share} /user:{user} {pass} /persistent:yes")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gw() -> Ipv4Addr {
        Ipv4Addr::new(10, 0, 0, 1)
    }

    #[test]
    fn linux_cifs_smb3_with_ro() {
        let (prog, args) = linux_mount_cmd(gw(), "src", "/mnt/src", "u", "p", true, false);
        assert_eq!(prog, "mount");
        let joined = args.join(" ");
        assert!(joined.contains("-t cifs"));
        assert!(joined.contains("//10.0.0.1/src"));
        assert!(joined.contains("/mnt/src"));
        assert!(joined.contains("vers=3.0"));
        assert!(joined.contains("username=u,password=p"));
        assert!(joined.contains(",ro"));
    }

    #[test]
    fn linux_cifs_smb1_no_ro() {
        let (_, args) = linux_mount_cmd(gw(), "old", "/mnt/old", "u", "p", false, true);
        let joined = args.join(" ");
        assert!(joined.contains("vers=1.0"));
        assert!(!joined.contains(",ro"));
    }

    #[test]
    fn drive_letter_detection() {
        assert!(is_drive_letter("X:"));
        assert!(is_drive_letter("d:\\"));
        assert!(!is_drive_letter("C:\\mnt\\data"));
        assert!(!is_drive_letter("/mnt/x"));
        assert!(!is_drive_letter("X"));
    }

    #[test]
    fn windows_drive_letter_net_use() {
        let cmds = windows_mount_cmds(gw(), "data", "X:", "u", "p");
        assert_eq!(cmds.len(), 2);
        // First clears any stale remembered mapping — and must always
        // exit 0 (the daemon retries failing steps).
        assert_eq!(cmds[0].program, "cmd");
        assert!(cmds[0].args[1].contains("net use X: /delete /y"));
        assert!(cmds[0].args[1].contains("exit /b 0"));
        // Then maps the drive.
        assert_eq!(cmds[1].program, "net");
        let joined = cmds[1].args.join(" ");
        assert!(joined.starts_with("use X: \\\\10.0.0.1\\data"));
        assert!(joined.contains("/user:u"));
        assert!(joined.contains("/persistent:yes"));
        assert!(cmds.iter().all(|c| c.refused_exit.is_none()));
    }

    #[test]
    fn windows_desktop_script_stores_session_credential() {
        let (prog, args) =
            windows_desktop_script_cmd(gw(), &[("data", "X:"), ("src", "Y:")], "u", "p");
        assert_eq!(prog, "cmd");
        let script = &args[1];
        assert!(script.contains("echo cmdkey /delete:10.0.0.1"));
        assert!(script.contains("echo cmdkey /add:10.0.0.1 /user:u /pass:p"));
        assert!(script.contains("X: Y:"));
        assert!(script.ends_with(&format!("> {DESKTOP_SCRIPT}")));
    }

    #[test]
    fn windows_logon_hook_registers_cmdkey() {
        let (prog, args) = windows_logon_cred_cmd(gw(), "u", "p");
        assert_eq!(prog, "reg");
        let joined = args.join(" ");
        assert!(joined.contains("CurrentVersion\\Run /v vmlab-shares"));
        assert!(joined.contains("cmdkey /add:10.0.0.1 /user:u /pass:p"));
        assert!(joined.ends_with("/f"));
    }

    /// The script a step carries, decoded the way PowerShell decodes
    /// `-EncodedCommand`.
    fn decoded(cmd: &WindowsMountCmd) -> String {
        use base64::Engine as _;
        assert_eq!(
            cmd.args[..3],
            ["-NoProfile", "-NonInteractive", "-EncodedCommand"]
        );
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&cmd.args[3])
            .unwrap();
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| u16::from_le_bytes(*b))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn windows_folder_path_links_through_the_encoded_script() {
        let cmds = windows_mount_cmds(gw(), "data", "C:\\mnt\\data", "u", "p");
        assert_eq!(cmds.len(), 2);
        // first authenticates
        assert_eq!(cmds[0].program, "net");
        assert!(
            cmds[0]
                .args
                .join(" ")
                .starts_with("use \\\\10.0.0.1\\data /user:u")
        );
        // second is the link step, which refuses with its own exit code
        assert_eq!(cmds[1].program, "powershell");
        assert_eq!(cmds[1].refused_exit, Some(LINK_REFUSED_EXIT));
        assert_eq!(
            decoded(&cmds[1]),
            windows_link_script(gw(), "data", "C:\\mnt\\data")
        );
    }

    /// Issue #141: the link step is run on every `up`, so it has to accept a
    /// link already there — and replace one left pointing at the gateway of
    /// a subnet the VM is no longer on.
    #[test]
    fn the_link_script_keeps_or_replaces_a_link_and_never_removes_a_folder() {
        let script = windows_link_script(gw(), "c_repo", "C:\\repo");
        assert!(script.contains("$path = 'C:\\repo'"), "{script}");
        assert!(
            script.contains("$target = '\\\\10.0.0.1\\c_repo'"),
            "{script}"
        );
        // The link itself, never what it points at.
        assert!(
            script.contains("Get-Item -LiteralPath $path -Force"),
            "{script}"
        );
        // Windows reports `UNC\host\share`; compared as `\\host\share`.
        assert!(
            script.contains(
                "if ($current -like 'UNC\\*') { $current = '\\' + $current.Substring(3) }"
            ),
            "{script}"
        );
        assert!(
            script.contains("if ($current -eq $target) { exit 0 }"),
            "{script}"
        );
        // Only a link to this share on some gateway is vmlab's to replace;
        // anything else at the path is refused, by name.
        assert!(
            script.contains(r"'^\\\\[^\\]+\\' + [regex]::Escape($share) + '\\?$'"),
            "{script}"
        );
        assert!(
            script.contains(&format!("exit {LINK_REFUSED_EXIT}")),
            "{script}"
        );
        assert!(script.contains("is not a link vmlab made"), "{script}");
        // Removing a directory symbolic link non-recursively removes the
        // link and nothing it points at.
        assert!(script.contains("$item.Delete()"), "{script}");
        assert!(!script.contains("Recurse"), "{script}");
        assert!(script.contains("mklink /D $path $target"), "{script}");
    }

    /// A quote in a path would end PowerShell's string early.
    #[test]
    fn the_link_script_quotes_the_path_for_powershell() {
        let script = windows_link_script(gw(), "data", "C:\\Bob's files");
        assert!(script.contains("$path = 'C:\\Bob''s files'"), "{script}");
    }

    #[test]
    fn xp_string_form() {
        let s = xp_net_use_string(gw(), "share", "Z:", "u", "p");
        assert_eq!(
            s,
            "net use Z: \\\\10.0.0.1\\share /user:u p /persistent:yes"
        );
    }
}
