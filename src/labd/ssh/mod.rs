//! The SSH facade: vmlab terminates SSH on the host (PRD §19.3, ADR-0012).
//!
//! No guest runs an sshd. vmlab terminates the protocol here, in the lab
//! daemon, beside the agent client and the resolved logon, and maps SSH
//! channels onto agent channels: `session` onto a terminal or an exec. The
//! transport is the agent channel, so a machine on no segment at all is
//! attachable, and a VM and a container micro-VM are attached to the same
//! way.
//!
//! **The endpoint is a stdio `ProxyCommand` and nothing else.** One lab
//! command ([`crate::proto::LabRequest::MachineSshOpen`]) returns the path of
//! a unix socket, and `vmlab ssh-proxy` connects it to stdin/stdout. Nothing
//! listens on the host and no port is leased; a proxy invocation costs a
//! `connect(2)` and a copy loop. The socket shape has precedent in tree —
//! `machine.tty_open` re-exposes an agent terminal the same way.
//!
//! **Auth is `none`.** There is no network path to the facade, so the trust
//! boundary is already "can you exec the proxy against this lab socket". The
//! SSH username is a *selector* over the machine's declared logins (§19.2)
//! carrying the label — never a credential, so `DOMAIN\user` never has to
//! survive an SSH username. See [`selector_for`].
//!
//! **The invariant that decides every refusal** (ADR-0013): the facade only
//! ever *answers* a channel open; it never initiates one. `forwarded-tcpip`,
//! `auth-agent@openssh.com` and `x11` are channel types it can never open,
//! which is exactly why `ssh -R`, agent forwarding and X11 are refused — one
//! rule, not a table a later reader has to keep extending.

pub mod host_key;
mod session;
#[cfg(test)]
mod tests;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use russh::keys::PrivateKey;
use serde_json::{Value, json};

use crate::config::model::Login;
use crate::labd::guest_os::GuestOs;
use crate::labd::vm_agent::AgentHandle;

pub use session::Facade;

/// Where a refusal goes so that it is not visible only in one developer's
/// terminal (§19.3). The lab daemon passes its event log; a test passes a
/// collector.
pub type Events = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// The lab event a refused channel or request lands on.
pub const REFUSED_EVENT: &str = "ssh.refused";

/// Everything about one machine the facade needs, resolved before a client
/// connects — a machine's declaration cannot change under an open
/// connection, and the facade never reaches back into the lab runtime.
pub struct FacadeSpec {
    /// The machine this facade fronts, for messages and events.
    pub machine: String,
    /// Its declared logins, in declaration order (§19.2).
    pub logins: Vec<Login>,
    /// Its guest family, which decides the identity floor's spelling.
    pub guest_os: GuestOs,
    /// The per-(lab, machine) host key ([`host_key`]).
    pub key: PrivateKey,
    /// The account name the daemon itself runs under — the username an
    /// `ssh` with no `-l` sends. See [`selector_for`].
    pub host_user: Option<String>,
    pub events: Events,
}

impl FacadeSpec {
    /// Record one refusal on the lab event log. `request` is the SSH request
    /// as the protocol spells it, so a reader can match on it; `reason` is
    /// vmlab's own words, which the SSH protocol has nowhere to put.
    ///
    /// The machine's name goes under both keys deliberately: `machine` is
    /// what it is, and `vm` is what the lab daemon's `handler {}` dispatch
    /// matches a target on, for containers as much as for VMs.
    fn refused(&self, request: &str, reason: &str) {
        (self.events)(
            REFUSED_EVENT,
            json!({
                "vm": self.machine,
                "machine": self.machine,
                "request": request,
                "reason": reason,
            }),
        );
    }
}

/// Bind `sock_path` and serve exactly one SSH connection over it.
///
/// One socket per proxy invocation, in and out with the process that
/// connects to it: the socket is unlinked as soon as the connection ends, so
/// nothing outlives the `ssh` that asked for it, and a second `ssh` asks the
/// lab for its own.
pub async fn expose_ssh_socket(
    spec: Arc<FacadeSpec>,
    agent: AgentHandle,
    sock_path: PathBuf,
) -> Result<()> {
    crate::labd::one_shot::serve_one(
        sock_path,
        move |stream| async move {
            if let Err(e) = serve_connection(spec.clone(), agent, stream).await {
                tracing::debug!(machine = %spec.machine, error = %e, "ssh facade connection ended");
            }
        },
        // Nothing was opened in the guest yet — the facade opens agent
        // channels only once a client asks for one — so an abandoned socket
        // has nothing to release.
        || async {},
    )
    .await
}

/// Serve one SSH connection over an already-accepted stream, returning when
/// the client hangs up.
pub async fn serve_connection<S>(spec: Arc<FacadeSpec>, agent: AgentHandle, stream: S) -> Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (handle_tx, handle_rx) = tokio::sync::watch::channel(None);
    let facade = Facade::new(spec, agent, handle_rx);
    let running = russh::server::run_stream(config(&facade), stream, facade)
        .await
        .context("starting the SSH facade")?;
    // The handler needs a handle of its own — to write a channel's output and
    // to say why a login label was refused — and russh only hands one out
    // once the session is running. Nothing can arrive before the key
    // exchange, and the handler awaits the slot rather than reading it, so
    // filling it here is ordered, not raced.
    let _ = handle_tx.send(Some(running.handle()));
    running.await.context("the SSH facade session failed")?;
    Ok(())
}

