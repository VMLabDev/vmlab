//! Compile the C POSIX adapter and run the same contract as any other binary.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, ensure};

use super::vm_agent::conformance::Fixture;
use super::vm_agent::{AgentHandle, SessionEvent};

const BUDGET: Duration = Duration::from_secs(12);

fn build(dir: &Path) -> Result<PathBuf> {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("guest/agent-legacy/src");
    let bin = dir.join("agent");
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args(["-std=c99", "-Wall", "-Wextra", "-Werror", "-O1"])
        .arg("-DAGENT_VERSION=\"agent-legacy=test\"")
        .arg("-o")
        .arg(&bin)
        .args(
            ["json.c", "wire.c", "agent.c", "plat_posix.c"]
                .iter()
                .map(|f| src.join(f)),
        )
        .status()
        .context("C compiler required for legacy agent conformance")?;
    ensure!(status.success(), "legacy agent did not compile cleanly");
    Ok(bin)
}

#[tokio::test]
async fn legacy_agent_conformance() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let bin = build(dir.path())?;
    let fixture = Fixture::process(&bin, "listen", vec![]).await?;
    fixture.run("legacy", None).await
}

#[tokio::test]
async fn legacy_close_ends_what_the_exec_started() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let fifo = dir.path().join("descendant");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )?;
    let bin = build(dir.path())?;
    let fixture = Fixture::process(&bin, "listen", vec![]).await?;
    let handle = AgentHandle::connect(fixture.socket(), BUDGET).await?;
    let mut reader = handle
        .open_exec(
            vec!["cat".into(), fifo.to_str().unwrap().into()],
            vec![],
            None,
            None,
        )
        .await?;
    // Preserve the exec's stdin in the background subshell. Closing the
    // session closes that pipe. A surviving descendant then writes to the
    // FIFO before exiting. The reader sees EOF in either case.
    let mut argv =
        sh("exec 3<&0 4>&1; (echo ready >&4; read -r line <&3; echo survived) >\"$1\" & wait");
    argv.extend(["sh".into(), fifo.to_str().unwrap().into()]);
    let mut exec = handle.open_exec(argv, vec![], None, None).await?;
    loop {
        match exec.recv().await {
            Some(SessionEvent::Data(data)) if !data.is_empty() => break,
            Some(SessionEvent::Exited(_) | SessionEvent::Error(_)) | None => {
                anyhow::bail!("descendant never became ready");
            }
            _ => {}
        }
    }
    exec.close().await;
    let mut stderr = Vec::new();
    while let Some(event) = reader.recv().await {
        match event {
            SessionEvent::Data(data) => {
                anyhow::bail!(
                    "descendant survived close: {}",
                    String::from_utf8_lossy(&data)
                );
            }
            SessionEvent::Stderr(data) => stderr.extend(data),
            SessionEvent::Exited(code) => {
                ensure!(
                    code == 0,
                    "FIFO reader exited with {code}: {}",
                    String::from_utf8_lossy(&stderr)
                );
                return Ok(());
            }
            SessionEvent::Error(err) => anyhow::bail!("FIFO reader: {err}"),
            _ => {}
        }
    }
    anyhow::bail!("FIFO reader did not report exit")
}

#[tokio::test]
async fn legacy_exec_reports_exit_with_inherited_output_pipes() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let bin = build(dir.path())?;
    let fixture = Fixture::process(&bin, "listen", vec![]).await?;
    let handle = AgentHandle::connect(fixture.socket(), BUDGET).await?;
    let keep_running = tempfile::tempdir()?;
    let mut argv = sh("(while [ -d \"$1\" ]; do sleep 1; done) & \
                       printf stdout; printf stderr >&2; exit 3");
    argv.push("sh".into());
    argv.push(keep_running.path().to_str().unwrap().into());

    let output = handle.exec(argv, vec![], None, None, BUDGET, None).await?;
    assert_eq!(output.exit_code, 3);
    assert_eq!(output.stdout, b"stdout");
    assert_eq!(output.stderr, b"stderr");
    Ok(())
}

