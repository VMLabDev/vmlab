//! Executable conformance through the production client. No guest implementation
//! is linked into the runner. Process fixtures are isolated with bubblewrap.

use super::*;
use std::collections::HashSet;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use tokio::io::unix::AsyncFd;
use vmlab_agent_proto::app::{AppRecord, RecordDecoder as AppDecoder};
use vmlab_agent_proto::fileops::SetAttrs;

const BUDGET: Duration = Duration::from_secs(12);
const LARGE: usize = 1_048_576;

#[derive(Default)]
struct Trace {
    ctrl: Vec<AgentMsg>,
    data: HashMap<u32, usize>,
}

impl Trace {
    fn push(&mut self, decoder: &mut FrameDecoder, bytes: &[u8]) {
        decoder.push(bytes);
        while let Some(frame) = decoder.next_frame() {
            if frame.kind == FrameKind::Ctrl {
                if let Ok(msg) = serde_json::from_slice(&frame.payload) {
                    self.ctrl.push(msg);
                }
            } else {
                *self.data.entry(frame.channel).or_default() += frame.payload.len();
            }
        }
    }
}

pub(in crate::labd) struct Fixture {
    _child: Option<AgentProcess>,
    jobs: Vec<tokio::task::JoinHandle<()>>,
    dir: tempfile::TempDir,
    socket: PathBuf,
    trace: Arc<std::sync::Mutex<Trace>>,
    isolated: bool,
    launch: Option<(PathBuf, String, Vec<String>)>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for job in &self.jobs {
            job.abort();
        }
    }
}

// The child guard also cleans up failures before Fixture construction completes.
struct AgentProcess(Child);

impl Drop for AgentProcess {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(self.0.id() as i32),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

impl Fixture {
    pub(in crate::labd) async fn process(
        bin: &Path,
        transport: &str,
        args: Vec<String>,
    ) -> Result<Self> {
        Self::start(Some(bin), transport, args, None).await
    }

