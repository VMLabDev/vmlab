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

/// Build the Windows mount command(s) for a share.
///
/// - **Drive-letter target** (`X:`): a single `net use X: \\<gw>\<share>
///   /user:<u> <p> /persistent:yes`. This maps directly (PRD §7.5).
/// - **Folder-path target** (e.g. `C:\mnt\data`): realised as a directory
///   symlink/junction to the UNC path — `cmd /c mklink /D <folder>
///   \\<gw>\<share>`. We additionally prime credentials with a credential-only
///   `net use \\<gw>\<share> /user:<u> <p>` so the symlink resolves
///   authenticated.
///
/// ⚠ PRD §7.5 implementation note: verify the folder-path mechanism against
/// current Windows at implementation time — specifically mklink-to-UNC
/// behaviour (symlink evaluation of remote-to-remote / `\\?\UNC` forms) and
/// whether the mapping persists profile-vs-machine across reboots. The
/// drive-letter path uses `/persistent:yes`; the symlink path relies on the
/// credential being re-established (stale TCP re-auth, §7.5) on each boot.
pub fn windows_mount_cmds(
    gateway: Ipv4Addr,
    share: &str,
    guest_path: &str,
    user: &str,
    pass: &str,
) -> Vec<(String, Vec<String>)> {
    let unc = format!("\\\\{gateway}\\{share}");
    if is_drive_letter(guest_path) {
        // Normalise `X:\` to `X:` for net use.
        let letter = &guest_path[..2];
        // A stale remembered mapping on the letter (e.g. from a previous
        // lab run) blocks the fresh `net use` — clear it first. `exit /b 0`
        // because "nothing to delete" exits 2 and must not count as a
        // failed mount step.
        let cleanup = (
            "cmd".to_string(),
            vec![
                "/c".to_string(),
                format!("net use {letter} /delete /y & exit /b 0"),
            ],
        );
        let map = (
            "net".to_string(),
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
        // Folder-path target: authenticate, then junction the folder to the UNC.
        let auth = (
            "net".to_string(),
            vec![
                "use".to_string(),
                unc.clone(),
                format!("/user:{user}"),
                pass.to_string(),
                "/persistent:yes".to_string(),
            ],
        );
        let link = (
            "cmd".to_string(),
            vec![
                "/c".to_string(),
                "mklink".to_string(),
                "/D".to_string(),
                guest_path.to_string(),
                unc,
            ],
        );
        vec![auth, link]
    }
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
        let (prog, args) = &cmds[0];
        assert_eq!(prog, "cmd");
        assert!(args[1].contains("net use X: /delete /y"));
        assert!(args[1].contains("exit /b 0"));
        // Then maps the drive.
        let (prog, args) = &cmds[1];
        assert_eq!(prog, "net");
        let joined = args.join(" ");
        assert!(joined.starts_with("use X: \\\\10.0.0.1\\data"));
        assert!(joined.contains("/user:u"));
        assert!(joined.contains("/persistent:yes"));
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

    #[test]
    fn windows_folder_path_mklink() {
        let cmds = windows_mount_cmds(gw(), "data", "C:\\mnt\\data", "u", "p");
        assert_eq!(cmds.len(), 2);
        // first authenticates
        assert_eq!(cmds[0].0, "net");
        // second is the mklink junction
        let (prog, args) = &cmds[1];
        assert_eq!(prog, "cmd");
        let joined = args.join(" ");
        assert!(joined.contains("mklink /D C:\\mnt\\data \\\\10.0.0.1\\data"));
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
