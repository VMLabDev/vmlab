//! Shared apps (PRD §20): one guest program's windows on the host desktop,
//! carried in the guest family's own window protocol (ADR-0016).
//!
//! An [`HostMsg::OpenApp`](vmlab_agent_proto::HostMsg::OpenApp) channel
//! carries length-prefixed [`AppRecord`]s, and every transport connection of
//! the protocol is a channel of its own, opened by the host with
//! [`HostMsg::OpenAppConn`](vmlab_agent_proto::HostMsg::OpenAppConn).
//!
//! Waypipe and X11 clients connect *out* of the guest, which ADR-0013 gives
//! no channel open. So the agent listens on the guest's behalf, parks every
//! connection it accepts, and announces it with one
//! [`Pending`](AppRecord::Pending); the host's `OpenAppConn` adopts the
//! oldest one. A connection nobody adopts within the grace is closed. RDP
//! runs the other way: each `OpenAppConn` dials the guest's own RDP listener
//! on loopback, and nothing else.
//!
//! The listening half needs unix sockets and is Linux's; the platform makes
//! the session's private directory, because only it knows whose it is and
//! where a container's root filesystem starts. The rest — the session table,
//! the records, the connection pumps — is the same on both guest targets.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use vmlab_agent_proto::app::{self, AppMode, AppRecord};
use vmlab_agent_proto::{AgentMsg, FrameKind, Logon, RecvWindow};

use crate::mux::{Credit, Input, Mux, Platform, pump_out};

/// Total time an RDP connection's dial may take. Under the host's wait for
/// `opened`, so a guest with no RDP listener comes back as an error naming
/// why rather than as the host giving up on the channel.
const DIAL_BUDGET: Duration = Duration::from_secs(10);

/// What the open asked to run, for the modes that run something.
#[cfg_attr(not(unix), allow(dead_code))]
pub struct AppSpec {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: Option<String>,
}

/// The knobs a test turns. Production runs on the contract's own values.
#[derive(Clone, Copy)]
struct Tuning {
    /// How long an accepted connection waits for the host to adopt it.
    #[cfg_attr(not(unix), allow(dead_code))]
    grace: Duration,
    /// Where an RDP connection is dialled, on the guest's loopback.
    rdp_port: u16,
}

impl Default for Tuning {
    fn default() -> Tuning {
        Tuning {
            grace: Duration::from_secs(app::PENDING_GRACE_SECS),
            rdp_port: app::RDP_PORT,
        }
    }
}

/// Every live app session on one mux, by the channel id it was opened on.
///
/// The connection channels a session spawns are not in here: once adopted
/// they are ordinary channels in the mux, and outlive the session that
/// parked them until their own close or EOF.
#[derive(Default)]
pub struct Apps {
    sessions: Mutex<HashMap<u32, Arc<AppSession>>>,
    tuning: Mutex<Tuning>,
}

impl Apps {
    /// Replace the grace and the RDP port, so a test neither waits 30 s nor
    /// needs port 3389.
    #[cfg(test)]
    pub fn tune(&self, grace: Duration, rdp_port: u16) {
        *self.tuning.lock().unwrap() = Tuning { grace, rdp_port };
    }

    fn tuning(&self) -> Tuning {
        *self.tuning.lock().unwrap()
    }

    fn get(&self, id: u32) -> Option<Arc<AppSession>> {
        self.sessions.lock().unwrap().get(&id).cloned()
    }

    /// Add a session, unless the id is already one.
    fn insert(&self, id: u32, session: AppSession) -> bool {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.contains_key(&id) {
            return false;
        }
        sessions.insert(id, Arc::new(session));
        true
    }

    /// End a session: take it out of the table and release what it holds.
    /// Idempotent, because both the program's exit and the host's close
    /// arrive here.
    fn end(&self, id: u32) {
        let session = self.sessions.lock().unwrap().remove(&id);
        if let Some(session) = session {
            session.close();
        }
    }
}

/// One app session, as `OpenAppConn` needs to find it.
enum AppSession {
    /// Connections are dialled on demand; there is nothing to hold.
    Rdp,
    /// Connections are accepted, parked and adopted.
    #[cfg(unix)]
    Parking(Arc<parking::Endpoint>),
}

