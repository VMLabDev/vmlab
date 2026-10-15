//! Shared-app sessions (PRD §20) end to end against real processes and real
//! sockets on the build host: the same code paths a guest runs, minus the
//! virtio port.
//!
//! The guest-side client — the program an X11 session runs, the `waypipe`
//! a waypipe session runs — is this test binary again, re-entered through
//! [`guest_client`], so the tests need no `socat` or Python on the host.

#![cfg(test)]
#![cfg(unix)]

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{SocketAddr, UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use vmlab_agent_proto::app::{AppMode, AppRecord, RecordDecoder};
use vmlab_agent_proto::{AgentMsg, Frame, FrameKind, HostMsg, Logon};

use crate::app::{FAMILY_LOCAL, FAMILY_WILD, waypipe_argv, xauthority};
use crate::fake_spawner::TestPlatform;
use crate::mux::{Input, Mux, Platform};
use crate::testutil::{Capture, capture_mux};

const COOKIE: &str = "00112233445566778899aabbccddeeff";

/// The guest-side client, when the test binary is re-entered as one. Run as
/// an ordinary test it finds no role and does nothing.
///
/// It connects where its role says — `x11` to `$DISPLAY`'s abstract socket,
/// `waypipe` to the `--socket` the fake waypipe was handed — says hello, and
/// answers what the host sends until the host shuts its half. `linger`
/// instead waits to be disconnected, reports it, and stays alive.
#[test]
fn guest_client() {
    let Ok(role) = std::env::var("VMLAB_APP_TEST_ROLE") else {
        return;
    };
    let mut stream = match role.as_str() {
        "waypipe" => UnixStream::connect(std::env::var("VMLAB_APP_TEST_SOCKET").unwrap()),
        _ => {
            let display = std::env::var("DISPLAY").unwrap();
            let name = format!("/tmp/.X11-unix/X{}", display.trim_start_matches(':'));
            UnixStream::connect_addr(&SocketAddr::from_abstract_name(name).unwrap())
        }
    }
    .expect("connect to the session's socket");
    println!("client: connected as {role}");
    stream
        .write_all(format!("hello from {role}").as_bytes())
        .unwrap();
    let mut got = Vec::new();
    let read = stream.read_to_end(&mut got);
    if role == "linger" {
        // Closed with the hello still unread, the socket resets rather than
        // ending cleanly: either is the agent hanging up.
        println!("client: the agent hung up");
        std::thread::sleep(Duration::from_secs(60));
    }
    read.unwrap();
    // Echo what the host said, so the host→guest direction is proven too.
    eprintln!("client: host said {}", String::from_utf8_lossy(&got));
}

/// The argv that re-enters this binary as [`guest_client`].
fn client_argv() -> Vec<String> {
    vec![
        std::env::current_exe().unwrap().display().to_string(),
        "app_test::guest_client".into(),
        "--exact".into(),
        "--nocapture".into(),
        "-q".into(),
    ]
}

/// A Linux platform whose app sessions live under `tmp`.
fn platform(tmp: &Path) -> impl Platform {
    crate::platform_impl::new_platform().with_app_paths(
        tmp.join("run").to_str().unwrap(),
        tmp.join("x11").to_str().unwrap(),
    )
}

fn send(mux: &Mux, p: &dyn Platform, msg: HostMsg) {
    let frame = Frame {
        kind: FrameKind::Ctrl,
        channel: 0,
        payload: serde_json::to_vec(&msg).unwrap(),
    };
    mux.handle_frame(frame, p);
}

fn open_app(mode: AppMode, argv: Vec<String>, env: Vec<(&str, String)>) -> HostMsg {
    HostMsg::OpenApp {
        id: 1,
        mode,
        argv,
        env: env.into_iter().map(|(k, v)| (k.to_string(), v)).collect(),
        cwd: None,
        logon: None,
    }
}

/// The host's end of the wire: control messages queued in arrival order,
/// each channel's bytes kept apart, the app channel's decoded into records.
struct Host {
    cap: Capture,
    ctrl: VecDeque<AgentMsg>,
    bytes: HashMap<u32, Vec<u8>>,
    records: RecordDecoder,
}

impl Host {
    fn new(cap: Capture) -> Host {
        Host {
            cap,
            ctrl: VecDeque::new(),
            bytes: HashMap::new(),
            records: RecordDecoder::new(),
        }
    }

    fn pull(&mut self) {
        let f = self.cap.frame();
        match f.kind {
            FrameKind::Ctrl => self
                .ctrl
                .push_back(serde_json::from_slice(&f.payload).unwrap()),
            FrameKind::Data | FrameKind::DataErr if f.channel == 1 => self.records.push(&f.payload),
            _ => self.bytes.entry(f.channel).or_default().extend(f.payload),
        }
    }

    /// The first control message `want` accepts. The ones before it stay
    /// queued: an `exited` may overtake the `eof` a test waits for first.
    fn ctrl(&mut self, want: impl Fn(&AgentMsg) -> bool) -> AgentMsg {
        loop {
            if let Some(at) = self.ctrl.iter().position(&want) {
                return self.ctrl.remove(at).unwrap();
            }
            self.pull();
        }
    }

    /// The app channel's next record that is not a log line.
    fn record(&mut self) -> AppRecord {
        loop {
            while let Some(record) = self.records.next_record().unwrap() {
                if !matches!(record, AppRecord::Log { .. }) {
                    return record;
                }
            }
            self.pull();
        }
    }

    /// Wait for the log line `text`.
    fn log(&mut self, text: &str) {
        loop {
            while let Some(record) = self.records.next_record().unwrap() {
                if record == (AppRecord::Log { text: text.into() }) {
                    return;
                }
            }
            self.pull();
        }
    }

    /// Wait until `needle` has arrived on connection channel `id`.
    fn bytes_until(&mut self, id: u32, needle: &[u8]) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !self
            .bytes
            .get(&id)
            .is_some_and(|b| b.windows(needle.len()).any(|w| w == needle))
        {
            assert!(Instant::now() < deadline, "timed out waiting on {id}");
            self.pull();
        }
    }
}