/// F49: a full argv (the agent's MAX_ARGV, 64) still reaches execvp with its
/// NULL terminator. F52: a child inherits none of the agent's descriptors —
/// not the channel, not the listener, not another exec's pipe ends.
#[tokio::test]
async fn legacy_agent_full_argv_and_no_inherited_descriptors() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let bin = build(dir.path())?;
    let fixture = Fixture::process(&bin, "listen", vec![]).await?;
    let handle = AgentHandle::connect(fixture.socket(), BUDGET).await?;

    let mut argv = sh("echo $#");
    argv.push("x".into());
    argv.extend((0..60).map(|i| format!("a{i}")));
    assert_eq!(argv.len(), 64);
    let out = handle.exec(argv, vec![], None, None, BUDGET, None).await?;
    ensure!(
        out.exit_code == 0 && out.stdout == b"60\n",
        "full argv: exit {} stdout {:?}",
        out.exit_code,
        String::from_utf8_lossy(&out.stdout)
    );

    // No child sees the channel or the listener (both sockets). Only
    // sockets are looked for: the test runner may hand the agent other
    // descriptors (a jobserver pipe) that are not the agent's to close.
    let script = "for f in /proc/$$/fd/*; do n=${f##*/}; l=$(readlink \"$f\"); \
                  case \"$l\" in socket:*) [ \"$n\" -gt 2 ] && echo \"$n -> $l\";; esac; done; true";
    let out = handle
        .exec(sh(script), vec![], None, None, BUDGET, None)
        .await?;
    ensure!(
        out.exit_code == 0 && out.stdout.is_empty(),
        "the child inherited the agent channel: {}",
        String::from_utf8_lossy(&out.stdout)
    );

    // A later child must not hold an earlier one's stdin open: `cat` sees
    // its EOF while `sleep` (forked after it) still runs.
    let mut cat = handle
        .open_exec(vec!["cat".into()], vec![], None, None)
        .await?;
    let sleeper = handle.open_exec(sh("sleep 20"), vec![], None, None).await?;
    cat.eof().await?;
    let exited = tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = cat.recv().await {
            if let SessionEvent::Exited(code) = event {
                return Some(code);
            }
        }
        None
    })
    .await;
    sleeper.close().await;
    ensure!(
        matches!(exited, Ok(Some(0))),
        "cat did not see its EOF while a later child ran: {exited:?}"
    );
    Ok(())
}

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

/// The NT build under Wine, on a COM1 that is a pty: the one way this host
/// can run `plat_win32.c`. `None` when the host has no mingw or no Wine.
struct WineAgent {
    dir: tempfile::TempDir,
    socket: PathBuf,
    child: Option<std::process::Child>,
}

/// Not a limit a step must meet: the client API wants a budget, and a first
/// Wine start builds a whole prefix. It only ends a run that hangs.
const WINE_HANG_GUARD: Duration = Duration::from_secs(1800);

fn found(tool: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(tool).is_file()))
}

impl WineAgent {
    fn wine(&self, program: &str) -> Command {
        let mut command = Command::new(program);
        command
            .env("WINEPREFIX", self.dir.path().join("prefix"))
            .env("WINEDEBUG", "-all")
            .env("WINEDLLOVERRIDES", "mscoree=;mshtml=;winemenubuilder.exe=")
            // The C locale gives the guest the Western code page (1252).
            .env("LC_ALL", "C")
            .env_remove("DISPLAY")
            .env_remove("WAYLAND_DISPLAY")
            .stdin(std::process::Stdio::null());
        command
    }

    fn start() -> Result<Option<Self>> {
        const CC: &str = "i686-w64-mingw32-gcc";
        if !found(CC) || !found("wine") || !found("wineserver") {
            eprintln!("skipped: the Win32 legacy agent needs {CC}, wine and wineserver");
            return Ok(None);
        }
        let dir = tempfile::tempdir()?;
        let src = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("guest/agent-legacy/src");
        let exe = dir.path().join("agent.exe");
        let status = Command::new(CC)
            .args(["-std=c99", "-Wall", "-Wextra", "-Werror"])
            .args(["-Wno-cast-function-type", "-Os", "-static"])
            .arg("-DAGENT_VERSION=\"agent-legacy=test\"")
            .arg("-o")
            .arg(&exe)
            .args(
                ["json.c", "wire.c", "agent.c", "plat_win32.c"]
                    .iter()
                    .map(|f| src.join(f)),
            )
            .args(["-ladvapi32", "-luser32"])
            .status()?;
        ensure!(status.success(), "the Win32 legacy agent did not compile");

        // COM1 is a pty. The slave stays open here so the master never
        // reports a hang-up between two opens on the Wine side.
        let master =
            rustix::pty::openpt(rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY)?;
        rustix::pty::grantpt(&master)?;
        rustix::pty::unlockpt(&master)?;
        let pts = rustix::pty::ptsname(&master, Vec::new())?;
        let pts = pts.to_str()?.to_string();
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&pts)?;
        let mut term = rustix::termios::tcgetattr(&slave)?;
        term.make_raw();
        rustix::termios::tcsetattr(&slave, rustix::termios::OptionalActions::Now, &term)?;

