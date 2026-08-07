//! One SSH connection, and the agent channels its channels ride.
//!
//! Every SSH request the facade answers turns into something the agent
//! already does: `shell` into `OpenTerminal`, `exec` into `OpenExec` (or
//! into a terminal hosting the command, where the client asked for one),
//! `window-change` into `Resize`, `direct-tcpip` into `OpenTunnel`,
//! `subsystem sftp` into a `fileops` session ([`super::sftp`] holds that
//! transcode), and the agent's exit code into `exit-status`. Everything else
//! is refused — see [`super`] for the one invariant that decides which.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use russh::server::{Auth, Handler, Msg, Session};
use russh::{Channel, ChannelId, ChannelOpenFailure, Disconnect, Pty};
use tokio::sync::{mpsc, watch};

use super::FacadeSpec;
use crate::labd::guest_os::GuestOs;
use crate::labd::identity;
use crate::labd::vm_agent::{AgentHandle, AgentSession, Logon, SessionEvent, TunnelError};
use crate::sync::LockRecover;

/// How many chunks of client bytes may be in flight towards the agent.
///
/// One. The pump takes a chunk, hands it to the agent — which blocks on the
/// guest's credit — and only then takes the next, so `labd` holds at most one
/// chunk per channel. This is the coupling §19.3 calls a requirement: the
/// facade must never grant SSH window it cannot back with agent credit, and a
/// deeper queue here would be exactly the unbounded buffer in the lab daemon
/// that rule exists to forbid.
const INFLIGHT_CHUNKS: usize = 1;

/// The size a shell gets when the client asked for no terminal. A shell
/// still needs one to be hosted on, and `ssh -T` never sends a size.
const DEFAULT_SIZE: (u16, u16) = (80, 24);

/// What a session channel carries towards the guest once it has started.
///
/// `pub(super)` because `subsystem sftp` is served by [`super::sftp`] off the
/// same channel: the client's bytes reach the transcode the same way they
/// reach a shell, and by the same depth-1 route.
pub(super) enum ToGuest {
    Data(Vec<u8>),
    Eof,
    Resize(u16, u16),
}

/// One channel a client opened, and the way to the agent channel behind it.
///
/// A `session` starts empty and gains its way to the guest at `shell` or
/// `exec`; `pty-req` and `env` arrive *before* that request, so what they
/// carry waits here until it does. A `direct-tcpip` has neither — it is
/// wired up at open, and nothing else ever applies to it.
#[derive(Default)]
struct ClientChannel {
    size: Option<(u16, u16)>,
    env: Vec<(String, String)>,
    to_guest: Option<mpsc::Sender<ToGuest>>,
}

/// What `pty-req` and `env` left on a channel, taken by the request that
/// starts something on it.
struct Pending {
    size: Option<(u16, u16)>,
    env: Vec<(String, String)>,
}

/// The server side of one SSH connection.
pub struct Facade {
    spec: Arc<FacadeSpec>,
    agent: AgentHandle,
    /// Who this connection's channels run as, resolved from the SSH username
    /// at auth and fixed for the connection: `None` is the agent identity.
    logon: Option<Logon>,
    /// Set when the username named no declared login: this connection is
    /// over, and this is what the developer is told. While it is set nothing
    /// is ever served — see `auth_none`.
    refusal: Option<String>,
    /// Shared because a tunnel's dial runs off the session loop and has to
    /// retire its own entry when it fails — the only channel open the facade
    /// answers from somewhere other than the handler itself.
    channels: Arc<Mutex<HashMap<ChannelId, ClientChannel>>>,
    handle: watch::Receiver<Option<russh::server::Handle>>,
}

impl Facade {
    pub fn new(
        spec: Arc<FacadeSpec>,
        agent: AgentHandle,
        handle: watch::Receiver<Option<russh::server::Handle>>,
    ) -> Self {
        Self {
            spec,
            agent,
            logon: None,
            refusal: None,
            channels: Arc::new(Mutex::new(HashMap::new())),
            handle,
        }
    }

    pub fn host_key(&self) -> russh::keys::PrivateKey {
        self.spec.key.clone()
    }

    /// This connection's handle, once the session is running. Awaiting it
    /// rather than reading it is what makes the slot the caller fills after
    /// `run_stream` ordered instead of raced.
    async fn handle(&mut self) -> Option<russh::server::Handle> {
        self.handle
            .wait_for(|slot| slot.is_some())
            .await
            .ok()?
            .clone()
    }

