//! The RDP host end (PRD §20.3): FreeRDP, dialling a private unix socket the
//! CLI listens on, and one `conns` connection per connection it makes.
//!
//! FreeRDP connects `AF_UNIX` to a `/v:` value that starts with `/`
//! (`research.md` §C), so no TCP port is opened anywhere. The agent dials each
//! adopted connection to the guest's own `127.0.0.1:3389`.
//!
//! The login's credentials must never reach argv, so FreeRDP is started with
//! `/args-from:stdin` alone — FreeRDP refuses that switch beside any other
//! argument — and reads every argument from stdin, one per line, up to an
//! empty line.

use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};

use super::{HostEvent, Request};

/// FreeRDP's whole argv: every real argument arrives on stdin.
pub const ARGV: [&str; 1] = ["/args-from:stdin"];

/// The account an RDP session logs on as: the resolved login the daemon's
/// reply carries, split the way FreeRDP takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    /// The `DOMAIN` of a `DOMAIN\user` account.
    pub domain: Option<String>,
    pub password: Option<String>,
}

impl Credentials {
    /// From the reply's `rdp` object, `{"user", "password"}`.
    pub fn from_reply(rdp: &Value) -> Result<Self> {
        let account = rdp["user"]
            .as_str()
            .filter(|u| !u.is_empty())
            .ok_or_else(|| {
                anyhow!("machine.app_open answered an rdp session without an account")
            })?;
        let (domain, user) = match account.split_once('\\') {
            Some((domain, user)) => (Some(domain.to_string()), user.to_string()),
            None => (None, account.to_string()),
        };
        Ok(Self {
            user,
            domain,
            password: rdp["password"].as_str().map(str::to_string),
        })
    }
}

/// Every argument FreeRDP is given, in the order it reads them from stdin.
pub fn freerdp_args(
    socket: &Path,
    machine: &str,
    argv: &[String],
    cwd: Option<&str>,
    creds: &Credentials,
) -> Result<Vec<String>> {
    let mut args = vec![
        format!("/v:{}", socket.display()),
        format!("/app:{}", app_option(argv, cwd)?),
        "/cert:ignore".into(),
        "+clipboard".into(),
        format!("/wm-class:vmlab-{machine}"),
        format!("/u:{}", creds.user),
    ];
    if let Some(domain) = &creds.domain {
        args.push(format!("/d:{domain}"));
    }
    if let Some(password) = &creds.password {
        args.push(format!("/p:{password}"));
    }
    Ok(args)
}

/// What FreeRDP reads on stdin: one argument per line, ended by an empty
/// line. An argument holding a line break cannot be carried, and is refused
/// rather than split.
pub fn stdin_payload(args: &[String]) -> Result<String> {
    let mut out = String::new();
    for arg in args {
        if arg.contains(['\n', '\r']) {
            let name = arg.split(':').next().unwrap_or(arg);
            bail!("cannot hand FreeRDP `{name}`: it contains a line break");
        }
        out.push_str(arg);
        out.push('\n');
    }
    out.push('\n');
    Ok(out)
}

/// The value of `/app:` — `program:<argv0>`, then `workdir:` for `--cwd`, then
/// `cmd:` with the rest of argv rendered as one Windows command line.
///
/// FreeRDP splits the value on commas, with its own quote and backslash
/// rules, and keeps the quotes and backslashes it scans past. So the value is
/// built, split again by [`freerdp_split`] exactly as FreeRDP would, and
/// refused when the two disagree, rather than starting the wrong program.
fn app_option(argv: &[String], cwd: Option<&str>) -> Result<String> {
    let (program, rest) = argv.split_first().ok_or_else(|| {
        anyhow!("nothing to run — usage: vmlab app run <machine> -- <cmd> [args...]")
    })?;
    let mut elements = vec![format!("program:{program}")];
    if let Some(cwd) = cwd {
        elements.push(format!("workdir:{cwd}"));
    }
    if !rest.is_empty() {
        elements.push(format!("cmd:{}", command_line(rest)));
    }
    let value = elements.join(",");
    if freerdp_split(&value).as_ref() != Some(&elements) {
        bail!(
            "FreeRDP's `/app` option cannot carry `{value}` intact: its comma, quote and \
             backslash rules would change it"
        );
    }
    Ok(value)
}