fn config(facade: &Facade) -> Arc<russh::server::Config> {
    use russh::MethodKind;

    Arc::new(russh::server::Config {
        server_id: russh::SshId::Standard(
            concat!("SSH-2.0-vmlab_", env!("CARGO_PKG_VERSION")).into(),
        ),
        // `none` and nothing else. Advertising a second method would only
        // invite a client to try a credential the facade has no use for:
        // the username is a selector, and the trust boundary is the lab
        // socket the proxy was exec'd against.
        methods: (&[MethodKind::None][..]).into(),
        // Nothing to rate-limit. Constant-time rejection exists to slow a
        // remote guesser down, and there is no network path to guess over.
        auth_rejection_time: Duration::ZERO,
        auth_rejection_time_initial: Some(Duration::ZERO),
        // One key per machine, so there is nothing to rotate: the facade
        // never advertises `hostkeys-00@openssh.com`, and russh has no way
        // to send it.
        keys: vec![facade.host_key()],
        // The SSH window must never exceed what the agent channel can be
        // asked to carry (§19.3): the facade hands every byte to the agent
        // before russh re-grants window, so matching the agent's own credit
        // keeps the two flow-control layers stacked rather than buffered
        // inside `labd`.
        window_size: vmlab_agent_proto::INITIAL_WINDOW as u32,
        // An attached editor or a shell at a prompt is idle for hours at a
        // time; the connection ends when the proxy's socket does.
        inactivity_timeout: None,
        ..Default::default()
    })
}

/// The account the daemon itself runs as, for [`selector_for`].
pub fn host_user() -> Option<String> {
    nix::unistd::User::from_uid(nix::unistd::Uid::effective())
        .ok()
        .flatten()
        .map(|u| u.name)
}

/// Which of the machine's declared logins an SSH username selects (§19.2).
///
/// The username is a *selector*, never a credential — which is what keeps a
/// domain-qualified account name out of an SSH username. `None` means "the
/// developer named nobody", which [`crate::labd::identity::resolve`] reads as
/// the machine's default login.
///
/// Two spellings mean that:
///
/// - a username the machine's declarations do not know, that matches the
///   account the daemon itself runs as. `ssh <alias>` with no `-l` sends the
///   client's local username, and on a single-host tool over a unix socket
///   the client *is* the daemon's user — so "my own name" is how OpenSSH
///   spells "I did not choose an identity".
/// - a declared login's own label, which the generated alias sets (§19.7).
///
/// Anything else is passed through as a selector, and a selector nothing
/// declares is not an identity — the caller answers it with a banner and an
/// auth failure rather than quietly attaching as somebody.
pub fn selector_for<'a>(
    username: &'a str,
    logins: &[Login],
    host_user: Option<&str>,
) -> Option<&'a str> {
    let declared = logins
        .iter()
        .any(|l| l.label == username || l.user == username);
    if !declared && host_user == Some(username) {
        return None;
    }
    Some(username)
}

/// What the developer is told when their username names no declared login.
///
/// The username is a selector over declared identities, so an unrecognised
/// one is not an identity and auth is the right layer to refuse it at — but
/// "permission denied" alone is unactionable when the fix is a word the
/// machine already knows. Naming the machine's declared logins is the whole
/// content of the refusal.
///
/// §19.3 calls this the banner. It reaches the developer on the disconnect
/// that ends the connection rather than as `SSH_MSG_USERAUTH_BANNER`, for
/// the reasons `session::Facade::auth_none` records — neither the banner nor
/// the auth failure can carry it to a real client.
pub fn unknown_login_banner(username: &str, spec: &FacadeSpec) -> String {
    let default = crate::config::model::default_login(&spec.logins).map(|l| l.label.as_str());
    let declared: Vec<String> = spec
        .logins
        .iter()
        .map(|l| match Some(l.label.as_str()) == default {
            true => format!("{} (default)", l.label),
            false => l.label.clone(),
        })
        .collect();
    let floor = crate::labd::identity::floor(spec.guest_os);
    let known = match declared.is_empty() {
        true => format!(
            "machine `{}` declares no login {{}} — attach as `{floor}`, which is the \
             identity its agent already runs as",
            spec.machine
        ),
        false => format!(
            "machine `{}` declares: {} (and `{floor}`, the identity its agent runs as)",
            spec.machine,
            declared.join(", ")
        ),
    };
    format!("vmlab: `{username}` is not a login on this machine.\n{known}\n")
}

/// The environment variables a client-sent `env` request never gets to set.
///
/// Load-bearing rather than defensive. A client-sent `USERPROFILE` would
/// silently undo the `LoadUserProfileW` that gave a never-logged-on domain
/// user a profile at all (§19.2), and the rest name the same thing on the
/// other guest family or point at something that does not exist on this side
/// of the agent channel. Dropped rather than refused: `env` is best-effort
/// by design, and most distributions ship `SendEnv LANG LC_*` unconditionally.
pub const ENV_DENY_LIST: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "USERNAME",
    "LOGNAME",
    "SSH_AUTH_SOCK",
];

/// Whether an `env` request's variable survives to the guest.
pub fn env_allowed(name: &str) -> bool {
    !ENV_DENY_LIST
        .iter()
        .any(|denied| denied.eq_ignore_ascii_case(name))
}
