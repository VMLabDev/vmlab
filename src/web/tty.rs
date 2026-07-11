//! `GET /api/labs/{lab}/containers/{name}/tty` — the container's interactive
//! shell (guest/cinit's `vmlab.tty.0` PTY, PRD §18) over a WebSocket. Binary
//! frames are raw PTY bytes both ways; text frames carry control JSON —
//! `{"resize": {"cols": N, "rows": M}}` is proxied to the lab daemon, which
//! resizes the guest PTY over the ctl channel.

use actix_web::{Error, HttpRequest, HttpResponse, web};
use actix_ws::AggregatedMessage;
use futures::StreamExt;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::state::AppState;

pub async fn tty(
    req: HttpRequest,
    body: web::Payload,
    path: web::Path<(String, String)>,
    state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let (lab, container) = path.into_inner();
    if !super::state::valid_name(&lab) || !super::state::valid_name(&container) {
        return Ok(
            HttpResponse::BadRequest().json(json!({"error": "invalid lab or container name"}))
        );
    }
    // The daemon owns the socket path — ask it rather than recomputing.
    let sock = match state
        .lab_call(&lab, "container.tty_path", json!({"container": container}))
        .await
    {
        Ok(v) => std::path::PathBuf::from(v.as_str().unwrap_or_default()),
        Err(e) => return Ok(super::api::fail(e)),
    };
    if !sock.exists() {
        return Ok(HttpResponse::Conflict()
            .json(json!({"error": format!("{lab}/{container} has no shell socket (stopped?)")})));
    }
    let unix = match UnixStream::connect(&sock).await {
        Ok(u) => u,
        Err(e) => {
            return Ok(HttpResponse::Conflict()
                .json(json!({"error": format!("cannot open shell socket: {e}")})));
        }
    };

    let (response, session, msg_stream) = actix_ws::handle(&req, body)?;
    let mut msg_stream = msg_stream.aggregate_continuations();
    let (mut unix_rx, mut unix_tx) = unix.into_split();

    // Guest PTY → browser.
    let mut out = session.clone();
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
    // on exit closes our side; the guest keeps its shell for the next attach.
    let mut pong = session.clone();
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
                        let args = json!({
                            "container": container,
                            "cols": r["cols"].as_u64().unwrap_or(80),
                            "rows": r["rows"].as_u64().unwrap_or(24),
                        });
                        let _ = state.lab_call(&lab, "container.tty_resize", args).await;
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