/// An argv rendered as one Windows command line, the rule
/// `CommandLineToArgvW` inverts (the agent's `spawn::command_line`). An
/// argument holding a comma, or ending in a backslash, is quoted too, so
/// FreeRDP's comma split cannot cut it and its backslash escape cannot
/// swallow what follows.
fn command_line(argv: &[String]) -> String {
    let mut out = String::new();
    for (i, arg) in argv.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        if !arg.is_empty() && !arg.contains([' ', '\t', '"', ',']) && !arg.ends_with('\\') {
            out.push_str(arg);
            continue;
        }
        out.push('"');
        let mut backslashes = 0;
        for ch in arg.chars() {
            match ch {
                '\\' => backslashes += 1,
                '"' => {
                    out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                    backslashes = 0;
                    out.push('"');
                }
                _ => {
                    out.extend(std::iter::repeat_n('\\', backslashes));
                    backslashes = 0;
                    out.push(ch);
                }
            }
        }
        out.extend(std::iter::repeat_n('\\', backslashes * 2));
        out.push('"');
    }
    out
}

/// FreeRDP's `is_quoted`.
fn quote_kind(c: u8) -> i32 {
    match c {
        b'"' => 1,
        b'\'' => -1,
        _ => 0,
    }
}

/// How FreeRDP splits a comma-separated option value: a transcription of
/// winpr's `CommandLineParseCommaSeparatedValuesEx` (`get_element_count`,
/// `get_next_comma`), quirks included. `None` where FreeRDP fails, or where
/// the value is "fully quoted", which this module never builds.
fn freerdp_split(list: &str) -> Option<Vec<String>> {
    let mut buf = list.as_bytes().to_vec();
    if let (Some(&first), Some(&last)) = (buf.first(), buf.last())
        && quote_kind(first) != 0
        && quote_kind(last) != 0
    {
        return None;
    }
    let count = element_count(&buf)?;
    let at = |buf: &[u8], i: usize| buf.get(i).copied().unwrap_or(0);
    let mut out = Vec::with_capacity(count);
    let mut start = 0;
    for _ in 0..count {
        let quote = quote_kind(at(&buf, start));
        let comma = next_comma(&buf, start);
        let ptr = if quote != 0 { start + 1 } else { start };
        if let Some(comma) = comma {
            let last = comma - 1;
            let last_quote = quote_kind(buf[last]);
            if last_quote != quote {
                return None;
            }
            if last_quote != 0 {
                buf[last] = 0;
            }
            buf[comma] = 0;
            start = comma + 1;
        } else if quote != 0 {
            let len = buf[ptr..]
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(buf.len() - ptr);
            let end = buf[ptr..ptr + len].iter().rposition(|&c| c == b'"')?;
            buf[ptr + end] = 0;
        }
        let len = buf[ptr..]
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(buf.len() - ptr);
        out.push(String::from_utf8_lossy(&buf[ptr..ptr + len]).into_owned());
    }
    Some(out)
}

/// winpr's `get_element_count`: every quote character toggles one flag, and
/// a backslash hides the character after it, the terminating NUL included.
fn element_count(list: &[u8]) -> Option<usize> {
    if list.is_empty() {
        return Some(0);
    }
    let (mut quoted, mut escaped, mut first, mut count) = (false, false, true, 0);
    for &cur in list.iter().chain(std::iter::once(&0)) {
        if escaped {
            escaped = false;
            continue;
        }
        let mut next_first = false;
        match cur {
            0 => return (!quoted).then_some(count + 1),
            b'\\' => {
                escaped = true;
                continue;
            }
            b'"' | b'\'' => quoted = !quoted,
            b',' => {
                if first {
                    return None;
                }
                if !quoted {
                    next_first = true;
                    count += 1;
                }
            }
            _ => {}
        }
        first = next_first;
    }
    None
}

/// winpr's `get_next_comma`: the next separating comma from `start`, or
/// `None` at the end of the element list — or where winpr logs an error,
/// which it reports the same way.
fn next_comma(buf: &[u8], start: usize) -> Option<usize> {
    let (mut quoted, mut first, mut escaped) = (0, true, false);
    let mut i = start;
    loop {
        let cur = buf.get(i).copied().unwrap_or(0);
        let pos = i;
        i += 1;
        if escaped {
            escaped = false;
            continue;
        }
        match cur {
            0 => return None,
            b'\\' => {
                escaped = true;
                continue;
            }
            b'"' | b'\'' => {
                let now = quote_kind(cur);
                if quoted == 0 && !first {
                    return None;
                }
                if now == quoted {
                    quoted = 0;
                } else if quoted == 0 {
                    quoted = now;
                }
            }
            b',' => {
                if first {
                    return None;
                }
                if quoted == 0 {
                    return Some(pos);
                }
            }
            _ => {}
        }
        first = false;
    }
}

