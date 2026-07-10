//! `GET /api/desktop/vnc/{lab}/{vm}` — the VM console, speaking the Forge
//! desktop-widget wire protocol (forge's `docs/widgets-protocol.md`): the
//! server decodes RFB from the VM's VNC unix socket and streams RGBA rect
//! frames; JSON text frames carry control. The client's opening `connect`
//! frame is required by the protocol but its host/port are ignored — the URL
//! path fixes the target VM.

use actix_web::{Error, HttpRequest, HttpResponse, web};
use actix_ws::AggregatedMessage;
use forge_core::widgets::proto::DesktopServerMsg;
use forge_core::widgets::vnc::session_over;
use forge_core::widgets::{CHANNEL_CAP, StreamClosed, WidgetMsg, WidgetStream};
use futures::StreamExt;
use tokio::net::UnixStream;
use tokio::sync::mpsc;

use super::state::AppState;
use vmlab::paths;

/// actix-ws → [`WidgetStream`] adapter. `actix_ws::MessageStream` is `!Send`
/// while the session engine requires `Send`, so a local reader task pumps
/// incoming frames into a bounded channel; the bridge reads from the channel
/// and writes through the cloneable [`actix_ws::Session`].
struct WsBridge {
    session: actix_ws::Session,
    inbox: mpsc::Receiver<WidgetMsg>,
}

impl WidgetStream for WsBridge {
    async fn recv(&mut self) -> Option<WidgetMsg> {
        self.inbox.recv().await
    }

    async fn send(&mut self, msg: WidgetMsg) -> Result<(), StreamClosed> {
        match msg {
            WidgetMsg::Text(t) => self.session.text(t).await.map_err(|_| StreamClosed),
            WidgetMsg::Binary(b) => self.session.binary(b).await.map_err(|_| StreamClosed),
            // `close` consumes the session; the engine sends Close last.
            WidgetMsg::Close => self
                .session
                .clone()
                .close(None)
                .await
                .map_err(|_| StreamClosed),
        }
    }
}

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

    let (response, session, msg_stream) = actix_ws::handle(&req, body)?;
    // Control frames are JSON; deliver them whole even if fragmented.
    let mut msg_stream = msg_stream.aggregate_continuations();

    let (tx, inbox) = mpsc::channel(CHANNEL_CAP);
    let mut pong = session.clone();
    actix_web::rt::spawn(async move {
        while let Some(Ok(msg)) = msg_stream.next().await {
            let frame = match msg {
                AggregatedMessage::Text(t) => WidgetMsg::Text(t.to_string()),
                AggregatedMessage::Binary(b) => WidgetMsg::Binary(b.to_vec()),
                AggregatedMessage::Ping(p) => {
                    if pong.pong(&p).await.is_err() {
                        break;
                    }
                    continue;
                }
                AggregatedMessage::Close(_) => break,
                AggregatedMessage::Pong(_) => continue,
            };
            // The session dropped its receiver: it is over, stop reading.
            if tx.send(frame).await.is_err() {
                return;
            }
        }
        let _ = tx.send(WidgetMsg::Close).await;
    });

    let mut bridge = WsBridge { session, inbox };
    actix_web::rt::spawn(async move {
        match UnixStream::connect(&sock).await {
            // QEMU's vnc.sock does an auth-less RFB handshake — no password.
            Ok(unix) => session_over(bridge, unix, None).await,
            Err(e) => {
                let msg = DesktopServerMsg::Error {
                    message: format!("cannot open VNC socket: {e}"),
                };
                let text = serde_json::to_string(&msg).expect("DesktopServerMsg serializes");
                let _ = bridge.send(WidgetMsg::Text(text)).await;
                let _ = bridge.send(WidgetMsg::Close).await;
            }
        }
    });

    Ok(response)
}