impl AppSession {
    fn close(&self) {
        match self {
            AppSession::Rdp => {}
            #[cfg(unix)]
            AppSession::Parking(endpoint) => endpoint.close(),
        }
    }
}

/// `HostMsg::OpenApp`.
pub fn open(
    mux: &Mux,
    platform: &dyn Platform,
    id: u32,
    mode: AppMode,
    spec: AppSpec,
    logon: Option<Logon>,
) {
    match mode {
        AppMode::Rdp => open_rdp(mux, platform, id, logon),
        #[cfg(unix)]
        AppMode::Waypipe => {
            parking::open(mux, platform, id, parking::Protocol::Waypipe, spec, logon)
        }
        #[cfg(unix)]
        AppMode::X11 { cookie } => match app::valid_cookie(&cookie) {
            true => parking::open(
                mux,
                platform,
                id,
                parking::Protocol::X11 { cookie },
                spec,
                logon,
            ),
            false => mux.send_error(Some(id), "app: an X11 cookie is 32 lowercase hex digits"),
        },
        #[cfg(not(unix))]
        AppMode::Waypipe | AppMode::X11 { .. } => {
            let _ = spec;
            mux.send_error(
                Some(id),
                "shared apps over waypipe and X11 need a Linux guest",
            )
        }
    }
}

/// The RDP half: make sure the guest can serve the account a RemoteApp
/// session, then hold the session open until the host closes it. It starts
/// no process — the RDP client names the program.
fn open_rdp(mux: &Mux, platform: &dyn Platform, id: u32, logon: Option<Logon>) {
    let Some(logon) = logon else {
        mux.send_error(Some(id), "RDP needs an account");
        return;
    };
    if let Err(e) = platform.app_rdp_prepare(&logon) {
        mux.send_error(Some(id), e);
        return;
    }
    if !mux.apps().insert(id, AppSession::Rdp) {
        mux.send_error(Some(id), "channel id already in use");
        return;
    }
    let end_mux = mux.clone();
    let Some((input, credit)) =
        mux.register(id, None, Some(Box::new(move || end_mux.apps().end(id))))
    else {
        mux.apps().end(id);
        return;
    };
    mux.send_ctrl(&AgentMsg::Opened { id });
    Records::new(mux, id, credit).send(&AppRecord::Ready { display: None });
    drain(mux, id, input);
}

/// `HostMsg::OpenAppConn`: one transport connection of session `app`, as
/// channel `id`.
pub fn open_conn(mux: &Mux, id: u32, app: u32) {
    let Some(session) = mux.apps().get(app) else {
        mux.send_error(Some(id), format!("app: no app session {app}"));
        return;
    };
    match &*session {
        AppSession::Rdp => dial_rdp(mux, id, mux.apps().tuning().rdp_port),
        #[cfg(unix)]
        AppSession::Parking(endpoint) => match endpoint.adopt(mux.apps().tuning().grace) {
            Some(stream) => match Conn::unix(stream) {
                Ok(conn) => serve(mux, id, conn),
                Err(e) => mux.send_error(Some(id), format!("app: cannot share the socket: {e}")),
            },
            None => mux.send_error(
                Some(id),
                format!("app: app session {app} has no parked connection"),
            ),
        },
    }
}

/// The app channel's record stream. Several threads write to it — the
/// listener announcing connections, a pump per output stream logging — and
/// a record must not interleave with another, so they take turns.
#[derive(Clone)]
struct Records {
    mux: Mux,
    id: u32,
    credit: Arc<Credit>,
    turn: Arc<Mutex<()>>,
}

impl Records {
    fn new(mux: &Mux, id: u32, credit: Arc<Credit>) -> Records {
        Records {
            mux: mux.clone(),
            id,
            credit,
            turn: Arc::new(Mutex::new(())),
        }
    }

    /// Send one record within the channel's credit. `false` once the
    /// channel has closed.
    fn send(&self, record: &AppRecord) -> bool {
        let bytes = app::encode_record(record);
        let _turn = self.turn.lock().unwrap();
        let mut off = 0;
        while off < bytes.len() {
            let take = self.credit.take(bytes.len() - off);
            if take == 0 {
                return false;
            }
            self.mux
                .send_data(FrameKind::Data, self.id, &bytes[off..off + take]);
            off += take;
        }
        true
    }
}