/// The CLI's side of an RDP session: the private socket FreeRDP dials, and
/// FreeRDP itself once launched. Dropping it kills FreeRDP and removes the
/// directory unless a failed client's log was retained.
pub struct Server {
    listener: UnixListener,
    socket: PathBuf,
    /// FreeRDP and its stdin, made ready by [`Self::prepare`] and started by
    /// [`Self::start`].
    pending: Option<(PathBuf, String, bool)>,
    child: Option<tokio::process::Child>,
    stderr: Option<tokio::task::JoinHandle<std::io::Result<()>>>,
    log: PathBuf,
    dir: Option<tempfile::TempDir>,
}

impl Server {
    /// Listen on a fresh private socket.
    pub fn bind() -> Result<Self> {
        let dir = super::private_dir()?;
        let socket = dir.path().join("rdp.sock");
        let listener = UnixListener::bind(&socket)
            .with_context(|| format!("listening on {}", socket.display()))?;
        Ok(Self {
            listener,
            socket,
            pending: None,
            child: None,
            stderr: None,
            log: dir.path().join("freerdp.log"),
            dir: Some(dir),
        })
    }

    /// Build FreeRDP's arguments, refusing any it cannot be handed, without
    /// starting it yet.
    pub fn prepare(
        &mut self,
        bin: Option<&Path>,
        req: &Request,
        creds: &Credentials,
    ) -> Result<()> {
        let bin = bin.ok_or_else(|| anyhow!("no FreeRDP 3 on this host"))?;
        let args = freerdp_args(
            &self.socket,
            &req.machine,
            &req.argv,
            req.cwd.as_deref(),
            creds,
        )?;
        self.pending = Some((bin.to_path_buf(), stdin_payload(&args)?, req.verbose));
        Ok(())
    }

    /// Start the FreeRDP [`Self::prepare`] made ready, handing it its
    /// arguments on stdin. The session's `ready` record calls it: until the
    /// agent says the guest's RDP listener accepts, a connection would be
    /// refused. Starts nothing the second time.
    pub fn start(&mut self) -> Result<()> {
        let Some((bin, payload, verbose)) = self.pending.take() else {
            return Ok(());
        };
        let log = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&self.log)
            .with_context(|| format!("opening {}", self.log.display()))?;
        let (stderr, tee) = if verbose {
            (Stdio::piped(), Some(log))
        } else {
            (Stdio::from(log), None)
        };
        let mut child = tokio::process::Command::new(&bin)
            .args(ARGV)
            .stdin(Stdio::piped())
            .stderr(stderr)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("starting `{}`", bin.display()))?;
        if let Some(log) = tee {
            let stderr = child.stderr.take().expect("stderr is piped");
            self.stderr = Some(tokio::spawn(tee_stderr(stderr, log, tokio::io::stderr())));
        }
        let mut stdin = child.stdin.take().expect("stdin is piped");
        tokio::spawn(async move {
            // FreeRDP stops reading at the empty line; closing stdin after it
            // is all that is left to do.
            let _ = stdin.write_all(payload.as_bytes()).await;
        });
        self.child = Some(child);
        Ok(())
    }

    /// The next connection FreeRDP makes, or its exit.
    pub(super) async fn event(&mut self) -> Result<HostEvent> {
        let child = self.child.as_mut();
        tokio::select! {
            accepted = self.listener.accept() => {
                let (stream, _): (UnixStream, _) = accepted?;
                Ok(HostEvent::Accepted(stream))
            }
            status = async move {
                match child {
                    Some(child) => child.wait().await,
                    None => std::future::pending().await,
                }
            } => {
                let code = status?.code().unwrap_or(-1);
                if let Some(stderr) = self.stderr.as_mut() {
                    stderr.await.context("joining FreeRDP's stderr relay")?
                        .context("relaying FreeRDP's stderr")?;
                    self.stderr = None;
                }
                Ok(HostEvent::Exited(code))
            },
        }
    }

    /// Retain a failed client's log before the session directory is dropped.
    pub(super) fn finish(&mut self, code: i32) -> Result<i32> {
        if !is_failure(code) {
            return Ok(0);
        }
        if let Some(dir) = self.dir.take() {
            let _ = dir.keep();
        }
        std::fs::remove_file(&self.socket)
            .with_context(|| format!("removing {}", self.socket.display()))?;
        eprintln!(
            "FreeRDP exited with code {code}; log: {}",
            self.log.display()
        );
        match log_tail(&self.log) {
            Ok(tail) if !tail.is_empty() => eprintln!("{tail}"),
            Ok(_) => {}
            Err(e) => eprintln!("could not read FreeRDP's log: {e:#}"),
        }
        Ok(code)
    }
}