    /// Refuse a channel open in vmlab's own words, and record it.
    ///
    /// A channel open failure is the one refusal SSH lets vmlab put text on,
    /// so both halves of §19.3's answer happen here: the developer's client
    /// is told, and the lab event log keeps it from being visible only in one
    /// terminal.
    async fn refuse_open(
        &self,
        request: &str,
        reason: &str,
        reply: russh::server::ChannelOpenHandle,
    ) {
        reply.reject(prohibited(reason)).await;
        self.spec.refused(request, reason);
    }

    /// Record a request refused because serving it would need a channel the
    /// facade opens — the one thing ADR-0013 says it never does. See
    /// [`no_channel_for`].
    fn needs_a_channel_we_never_open(&self, request: &str, channel_type: &str) {
        self.spec.refused(
            request,
            &no_channel_for(&format!("`{request}`"), channel_type),
        );
    }

    /// What the channel's `pty-req` and `env` left for the request that is
    /// about to start something on it, taken — or `None` where there is no
    /// such channel, or something already started on it.
    ///
    /// One shell, one command or one subsystem per channel; a second request
    /// on a started channel is a client bug, not something to race, and a
    /// `direct-tcpip` was started at its open.
    fn claim(&mut self, channel: ChannelId) -> Option<Pending> {
        let mut channels = self.channels.lock_recover();
        let chan = channels.get_mut(&channel)?;
        if chan.to_guest.is_some() {
            return None;
        }
        Some(Pending {
            size: chan.size,
            env: std::mem::take(&mut chan.env),
        })
    }

    /// Mark `channel` started, handing back the end the client's bytes will
    /// arrive on. A channel that went away under the open keeps nothing, and
    /// the receiver ends with the sender that is dropped here.
    fn mark_started(&mut self, channel: ChannelId) -> mpsc::Receiver<ToGuest> {
        let (tx, rx) = mpsc::channel(INFLIGHT_CHUNKS);
        if let Some(chan) = self.channels.lock_recover().get_mut(&channel) {
            chan.to_guest = Some(tx);
        }
        rx
    }

    /// Start an agent session on `channel` and pump it until it ends.
    async fn start(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
        start: Start,
    ) -> Result<(), russh::Error> {
        let Some(Pending { size: pty, env }) = self.claim(channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };

        let agent = self.agent.clone();
        let logon = self.logon.clone();
        let opened = start
            .open(&agent, self.spec.guest_os, pty, env, logon)
            .await;
        let agent_session = match opened {
            Ok(s) => s,
            Err(e) => {
                self.spec.refused("session", &format!("{e:#}"));
                session.channel_failure(channel)?;
                return Ok(());
            }
        };

        let rx = self.mark_started(channel);
        session.channel_success(channel)?;
        if let Some(handle) = self.handle().await {
            tokio::spawn(pump(agent_session, rx, handle, channel, Carries::Session));
        }
        Ok(())
    }

    /// `subsystem sftp`, answered host-side over a `fileops` session
    /// (§19.3, §19.5). See [`super::sftp`] for the transcode itself.
    async fn start_sftp(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), russh::Error> {
        // The `env` a client sent goes nowhere: a file session spawns no
        // process, so there is no environment to apply it over.
        if self.claim(channel).is_none() {
            session.channel_failure(channel)?;
            return Ok(());
        }
        // **The property this whole ticket is about**: the file session is
        // opened with the connection's own logon, so it resolves the same
        // (account, secret) as the shell and lands on the same cached logon,
        // the same `LogonId` and the same view of mapped drives (§19.2). One
        // value, carried by both opens — true by construction rather than by
        // discipline.
        let ops = match self.agent.open_fileops(self.logon.clone()).await {
            Ok(ops) => ops,
            // §19.3's per-channel degradation: an agent with no `fileops`
            // still serves a shell, and `sftp` refuses **by name** — the
            // message names the capability and the repair verb, so a
            // developer is told what to do rather than watching an editor
            // hang.
            Err(e) => {
                self.spec.refused("subsystem sftp", &format!("{e:#}"));
                session.channel_failure(channel)?;
                return Ok(());
            }
        };
        let rx = self.mark_started(channel);
        session.channel_success(channel)?;
        if let Some(handle) = self.handle().await {
            tokio::spawn(super::sftp::serve(ops, rx, handle, channel));
        }
        Ok(())
    }