/// The host writes nothing an app channel acts on; consume it anyway, so a
/// host that does cannot stall on its window.
fn drain(mux: &Mux, id: u32, input: std::sync::mpsc::Receiver<Input>) {
    let mux = mux.clone();
    thread::spawn(move || {
        let mut window = RecvWindow::default();
        for chunk in input {
            if let Input::Bytes(b) = chunk
                && let Some(grant) = window.recv(b.len())
            {
                mux.send_ctrl(&AgentMsg::WindowAdjust { id, bytes: grant });
            }
        }
    });
}

// ---- connection channels ----------------------------------------------------

/// One transport connection, split so each direction can run on its own
/// thread and half-close on its own.
struct Conn {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    /// Shut a direction of the socket down: the write half for a host EOF,
    /// both for a host close.
    shutdown: Arc<dyn Fn(Shutdown) + Send + Sync>,
}

impl Conn {
    fn tcp(stream: TcpStream) -> std::io::Result<Conn> {
        let (reader, writer) = (stream.try_clone()?, stream.try_clone()?);
        Ok(Conn {
            reader: Box::new(reader),
            writer: Box::new(writer),
            shutdown: Arc::new(move |how| {
                let _ = stream.shutdown(how);
            }),
        })
    }

    #[cfg(unix)]
    fn unix(stream: std::os::unix::net::UnixStream) -> std::io::Result<Conn> {
        let (reader, writer) = (stream.try_clone()?, stream.try_clone()?);
        Ok(Conn {
            reader: Box::new(reader),
            writer: Box::new(writer),
            shutdown: Arc::new(move |how| {
                let _ = stream.shutdown(how);
            }),
        })
    }
}

/// The socket a connection channel is carrying, shared with its kill hook so
/// a host `close` interrupts a blocked dial or a blocked read. `closed` lives
/// under the same lock as the socket: a close racing the dial must find
/// either the socket to shut down or the flag that stops the dial publishing
/// one.
#[derive(Default)]
struct Shared {
    shutdown: Option<Arc<dyn Fn(Shutdown) + Send + Sync>>,
    closed: bool,
}

/// Register channel `id` with a kill hook that shuts down whatever socket
/// `shared` ends up holding.
fn register_conn(
    mux: &Mux,
    id: u32,
    shared: &Arc<Mutex<Shared>>,
) -> Option<(std::sync::mpsc::Receiver<Input>, Arc<Credit>)> {
    let kill_shared = shared.clone();
    mux.register(
        id,
        None,
        Some(Box::new(move || {
            let mut s = kill_shared.lock().unwrap();
            s.closed = true;
            if let Some(shutdown) = s.shutdown.take() {
                shutdown(Shutdown::Both);
            }
        })),
    )
}

/// Carry an adopted connection as channel `id`.
#[cfg(unix)]
fn serve(mux: &Mux, id: u32, conn: Conn) {
    let shared = Arc::new(Mutex::new(Shared {
        shutdown: Some(conn.shutdown.clone()),
        closed: false,
    }));
    let Some((input, credit)) = register_conn(mux, id, &shared) else {
        (conn.shutdown)(Shutdown::Both);
        return;
    };
    mux.send_ctrl(&AgentMsg::Opened { id });
    let mux = mux.clone();
    thread::spawn(move || pump_conn(&mux, id, conn, input, credit, shared));
}

/// Dial the guest's own RDP listener as channel `id`. The channel is
/// registered before the dial, not after: a host that gives up while the
/// agent is still connecting sends `close`, and the kill hook has to be in
/// place to catch it. `opened` follows only once connected.
fn dial_rdp(mux: &Mux, id: u32, port: u16) {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let Some((input, credit)) = register_conn(mux, id, &shared) else {
        return;
    };
    let mux = mux.clone();
    thread::spawn(move || {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let conn = match TcpStream::connect_timeout(&addr, DIAL_BUDGET).and_then(|stream| {
            // Interactive traffic: a forwarded keystroke must not wait for
            // Nagle to fill a segment.
            let _ = stream.set_nodelay(true);
            Conn::tcp(stream)
        }) {
            Ok(conn) => conn,
            Err(e) => {
                if !shared.lock().unwrap().closed {
                    mux.send_error(Some(id), format!("app: dial {addr}: {e}"));
                }
                mux.remove_finished(id);
                return;
            }
        };
        {
            let mut s = shared.lock().unwrap();
            if s.closed {
                (conn.shutdown)(Shutdown::Both);
                return; // the host gave up while we were dialling
            }
            s.shutdown = Some(conn.shutdown.clone());
        }
        mux.send_ctrl(&AgentMsg::Opened { id });
        pump_conn(&mux, id, conn, input, credit, shared);
    });
}