    async fn start(
        bin: Option<&Path>,
        transport: &str,
        args: Vec<String>,
        listening: Option<&Path>,
    ) -> Result<Self> {
        let launch = bin.map(|bin| (bin.to_path_buf(), transport.to_string(), args.clone()));
        let dir = tempfile::tempdir()?;
        let socket = dir.path().join("client.sock");
        let listener = UnixListener::bind(&socket)?;
        let target = dir.path().join("agent.sock");
        let trace = Arc::new(std::sync::Mutex::new(Trace::default()));
        let mut jobs = Vec::new();
        let mut slave = None;
        let mut master = None;
        let port = if bin.is_some() && transport == "pty" {
            let fd = rustix::pty::openpt(
                rustix::pty::OpenptFlags::RDWR | rustix::pty::OpenptFlags::NOCTTY,
            )?;
            rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::RDWR | rustix::fs::OFlags::NONBLOCK)?;
            rustix::pty::grantpt(&fd)?;
            rustix::pty::unlockpt(&fd)?;
            let name = rustix::pty::ptsname(&fd, Vec::new())?;
            let path = PathBuf::from(name.to_str()?);
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)?;
            let mut term = rustix::termios::tcgetattr(&file)?;
            term.make_raw();
            rustix::termios::tcsetattr(&file, rustix::termios::OptionalActions::Now, &term)?;
            slave = Some(file);
            master = Some(AsyncFd::new(fd)?);
            path
        } else {
            target.clone()
        };
        let child = if let Some(bin) = bin {
            let bin = std::fs::canonicalize(bin)?;
            let mut command = Command::new("bwrap");
            command.args([
                "--unshare-all",
                "--die-with-parent",
                "--ro-bind",
                "/",
                "/",
                "--dev-bind",
                "/dev",
                "/dev",
                "--proc",
                "/proc",
                "--tmpfs",
                "/run",
                "--tmpfs",
                "/tmp",
                "--cap-drop",
                "ALL",
                "--bind",
            ]);
            command
                .arg(dir.path())
                .arg(dir.path())
                .arg("--bind")
                .arg(dir.path())
                .arg("/root")
                .arg("--ro-bind")
                .arg(&bin)
                .arg(&bin)
                .arg("--")
                .arg(bin);
            let args = if args.is_empty() {
                vec![
                    if transport == "pty" {
                        "--port"
                    } else {
                        "--listen"
                    }
                    .into(),
                    "{endpoint}".into(),
                ]
            } else {
                args
            };
            for arg in args {
                command.arg(arg.replace("{endpoint}", port.to_str().unwrap()));
            }
            command
                .env_remove("DISPLAY")
                .env_remove("WAYLAND_DISPLAY")
                .env_remove("XAUTHORITY")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::inherit())
                .process_group(0);
            Some(AgentProcess(command.spawn().context(
                "starting namespace-isolated agent (bwrap required)",
            )?))
        } else {
            None
        };
        let observed = trace.clone();
        if let Some(master) = master {
            jobs.push(tokio::spawn(async move {
                // Keep the slave open until the agent opens it; otherwise the
                // master reports EIO in the startup race.
                let _slave = slave;
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (mut read, mut write) = stream.into_split();
                let master = Arc::new(master);
                let tx_master = master.clone();
                let outgoing = async move {
                    let mut buf = [0; 32768];
                    loop {
                        let n = read.read(&mut buf).await?;
                        if n == 0 {
                            return Ok::<_, std::io::Error>(());
                        }
                        let mut bytes = &buf[..n];
                        while !bytes.is_empty() {
                            let mut ready = tx_master.writable().await?;
                            if let Ok(result) =
                                ready.try_io(|fd| Ok(rustix::io::write(fd.get_ref(), bytes)?))
                            {
                                let n = result?;
                                bytes = &bytes[n..];
                            }
                        }
                    }
                };
                let incoming = async move {
                    // Exercise host resynchronisation before its token echo.
                    write.write_all(b"line noise\xffVM").await?;
                    let mut decoder = FrameDecoder::new();
                    let mut buf = [0; 32768];
                    loop {
                        let mut ready = master.readable().await?;
                        if let Ok(result) =
                            ready.try_io(|fd| Ok(rustix::io::read(fd.get_ref(), &mut buf)?))
                        {
                            let n = result?;
                            if n == 0 {
                                return Ok::<_, std::io::Error>(());
                            }
                            observed.lock().unwrap().push(&mut decoder, &buf[..n]);
                            write.write_all(&buf[..n]).await?;
                        }
                    }
                };
                tokio::select! { _ = outgoing => {}, _ = incoming => {} }
            }));
        } else {
            let endpoint = listening.unwrap_or(&target).to_path_buf();
            // Only path appearance is retried. Do not repeatedly connect to an
            // external single-client listener and consume its first handshake.
            tokio::time::timeout(BUDGET, async {
                while !endpoint.exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .context("agent did not create its socket")?;
            let upstream = UnixStream::connect(endpoint).await?;
            jobs.push(tokio::spawn(async move {
                let Ok((downstream, _)) = listener.accept().await else {
                    return;
                };
                let (mut hr, mut hw) = downstream.into_split();
                let (mut ar, mut aw) = upstream.into_split();
                let incoming = async move {
                    hw.write_all(b"line noise\xffVM").await?;
                    let mut decoder = FrameDecoder::new();
                    let mut buf = [0; 32768];
                    loop {
                        let n = ar.read(&mut buf).await?;
                        if n == 0 {
                            return Ok::<_, std::io::Error>(());
                        }
                        observed.lock().unwrap().push(&mut decoder, &buf[..n]);
                        hw.write_all(&buf[..n]).await?;
                    }
                };
                tokio::select! { _ = tokio::io::copy(&mut hr, &mut aw) => {}, _ = incoming => {} }
            }));
        }
        Ok(Self {
            _child: child,
            jobs,
            dir,
            socket,
            trace,
            isolated: bin.is_some(),
            launch,
        })
    }

    pub(in crate::labd) async fn run(&self, tier: &str, root: Option<String>) -> Result<()> {
        let handle = AgentHandle::connect(&self.socket, BUDGET).await?;
        let root = root.unwrap_or_else(|| self.dir.path().to_string_lossy().into_owned());
        let mut report = Report::default();
        macro_rules! check {
            ($name:expr, $body:expr) => {{
                report.check(
                    $name,
                    tokio::time::timeout(Duration::from_secs(45), $body)
                        .await
                        .unwrap_or_else(|_| Err(anyhow!("case exceeded 45 seconds"))),
                );
            }};
        }
        check!("handshake", self.handshake(&handle, tier));
        check!("baseline", async {
            ensure(handle.ping(BUDGET).await, "ping failed")?;
            let info = handle.osinfo(BUDGET).await?;
            ensure(
                !info.id.is_empty() && !info.arch.is_empty(),
                "incomplete os_info",
            )?;
            handle.net_interfaces(BUDGET).await?;
            Ok(())
        });
        check!("resync", self.resync(&handle));
        let features = handle.info().features;
        for feature in &features {
            match feature.as_str() {
                "exec" => {
                    check!("exec", exec_case(&handle, &root));
                    check!("flow_control", self.flow_control(&handle));
                    match tokio::time::timeout(BUDGET, self.cancellation(&handle, &root)).await {
                        Ok(Ok(true)) => report.deviation(
                            "cancellation",
                            "F11",
                            "process killed, but closed channel emitted EOF/exited",
                        ),
                        Ok(Ok(false)) => report.check("cancellation", Ok(())),
                        Ok(Err(e)) => report.check("cancellation", Err(e)),
                        Err(e) => report.check("cancellation", Err(e.into())),
                    }
                }
                "terminal" => check!("terminal", terminal_case(&handle)),
                "fileops" => check!("fileops", fileops_case(&handle, &root)),
                "tail" => check!("tail", tail_case(&handle, &root)),
                "watch" => check!("watch", watch_case(&handle, &root)),
                "metrics" => check!("metrics", self.metrics(&handle)),
                "get_time" => check!("get_time", async {
                    let time = handle.get_time(BUDGET).await?;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_millis() as i64;
                    ensure(
                        (time - now).abs() < 30_000,
                        "clock differs by more than 30 seconds",
                    )
                }),
                "set_time" if self.isolated => check!("set_time", async {
                    let error = handle
                        .set_time(std::time::SystemTime::now(), BUDGET)
                        .await
                        .expect_err("namespace agent must not change the host clock");
                    ensure(
                        error.to_string().contains("guest refused"),
                        &error.to_string(),
                    )
                }),
                "set_time" => {
                    report.skip("set_time", "external socket: clock mutation not authorized")
                }
                "clipboard" => check!("clipboard", clipboard_case(&handle)),
                "clipboard_reply" => {
                    check!("clipboard_reply", async {
                        ensure(
                            handle.has_feature("clipboard"),
                            "clipboard_reply requires clipboard",
                        )
                    });
                }
                "tree" => check!("tree", tree_case(&handle)),
                "app_x11" => check!(
                    "app_x11",
                    app_case(
                        &handle,
                        AppMode::X11 {
                            cookie: "ab".repeat(16)
                        }
                    )
                ),
                "app_waypipe" => check!("app_waypipe", app_case(&handle, AppMode::Waypipe)),
                "app_rdp" => check!("app_rdp", async {
                    let result = handle
                        .open_app(AppMode::Rdp, vec![], vec![], None, None)
                        .await;
                    refused(result, "RDP without logon")
                }),
                "eventlog" => check!("eventlog", async {
                    let session = handle.open_eventlog(Some("[invalid XPath".into())).await;
                    refused(session, "invalid eventlog query")
                }),
                other => report.check(
                    other,
                    Err(anyhow!("no conformance case for advertised feature")),
                ),
            }
        }
        check!("absent_features", absent_cases(&handle));
        check!("absent_apps", self.absent_apps(&handle));
        check!("logons", async {
            refused(
                handle
                    .open_exec(
                        vec!["true".into()],
                        vec![],
                        None,
                        Some(Logon {
                            user: "vmlab-conformance-nonexistent-account".into(),
                            secret: "invalid".into(),
                            elevated: false,
                        }),
                    )
                    .await,
                "nonexistent logon",
            )
        });
        check!("rehandshake", self.rehandshake(&handle, &root));
        if self.isolated {
            check!("shutdown", async {
                if let Some((bin, transport, args)) = &self.launch {
                    for mode in [ShutdownMode::Reboot, ShutdownMode::Halt] {
                        let fixture = Fixture::process(bin, transport, args.clone()).await?;
                        let client = AgentHandle::connect(&fixture.socket, BUDGET).await?;
                        client.shutdown_guest(mode, BUDGET).await?;
                        ensure(fixture.trace.lock().unwrap().ctrl.iter().any(|m|
                            matches!(m, AgentMsg::ShuttingDown { mode: actual } if *actual == mode)),
                            "missing reboot/halt acknowledgement")?;
                        client.shutdown().await;
                    }
                }
                handle
                    .shutdown_guest(ShutdownMode::Powerdown, BUDGET)
                    .await?;
                ensure(
                    self.trace.lock().unwrap().ctrl.iter().any(|m| {
                        matches!(
                            m,
                            AgentMsg::ShuttingDown {
                                mode: ShutdownMode::Powerdown
                            }
                        )
                    }),
                    "shutdown helper hid a missing acknowledgement",
                )
            });
        } else {
            report.skip("shutdown", "external socket: power action not authorized");
        }
        handle.shutdown().await;
        report.finish()
    }

    async fn handshake(&self, handle: &AgentHandle, tier: &str) -> Result<()> {
        let info = handle.info();
        ensure(
            !info.agent_version.is_empty() && !info.os.is_empty(),
            "empty hello metadata",
        )?;
        let unique: HashSet<_> = info.features.iter().collect();
        ensure(
            unique.len() == info.features.len(),
            "duplicate feature advertisement",
        )?;
        let required: &[&str] = match tier {
            "full" => &[
                "exec", "terminal", "fileops", "tail", "metrics", "set_time", "get_time",
            ],
            "legacy" => &["exec", "set_time"],
            "templeos" => &["exec"],
            "auto" => &["exec"],
            _ => bail!("unknown tier {tier}"),
        };
        for feature in required {
            ensure(handle.has_feature(feature), &format!("missing {feature}"))?;
        }
        eprintln!(
            "AGENT {} os={} features={}",
            info.agent_version,
            info.os,
            info.features.join(",")
        );
        if tier == "full" {
            ensure(info.instance.is_some(), "full agent omitted instance")?;
            let platform: &[&str] = match info.os.as_str() {
                "linux" => &["watch", "tree", "app_x11", "app_waypipe"],
                "windows" => &[
                    "watch",
                    "tree",
                    "clipboard",
                    "clipboard_reply",
                    "eventlog",
                    "app_rdp",
                ],
                "macos" => &[],
                other => bail!("unrecognized full-agent OS {other}"),
            };
            for feature in platform {
                ensure(
                    handle.has_feature(feature),
                    &format!("missing {feature} on {}", info.os),
                )?;
            }
        }
        Ok(())
    }

    async fn resync(&self, handle: &AgentHandle) -> Result<()> {
        let mut noise = b"garbage\xffVMLB".to_vec();
        noise.extend(u32::MAX.to_le_bytes());
        noise.extend([0, 0, 0, 0, 0]);
        noise.extend(b"VMLB\x00\x00\x00\x00\xee\x00\x00\x00\x00VM");
        {
            let mut writer = handle.inner.writer.lock().await;
            for part in noise.chunks(3) {
                writer.write_all(part).await?;
            }
        }
        ensure(
            handle.ping(BUDGET).await,
            "no pong after injected noise and false headers",
        )
    }

    async fn flow_control(&self, handle: &AgentHandle) -> Result<()> {
        let mut session = handle
            .open_exec(sh("head -c 1048576 /dev/zero"), vec![], None, None)
            .await?;
        let id = session.id;
        session.eof().await?;
        tokio::time::sleep(Duration::from_millis(350)).await;
        let received = self
            .trace
            .lock()
            .unwrap()
            .data
            .get(&id)
            .copied()
            .unwrap_or(0);
        ensure(
            received > 0 && received <= INITIAL_WINDOW as usize,
            &format!("stalled consumer received {received} bytes, initial credit {INITIAL_WINDOW}"),
        )?;
        // A stalled stream must leave control and other channels usable.
        ensure(handle.ping(BUDGET).await, "bulk channel blocked ping")?;
        let output = collect(&mut session).await?;
        ensure(
            output.stdout == vec![0; LARGE] && output.exit_code == 0,
            "large output corrupted",
        )?;
        let mut session = handle
            .open_exec(sh("sleep 0.2; cat"), vec![], None, None)
            .await?;
        let sender = session.sender();
        let bytes: Vec<_> = (0..LARGE).map(|i| (i % 251) as u8).collect();
        let sent = bytes.clone();
        let producer = tokio::spawn(async move {
            sender.send(&sent).await?;
            sender.eof().await
        });
        let output = collect(&mut session).await;
        producer.await??;
        ensure(output?.stdout == bytes, "duplex transfer corrupted")?;
        Ok(())
    }

    async fn cancellation(&self, handle: &AgentHandle, root: &str) -> Result<bool> {
        let path = format!("{root}/cancelled");
        let script = "import os,sys,time; time.sleep(0.8); open(sys.argv[1],'w').write('alive')";
        let session = handle
            .open_exec(
                vec!["python3".into(), "-c".into(), script.into(), path.clone()],
                vec![],
                None,
                None,
            )
            .await?;
        let id = session.id;
        session.close().await;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let out = handle
            .exec(
                vec!["test".into(), "!".into(), "-e".into(), path],
                vec![],
                None,
                None,
                BUDGET,
                None,
            )
            .await?;
        ensure(out.exit_code == 0, "closed exec continued running")?;
        let trace = self.trace.lock().unwrap();
        ensure(
            trace.data.get(&id).copied().unwrap_or(0) == 0,
            "closed silent process emitted data",
        )?;
        Ok(trace.ctrl.iter().any(|m|
            matches!(m, AgentMsg::Exited { id: channel, .. } | AgentMsg::Eof { id: channel } if *channel == id)))
    }

    async fn metrics(&self, handle: &AgentHandle) -> Result<()> {
        let sample = handle.stats(BUDGET).await?;
        ensure(
            sample.cpu_pct.is_finite() && (0.0..=100.0).contains(&sample.cpu_pct),
            "invalid CPU percentage",
        )?;
        ensure(
            sample.mem_total > 0 && sample.mem_used <= sample.mem_total,
            "invalid memory sample",
        )?;
        handle.send_msg(&HostMsg::UnsubscribeMetrics).await?;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let before = self
            .trace
            .lock()
            .unwrap()
            .ctrl
            .iter()
            .filter(|m| matches!(m, AgentMsg::Metrics { .. }))
            .count();
        tokio::time::sleep(Duration::from_millis(2200)).await;
        let after = self
            .trace
            .lock()
            .unwrap()
            .ctrl
            .iter()
            .filter(|m| matches!(m, AgentMsg::Metrics { .. }))
            .count();
        ensure(before == after, "metrics continued after unsubscribe")
    }

    async fn absent_apps(&self, handle: &AgentHandle) -> Result<()> {
        for (feature, mode) in [
            (
                "app_x11",
                AppMode::X11 {
                    cookie: "ab".repeat(16),
                },
            ),
            ("app_waypipe", AppMode::Waypipe),
            ("app_rdp", AppMode::Rdp),
        ] {
            if handle.has_feature(feature) {
                continue;
            }
            let id = u32::MAX - 10;
            let before = self.trace.lock().unwrap().ctrl.len();
            handle
                .send_msg(&HostMsg::OpenApp {
                    id,
                    mode,
                    argv: vec!["true".into()],
                    env: vec![],
                    cwd: None,
                    logon: None,
                })
                .await?;
            tokio::time::timeout(BUDGET, async {
                loop {
                    for message in &self.trace.lock().unwrap().ctrl[before..] {
                        if let AgentMsg::Error { id: reply_id, msg } = message {
                            return ensure(
                                *reply_id == Some(id),
                                &format!("{feature} refusal lost channel id: {msg}"),
                            );
                        }
                        if matches!(message, AgentMsg::Opened { id: opened } if *opened == id) {
                            bail!("unadvertised {feature} opened");
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await??;
        }
        Ok(())
    }

    async fn rehandshake(&self, handle: &AgentHandle, root: &str) -> Result<()> {
        let path = format!("{root}/old-epoch");
        let held = handle
            .open_exec(
                vec![
                    "python3".into(),
                    "-c".into(),
                    "import sys,time; time.sleep(0.8); open(sys.argv[1],'w').write('alive')".into(),
                    path.clone(),
                ],
                vec![],
                None,
                None,
            )
            .await?;
        let before = self.trace.lock().unwrap().ctrl.len();
        // Same transport, fresh epoch: no disconnect/reconnect substitute.
        handle
            .send_msg(&HostMsg::Hello {
                proto_version: PROTO_VERSION,
                token: "conformance-new-epoch".into(),
            })
            .await?;
        tokio::time::timeout(BUDGET, async {
            loop {
                if self.trace.lock().unwrap().ctrl[before..].iter().any(|m|
                    matches!(m, AgentMsg::Hello { token, .. } if token == "conformance-new-epoch")) { break; }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await?;
        tokio::time::sleep(Duration::from_millis(1000)).await;
        let out = handle
            .exec(
                vec!["test".into(), "!".into(), "-e".into(), path],
                vec![],
                None,
                None,
                BUDGET,
                None,
            )
            .await?;
        ensure(out.exit_code == 0, "old epoch process survived hello")?;
        {
            let trace = self.trace.lock().unwrap();
            let instance = trace.ctrl[before..].iter().find_map(|m| match m {
                AgentMsg::Hello { instance, .. } => Some(instance),
                _ => None,
            });
            ensure(
                instance == Some(&handle.info().instance),
                "hello changed instance without restart",
            )?;
        }
        ensure(
            handle.ping(BUDGET).await,
            "rehandshake broke the connection",
        )?;
        held.close().await;
        Ok(())
    }
}

#[derive(Default)]
struct Report {
    failures: Vec<String>,
    skipped: usize,
    passed: usize,
    known: usize,
}

impl Report {
    fn check(&mut self, feature: &str, result: Result<()>) {
        match result {
            Ok(()) => {
                self.passed += 1;
                eprintln!("CONFORMANCE {feature} PASS");
            }
            Err(e) => {
                eprintln!("CONFORMANCE {feature} FAIL {e:#}");
                self.failures.push(feature.into());
            }
        }
    }
    fn skip(&mut self, feature: &str, reason: &str) {
        self.skipped += 1;
        eprintln!("CONFORMANCE {feature} SKIP {reason}");
    }
    fn deviation(&mut self, feature: &str, id: &str, reason: &str) {
        let allowed = std::env::var("VMLAB_CONFORMANCE_ALLOW_FINDINGS").unwrap_or_default();
        if allowed.split(',').any(|s| s == id) {
            self.known += 1;
            eprintln!("CONFORMANCE {feature} KNOWN {id}: {reason}");
        } else {
            self.check(feature, Err(anyhow!("{id}: {reason}")));
        }
    }
    fn finish(self) -> Result<()> {
        eprintln!(
            "CONFORMANCE total PASS={} FAIL={} SKIP={} KNOWN={}",
            self.passed,
            self.failures.len(),
            self.skipped,
            self.known
        );
        ensure(
            self.failures.is_empty(),
            &format!("failed cases: {}", self.failures.join(", ")),
        )
    }
}

fn ensure(condition: bool, message: &str) -> Result<()> {
    if !condition {
        bail!("{message}");
    }
    Ok(())
}

fn refused<T>(result: Result<T>, what: &str) -> Result<()> {
    match result {
        Err(e) => ensure(
            !e.to_string().contains("in time"),
            &format!("{what} timed out instead of refusing: {e}"),
        ),
        Ok(_) => bail!("{what} unexpectedly accepted"),
    }
}

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

async fn collect(session: &mut AgentSession) -> Result<ExecOutput> {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut eof = false;
    loop {
        match tokio::time::timeout(BUDGET, session.recv()).await? {
            Some(SessionEvent::Data(b)) => {
                ensure(!eof, "data after EOF")?;
                stdout.extend(b);
            }
            Some(SessionEvent::Stderr(b)) => {
                ensure(!eof, "stderr after EOF")?;
                stderr.extend(b);
            }
            Some(SessionEvent::Eof) => {
                ensure(!eof, "duplicate EOF")?;
                eof = true;
            }
            Some(SessionEvent::Exited(exit_code)) => {
                return Ok(ExecOutput {
                    exit_code,
                    stdout,
                    stderr,
                });
            }
            other => bail!("unexpected session event {other:?}"),
        }
    }
}

async fn exec_case(handle: &AgentHandle, root: &str) -> Result<()> {
    let out = handle
        .exec(
            sh("printf '%s\\n' \"$FOO\"; pwd; cat; printf err >&2; exit 3"),
            vec![("FOO".into(), "bar baz".into())],
            Some(root.into()),
            Some(b"stdin\n".to_vec()),
            BUDGET,
            None,
        )
        .await?;
    ensure(
        out.exit_code == 3
            && out.stdout == format!("bar baz\n{root}\nstdin\n").as_bytes()
            && out.stderr == b"err",
        "exec argv/env/cwd/stdin/stderr/exit mismatch",
    )?;
    refused(
        handle.open_exec(vec![], vec![], None, None).await,
        "empty argv",
    )?;
    match handle
        .exec(
            vec!["/vmlab-conformance-no-such-program".into()],
            vec![],
            None,
            None,
            BUDGET,
            None,
        )
        .await
    {
        Ok(out) => ensure(out.exit_code == 127, "missing program must fail")?,
        Err(e) => ensure(
            !e.to_string().contains("timed out"),
            "missing program timed out",
        )?,
    }
    let mut sessions = Vec::new();
    for i in 0..3 {
        let s = handle
            .open_exec(
                sh(&format!("sleep 0.1; printf chan{i}; exit {i}")),
                vec![],
                None,
                None,
            )
            .await?;
        s.eof().await?;
        sessions.push(s);
    }
    for (i, mut session) in sessions.into_iter().enumerate() {
        let out = collect(&mut session).await?;
        ensure(
            out.exit_code == i as i32 && out.stdout == format!("chan{i}").as_bytes(),
            "channels crossed",
        )?;
    }
    Ok(())
}

async fn terminal_case(handle: &AgentHandle) -> Result<()> {
    let mut terminal = handle
        .open_terminal(
            80,
            24,
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "read line; stty size; printf '%s' \"$line\"; exit 4".into(),
            ]),
            vec![],
            None,
        )
        .await?;
    terminal.resize(101, 33).await?;
    terminal.send(b"terminal-input\n").await?;
    let out = collect(&mut terminal).await?;
    let text = String::from_utf8_lossy(&out.stdout);
    ensure(
        out.exit_code == 4 && text.contains("33 101") && text.contains("terminal-input"),
        &format!(
            "PTY input/resize/exit failed: code={} output={text:?}",
            out.exit_code
        ),
    )
}

async fn ok(files: &FileOps, op: Op, payload: &[u8]) -> Result<()> {
    let (reply, data) = files.request(op, payload).await?;
    ensure(
        reply == Reply::Ok && data.is_empty(),
        &format!("expected ok, got {reply:?}"),
    )
}

async fn fileops_case(handle: &AgentHandle, root: &str) -> Result<()> {
    let files = Arc::new(handle.open_fileops(None).await?);
    let directory = format!("{root}/files");
    files.mkdir(&directory, true).await?;
    let path = format!("{directory}/data");
    let (reply, _) = files
        .request(
            Op::Open {
                path: path.clone(),
                flags: OpenFlags::create_truncate(),
                mode: Some(0o600),
            },
            &[],
        )
        .await?;
    let Reply::Handle { handle: fd } = reply else {
        bail!("open returned {reply:?}")
    };
    let mut writes = Vec::new();
    for i in 0..32 {
        let files = files.clone();
        writes.push(tokio::spawn(async move {
            ok(
                &files,
                Op::Write {
                    handle: fd,
                    offset: i * 32768,
                },
                &vec![i as u8; 32768],
            )
            .await
        }));
    }
    for write in writes {
        write.await??;
    }
    let (reply, _) = files.request(Op::Fstat { handle: fd }, &[]).await?;
    ensure(
        matches!(reply, Reply::Attrs { attrs } if attrs.size == LARGE as u64),
        "pipelined writes lost",
    )?;
    for i in 0..8 {
        let (reply, data) = files
            .request(
                Op::Read {
                    handle: fd,
                    offset: i * 131072,
                    len: 131072,
                },
                &[],
            )
            .await?;
        // The write-only handle must refuse a read, without killing the session.
        ensure(
            matches!(reply, Reply::Error { .. }) && data.is_empty(),
            "write-only read accepted",
        )?;
    }
    ok(
        &files,
        Op::Fsetstat {
            handle: fd,
            attrs: SetAttrs {
                size: Some(LARGE as u64 + 1),
                ..Default::default()
            },
        },
        &[],
    )
    .await?;
    ok(&files, Op::Close { handle: fd }, &[]).await?;
    let (reply, _) = files
        .request(
            Op::Read {
                handle: fd,
                offset: 0,
                len: 1,
            },
            &[],
        )
        .await?;
    ensure(
        matches!(
            reply,
            Reply::Error {
                code: ErrorCode::BadHandle,
                ..
            }
        ),
        "stale handle not refused",
    )?;
    let mut expected = Vec::new();
    for i in 0..32 {
        expected.extend(vec![i; 32768]);
    }
    expected.push(0);
    let (reply, _) = files
        .request(
            Op::Open {
                path: path.clone(),
                flags: OpenFlags::default(),
                mode: None,
            },
            &[],
        )
        .await?;
    let Reply::Handle { handle: reader } = reply else {
        bail!("read-only open: {reply:?}")
    };
    let (reply, bytes) = files
        .request(
            Op::Read {
                handle: reader,
                offset: LARGE as u64,
                len: 100,
            },
            &[],
        )
        .await?;
    ensure(
        reply == Reply::Data && bytes == [0],
        "short read is not EOF",
    )?;
    let (reply, bytes) = files
        .request(
            Op::Read {
                handle: reader,
                offset: LARGE as u64 + 1,
                len: 100,
            },
            &[],
        )
        .await?;
    ensure(
        reply == Reply::Data && bytes.is_empty(),
        "read beyond EOF not empty",
    )?;
    let (reply, _) = files
        .request(
            Op::Read {
                handle: reader,
                offset: 0,
                len: 131073,
            },
            &[],
        )
        .await?;
    ensure(
        matches!(reply, Reply::Error { .. }),
        "oversize read silently clamped",
    )?;
    let other = handle.open_fileops(None).await?;
    let (reply, _) = other.request(Op::Fstat { handle: reader }, &[]).await?;
    ensure(
        matches!(
            reply,
            Reply::Error {
                code: ErrorCode::BadHandle,
                ..
            }
        ),
        "handle escaped its session",
    )?;
    other.close().await;
    ok(&files, Op::Close { handle: reader }, &[]).await?;
    let (reply, _) = files
        .request(
            Op::Open {
                path: path.clone(),
                flags: OpenFlags {
                    write: true,
                    exclusive: true,
                    ..Default::default()
                },
                mode: None,
            },
            &[],
        )
        .await?;
    ensure(
        matches!(
            reply,
            Reply::Error {
                code: ErrorCode::AlreadyExists,
                ..
            }
        ),
        "exclusive open overwrote existing file",
    )?;
    let (reply, _) = files
        .request(
            Op::Open {
                path: path.clone(),
                flags: OpenFlags {
                    append: true,
                    ..Default::default()
                },
                mode: None,
            },
            &[],
        )
        .await?;
    let Reply::Handle { handle: append } = reply else {
        bail!("append open: {reply:?}")
    };
    ok(
        &files,
        Op::Write {
            handle: append,
            offset: 0,
        },
        b"append",
    )
    .await?;
    ok(&files, Op::Close { handle: append }, &[]).await?;
    expected.extend(b"append");
    let (_, bytes) = handle.pull_bytes(&path, (LARGE + 8) as u64).await?;
    ensure(bytes == expected, "offset transfer differs")?;
    let digest = files.digest(&path).await?;
    use sha2::{Digest, Sha256};
    ensure(
        digest.0 == hex::encode(Sha256::digest(&expected)),
        "wrong SHA-256",
    )?;
    ok(
        &files,
        Op::Setstat {
            path: path.clone(),
            attrs: SetAttrs {
                mtime_ns: Some(1_700_000_000_123_456_789),
                ..Default::default()
            },
        },
        &[],
    )
    .await?;
    ensure(
        files.stat(&path).await?.mtime_ns == 1_700_000_000_123_456_789,
        "mtime not preserved",
    )?;
    let (reply, _) = files
        .request(Op::Realpath { path: path.clone() }, &[])
        .await?;
    ensure(
        matches!(reply, Reply::Name { path: p } if p == path),
        "realpath incorrect",
    )?;
    let link = format!("{directory}/link");
    files.symlink("data", &link, LinkKind::File).await?;
    ensure(files.readlink(&link).await? == "data", "readlink differs")?;
    ensure(
        files
            .lstat(&link)
            .await?
            .is_some_and(|s| s.kind == EntryKind::Symlink),
        "lstat followed link",
    )?;
    ensure(
        files.stat(&link).await?.kind == EntryKind::File,
        "stat did not follow link",
    )?;
    ensure(
        files.readdir(&directory).await?.len() == 2,
        "readdir count differs",
    )?;
    let (reply, _) = files
        .request(
            Op::Rmdir {
                path: directory.clone(),
            },
            &[],
        )
        .await?;
    ensure(
        matches!(
            reply,
            Reply::Error {
                code: ErrorCode::NotEmpty,
                ..
            }
        ),
        "nonempty directory removed",
    )?;
    let (reply, _) = files
        .request(Op::OpenDir { path: path.clone() }, &[])
        .await?;
    ensure(
        matches!(
            reply,
            Reply::Error {
                code: ErrorCode::NotADirectory,
                ..
            }
        ),
        "file accepted as directory",
    )?;
    let renamed = format!("{directory}/renamed");
    files.rename(&path, &renamed).await?;
    files.remove(&renamed).await?;
    files.remove(&link).await?;
    files.rmdir(&directory).await?;
    let (reply, _) = files.request(Op::Stat { path }, &[]).await?;
    ensure(
        matches!(
            reply,
            Reply::Error {
                code: ErrorCode::NoSuchFile,
                ..
            }
        ),
        "missing stat not classified",
    )?;
    Arc::try_unwrap(files)
        .map_err(|_| anyhow!("file requests still running"))?
        .close()
        .await;
    // A malformed nested record fails just this channel.
    let broken = handle
        .open(|id| HostMsg::OpenFileOps { id, logon: None })
        .await?;
    broken.send(&u32::MAX.to_le_bytes().repeat(2)).await?;
    let mut broken = broken;
    ensure(
        matches!(
            tokio::time::timeout(BUDGET, broken.recv()).await?,
            Some(SessionEvent::Error(_))
        ),
        "oversized record did not fail",
    )?;
    ensure(
        handle.ping(BUDGET).await,
        "malformed record killed connection",
    )
}

async fn read_until(session: &mut AgentSession, needle: &[u8]) -> Result<()> {
    let mut bytes = Vec::new();
    tokio::time::timeout(BUDGET, async {
        loop {
            match session.recv().await {
                Some(SessionEvent::Data(data)) => bytes.extend(data),
                other => bail!("unexpected stream event {other:?}"),
            }
            if bytes.windows(needle.len()).any(|w| w == needle) {
                return Ok(());
            }
        }
    })
    .await?
}

async fn tail_case(handle: &AgentHandle, root: &str) -> Result<()> {
    let files = handle.open_fileops(None).await?;
    let path = format!("{root}/tail");
    files.push_bytes(b"initial\n", &path, None).await?;
    let mut tail = handle.open_tail(path.clone(), None).await?;
    read_until(&mut tail, b"initial").await?;
    files.push_bytes(b"new\n", &path, None).await?;
    read_until(&mut tail, b"new").await?;
    files.rename(&path, &format!("{path}.old")).await?;
    files
        .push_bytes(b"rotated-is-longer\n", &path, None)
        .await?;
    read_until(&mut tail, b"rotated-is-longer").await?;
    tail.close().await;
    refused(
        handle.open_tail(format!("{root}/nonexistent"), None).await,
        "missing tail",
    )?;
    files.remove(&path).await?;
    files.remove(&format!("{path}.old")).await?;
    files.close().await;
    Ok(())
}

async fn watch_case(handle: &AgentHandle, root: &str) -> Result<()> {
    let files = handle.open_fileops(None).await?;
    let path = format!("{root}/watch");
    files.mkdir(&path, false).await?;
    files.mkdir(&format!("{path}/pruned"), false).await?;
    let mut watch = handle
        .open_watch(path.clone(), vec!["pruned".into()])
        .await?;
    files
        .push_bytes(b"hidden", &format!("{path}/pruned/hidden"), None)
        .await?;
    files
        .push_bytes(b"visible", &format!("{path}/visible"), None)
        .await?;
    tokio::time::timeout(BUDGET, async {
        loop {
            if matches!(watch.recv().await, Some(WatchReport::Dirty)) {
                return;
            }
        }
    })
    .await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    watch.drain().await?;
    let entries = tokio::time::timeout(BUDGET, async {
        loop {
            match watch.recv().await {
                Some(WatchReport::Batch(e)) => return Ok(e),
                Some(WatchReport::Dirty) => {}
                other => bail!("unexpected watch report {other:?}"),
            }
        }
    })
    .await??;
    ensure(
        entries
            .iter()
            .any(|e| e.path == "visible" && e.stat.is_some()),
        "watch omitted changed file",
    )?;
    ensure(
        !entries.iter().any(|e| e.path.starts_with("pruned/")),
        "prune ignored",
    )?;
    files.remove(&format!("{path}/visible")).await?;
    tokio::time::sleep(Duration::from_millis(150)).await;
    watch.drain().await?;
    tokio::time::timeout(BUDGET, async {
        loop {
            match watch.recv().await {
                Some(WatchReport::Batch(e)) => {
                    return ensure(
                        e.iter().any(|e| e.path == "visible" && e.stat.is_none()),
                        "missing tombstone",
                    );
                }
                Some(WatchReport::Dirty) => {}
                other => bail!("unexpected watch report {other:?}"),
            }
        }
    })
    .await??;
    watch.close().await;
    refused(
        handle
            .open_watch(format!("{root}/absent-watch"), vec![])
            .await,
        "missing watch root",
    )?;
    files.remove(&format!("{path}/pruned/hidden")).await?;
    files.rmdir(&format!("{path}/pruned")).await?;
    files.rmdir(&path).await?;
    files.close().await;
    Ok(())
}

async fn tree_case(handle: &AgentHandle) -> Result<()> {
    ensure(
        handle.tree_alive("conformance-absent", BUDGET).await? == 0,
        "unknown tree is alive",
    )?;
    refused(
        handle
            .open_exec_in_tree(vec!["true".into()], "../invalid")
            .await,
        "invalid tree name",
    )?;
    let mut session = handle
        .open_exec_in_tree(sh("sleep 1"), "conformance_tree")
        .await?;
    ensure(
        handle.tree_alive("conformance_tree", BUDGET).await? > 0,
        "live tree empty",
    )?;
    session.eof().await?;
    collect(&mut session).await?;
    ensure(
        handle.tree_alive("conformance_tree", BUDGET).await? == 0,
        "exited tree remains alive",
    )
}

async fn clipboard_case(handle: &AgentHandle) -> Result<()> {
    let text = "vmlab conformance clipboard".to_string();
    match handle.set_clipboard(text.clone(), BUDGET).await {
        Ok(()) => ensure(
            handle.get_clipboard(BUDGET).await? == text,
            "clipboard round trip differs",
        ),
        Err(e) if e.downcast_ref::<ClipboardRefused>().is_some() => ensure(
            handle
                .get_clipboard(BUDGET)
                .await
                .unwrap_err()
                .downcast_ref::<ClipboardRefused>()
                .is_some(),
            "clipboard get did not refuse explicitly",
        ),
        Err(e) => Err(e),
    }
}

async fn app_case(handle: &AgentHandle, mode: AppMode) -> Result<()> {
    let x11 = matches!(mode, AppMode::X11 { .. });
    let argv = if x11 {
        vec!["python3".into(), "-c".into(),
            "import os,socket; d=os.environ['DISPLAY'].split(':')[-1].split('.')[0]; s=socket.socket(socket.AF_UNIX); s.connect('/tmp/.X11-unix/X'+d); s.sendall(b'from-app'); s.shutdown(socket.SHUT_WR); assert s.recv(6)==b'to-app'; print('app-log')".into()]
    } else {
        sh("sleep 0.5; echo app-log; exit 0")
    };
    let mut app = handle.open_app(mode, argv, vec![], None, None).await?;
    let mut decoder = AppDecoder::new();
    let mut ready = false;
    let mut connections = Vec::new();
    let mut pending = 0;
    loop {
        match tokio::time::timeout(BUDGET, app.recv()).await? {
            Some(SessionEvent::Data(bytes)) => {
                decoder.push(&bytes);
                while let Some(record) = decoder.next_record().map_err(|e| anyhow!(e))? {
                    match record {
                        AppRecord::Ready { .. } => ready = true,
                        AppRecord::Pending => {
                            ensure(ready, "pending before ready")?;
                            pending += 1;
                            let mut connection = handle.open_app_conn(app.id).await?;
                            if x11 {
                                read_until(&mut connection, b"from-app").await?;
                                ensure(
                                    matches!(
                                        tokio::time::timeout(BUDGET, connection.recv()).await?,
                                        Some(SessionEvent::Eof)
                                    ),
                                    "app connection did not half-close",
                                )?;
                                connection.send(b"to-app").await?;
                                connection.eof().await?;
                                connections.push(connection);
                            } else {
                                connection.close().await;
                            }
                        }
                        AppRecord::Log { .. } => {}
                        other => bail!("agent sent daemon-only app record {other:?}"),
                    }
                }
            }
            Some(SessionEvent::Exited(code)) => {
                if x11 {
                    ensure(code == 0, &format!("X11 transport program exited {code}"))?;
                }
                break;
            }
            Some(SessionEvent::Eof) => {}
            other => bail!("app failed {other:?}"),
        }
    }
    ensure(ready, "app never became ready")?;
    if x11 {
        ensure(pending > 0, "X11 emitted no pending connection")?;
    }
    for connection in connections {
        connection.close().await;
    }
    refused(
        handle.open_app_conn(u32::MAX).await,
        "unknown app connection",
    )
}

async fn absent_cases(handle: &AgentHandle) -> Result<()> {
    if !handle.has_feature("terminal") {
        refused(
            handle.open_terminal(80, 24, None, vec![], None).await,
            "unadvertised terminal",
        )?;
    }
    if !handle.has_feature("fileops") {
        refused(handle.open_fileops(None).await, "unadvertised fileops")?;
        refused(
            handle
                .open(|id| HostMsg::OpenFileOps { id, logon: None })
                .await,
            "raw unadvertised fileops",
        )?;
    }
    if !handle.has_feature("watch") {
        refused(
            handle
                .open(|id| HostMsg::OpenWatch {
                    id,
                    path: "/".into(),
                    prune: vec![],
                })
                .await,
            "raw unadvertised watch",
        )?;
    }
    if !handle.has_feature("tail") {
        refused(
            handle.open_tail("/".into(), None).await,
            "unadvertised tail",
        )?;
    }
    if !handle.has_feature("eventlog") {
        refused(handle.open_eventlog(None).await, "unadvertised eventlog")?;
    }
    if !handle.has_feature("clipboard") {
        refused(handle.get_clipboard(BUDGET).await, "unadvertised clipboard")?;
    }
    if !handle.has_feature("get_time") {
        refused(handle.get_time(BUDGET).await, "unadvertised get_time")?;
    }
    if !handle.has_feature("set_time") {
        refused(
            handle.set_time(std::time::SystemTime::now(), BUDGET).await,
            "unadvertised set_time",
        )?;
    }
    if !handle.has_feature("tree") {
        refused(
            handle.tree_alive("absent", BUDGET).await,
            "unadvertised tree",
        )?;
    }
    Ok(())
}

/// Environment is used instead of libtest arguments so the same executable
/// works through `cargo test` and directly through an already-built test binary.
#[tokio::test]
#[ignore = "requires an explicit agent binary or listening socket"]
async fn external_agent() -> Result<()> {
    let agent = std::env::var_os("VMLAB_CONFORMANCE_AGENT").map(PathBuf::from);
    let socket = std::env::var_os("VMLAB_CONFORMANCE_SOCKET").map(PathBuf::from);
    ensure(
        agent.is_some() != socket.is_some(),
        "set exactly one agent or socket",
    )?;
    let transport = std::env::var("VMLAB_CONFORMANCE_TRANSPORT").unwrap_or_else(|_| "pty".into());
    ensure(
        ["pty", "listen"].contains(&transport.as_str()),
        "transport must be pty or listen",
    )?;
    let args: Vec<String> = std::env::var("VMLAB_CONFORMANCE_ARGS")
        .ok()
        .map(|s| serde_json::from_str(&s))
        .transpose()?
        .unwrap_or_default();
    let fixture = Fixture::start(agent.as_deref(), &transport, args, socket.as_deref()).await?;
    fixture
        .run(
            &std::env::var("VMLAB_CONFORMANCE_TIER").unwrap_or_else(|_| "auto".into()),
            std::env::var("VMLAB_CONFORMANCE_ROOT").ok(),
        )
        .await
}