    /// Hand something to a started channel, doing nothing for one that never
    /// started — a client may send data before `shell`, and dropping it is
    /// the honest answer when there is nothing to receive it.
    async fn forward(&mut self, channel: ChannelId, msg: ToGuest) {
        let to_guest = self
            .channels
            .lock_recover()
            .get(&channel)
            .and_then(|chan| chan.to_guest.clone());
        if let Some(tx) = to_guest {
            let _ = tx.send(msg).await;
        }
    }
}

/// What a `session` channel starts, once `shell` or `exec` says which.
///
/// `pty-req` and `env` have already happened by then, which is why the
/// terminal size and the environment are arguments here rather than state
/// the opener went and read.
enum Start {
    /// `shell` — the interactive case, and the one an editor's terminal
    /// opens.
    Shell,
    /// `exec` — one command line. VS Code's bootstrap is one of these.
    Exec(Vec<u8>),
}

impl Start {
    /// `pty` is the size the client asked for, or `None` where it asked for
    /// no terminal at all — which is the whole difference between the two
    /// agent opens, for a shell and for a command alike.
    async fn open(
        self,
        agent: &AgentHandle,
        guest_os: GuestOs,
        pty: Option<(u16, u16)>,
        env: Vec<(String, String)>,
        logon: Option<Logon>,
    ) -> anyhow::Result<AgentSession> {
        let command = match self {
            Start::Shell => None,
            Start::Exec(line) => Some(shell_command(
                guest_os,
                String::from_utf8_lossy(&line).as_ref(),
            )),
        };
        match (pty, command) {
            // `ssh -t dev01 top`: a command the client wants a terminal for,
            // which is a terminal hosting that command rather than the
            // guest's shell — `exec` with piped stdio would leave `top`
            // talking to a pipe. `sshd` draws the line in the same place.
            (Some((cols, rows)), command) => {
                agent.open_terminal(cols, rows, command, env, logon).await
            }
            // `ssh -T`, and every remote-dev client's control connection:
            // no terminal, and stdout and stderr stay separate streams.
            (None, Some(argv)) => agent.open_exec(argv, env, None, logon).await,
            (None, None) => {
                let (cols, rows) = DEFAULT_SIZE;
                agent.open_terminal(cols, rows, None, env, logon).await
            }
        }
    }
}

/// Why the facade refuses everything it refuses (ADR-0013).
///
/// `forwarded-tcpip`, `auth-agent@openssh.com` and `x11` are all channels
/// the *guest* would have to open, and the agent protocol has no such
/// direction: every `Open*` is a host message. So `ssh -R`, agent forwarding
/// and X11 are refused for one reason rather than three, and a request added
/// later is refused by the same rule without anybody extending a list.
///
/// The refusal itself is narrated by the client — `SSH_MSG_CHANNEL_FAILURE`
/// and `SSH_MSG_REQUEST_FAILURE` carry no text — so these words reach the
/// developer through the lab event log, which is the one place a refusal is
/// not visible only in one terminal.
fn no_channel_for(what: &str, channel_type: &str) -> String {
    format!(
        "serving {what} would need vmlab to open a `{channel_type}` channel into the guest, \
         and the agent protocol has no guest-initiated channel (ADR-0013)"
    )
}

/// vmlab refusing the open, in its own words.
///
/// A channel open failure is the one refusal SSH lets vmlab put text on, so
/// the reason travels with the code rather than only reaching the lab event
/// log. `Other` carries the description; the code is unchanged, so a client
/// reads it as the `ADMINISTRATIVELY_PROHIBITED` it is.
fn prohibited(reason: &str) -> ChannelOpenFailure {
    with_words(ChannelOpenFailure::AdministrativelyProhibited, reason)
}

/// The guest dialled and nothing answered — which is *not* a refusal, and
/// must not be dressed as one: a SOCKS client has to tell "nothing is
/// listening" from "vmlab refused you", and only the code says which.
fn connect_failed(reason: &str) -> ChannelOpenFailure {
    with_words(ChannelOpenFailure::ConnectFailed, reason)
}

fn with_words(failure: ChannelOpenFailure, reason: &str) -> ChannelOpenFailure {
    ChannelOpenFailure::Other {
        code: failure.code(),
        reason: reason.to_string(),
    }
}