        let socket = dir.path().join("agent.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket)?;
        let master = std::fs::File::from(master);
        std::thread::spawn(move || {
            let _slave = slave;
            let Ok((stream, _)) = listener.accept() else {
                return;
            };
            let (Ok(mut to_guest), Ok(mut from_host)) = (master.try_clone(), stream.try_clone())
            else {
                return;
            };
            std::thread::spawn(move || std::io::copy(&mut from_host, &mut to_guest));
            let (mut from_guest, mut to_host) = (master, stream);
            let _ = std::io::copy(&mut from_guest, &mut to_host);
        });

        let mut agent = Self {
            dir,
            socket,
            child: None,
        };
        // Wine reads its port map when the prefix starts, so the prefix is
        // made, told, and let stop before the agent starts it again.
        let status = agent
            .wine("wine")
            .args(["reg", "add", r"HKLM\Software\Wine\Ports", "/v", "COM1"])
            .args(["/t", "REG_SZ", "/d", &pts, "/f"])
            .stdout(std::process::Stdio::null())
            .status()?;
        ensure!(status.success(), "wine reg add failed: {status}");
        ensure!(agent.wine("wineserver").arg("-w").status()?.success());
        let child = agent
            .wine("wine")
            .arg(&exe)
            .args(["--console", "--port", "COM1"])
            .stdout(std::process::Stdio::null())
            .spawn()?;
        agent.child = Some(child);
        Ok(Some(agent))
    }

    /// A host path as the guest names it: Wine's `Z:` is the host's `/`.
    fn guest_path(path: &Path) -> String {
        format!("Z:{}", path.to_str().unwrap().replace('/', "\\"))
    }
}

impl Drop for WineAgent {
    fn drop(&mut self) {
        if let Some(child) = &mut self.child {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Everything else the prefix started (services, the server itself).
        let _ = self.wine("wineserver").arg("-k").status();
        let _ = self.wine("wineserver").arg("-w").status();
    }
}

/// Review `legacy-agents.md` F1. A character the guest's code page lacks
/// used to reach the command as `?`, which `del` reads as a wildcard: the
/// two-character name the host sent deleted another two-character file.
/// The exec is refused instead, by naming what did not convert, for an
/// argument, an environment entry and the `cwd` alike.
#[tokio::test]
async fn legacy_win32_exec_refuses_text_outside_the_code_page() -> Result<()> {
    let Some(agent) = WineAgent::start()? else {
        return Ok(());
    };
    let handle = AgentHandle::connect_serial(
        &agent.socket,
        WINE_HANG_GUARD,
        super::vm_agent::Greeting::GuestFirst { fallback: false },
    )
    .await?;
    let exec = |argv: &[&str], env: &[(&str, &str)], cwd: Option<&str>| {
        let argv = argv.iter().map(|a| a.to_string()).collect();
        let env = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let handle = handle.clone();
        let cwd = cwd.map(str::to_string);
        async move {
            handle
                .exec(argv, env, cwd, None, WINE_HANG_GUARD, None)
                .await
        }
    };
    let refused = |what: &str, result: Result<super::vm_agent::ExecOutput>, wants: &[&str]| {
        let text = match result {
            Ok(out) => format!("it ran and exited {}", out.exit_code),
            Err(e) => format!("{e:#}"),
        };
        ensure!(
            wants.iter().all(|want| text.contains(want)),
            "{what}: expected a refusal with {wants:?}, got: {text}"
        );
        Ok(())
    };

    let victims = agent.dir.path().join("t");
    std::fs::create_dir(&victims)?;
    let victim = victims.join("ab.txt");
    std::fs::write(&victim, "kept")?;
    let dir = WineAgent::guest_path(&victims);

    // The control: the guest runs commands, and text the code page holds
    // passes (U+00E9 is in 1252).
    let out = exec(&["cmd", "/c", "echo", "caf\u{e9}"], &[], Some(&dir)).await?;
    ensure!(
        out.exit_code == 0 && out.stdout.starts_with(b"caf"),
        "control: exit {} stdout {:?}",
        out.exit_code,
        String::from_utf8_lossy(&out.stdout)
    );

    let name = format!("{dir}\\\u{65e5}\u{672c}.txt");
    let result = exec(&["cmd", "/c", "del", &name], &[], None).await;
    ensure!(
        victim.exists(),
        "`del` of a name outside the code page deleted {}",
        victim.display()
    );
    refused("argument", result, &["argument 3", "U+65E5", "code page"])?;

    // A best-fit look-alike is as wrong as `?`: a full-width quote must not
    // arrive as the ASCII one.
    let result = exec(&["cmd", "/c", "echo", "\u{ff02}"], &[], None).await;
    refused("best fit", result, &["argument 3", "U+FF02"])?;

    let result = exec(
        &["cmd", "/c", "echo"],
        &[("A", "plain"), ("B", "\u{65e5}")],
        None,
    )
    .await;
    refused("env", result, &["env entry 1", "U+65E5"])?;

    let missing = format!("{dir}\\\u{672c}");
    let result = exec(&["cmd", "/c", "echo"], &[], Some(&missing)).await;
    refused("cwd", result, &["cwd", "U+672C"])?;
    Ok(())
}
