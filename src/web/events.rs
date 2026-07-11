//! `GET /api/events` — a WebSocket that merges the daemons' event streams and
//! forwards each event to the browser as a JSON text frame. The SPA uses these
//! to live-update VM state without polling.

use actix_web::{Error, HttpRequest, HttpResponse, web};
use futures::StreamExt;

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

        // Supervisor (host-scoped events: lab daemon crashes, etc.).
        if let Ok(sup) = state.supervisor().await
            && let Ok(mut events) = sup.subscribe().await
        {
            let tx = tx.clone();
            actix_web::rt::spawn(async move {
                while let Some(ev) = events.recv().await {
                    if let Ok(s) = serde_json::to_string(&ev)
                        && tx.send(s).await.is_err()
                    {
                        break;
                    }
                }
            });
        }

        // Each lab daemon that is already up. Best-effort and never blocks
        // the forward loop below (no subscribing through `ensure`, which
        // would spawn daemons just to watch them). Labs that come up later —
        // including their `template.pull.*` progress events, which originate
        // in the lab daemon's event log — are picked up via the supervisor's
        // aggregate forwarding (`watch_lab_events`).
        {
            let state = state.clone();
            let tx = tx.clone();
            actix_web::rt::spawn(async move {
                for lab in state.lab_names().await {
                    let Some(client) = vmlab::cli::daemon::try_lab_daemon(&lab).await else {
                        continue;
                    };
                    let Ok(mut events) = client.subscribe().await else {
                        continue;
                    };
                    let tx = tx.clone();
                    actix_web::rt::spawn(async move {
                        while let Some(ev) = events.recv().await {
                            if let Ok(s) = serde_json::to_string(&ev)
                                && tx.send(s).await.is_err()
                            {
                                break;
                            }
                        }
                    });
                }
            });
        }
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