/// The argv that runs one client-sent command line through a shell in the
/// guest.
///
/// SSH carries `exec` as one string precisely so the *guest's* shell splits
/// it — that is what makes `ssh dev01 'a | b > c'` mean anything — while the
/// agent's `exec` takes argv. So the shell is named here rather than the
/// string being split into something the client never asked for, and it is
/// named per guest family because that is the one thing the host knows and
/// the string does not.
fn shell_command(guest_os: GuestOs, line: &str) -> Vec<String> {
    match guest_os {
        // The same interpreter the agent's own terminal hosts on Windows, so
        // `ssh dev01 <cmd>` and a shell on `dev01` are the same language.
        GuestOs::Windows => vec![
            "powershell.exe".into(),
            "-NoLogo".into(),
            "-NonInteractive".into(),
            "-Command".into(),
            line.into(),
        ],
        GuestOs::Linux => vec!["/bin/sh".into(), "-c".into(), line.into()],
    }
}

/// What a channel carries, which is the whole of the difference between the
/// two channel types the facade answers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Carries {
    /// A `session`: stdout and stderr are separate streams and the guest's
    /// exit code is an `exit-status`.
    Session,
    /// A `direct-tcpip`: one byte stream each way and nothing else. Whatever
    /// rides it is the client's own protocol — a SOCKS conversation, an
    /// editor's — so vmlab never writes a word of its own into it, and there
    /// is no exit code to report because a TCP connection does not have one.
    Tunnel,
}

/// Carry one channel both ways until either end finishes.
///
/// The agent session lives here and nowhere else: one owner, no lock, and
/// the guest's credit window is what limits how fast the client may push,
/// because `send` waits for it.
async fn pump(
    mut agent: AgentSession,
    mut from_client: mpsc::Receiver<ToGuest>,
    handle: russh::server::Handle,
    channel: ChannelId,
    carries: Carries,
) {
    loop {
        tokio::select! {
            msg = from_client.recv() => match msg {
                Some(ToGuest::Data(bytes)) => {
                    if agent.send(&bytes).await.is_err() {
                        break;
                    }
                }
                Some(ToGuest::Eof) => {
                    let _ = agent.eof().await;
                }
                Some(ToGuest::Resize(cols, rows)) => {
                    let _ = agent.resize(cols, rows).await;
                }
                // The SSH channel is gone: nothing is left to read this
                // session's output.
                None => break,
            },
            event = agent.recv() => match event {
                Some(SessionEvent::Data(bytes)) => {
                    if handle.data(channel, bytes).await.is_err() {
                        break;
                    }
                }
                // Extended data code 1 is stderr; nothing else is defined,
                // and `ssh` splits on exactly this. A tunnel has no second
                // stream and the agent never gives it one.
                Some(SessionEvent::Stderr(bytes)) if carries == Carries::Session => {
                    if handle.extended_data(channel, 1, bytes).await.is_err() {
                        break;
                    }
                }
                Some(SessionEvent::Stderr(_)) => {}
                Some(SessionEvent::Eof) => {
                    // On a tunnel this is the guest peer's FIN, and the
                    // channel stays open the other way for it (§19.5).
                    let _ = handle.eof(channel).await;
                }
                Some(SessionEvent::Exited(code)) if carries == Carries::Session => {
                    // `exit-status` and never `exit-signal`: the agent
                    // reports `128 + signal` rather than a signal name, so
                    // `ssh guest 'kill -9 $$'` reports 137 — the honest
                    // translation of what the agent knows. Inventing a
                    // signal name to put in an `exit-signal` would be the
                    // facade claiming to know something it does not.
                    let _ = handle
                        .exit_status_request(channel, code.max(0) as u32)
                        .await;
                    let _ = handle.eof(channel).await;
                    let _ = handle.close(channel).await;
                    return;
                }
                // A tunnel has no exit code, so an agent that somehow sends
                // one is simply the connection ending.
                Some(SessionEvent::Exited(_)) => break,
                Some(SessionEvent::Error(e)) if carries == Carries::Session => {
                    let _ = handle.extended_data(channel, 1, format!("vmlab: {e}\n")).await;
                    break;
                }
                // The same failure on a tunnel is only a closed channel:
                // writing vmlab's words into it would corrupt the bytes
                // whatever rides it is in the middle of.
                Some(SessionEvent::Error(e)) => {
                    tracing::debug!(error = %e, "the guest end of a tunnel failed");
                    break;
                }
                None => break,
            },
        }
    }
    agent.close().await;
    let _ = handle.close(channel).await;
}

