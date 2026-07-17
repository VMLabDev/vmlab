//! `GET /api/events` — a WebSocket that merges the daemons' event streams and
//! forwards each event to the browser as a JSON text frame. The SPA uses these
//! to live-update VM state without polling.

use actix_web::{Error, HttpRequest, HttpResponse, web};
use futures::StreamExt;
use serde_json::Value;

use super::state::AppState;

pub async fn events(
    req: HttpRequest,
    body: web::Payload,
    state: web::Data<AppState>,
) -> Result<HttpResponse, Error> {
    let (response, mut session, mut msg_stream) = actix_ws::handle(&req, body)?;

    actix_web::rt::spawn(async move {
        // A single merge channel fed by the supervisor and every lab daemon.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(256);

        // Supervisor aggregate (host-scoped events plus every lab stream
        // forwarded by watch_lab_events). Self-healing: a one-shot
        // subscription dies silently when the supervisor restarts — the WS
        // still looks connected while the browser misses every later event
        // (a segment.peer.* transition, say) and its status goes stale. So:
        // (re)subscribe in a loop, and after each (re)subscribe send a
        // synthetic resync nudge — the SPA refetches status on it, covering
        // both the initial fetch→subscribe race and any gap while the
        // subscription was down.
        {
            let state = state.clone();
            let tx = tx.clone();
            actix_web::rt::spawn(async move {
                loop {
                    // Ensure the supervisor is up via the shared client, but
                    // subscribe on a DEDICATED connection: Client::subscribe
                    // is one-slot-per-client, so subscribing on the shared
                    // cached client would let every new WS session steal the
                    // previous one's event stream (older tabs go silent).
                    if state.supervisor().await.is_ok()
                        && let Ok(sub) = vmlab::proto::client::Client::connect(
                            &vmlab::paths::supervisor_socket(),
                        )
                        .await
                        && let Ok(mut events) = sub.subscribe().await
                    {
                        let nudge = vmlab::proto::Event::new("web.resync", "", Value::Null);
                        if let Ok(s) = serde_json::to_string(&nudge)
                            && tx.send(s).await.is_err()
                        {
                            return; // browser gone
                        }
                        while let Some(ev) = events.recv().await {
                            if let Ok(s) = serde_json::to_string(&ev)
                                && tx.send(s).await.is_err()
                            {
                                return;
                            }
                        }
                    }
                    // Supervisor down or restarting: retry while the WS lives.
                    if tx.is_closed() {
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                }
            });
        }

        // No direct per-lab subscriptions: the supervisor watches every lab
        // it starts or finds at its own startup (`watch_lab_events`) and
        // forwards those streams into the aggregate above — a second, direct
        // subscription would deliver every lab event twice (it did: playbook
        // op logs showed each line duplicated).
        drop(tx);

        loop {
            tokio::select! {
                // Forward merged events to the browser.
                msg = rx.recv() => match msg {
                    Some(json) => {
                        if session.text(json).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
                // Drain the client side: respond to pings, exit on close.
                incoming = msg_stream.next() => match incoming {
                    Some(Ok(actix_ws::Message::Ping(p))) => {
                        if session.pong(&p).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(actix_ws::Message::Close(_))) | None => break,
                    _ => {}
                },
            }
        }
        let _ = session.close(None).await;
    });

    Ok(response)
}