/// The one private directory the session made under `run`.
fn private_dir(run: &Path) -> PathBuf {
    let mut dirs: Vec<_> = std::fs::read_dir(run)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(dirs.len(), 1, "one session, one directory: {dirs:?}");
    dirs.pop().unwrap()
}

/// libXau's reading of an Xauthority file: (family, address, number, name,
/// data) per entry.
type XauEntry = (u16, Vec<u8>, String, String, Vec<u8>);

fn parse_xauthority(mut bytes: &[u8]) -> Vec<XauEntry> {
    fn u16_be(bytes: &mut &[u8]) -> u16 {
        let v = u16::from_be_bytes([bytes[0], bytes[1]]);
        *bytes = &bytes[2..];
        v
    }
    fn field(bytes: &mut &[u8]) -> Vec<u8> {
        let len = u16_be(bytes) as usize;
        let v = bytes[..len].to_vec();
        *bytes = &bytes[len..];
        v
    }
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let family = u16_be(&mut bytes);
        let address = field(&mut bytes);
        let number = String::from_utf8(field(&mut bytes)).unwrap();
        let name = String::from_utf8(field(&mut bytes)).unwrap();
        let data = field(&mut bytes);
        out.push((family, address, number, name, data));
    }
    out
}

/// A program the X11 session starts reaches its display through `$DISPLAY`
/// alone: the host is told the display, then that a connection is waiting,
/// adopts it, and the bytes cross both ways. The program's exit ends the
/// session and takes the display's socket and the private directory with it.
#[test]
fn an_x11_program_connects_and_its_bytes_reach_the_host() {
    let tmp = tempfile::tempdir().unwrap();
    let p = platform(tmp.path());
    let (mux, cap) = capture_mux();
    let mut host = Host::new(cap);
    send(
        &mux,
        &p,
        open_app(
            AppMode::X11 {
                cookie: COOKIE.into(),
            },
            client_argv(),
            vec![("VMLAB_APP_TEST_ROLE", "x11".into())],
        ),
    );
    assert_eq!(host.ctrl(|_| true), AgentMsg::Opened { id: 1 });
    let AppRecord::Ready {
        display: Some(display),
    } = host.record()
    else {
        panic!("an X11 session names its display");
    };
    let n: u32 = display.strip_prefix(':').unwrap().parse().unwrap();
    assert!(n >= 10, "displays below :10 are the guest's own: {display}");
    let path_socket = tmp.path().join(format!("x11/X{n}"));
    assert!(path_socket.exists(), "the path socket is bound too");
    let dir = private_dir(&tmp.path().join("run"));
    assert_eq!(
        std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
        0o700
    );

    assert_eq!(host.record(), AppRecord::Pending);
    send(&mux, &p, HostMsg::OpenAppConn { id: 2, app: 1 });
    assert_eq!(
        host.ctrl(|m| matches!(
            m,
            AgentMsg::Opened { id: 2 } | AgentMsg::Error { id: Some(2), .. }
        )),
        AgentMsg::Opened { id: 2 }
    );
    host.bytes_until(2, b"hello from x11");
    mux.route_input(2, Input::Bytes(b"bye".to_vec()));
    mux.route_input(2, Input::Eof);
    host.log("client: host said bye");
    host.ctrl(|m| *m == AgentMsg::Eof { id: 2 });
    assert_eq!(
        host.ctrl(|m| matches!(m, AgentMsg::Exited { .. })),
        AgentMsg::Exited { id: 1, code: 0 }
    );
    assert!(
        !path_socket.exists(),
        "the display's path socket is unlinked"
    );
    assert!(!dir.exists(), "the private directory is removed");
}