impl Handler for Facade {
    type Error = russh::Error;

    /// `none`, and the username is a selector rather than a credential.
    ///
    /// OpenSSH's opening `none` probe is unconditional — it is how the
    /// client enumerates methods — so `PreferredAuthentications`,
    /// `BatchMode`, `NumberOfPasswordPrompts=0` and
    /// `PasswordAuthentication=no` all still authenticate here. `none`
    /// cannot be talked out of.
    ///
    /// **An unrecognised label is refused, but not here.** §19.3 puts the
    /// refusal at the auth layer and its words in a `USERAUTH_BANNER`, and
    /// neither half survives contact with a real client:
    ///
    /// - russh asks a server for its banner when the client requests the
    ///   userauth *service*, before the client has sent a username, so a
    ///   banner naming the declared logins would print on every successful
    ///   attach and could not name the label that was refused.
    /// - A `USERAUTH_FAILURE` carries no text at all, and once it leaves the
    ///   client with no methods to try, OpenSSH prints
    ///   `Permission denied ()` and closes **without reading further** — so
    ///   anything sent after it is a race the developer loses. Measured:
    ///   client 10.4 displayed the disconnect that followed, the 9.x on a CI
    ///   runner did not.
    ///
    /// So the probe is answered — a `none` probe is not the decision — and
    /// the refusal is delivered where the protocol can actually carry words
    /// to a client that is still reading: a disconnect from
    /// [`Handler::auth_succeeded`], with every channel open refused by the
    /// same words as a backstop. Nothing is ever served either way; what
    /// changes is only that the developer is told why.
    async fn auth_none(&mut self, user: &str) -> Result<Auth, Self::Error> {
        let selector = super::selector_for(user, &self.spec.logins, self.spec.host_user.as_deref());
        match identity::resolve(
            &self.spec.machine,
            &self.spec.logins,
            self.spec.guest_os,
            selector,
            None,
        ) {
            Ok(logon) => self.logon = logon,
            Err(_) => {
                let refusal = super::unknown_login_banner(user, &self.spec);
                self.spec.refused("userauth", &refusal);
                // The logon stays `None`, which is the agent identity — so
                // the refusal below is what keeps an unrecognised label from
                // silently attaching as one, and it is checked again at every
                // channel open rather than trusted once.
                self.refusal = Some(refusal);
            }
        }
        Ok(Auth::Accept)
    }

    /// The one place the facade holds a `Session` while the client is
    /// certainly still reading. A refused label ends the connection here,
    /// carrying vmlab's words: `ILLEGAL_USER_NAME` is exactly what an
    /// unrecognised selector is.
    async fn auth_succeeded(&mut self, session: &mut Session) -> Result<(), Self::Error> {
        if let Some(refusal) = self.refusal.clone() {
            session.disconnect(Disconnect::IllegalUserName, &refusal, "")?;
        }
        Ok(())
    }