/// Both directions of a connection channel. Each half-closes on its own:
/// the socket reaching EOF sends `AgentMsg::Eof` and leaves the channel
/// open for host→guest bytes; a host `Eof` shuts the socket's write half
/// and leaves it open for guest→host bytes. The channel goes once both
/// halves are done, or as soon as the host closes it.
fn pump_conn(
    mux: &Mux,
    id: u32,
    conn: Conn,
    input: std::sync::mpsc::Receiver<Input>,
    credit: Arc<Credit>,
    shared: Arc<Mutex<Shared>>,
) {
    let Conn {
        reader,
        mut writer,
        shutdown,
    } = conn;
    let halves = Arc::new(AtomicU8::new(0));
    {
        let (mux, halves) = (mux.clone(), halves.clone());
        thread::spawn(move || {
            pump_out(&mux, id, FrameKind::Data, &credit, reader);
            if !shared.lock().unwrap().closed {
                mux.send_ctrl(&AgentMsg::Eof { id });
            }
            finish_half(&mux, id, &halves);
        });
    }
    let mut window = RecvWindow::default();
    for msg in input {
        match msg {
            Input::Bytes(bytes) => {
                if writer.write_all(&bytes).is_err() {
                    break; // the peer is gone; the read half reports it
                }
                if let Some(grant) = window.recv(bytes.len()) {
                    mux.send_ctrl(&AgentMsg::WindowAdjust { id, bytes: grant });
                }
            }
            Input::Eof => {
                shutdown(Shutdown::Write);
                break;
            }
        }
    }
    finish_half(mux, id, &halves);
}

/// Retire one direction; the second to arrive drops the channel.
fn finish_half(mux: &Mux, id: u32, halves: &AtomicU8) {
    if halves.fetch_add(1, Ordering::SeqCst) == 1 {
        mux.remove_finished(id);
    }
}

// ---- the parking modes: waypipe and X11 -------------------------------------

/// Where a waypipe or X11 session's guest end lives, as the platform made it.
#[cfg(unix)]
pub struct AppHome {
    /// The session's private directory as the agent opens it. The platform
    /// has created it, mode 0700, owned by [`owner`](Self::owner).
    pub dir: std::path::PathBuf,
    /// The same directory as the program sees it — different inside a
    /// container, whose root filesystem the agent reaches from outside.
    pub seen: String,
    /// The X socket directory, `/tmp/.X11-unix`, as the agent opens it.
    pub x11_unix: std::path::PathBuf,
    /// Who owns the directory and everything the agent writes into it:
    /// `(uid, gid)`, or `None` where the program runs as the agent itself.
    pub owner: Option<(u32, u32)>,
    /// The guest's hostname, for the Xauthority file's local entry.
    pub hostname: String,
}

#[cfg(unix)]
mod parking {
    use std::collections::VecDeque;
    use std::io::{BufRead, BufReader, Read};
    use std::os::fd::AsFd;
    use std::os::linux::net::SocketAddrExt;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use vmlab_agent_proto::app::AppRecord;
    use vmlab_agent_proto::{AgentMsg, Logon};

    use super::{AppHome, AppSession, AppSpec, Records, drain};
    use crate::mux::{Mux, Platform};
    use crate::spawn::{Identity, ProcessSpec, Spawned};

    /// The longest log line sent as one record; a longer one is split.
    /// Far under the record bound.
    const LOG_LINE_MAX: u64 = 16 * 1024;

    /// The first X display a session may claim. Below it are the displays a
    /// guest's own X servers conventionally take.
    const FIRST_DISPLAY: u32 = 10;