/// The Xauthority file a session writes is one libXau reads back to the
/// host's cookie, for this host by name and for any address, and only its
/// owner may read it. Closing the session kills the program and removes it.
#[test]
fn the_xauthority_file_holds_the_cookie_twice_and_only_for_its_owner() {
    let tmp = tempfile::tempdir().unwrap();
    let p = platform(tmp.path());
    let (mux, cap) = capture_mux();
    let mut host = Host::new(cap);
    send(
        &mux,
        &p,
        open_app(
            AppMode::X11 {
                cookie: COOKIE.into(),
            },
            vec!["/bin/sh".into(), "-c".into(), "sleep 60".into()],
            vec![],
        ),
    );
    let AppRecord::Ready {
        display: Some(display),
    } = host.record()
    else {
        panic!("an X11 session names its display");
    };
    let file = private_dir(&tmp.path().join("run")).join("Xauthority");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let entries = parse_xauthority(&std::fs::read(&file).unwrap());
    let number = display.trim_start_matches(':').to_string();
    let cookie: Vec<u8> = (0..16)
        .map(|i| u8::from_str_radix(&COOKIE[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    let hostname = p.os_info().unwrap().hostname.into_bytes();
    assert_eq!(
        entries,
        vec![
            (
                FAMILY_LOCAL,
                hostname,
                number.clone(),
                "MIT-MAGIC-COOKIE-1".into(),
                cookie.clone()
            ),
            (
                FAMILY_WILD,
                vec![],
                number,
                "MIT-MAGIC-COOKIE-1".into(),
                cookie
            ),
        ]
    );

    send(&mux, &p, HostMsg::Close { id: 1 });
    assert_eq!(
        host.ctrl(|m| matches!(m, AgentMsg::Exited { .. })),
        AgentMsg::Exited {
            id: 1,
            code: 128 + 9
        }
    );
    assert!(!file.exists(), "closing removes the private directory");
}

/// Closing a session kills what the program started as well as the program:
/// a child left holding the program's output would keep the session from
/// ever reporting its end, and would run on in the guest.
#[test]
fn closing_a_session_kills_the_programs_children_too() {
    let tmp = tempfile::tempdir().unwrap();
    let p = platform(tmp.path());
    let (mux, cap) = capture_mux();
    let mut host = Host::new(cap);
    let pidfile = tmp.path().join("child.pid");
    send(
        &mux,
        &p,
        open_app(
            AppMode::X11 {
                cookie: COOKIE.into(),
            },
            vec![
                "/bin/sh".into(),
                "-c".into(),
                format!("sleep 60 & echo $! > {}; wait", pidfile.display()),
            ],
            vec![],
        ),
    );
    assert!(matches!(host.record(), AppRecord::Ready { .. }));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let child: u32 = loop {
        if let Some(pid) = std::fs::read_to_string(&pidfile)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            break pid;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the child never started"
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    send(&mux, &p, HostMsg::Close { id: 1 });
    assert!(matches!(
        host.ctrl(|m| matches!(m, AgentMsg::Exited { .. })),
        AgentMsg::Exited { id: 1, .. }
    ));
    let gone = |pid: u32| match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        // A zombie is dead, waiting only for whoever adopted it to reap it.
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .is_some_and(|rest| rest.trim_start().starts_with('Z')),
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !gone(child) {
        assert!(
            std::time::Instant::now() < deadline,
            "the program's child survived the close"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The encoding itself, against bytes written out by hand from libXau's
/// format, so the parser above is not only checked against itself.
#[test]
fn an_xauthority_entry_is_libxaus_big_endian_layout() {
    let bytes = xauthority("h", 12, &[0xab; 16]);
    let mut want = vec![0x01, 0x00, 0, 1, b'h', 0, 2, b'1', b'2', 0, 18];
    want.extend_from_slice(b"MIT-MAGIC-COOKIE-1");
    want.extend_from_slice(&[0, 16]);
    want.extend_from_slice(&[0xab; 16]);
    want.extend_from_slice(&[0xff, 0xff, 0, 0, 0, 2, b'1', b'2', 0, 18]);
    want.extend_from_slice(b"MIT-MAGIC-COOKIE-1");
    want.extend_from_slice(&[0, 16]);
    want.extend_from_slice(&[0xab; 16]);
    assert_eq!(bytes, want);
}

/// A waypipe session runs `waypipe server` on the session's private
/// socket, compressed the way the host's client will be, with the program
/// after `--`; each connection waypipe makes reaches the host like an X
/// client's.
#[test]
fn a_waypipe_session_runs_waypipe_server_and_carries_its_connection() {
    let tmp = tempfile::tempdir().unwrap();
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let argv_file = tmp.path().join("argv");
    let fake = bin.join("waypipe");
    // Records its argv, then runs the guest client in the program's place,
    // connecting to `--socket` the way `waypipe server` does.
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$VMLAB_APP_TEST_ARGV\"\n\
             for a; do [ \"$prev\" = --socket ] && sock=$a; prev=$a; done\n\
             VMLAB_APP_TEST_SOCKET=$sock exec {}\n",
            client_argv()
                .iter()
                .map(|a| format!("'{a}'"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    )
    .unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();

    let p = platform(tmp.path());
    let (mux, cap) = capture_mux();
    let mut host = Host::new(cap);
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap());
    send(
        &mux,
        &p,
        open_app(
            AppMode::Waypipe,
            vec!["gedit".into(), "--new-window".into()],
            vec![
                ("PATH", path),
                ("VMLAB_APP_TEST_ROLE", "waypipe".into()),
                ("VMLAB_APP_TEST_ARGV", argv_file.display().to_string()),
            ],
        ),
    );
    assert_eq!(host.ctrl(|_| true), AgentMsg::Opened { id: 1 });
    assert_eq!(host.record(), AppRecord::Ready { display: None });
    let dir = private_dir(&tmp.path().join("run"));
    let sock = dir.join("wp.sock");
    assert_eq!(
        std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777,
        0o600
    );

    assert_eq!(host.record(), AppRecord::Pending);
    let seen = dir.display().to_string();
    assert_eq!(
        std::fs::read_to_string(&argv_file).unwrap(),
        [
            "--compress",
            "lz4",
            "--socket",
            &format!("{seen}/wp.sock"),
            "--display",
            &format!("{seen}/wayland-vmlab"),
            "server",
            "--",
            "gedit",
            "--new-window",
        ]
        .map(|a| format!("{a}\n"))
        .concat(),
        "waypipe gets exactly this argv"
    );
    send(&mux, &p, HostMsg::OpenAppConn { id: 2, app: 1 });
    assert_eq!(
        host.ctrl(|m| matches!(
            m,
            AgentMsg::Opened { id: 2 } | AgentMsg::Error { id: Some(2), .. }
        )),
        AgentMsg::Opened { id: 2 }
    );
    host.bytes_until(2, b"hello from waypipe");
    mux.route_input(2, Input::Bytes(b"bye".to_vec()));
    mux.route_input(2, Input::Eof);
    host.log("client: host said bye");
    assert_eq!(
        host.ctrl(|m| matches!(m, AgentMsg::Exited { .. })),
        AgentMsg::Exited { id: 1, code: 0 }
    );
    assert!(!dir.exists(), "the private directory is removed");
}

/// `XDG_RUNTIME_DIR` is the login's to set; the wrapper only fills it in
/// where the login left it unset, and hands waypipe its argv untouched.
#[test]
fn the_waypipe_wrapper_fills_in_xdg_runtime_dir_only_when_unset() {
    let argv = waypipe_argv("/run/vmlab-app/9-1", vec!["printenv".into()]);
    let run = |env: &[(&str, &str)]| {
        let mut argv = argv.clone();
        // Stand `env` in for waypipe, which is what the wrapper execs.
        let at = argv.iter().position(|a| a == "waypipe").unwrap();
        argv.truncate(at);
        argv.extend([
            "/bin/sh".into(),
            "-c".into(),
            "echo $XDG_RUNTIME_DIR".into(),
        ]);
        let mut cmd = std::process::Command::new(&argv[0]);
        cmd.args(&argv[1..]).env_remove("XDG_RUNTIME_DIR");
        for (k, v) in env {
            cmd.env(k, v);
        }
        String::from_utf8(cmd.output().unwrap().stdout).unwrap()
    };
    assert_eq!(run(&[]), "/run/vmlab-app/9-1\n");
    assert_eq!(
        run(&[("XDG_RUNTIME_DIR", "/run/user/1000")]),
        "/run/user/1000\n"
    );
}

/// A connection the host never adopts is closed once the grace is up, and
/// an `open_app_conn` after that has nothing to adopt — while the session
/// itself lives on.
#[test]
fn a_parked_connection_past_the_grace_is_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let p = platform(tmp.path());
    let (mux, cap) = capture_mux();
    mux.apps()
        .tune(Duration::from_millis(200), vmlab_agent_proto::app::RDP_PORT);
    let mut host = Host::new(cap);
    send(
        &mux,
        &p,
        open_app(
            AppMode::X11 {
                cookie: COOKIE.into(),
            },
            client_argv(),
            vec![("VMLAB_APP_TEST_ROLE", "linger".into())],
        ),
    );
    assert!(matches!(host.record(), AppRecord::Ready { .. }));
    assert_eq!(host.record(), AppRecord::Pending);
    host.log("client: the agent hung up");
    send(&mux, &p, HostMsg::OpenAppConn { id: 2, app: 1 });
    match host.ctrl(|m| {
        matches!(
            m,
            AgentMsg::Opened { id: 2 } | AgentMsg::Error { id: Some(2), .. }
        )
    }) {
        AgentMsg::Error { id: Some(2), msg } => {
            assert!(msg.contains("no parked connection"), "{msg}")
        }
        other => panic!("expected the open to fail, got {other:?}"),
    }
    send(&mux, &p, HostMsg::Close { id: 1 });
    host.ctrl(|m| matches!(m, AgentMsg::Exited { id: 1, .. }));
}

/// An `open_app_conn` naming no live session fails, rather than hanging.
#[test]
fn a_connection_to_no_session_is_refused() {
    let (mux, cap) = capture_mux();
    let mut host = Host::new(cap);
    send(
        &mux,
        &TestPlatform::new(),
        HostMsg::OpenAppConn { id: 2, app: 9 },
    );
    match host.ctrl(|_| true) {
        AgentMsg::Error { id: Some(2), msg } => assert!(msg.contains("no app session 9"), "{msg}"),
        other => panic!("expected an error, got {other:?}"),
    }
}

fn logon() -> Option<Logon> {
    Some(Logon {
        user: "dev".into(),
        secret: "pw".into(),
        elevated: false,
    })
}

/// RDP logs an account on, so an open without one is refused; with one,
/// every connection is a dial to the guest's own RDP listener, and each
/// direction closes on its own.
#[test]
fn rdp_dials_the_local_listener_and_refuses_no_account() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let p = TestPlatform::new();
    let (mux, cap) = capture_mux();
    mux.apps().tune(Duration::from_secs(30), port);
    let mut host = Host::new(cap);

    send(&mux, &p, open_app(AppMode::Rdp, vec![], vec![]));
    assert_eq!(
        host.ctrl(|_| true),
        AgentMsg::Error {
            id: Some(1),
            msg: "RDP needs an account".into()
        }
    );

    let mut open = open_app(AppMode::Rdp, vec![], vec![]);
    if let HostMsg::OpenApp { logon: l, .. } = &mut open {
        *l = logon();
    }
    send(&mux, &p, open);
    assert_eq!(host.ctrl(|_| true), AgentMsg::Opened { id: 1 });
    assert_eq!(host.record(), AppRecord::Ready { display: None });
    // The connection that proved the listener up, before `ready`.
    drop(listener.accept().unwrap());

    send(&mux, &p, HostMsg::OpenAppConn { id: 2, app: 1 });
    let (mut server, _) = listener.accept().unwrap();
    assert_eq!(host.ctrl(|_| true), AgentMsg::Opened { id: 2 });
    server.write_all(b"rdp server hello").unwrap();
    host.bytes_until(2, b"rdp server hello");
    mux.route_input(2, Input::Bytes(b"client hello".to_vec()));
    mux.route_input(2, Input::Eof);
    let mut got = Vec::new();
    server.read_to_end(&mut got).unwrap();
    assert_eq!(got, b"client hello", "a host EOF half-closes the socket");
    drop(server);
    assert_eq!(host.ctrl(|_| true), AgentMsg::Eof { id: 2 });

    // The session lasts until the host closes it, and then is gone.
    send(&mux, &p, HostMsg::Close { id: 1 });
    send(&mux, &p, HostMsg::OpenAppConn { id: 3, app: 1 });
    assert!(matches!(
        host.ctrl(|_| true),
        AgentMsg::Error { id: Some(3), .. }
    ));
}

/// Turning Remote Desktop on leaves the guest's listener refusing
/// connections for a while (about 40 s on Windows Server 2022), and the host
/// starts its RDP client on `ready`. So `ready` waits for the listener,
/// saying once why it waits.
#[test]
fn rdp_is_ready_only_once_the_listener_accepts() {
    let port = {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let p = TestPlatform::new();
    let (mux, cap) = capture_mux();
    mux.apps().tune(Duration::from_secs(30), port);
    let mut host = Host::new(cap);
    let mut open = open_app(AppMode::Rdp, vec![], vec![]);
    if let HostMsg::OpenApp { logon: l, .. } = &mut open {
        *l = logon();
    }
    send(&mux, &p, open);
    assert_eq!(host.ctrl(|_| true), AgentMsg::Opened { id: 1 });
    host.log(&format!(
        "waiting for Remote Desktop to listen on 127.0.0.1:{port}"
    ));

    let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
    assert_eq!(host.record(), AppRecord::Ready { display: None });
    drop(listener);
}

/// A listener that never comes up fails the session once the wait runs out,
/// rather than leaving the host waiting on a `ready` that cannot come.
#[test]
fn rdp_with_no_listener_fails_the_session() {
    let port = {
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        probe.local_addr().unwrap().port()
    };
    let p = TestPlatform::new();
    let (mux, cap) = capture_mux();
    mux.apps().tune(Duration::from_secs(30), port);
    mux.apps().tune_listener(Duration::from_millis(500));
    let mut host = Host::new(cap);
    let mut open = open_app(AppMode::Rdp, vec![], vec![]);
    if let HostMsg::OpenApp { logon: l, .. } = &mut open {
        *l = logon();
    }
    send(&mux, &p, open);
    assert_eq!(host.ctrl(|_| true), AgentMsg::Opened { id: 1 });
    assert_eq!(
        host.ctrl(|m| matches!(m, AgentMsg::Error { .. })),
        AgentMsg::Error {
            id: Some(1),
            msg: format!("app: Remote Desktop is not listening on 127.0.0.1:{port} after 500ms"),
        }
    );
}

/// A guest that cannot serve RemoteApp fails the open with its reason; a
/// Linux guest is one.
#[test]
fn rdp_on_a_guest_that_cannot_serve_it_fails_the_open() {
    let (mux, cap) = capture_mux();
    let mut host = Host::new(cap);
    let mut open = open_app(AppMode::Rdp, vec![], vec![]);
    if let HostMsg::OpenApp { logon: l, .. } = &mut open {
        *l = logon();
    }
    let tmp = tempfile::tempdir().unwrap();
    send(&mux, &platform(tmp.path()), open);
    assert_eq!(
        host.ctrl(|_| true),
        AgentMsg::Error {
            id: Some(1),
            msg: "shared apps over RDP need a Windows guest".into()
        }
    );
}