async fn tee_stderr(
    mut source: impl tokio::io::AsyncRead + Unpin,
    log: std::fs::File,
    mut terminal: impl tokio::io::AsyncWrite + Unpin,
) -> std::io::Result<()> {
    let mut log = tokio::fs::File::from_std(log);
    let mut buf = [0u8; 8192];
    loop {
        let n = source.read(&mut buf).await?;
        if n == 0 {
            return log.flush().await;
        }
        log.write_all(&buf[..n]).await?;
        terminal.write_all(&buf[..n]).await?;
        terminal.flush().await?;
    }
}

/// FreeRDP's protocol-independent disconnect/logoff codes (including raw
/// ERRINFO_LOGOFF_BY_USER = 12) are normal session endings.
fn is_failure(code: i32) -> bool {
    !matches!(code, 0 | 1 | 2 | 11 | 12)
}

fn is_backtrace(line: &str) -> bool {
    let Some((_, message)) = line.split_once(" - ") else {
        return false;
    };
    if let Some(rest) = message.strip_prefix("[winpr_log_backtrace_ex]") {
        return rest.starts_with(':') || rest.starts_with('[');
    }
    message.starts_with("winpr_log_backtrace_ex:") || message.starts_with("winpr_log_backtrace_ex ")
}

/// Bound diagnostic memory even when the client writes a very large log or
/// a line without a newline.
fn log_tail(path: &Path) -> Result<String> {
    const MAX_BYTES: u64 = 64 * 1024;
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let start = size.saturating_sub(MAX_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(MAX_BYTES).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let text = if start > 0 {
        text.split_once('\n').map_or("", |(_, text)| text)
    } else {
        &text
    };
    let mut lines: Vec<_> = text
        .lines()
        .rev()
        .filter(|line| !is_backtrace(line))
        .take(20)
        .collect();
    lines.reverse();
    Ok(lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn freerdp_stderr_is_logged_in_the_session_directory() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = tempfile::tempdir().unwrap();
        let bin = scratch.path().join("freerdp");
        std::fs::write(
            &bin,
            "#!/bin/sh\ncat >/dev/null\necho client-diagnostic >&2\nexit 147\n",
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut server = Server::bind().unwrap();
        let log = server.socket.with_file_name("freerdp.log");
        let req = Request {
            machine: "win".into(),
            argv: vec!["notepad.exe".into()],
            cwd: None,
            user: None,
            password: None,
            verbose: false,
        };
        server.prepare(Some(&bin), &req, &creds()).unwrap();
        server.start().unwrap();
        assert!(matches!(
            server.event().await.unwrap(),
            HostEvent::Exited(147)
        ));
        assert_eq!(std::fs::read_to_string(log).unwrap(), "client-diagnostic\n");
    }

    #[test]
    fn disconnect_and_logoff_are_not_failures() {
        for code in [0, 1, 2, 11, 12] {
            assert!(!is_failure(code), "{code}");
        }
        for code in [-1, 3, 4, 5, 6, 7, 10, 16, 32, 128, 134, 147, 255] {
            assert!(is_failure(code), "{code}");
        }
    }

    #[tokio::test]
    async fn failed_logs_survive_drop_and_normal_endings_remove_them() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = tempfile::tempdir().unwrap();
        let bin = scratch.path().join("freerdp");
        for code in [0, 1, 2, 11, 12, 147] {
            std::fs::write(
                &bin,
                format!("#!/bin/sh\ncat >/dev/null\necho diagnostic >&2\nexit {code}\n"),
            )
            .unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            let mut server = Server::bind().unwrap();
            let dir = server.socket.parent().unwrap().to_path_buf();
            let log = server.log.clone();
            let req = Request {
                machine: "win".into(),
                argv: vec!["notepad.exe".into()],
                cwd: None,
                user: None,
                password: None,
                verbose: false,
            };
            server.prepare(Some(&bin), &req, &creds()).unwrap();
            server.start().unwrap();
            assert!(matches!(server.event().await.unwrap(), HostEvent::Exited(got) if got == code));
            assert_eq!(
                std::fs::metadata(&log).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                server.finish(code).unwrap(),
                if code == 147 { 147 } else { 0 }
            );
            drop(server);
            if code == 147 {
                assert_eq!(std::fs::read_to_string(&log).unwrap(), "diagnostic\n");
                assert!(!dir.join("rdp.sock").exists());
                std::fs::remove_file(log).unwrap();
                std::fs::remove_dir(dir).unwrap();
            } else {
                assert!(!dir.exists());
            }
        }
    }

    #[tokio::test]
    async fn verbose_stderr_is_copied_to_the_log_and_terminal() {
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("log");
        let log = std::fs::File::create(&path).unwrap();
        let (terminal, mut output) = tokio::io::duplex(1024);
        tee_stderr(&b"first\nsecond\n"[..], log, terminal)
            .await
            .unwrap();
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).await.unwrap();
        assert_eq!(bytes, b"first\nsecond\n");
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }

    #[tokio::test]
    async fn cancelling_an_exit_event_keeps_the_stderr_drain_attached() {
        use std::os::unix::fs::PermissionsExt;

        let scratch = tempfile::tempdir().unwrap();
        let bin = scratch.path().join("freerdp");
        std::fs::write(&bin, "#!/bin/sh\ncat >/dev/null\nexit 0\n").unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let req = Request {
            machine: "win".into(),
            argv: vec!["notepad.exe".into()],
            cwd: None,
            user: None,
            password: None,
            verbose: false,
        };
        let mut server = Server::bind().unwrap();
        server.prepare(Some(&bin), &req, &creds()).unwrap();
        server.start().unwrap();
        let (release, wait) = tokio::sync::oneshot::channel();
        server.stderr = Some(tokio::spawn(async move {
            wait.await.unwrap();
            Ok(())
        }));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), server.event())
                .await
                .is_err()
        );
        assert!(
            server.stderr.is_some(),
            "cancelled event detached the stderr drain"
        );
        release.send(()).unwrap();
        assert!(matches!(
            server.event().await.unwrap(),
            HostEvent::Exited(0)
        ));
        assert!(server.stderr.is_none());
    }

    #[test]
    fn tail_keeps_twenty_diagnostics_and_excludes_backtrace_records() {
        let scratch = tempfile::tempdir().unwrap();
        let log = scratch.path().join("freerdp.log");
        let mut text = String::new();
        for n in 0..25 {
            text.push_str(&format!("diagnostic {n}\n"));
            text.push_str(
                "[12:00] [1:2] [ERROR][com.freerdp] - [winpr_log_backtrace_ex]: 0: frame\n",
            );
            text.push_str(
                "[12:00] [1:2] [ERROR][com.freerdp] - [winpr_log_backtrace_ex][0x1]: 1: frame\n",
            );
        }
        std::fs::write(&log, text).unwrap();
        assert_eq!(
            log_tail(&log).unwrap(),
            (5..25)
                .map(|n| format!("diagnostic {n}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        assert!(!is_backtrace("program mentions winpr_log_backtrace_ex"));
        assert!(!is_backtrace(
            "[ERROR] - [other]: winpr_log_backtrace_ex failed"
        ));
        assert!(is_backtrace("[ERROR] - winpr_log_backtrace_ex: 0: frame"));
    }

    #[test]
    fn tail_bounds_a_large_log_and_tolerates_non_utf8() {
        let scratch = tempfile::tempdir().unwrap();
        let log = scratch.path().join("freerdp.log");
        let mut bytes = vec![b'x'; 128 * 1024];
        bytes.extend_from_slice(b"\nfinal \xff\n");
        std::fs::write(&log, bytes).unwrap();
        assert_eq!(log_tail(&log).unwrap(), "final \u{fffd}");
    }

    fn creds() -> Credentials {
        Credentials {
            user: "dev".into(),
            domain: Some("LAB".into()),
            password: Some("s3cret-pw".into()),
        }
    }

    fn argv(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    /// A program with no arguments is `program:` alone; with arguments the
    /// rest becomes `cmd:`, quoted the way the guest's argv parser undoes.
    #[test]
    fn freerdp_is_told_the_program_and_its_command_line() {
        let socket = Path::new("/run/user/1000/vmlab-app-x/rdp.sock");
        assert_eq!(
            freerdp_args(socket, "win11", &argv(&["notepad.exe"]), None, &creds()).unwrap(),
            [
                "/v:/run/user/1000/vmlab-app-x/rdp.sock",
                "/app:program:notepad.exe",
                "/cert:ignore",
                "+clipboard",
                "/wm-class:vmlab-win11",
                "/u:dev",
                "/d:LAB",
                "/p:s3cret-pw",
            ]
        );
        let with_args = freerdp_args(
            socket,
            "win11",
            &argv(&["notepad.exe", r"C:\Users\dev\my notes.txt", "/A"]),
            Some(r"C:\Users\dev"),
            &creds(),
        )
        .unwrap();
        assert_eq!(
            with_args[1],
            r#"/app:program:notepad.exe,workdir:C:\Users\dev,cmd:"C:\Users\dev\my notes.txt" /A"#
        );
    }

    /// The password reaches FreeRDP on stdin and nowhere else: argv is the
    /// one switch that says to read stdin.
    #[test]
    fn the_password_is_only_in_the_stdin_payload() {
        let socket = Path::new("/tmp/rdp.sock");
        let args = freerdp_args(socket, "win11", &argv(&["calc.exe"]), None, &creds()).unwrap();
        let payload = stdin_payload(&args).unwrap();
        assert!(payload.contains("\n/p:s3cret-pw\n"), "{payload}");
        assert!(
            payload.ends_with("\n\n"),
            "an empty line ends the arguments"
        );
        assert_eq!(ARGV, ["/args-from:stdin"]);
        assert!(!ARGV.iter().any(|a| a.contains("s3cret-pw")));
    }

    /// A line break would split an argument in two, so it is refused, and
    /// the refusal does not repeat the secret.
    #[test]
    fn an_argument_with_a_line_break_is_refused() {
        let err = stdin_payload(&["/p:two\nlines".into()])
            .unwrap_err()
            .to_string();
        assert_eq!(err, "cannot hand FreeRDP `/p`: it contains a line break");
    }

    /// `DOMAIN\user` is split into `/d:` and `/u:`; a bare account has no
    /// domain.
    #[test]
    fn a_domain_account_is_split() {
        let reply = serde_json::json!({"user": r"LAB\dev", "password": "pw"});
        assert_eq!(
            Credentials::from_reply(&reply).unwrap(),
            creds_with("dev", Some("LAB"))
        );
        let reply = serde_json::json!({"user": "dev", "password": "pw"});
        assert_eq!(
            Credentials::from_reply(&reply).unwrap(),
            creds_with("dev", None)
        );
        assert!(Credentials::from_reply(&serde_json::json!(null)).is_err());
    }

    fn creds_with(user: &str, domain: Option<&str>) -> Credentials {
        Credentials {
            user: user.into(),
            domain: domain.map(str::to_string),
            password: Some("pw".into()),
        }
    }

    /// The transcription of winpr's split agrees with what FreeRDP does with
    /// the shapes it documents: plain elements, a quoted element with a comma
    /// inside, and a failed empty element.
    #[test]
    fn the_split_follows_winpr() {
        assert_eq!(
            freerdp_split("program:a,cmd:b").unwrap(),
            ["program:a", "cmd:b"]
        );
        assert_eq!(
            freerdp_split(r#"program:a,"cmd:b,c""#).unwrap(),
            ["program:a", "cmd:b,c"]
        );
        assert_eq!(freerdp_split(r#""program:a,cmd:b""#), None, "fully quoted");
        assert_eq!(
            freerdp_split(r#"program:a,"cmd:b,c",x"#).unwrap(),
            ["program:a", "cmd:b,c", "x"]
        );
        assert_eq!(freerdp_split("program:a,,cmd:b"), None);
        assert_eq!(
            freerdp_split(r"program:a\"),
            None,
            "a trailing backslash hides the end"
        );
    }

    /// Arguments with commas are quoted so the split cannot cut them; one
    /// FreeRDP would mangle — an apostrophe toggles its quote state — is
    /// refused rather than run as something else.
    #[test]
    fn a_command_line_freerdp_would_mangle_is_refused() {
        assert_eq!(
            app_option(&argv(&["app.exe", "a,b", r"C:\dir\"]), None).unwrap(),
            r#"program:app.exe,cmd:"a,b" "C:\dir\\""#
        );
        let err = app_option(&argv(&["notepad.exe", "Bob's notes.txt"]), None).unwrap_err();
        assert!(err.to_string().contains("cannot carry"), "{err}");
        assert!(app_option(&[], None).is_err());
    }
}