    /// The last one tried before giving up.
    const LAST_DISPLAY: u32 = 1000;

    pub enum Protocol {
        Waypipe,
        X11 { cookie: String },
    }

    /// The guest end of a parking session: its listeners, the connections
    /// waiting for the host, and what to remove when it ends.
    pub struct Endpoint {
        listeners: Vec<UnixListener>,
        parked: Mutex<VecDeque<(Instant, UnixStream)>>,
        closed: AtomicBool,
        dir: PathBuf,
        /// Sockets outside the private directory: the X display's path.
        unlink: Vec<PathBuf>,
    }

    impl Endpoint {
        /// The oldest connection still within the grace.
        pub fn adopt(&self, grace: Duration) -> Option<UnixStream> {
            let mut parked = self.parked.lock().unwrap();
            while let Some((at, stream)) = parked.pop_front() {
                if at.elapsed() <= grace {
                    return Some(stream);
                }
            }
            None
        }

        /// Close every connection parked longer than the grace.
        fn expire(&self, grace: Duration) {
            self.parked
                .lock()
                .unwrap()
                .retain(|(at, _)| at.elapsed() <= grace);
        }

        /// Stop listening, drop what is parked, remove the directory and any
        /// socket outside it. Connections already adopted are channels of
        /// their own and are not touched.
        pub fn close(&self) {
            if self.closed.swap(true, Ordering::SeqCst) {
                return;
            }
            self.parked.lock().unwrap().clear();
            for path in &self.unlink {
                let _ = std::fs::remove_file(path);
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    pub fn open(
        mux: &Mux,
        platform: &dyn Platform,
        id: u32,
        protocol: Protocol,
        spec: AppSpec,
        logon: Option<Logon>,
    ) {
        let Some(exe) = spec.argv.first().cloned() else {
            mux.send_error(Some(id), "app: empty argv");
            return;
        };
        let identity: Identity = logon.into();
        // Named for this agent process as well as the channel: a directory a
        // killed agent left behind is replaced, never shared.
        let name = format!("{}-{id}", std::process::id());
        let home = match platform.app_home(&identity, &name) {
            Ok(home) => home,
            Err(e) => {
                mux.send_error(Some(id), format!("app: {e}"));
                return;
            }
        };
        let endpoint = match listen(&home, &protocol) {
            Ok(endpoint) => endpoint,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&home.dir);
                mux.send_error(Some(id), format!("app: {e}"));
                return;
            }
        };
        let (argv, env, display) = match &protocol {
            Protocol::Waypipe => (waypipe_argv(&home.seen, spec.argv), spec.env, None),
            Protocol::X11 { .. } => {
                let display = endpoint.display.expect("an X11 endpoint has a display");
                let mut env = spec.env;
                env.push(("DISPLAY".into(), format!(":{display}")));
                env.push(("XAUTHORITY".into(), format!("{}/Xauthority", home.seen)));
                (spec.argv, env, Some(format!(":{display}")))
            }
        };
        let endpoint = Arc::new(endpoint.endpoint);
        let Spawned {
            input,
            output,
            errors,
            resize: _,
            kill,
            wait,
        } = match platform.spawner().exec(
            &identity,
            ProcessSpec {
                argv,
                env,
                cwd: spec.cwd,
                tree: None,
            },
        ) {
            Ok(p) => p,
            Err(e) => {
                endpoint.close();
                mux.send_error(Some(id), format!("app {exe}: {e}"));
                return;
            }
        };
        // The program reads nothing from the agent.
        drop(input);

        if !mux.apps().insert(id, AppSession::Parking(endpoint.clone())) {
            kill();
            wait();
            endpoint.close();
            mux.send_error(Some(id), "channel id already in use");
            return;
        }
        // The kill hook fires on host `close`; skip the kill once the program
        // has been reaped (its pid may be recycled).
        let done = Arc::new(AtomicBool::new(false));
        let kill: Arc<dyn Fn() + Send + Sync> = Arc::from(kill);
        let (kill_done, session_kill, end_mux) = (done.clone(), kill.clone(), mux.clone());
        let Some((host_input, credit)) = mux.register(
            id,
            None,
            Some(Box::new(move || {
                if !kill_done.load(Ordering::SeqCst) {
                    session_kill();
                }
                end_mux.apps().end(id);
            })),
        ) else {
            kill();
            wait();
            mux.apps().end(id);
            return;
        };
        mux.send_ctrl(&AgentMsg::Opened { id });
        let records = Records::new(mux, id, credit);
        // `ready` goes before the listener is read, so it precedes every
        // `pending`: connections the program made already wait in the
        // backlog.
        records.send(&AppRecord::Ready { display });
        drain(mux, id, host_input);

        {
            let (endpoint, records) = (endpoint.clone(), records.clone());
            let grace = mux.apps().tuning().grace;
            thread::spawn(move || accept_loop(&endpoint, &records, grace));
        }
        let pumps: Vec<_> = std::iter::once(output)
            .chain(errors)
            .map(|src| {
                let records = records.clone();
                thread::spawn(move || pump_log(&records, src))
            })
            .collect();

        // Reaper: the program's exit is the session's end. Tear the endpoint
        // down first, so a host told `exited` finds nothing left behind.
        let mux = mux.clone();
        thread::spawn(move || {
            let code = wait();
            done.store(true, Ordering::SeqCst);
            mux.apps().end(id);
            for pump in pumps {
                let _ = pump.join();
            }
            mux.send_ctrl(&AgentMsg::Exited { id, code });
            mux.remove_finished(id);
        });
    }

