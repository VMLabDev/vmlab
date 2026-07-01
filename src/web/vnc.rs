//! `GET /vnc/{lab}/{vm}` — a raw byte bridge between the browser's WebSocket
//! and the VM's VNC unix socket. RFB is just a byte stream, so noVNC (which
//! speaks RFB over a binary WebSocket) talks to QEMU verbatim through here —
//! the same unframed copy the CLI's `console` bridge does over TCP.

use actix_web::{Error, HttpRequest, HttpResponse, web};
use bytes::Bytes;
use futures::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use super::state::AppState;
use vmlab::paths;

pub async fn vnc(
    req: HttpRequest,
    body: web::Payload,
    path: web::Path<(String, String)>,
    _state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let (lab, vm) = path.into_inner();
    // The names come straight from the URL and become a socket path.
    if !super::state::valid_name(&lab) || !super::state::valid_name(&vm) {
        return Ok(
            HttpResponse::BadRequest().json(serde_json::json!({"error": "invalid lab or vm name"}))
        );
    }
    let sock = paths::lab_runtime_dir(&lab)
        .join("vms")
        .join(&vm)
        .join("vnc.sock");

    if !sock.exists() {
        return Ok(HttpResponse::Conflict().json(
            serde_json::json!({"error": format!("{lab}/{vm} has no VNC socket (powered off?)")}),
        ));
    }

    let (response, session, mut msg_stream) = actix_ws::handle(&req, body)?;

    actix_web::rt::spawn(async move {
        let unix = match UnixStream::connect(&sock).await {
            Ok(u) => u,
            Err(_) => {
                let _ = session.close(None).await;
                return;
            }
        };
        let (mut ur, mut uw) = unix.into_split();

        // socket → browser
        let mut sock_to_ws = session.clone();
        let pump = actix_web::rt::spawn(async move {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match ur.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if sock_to_ws
                            .binary(Bytes::copy_from_slice(&buf[..n]))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
        });

        // browser → socket
        let mut s = session.clone();
        while let Some(Ok(msg)) = msg_stream.next().await {
            match msg {
                actix_ws::Message::Binary(b) => {
                    if uw.write_all(&b).await.is_err() {
                        break;
                    }
                }
                actix_ws::Message::Text(t) => {
                    if uw.write_all(t.as_bytes()).await.is_err() {
                        break;
                    }
                }
                actix_ws::Message::Ping(p) => {
                    if s.pong(&p).await.is_err() {
                        break;
                    }
                }
                actix_ws::Message::Close(_) => break,
                _ => {}
            }
        }

        pump.abort();
        let _ = session.close(None).await;
    });

    Ok(response)
}
