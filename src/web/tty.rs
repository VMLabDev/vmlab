//! Interactive-terminal WebSocket: `GET /api/labs/{lab}/machines/{name}/tty`
//! — a shell inside the guest served
//! by vmlab-agent (guest/agent-proto) over a per-session unix socket the
//! daemon exposes. Binary frames are raw PTY bytes both ways; text frames
//! carry control JSON — `{"resize": {"cols": N, "rows": M}}` is proxied to
//! the lab daemon, which resizes the guest PTY over the agent channel.

use actix_web::{Error, HttpRequest, HttpResponse, web};
use actix_ws::AggregatedMessage;
use futures::StreamExt;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use vmlab::proto::LabRequest;

use super::state::AppState;

/// `GET /api/labs/{lab}/machines/{machine}/tty` — a shell inside the guest.
///
/// One handler for both kinds: the terminal rides the same `vmlab.agent.0`
/// channel whether the machine is a VM or a container.
pub async fn machine_tty(
    req: HttpRequest,
    body: web::Payload,
    path: web::Path<(String, String)>,
    state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let (lab, machine) = path.into_inner();
    bridge(req, body, state, lab, machine).await
}

async fn bridge(
    req: HttpRequest,
    body: web::Payload,
    state: web::Data<AppState>,
    lab: String,
    machine: String,
) -> Result<HttpResponse, Error> {
    if !super::state::valid_name(&lab) || !super::state::valid_name(&machine) {
        return Ok(HttpResponse::BadRequest().json(json!({"error": "invalid lab or machine name"})));
    }
    // Open a fresh terminal session; the daemon binds a per-session socket
    // and hands back its path. Open failures (no agent in the guest, VM not
    // running) surface as a conflict with the daemon's actionable message.
    let opened = match state
        .lab_call(
            &lab,
            LabRequest::MachineTtyOpen {
                machine: machine.clone(),
                // A placeholder: xterm.js measures the pane and sends a
                // resize as soon as the socket is up.
                cols: 80,
                rows: 24,
            },
        )
        .await
    {
        Ok(v) => v,
        Err(e) => return Ok(super::api::fail(e)),
    };
    let session = opened["session"].as_u64().unwrap_or(0) as u32;
    let sock = std::path::PathBuf::from(opened["path"].as_str().unwrap_or_default());
    let unix = match UnixStream::connect(&sock).await {
        Ok(u) => u,
        Err(e) => {
            return Ok(HttpResponse::Conflict()
                .json(json!({"error": format!("cannot open shell socket: {e}")})));
        }
    };

    let (response, session_ws, msg_stream) = actix_ws::handle(&req, body)?;
    let mut msg_stream = msg_stream.aggregate_continuations();
    let (mut unix_rx, mut unix_tx) = unix.into_split();

    // Guest PTY → browser.
    let mut out = session_ws.clone();
    actix_web::rt::spawn(async move {
        let mut buf = [0u8; 8192];
        loop {
            match unix_rx.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if out.binary(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = out.close(None).await;
    });

    // Browser → guest PTY, resize control → daemon. Dropping the write half
    // on exit closes our side, which ends the agent session.
    let mut pong = session_ws.clone();
    actix_web::rt::spawn(async move {
        while let Some(Ok(msg)) = msg_stream.next().await {
            match msg {
                AggregatedMessage::Binary(b) => {
                    if unix_tx.write_all(&b).await.is_err() {
                        break;
                    }
                }
                AggregatedMessage::Text(t) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t)
                        && let Some(r) = v.get("resize")
                    {
                        let _ = state
                            .lab_call(
                                &lab,
                                LabRequest::MachineTtyResize {
                                    machine: machine.clone(),
                                    session,
                                    cols: r["cols"].as_u64().unwrap_or(80) as u16,
                                    rows: r["rows"].as_u64().unwrap_or(24) as u16,
                                },
                            )
                            .await;
                    }
                }
                AggregatedMessage::Ping(p) => {
                    if pong.pong(&p).await.is_err() {
                        break;
                    }
                }
                AggregatedMessage::Close(_) => break,
                AggregatedMessage::Pong(_) => {}
            }
        }
    });

    Ok(response)
}