    /// The bound endpoint, and the display it claimed (X11 only).
    struct Bound {
        endpoint: Endpoint,
        display: Option<u32>,
    }

    /// Bind the protocol's listeners and write what the program will read.
    fn listen(home: &AppHome, protocol: &Protocol) -> std::io::Result<Bound> {
        let (listeners, unlink, display) = match protocol {
            Protocol::Waypipe => {
                let path = home.dir.join("wp.sock");
                let listener = UnixListener::bind(&path)?;
                give(&path, home.owner, 0o600)?;
                (vec![listener], vec![], None)
            }
            Protocol::X11 { cookie } => {
                let (n, listeners, path) = claim_display(&home.x11_unix)?;
                let claimed = (|| {
                    give(&path, home.owner, 0o600)?;
                    let file = home.dir.join("Xauthority");
                    std::fs::write(&file, xauthority(&home.hostname, n, &cookie_bytes(cookie)))?;
                    give(&file, home.owner, 0o600)
                })();
                if let Err(e) = claimed {
                    let _ = std::fs::remove_file(&path);
                    return Err(e);
                }
                (listeners, vec![path], Some(n))
            }
        };
        for listener in &listeners {
            listener.set_nonblocking(true)?;
        }
        Ok(Bound {
            endpoint: Endpoint {
                listeners,
                parked: Mutex::new(VecDeque::new()),
                closed: AtomicBool::new(false),
                dir: home.dir.clone(),
                unlink,
            },
            display,
        })
    }