    /// `session` and `direct-tcpip` are the two channel types a client the
    /// facade serves ever opens; everything else is refused by the
    /// invariant.
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // The backstop for a refused login label: the disconnect
        // `auth_succeeded` sent has already ended this connection, so
        // reaching here at all means a client that ignored it. A channel
        // open failure is the one refusal that carries a description, so it
        // says the same thing rather than failing blankly.
        if let Some(refusal) = self.refusal.clone() {
            self.refuse_open("session", &refusal, reply).await;
            return Ok(());
        }
        // Many `session` channels per connection are expected, not a
        // surprise: `ControlMaster` exists to put them there, and an editor
        // opens one per terminal it shows.
        self.channels
            .lock_recover()
            .insert(channel.id(), ClientChannel::default());
        reply.accept().await;
        Ok(())
    }

    /// `direct-tcpip` onto the agent's tunnel: the guest dials, and the
    /// channel is that connection's byte pipe (§19.5).
    ///
    /// Mandatory rather than a convenience. VS Code runs `ssh -T -D <port>`
    /// and rides its *entire* protocol over that SOCKS forward, so refusing
    /// this does not degrade the editor — it breaks it.
    ///
    /// The destination crosses verbatim, name and all, because the guest's
    /// own resolver is what turns it into an address — which is what makes a
    /// SOCKS request for a hostname mean what the developer meant. No
    /// destination policy applies to it: a dynamic forward dials whatever the
    /// developer's tooling asks for, and vmlab is not a security boundary
    /// (§1.2).
    async fn channel_open_direct_tcpip(
        &mut self,
        channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // The same backstop a `session` gets, for the same reason.
        if let Some(refusal) = self.refusal.clone() {
            self.refuse_open("direct-tcpip", &refusal, reply).await;
            return Ok(());
        }
        let host = host_to_connect.to_string();
        let Ok(port) = u16::try_from(port_to_connect) else {
            // Nothing can be listening outside the TCP port range, which
            // makes this a failed connect rather than something vmlab
            // refuses.
            reply
                .reject(connect_failed(&format!(
                    "{host}:{port_to_connect} is not a TCP port"
                )))
                .await;
            return Ok(());
        };

        // The dial runs off the session loop, because it is the one thing
        // here that takes seconds: the guest resolves the name and connects,
        // and a dead destination spends the agent's whole dial budget.
        // Holding the loop for that would freeze every other channel on the
        // connection — and `ssh -D` puts one there per TCP connection the
        // developer's tooling makes.
        //
        // The channel's entry is made *here* rather than in the task, so the
        // first `data` after the client sees the confirmation always finds
        // somewhere to go; the task retires it again if the dial fails,
        // since a rejected channel is never closed.
        let id = channel.id();
        let (tx, rx) = mpsc::channel(INFLIGHT_CHUNKS);
        self.channels.lock_recover().insert(
            id,
            ClientChannel {
                to_guest: Some(tx),
                ..ClientChannel::default()
            },
        );

        let agent = self.agent.clone();
        let spec = self.spec.clone();
        // Weak, and never strong: the connection dying drops the `Facade`,
        // which drops the map, which drops every channel's sender and is
        // what winds each pump — and its agent channel — down. A strong
        // handle here would outlive the facade for as long as this tunnel
        // pumps, and hold *every* other channel's sender open with it.
        let channels = Arc::downgrade(&self.channels);
        let handle = self.handle().await;
        tokio::spawn(async move {
            match agent.open_tunnel(host.clone(), port).await {
                Ok(tunnel) => {
                    reply.accept().await;
                    if let Some(handle) = handle {
                        pump(tunnel, rx, handle, id, Carries::Tunnel).await;
                    }
                }
                Err(failure) => {
                    // A rejected channel is never closed, so the entry the
                    // dial was given up front is retired here instead —
                    // unless the connection already went, which retired all
                    // of them.
                    if let Some(channels) = channels.upgrade() {
                        channels.lock_recover().remove(&id);
                    }
                    match failure {
                        // "Nothing is listening" and "vmlab refused you" are
                        // different answers, and a SOCKS client has to be
                        // able to tell them apart — so a failed dial is the
                        // connect-failure code and never the prohibited one,
                        // which is spent on what vmlab genuinely refuses.
                        //
                        // It is deliberately not on the lab event log
                        // either: a dynamic forward dials whatever it is
                        // asked to, a closed port is ordinary, and the
                        // failure's own words reach the client on the open
                        // failure that carries them.
                        TunnelError::ConnectFailed(why) => {
                            tracing::debug!(machine = %spec.machine, %host, port, reason = %why, "a guest dial failed");
                            reply.reject(connect_failed(&why)).await;
                        }
                        // Everything that is not a dial: an agent with no
                        // `tunnel` — which says so by name, telling the
                        // developer to rebuild the template or run the
                        // repair verb, while the shell the facade can still
                        // serve carries on — and the channel simply never
                        // coming up. Neither reached a destination, so
                        // neither may borrow the connect-failure code, and
                        // the reason travels with the refusal.
                        TunnelError::Refused(why) => {
                            reply.reject(prohibited(&why)).await;
                            spec.refused("direct-tcpip", &why);
                        }
                    }
                }
            }
        });
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Dropping the sender ends the pump, which closes the agent session.
        self.channels.lock_recover().remove(&channel);
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.forward(channel, ToGuest::Eof).await;
        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Awaiting the pump here is the backpressure. russh re-grants the
        // SSH window before it calls this, so the grant is not what waits —
        // the *reading* is: this returns only once the agent has taken the
        // bytes, and until it does russh reads nothing more off the
        // transport. That is what keeps §19.3's coupling: `labd` holds one
        // chunk, and the client is stopped by TCP rather than by a buffer
        // the lab daemon grew.
        self.forward(channel, ToGuest::Data(data.to_vec())).await;
        Ok(())
    }

    async fn pty_request(
        &mut self,
        channel: ChannelId,
        _term: &str,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        _modes: &[(Pty, u32)],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        // The size is all the agent's terminal takes: `TERM` and the mode
        // flags are the guest shell's business, and the agent hosts a real
        // PTY/ConPTY that sets its own sane modes.
        match self.channels.lock_recover().get_mut(&channel) {
            Some(chan) => {
                chan.size = Some((col_width.max(1) as u16, row_height.max(1) as u16));
                session.channel_success(channel)?;
            }
            None => session.channel_failure(channel)?,
        }
        Ok(())
    }

    /// Applied over the logon's environment, minus the deny-list — and
    /// *dropped* rather than refused, because the request is best-effort by
    /// design and most distributions ship `SendEnv LANG LC_*`.
    async fn env_request(
        &mut self,
        channel: ChannelId,
        variable_name: &str,
        variable_value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        match self.channels.lock_recover().get_mut(&channel) {
            Some(chan) => {
                if super::env_allowed(variable_name) {
                    chan.env
                        .push((variable_name.to_string(), variable_value.to_string()));
                }
                session.channel_success(channel)?;
            }
            None => session.channel_failure(channel)?,
        }
        Ok(())
    }

    async fn shell_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.start(channel, session, Start::Shell).await
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.start(channel, session, Start::Exec(data.to_vec()))
            .await
    }

    async fn window_change_request(
        &mut self,
        channel: ChannelId,
        col_width: u32,
        row_height: u32,
        _pix_width: u32,
        _pix_height: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let (cols, rows) = (col_width.max(1) as u16, row_height.max(1) as u16);
        if let Some(chan) = self.channels.lock_recover().get_mut(&channel) {
            chan.size = Some((cols, rows));
        }
        self.forward(channel, ToGuest::Resize(cols, rows)).await;
        session.channel_success(channel)?;
        Ok(())
    }

    /// `sftp` is served here, over `fileops`; anything else is refused
    /// because nothing in the client set sends it.
    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "sftp" {
            return self.start_sftp(channel, session).await;
        }
        self.spec.refused(
            "subsystem",
            &format!("`{name}` is not served by this facade"),
        );
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn agent_request(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
    ) -> Result<bool, Self::Error> {
        // The one refusal a developer gets no signal from — `SSH_AUTH_SOCK`
        // is simply empty in the guest — which is why it is worth the event
        // log entry even though the client says nothing.
        self.needs_a_channel_we_never_open("auth-agent-req@openssh.com", "auth-agent@openssh.com");
        // The refusal a client is waiting for is a *channel* failure, and
        // russh answers this one's return value with a **global**
        // `SSH_MSG_REQUEST_FAILURE` instead — so the channel reply is sent
        // here, and russh's global one is a spare a client drops for having
        // nothing pending.
        session.channel_failure(channel)?;
        Ok(false)
    }

    async fn x11_request(
        &mut self,
        channel: ChannelId,
        _single_connection: bool,
        _x11_auth_protocol: &str,
        _x11_auth_cookie: &str,
        _x11_screen_number: u32,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.needs_a_channel_we_never_open("x11-req", "x11");
        session.channel_failure(channel)?;
        Ok(())
    }

    async fn tcpip_forward(
        &mut self,
        address: &str,
        port: &mut u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.spec.refused(
            "tcpip-forward",
            &no_channel_for(
                &format!("a reverse forward of {address}:{port}"),
                "forwarded-tcpip",
            ),
        );
        Ok(false)
    }

    async fn cancel_tcpip_forward(
        &mut self,
        _address: &str,
        _port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        // Nothing was ever forwarded, so there is nothing to cancel.
        Ok(false)
    }

    async fn streamlocal_forward(
        &mut self,
        socket_path: &str,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.spec.refused(
            "streamlocal-forward@openssh.com",
            &no_channel_for(
                &format!("a reverse forward of {socket_path}"),
                "forwarded-streamlocal@openssh.com",
            ),
        );
        Ok(false)
    }
}
