//! One SSH connection, and the agent channels its `session` channels ride.
//!
//! Every SSH request the facade answers turns into something the agent
//! already does: `shell` into `OpenTerminal`, `exec` into `OpenExec` (or
//! into a terminal hosting the command, where the client asked for one),
//! `window-change` into `Resize`, and the agent's exit code into
//! `exit-status`. Everything else is refused — see [`super`] for the one
//! invariant that decides which.

use std::collections::HashMap;
use std::sync::Arc;

use russh::server::{Auth, Handler, Msg, Session};
use russh::{Channel, ChannelId, Disconnect, Pty};
use tokio::sync::{mpsc, watch};

use super::FacadeSpec;
use crate::labd::guest_os::GuestOs;
use crate::labd::identity;
use crate::labd::vm_agent::{AgentHandle, AgentSession, Logon, SessionEvent};

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
enum ToGuest {
    Data(Vec<u8>),
    Eof,
    Resize(u16, u16),
}

/// One `session` channel, from open to `shell`/`exec`.
///
/// `pty-req` and `env` arrive *before* the request that starts anything, so
/// what they carry waits here until it does.
#[derive(Default)]
struct SessionChannel {
    size: Option<(u16, u16)>,
    env: Vec<(String, String)>,
    to_guest: Option<mpsc::Sender<ToGuest>>,
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
    channels: HashMap<ChannelId, SessionChannel>,
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
            channels: HashMap::new(),
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

    /// Record a request refused because serving it would need a channel the
    /// facade opens — the one thing ADR-0013 says it never does. See
    /// [`no_channel_for`].
    fn needs_a_channel_we_never_open(&self, request: &str, channel_type: &str) {
        self.spec.refused(
            request,
            &no_channel_for(&format!("`{request}`"), channel_type),
        );
    }

    /// Start an agent session on `channel` and pump it until it ends.
    async fn start(
        &mut self,
        channel: ChannelId,
        session: &mut Session,
        start: Start,
    ) -> Result<(), russh::Error> {
        let Some(chan) = self.channels.get_mut(&channel) else {
            session.channel_failure(channel)?;
            return Ok(());
        };
        if chan.to_guest.is_some() {
            // One shell or one command per channel; a second request on a
            // started channel is a client bug, not something to race.
            session.channel_failure(channel)?;
            return Ok(());
        }
        let pty = chan.size;
        let env = std::mem::take(&mut chan.env);

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

        let (tx, rx) = mpsc::channel(INFLIGHT_CHUNKS);
        if let Some(chan) = self.channels.get_mut(&channel) {
            chan.to_guest = Some(tx);
        }
        session.channel_success(channel)?;
        if let Some(handle) = self.handle().await {
            tokio::spawn(pump(agent_session, rx, handle, channel));
        }
        Ok(())
    }

    /// Hand something to a started channel, doing nothing for one that never
    /// started — a client may send data before `shell`, and dropping it is
    /// the honest answer when there is nothing to receive it.
    async fn forward(&mut self, channel: ChannelId, msg: ToGuest) {
        if let Some(tx) = self
            .channels
            .get(&channel)
            .and_then(|chan| chan.to_guest.clone())
        {
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
                Some(SessionEvent::Stderr(bytes)) => {
                    // Extended data code 1 is stderr; nothing else is
                    // defined, and `ssh` splits on exactly this.
                    if handle.extended_data(channel, 1, bytes).await.is_err() {
                        break;
                    }
                }
                Some(SessionEvent::Eof) => {
                    let _ = handle.eof(channel).await;
                }
                Some(SessionEvent::Exited(code)) => {
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
                Some(SessionEvent::Error(e)) => {
                    let _ = handle.extended_data(channel, 1, format!("vmlab: {e}\n")).await;
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

    /// `session`, and no other channel type is opened by a client the facade
    /// serves. `direct-tcpip` joins it when the guest tunnel lands (#89);
    /// everything else is refused by the invariant.
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
        if let Some(refusal) = &self.refusal {
            let refusal = refusal.clone();
            reply
                .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
                .await;
            self.spec.refused("session", &refusal);
            return Ok(());
        }
        // Many `session` channels per connection are expected, not a
        // surprise: `ControlMaster` exists to put them there, and an editor
        // opens one per terminal it shows.
        self.channels
            .insert(channel.id(), SessionChannel::default());
        reply.accept().await;
        Ok(())
    }

    /// `direct-tcpip` is the second channel type the facade will answer —
    /// VS Code rides its whole protocol over `ssh -T -D` — and it lands with
    /// the agent tunnel (#89). Until then it is refused by name rather than
    /// silently, so an editor that needs it fails legibly.
    async fn channel_open_direct_tcpip(
        &mut self,
        _channel: Channel<Msg>,
        host_to_connect: &str,
        port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.spec.refused(
            "direct-tcpip",
            &format!(
                "this vmlab does not forward {host_to_connect}:{port_to_connect} into the guest yet"
            ),
        );
        reply
            .reject(russh::ChannelOpenFailure::AdministrativelyProhibited)
            .await;
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Dropping the sender ends the pump, which closes the agent session.
        self.channels.remove(&channel);
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
        // Awaiting the pump here is the backpressure: russh re-grants the
        // SSH window when this returns, and this returns when the agent has
        // taken the bytes.
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
        match self.channels.get_mut(&channel) {
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
        match self.channels.get_mut(&channel) {
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
        if let Some(chan) = self.channels.get_mut(&channel) {
            chan.size = Some((cols, rows));
        }
        self.forward(channel, ToGuest::Resize(cols, rows)).await;
        session.channel_success(channel)?;
        Ok(())
    }

    /// `sftp` is a separate ticket (#88); anything else is refused because
    /// nothing in the client set sends it.
    async fn subsystem_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
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