    /// Hand `path` to the session's identity with `mode`.
    fn give(path: &Path, owner: Option<(u32, u32)>, mode: u32) -> std::io::Result<()> {
        if let Some((uid, gid)) = owner {
            std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }

    /// Claim the first display from [`FIRST_DISPLAY`] up whose abstract
    /// *and* path sockets are both free. Both, because a libxcb release
    /// tries the abstract name first and a future one only the path; a
    /// display holding one of the two would let whoever owns the other
    /// capture the program.
    fn claim_display(x11_unix: &Path) -> std::io::Result<(u32, Vec<UnixListener>, PathBuf)> {
        if !x11_unix.is_dir() {
            std::fs::create_dir_all(x11_unix)?;
            std::fs::set_permissions(x11_unix, std::fs::Permissions::from_mode(0o1777))?;
        }
        for n in FIRST_DISPLAY..=LAST_DISPLAY {
            let name = format!("/tmp/.X11-unix/X{n}");
            let Ok(abstract_) = SocketAddr::from_abstract_name(name.as_bytes())
                .and_then(|a| UnixListener::bind_addr(&a))
            else {
                continue;
            };
            let path = x11_unix.join(format!("X{n}"));
            if let Ok(listener) = UnixListener::bind(&path) {
                return Ok((n, vec![abstract_, listener], path));
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            format!("no free X display between :{FIRST_DISPLAY} and :{LAST_DISPLAY}"),
        ))
    }

    /// `waypipe server` on the session's socket, with the program after it.
    ///
    /// A `sh` in front fills in `XDG_RUNTIME_DIR` only where the identity's
    /// own environment has none — the login decides that, and the agent
    /// cannot see what `su -l` gives it. The absolute `--display` means
    /// waypipe itself needs none.
    pub(crate) fn waypipe_argv(seen: &str, argv: Vec<String>) -> Vec<String> {
        let mut out: Vec<String> = [
            "/bin/sh",
            "-c",
            r#"XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-$1}"; export XDG_RUNTIME_DIR; shift; exec "$@""#,
            "sh",
            seen,
            "waypipe",
            "--compress",
            "lz4",
            "--socket",
            &format!("{seen}/wp.sock"),
            "--display",
            &format!("{seen}/wayland-vmlab"),
            "server",
            "--",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        out.extend(argv);
        out
    }

    /// Accept and park until the session ends, announcing each connection
    /// and closing whatever outstays the grace.
    fn accept_loop(endpoint: &Endpoint, records: &Records, grace: Duration) {
        let tick = (grace / 4).clamp(Duration::from_millis(10), Duration::from_millis(250));
        let tick = PollTimeout::try_from(tick).unwrap_or(PollTimeout::MAX);
        while !endpoint.closed.load(Ordering::SeqCst) {
            let mut fds: Vec<PollFd> = endpoint
                .listeners
                .iter()
                .map(|l| PollFd::new(l.as_fd(), PollFlags::POLLIN))
                .collect();
            if poll(&mut fds, tick).is_err() {
                thread::sleep(Duration::from_millis(10));
            }
            for listener in &endpoint.listeners {
                while let Ok((stream, _)) = listener.accept() {
                    if stream.set_nonblocking(false).is_err() {
                        continue;
                    }
                    endpoint
                        .parked
                        .lock()
                        .unwrap()
                        .push_back((Instant::now(), stream));
                    records.send(&AppRecord::Pending);
                }
            }
            endpoint.expire(grace);
        }
    }

    /// The program's output, a record per line. Once the channel has gone
    /// the lines are still read, so a chatty program never blocks on a full
    /// pipe.
    fn pump_log(records: &Records, src: Box<dyn Read + Send>) {
        let mut src = BufReader::new(src);
        let mut line = Vec::new();
        loop {
            line.clear();
            match (&mut src).take(LOG_LINE_MAX).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line.ends_with(b"\n") {
                line.pop();
                if line.ends_with(b"\r") {
                    line.pop();
                }
            }
            records.send(&AppRecord::Log {
                text: String::from_utf8_lossy(&line).into_owned(),
            });
        }
    }

    /// A cookie's 32 hex digits as its 16 bytes. The caller has checked
    /// them with `valid_cookie`.
    fn cookie_bytes(cookie: &str) -> Vec<u8> {
        (0..cookie.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&cookie[i..i + 2], 16).unwrap_or(0))
            .collect()
    }

    /// An Xauthority file holding `cookie` as `MIT-MAGIC-COOKIE-1` for
    /// display `n`, twice: once for the local host by name, and once for
    /// any address, so a client that names the host differently still
    /// finds it. libXau's format: big-endian `u16` family, then four
    /// `u16`-length-prefixed fields — address, display number, auth name,
    /// auth data.
    pub(crate) fn xauthority(hostname: &str, n: u32, cookie: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        for (family, address) in [(FAMILY_LOCAL, hostname.as_bytes()), (FAMILY_WILD, &[][..])] {
            out.extend_from_slice(&family.to_be_bytes());
            for field in [
                address,
                n.to_string().as_bytes(),
                b"MIT-MAGIC-COOKIE-1",
                cookie,
            ] {
                out.extend_from_slice(&(field.len() as u16).to_be_bytes());
                out.extend_from_slice(field);
            }
        }
        out
    }

    pub(crate) const FAMILY_LOCAL: u16 = 256;
    pub(crate) const FAMILY_WILD: u16 = 65535;
}

#[cfg(all(test, unix))]
pub(crate) use parking::{FAMILY_LOCAL, FAMILY_WILD, waypipe_argv, xauthority};
