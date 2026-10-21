//! The input feed (D3, D12, D13): every input vmlab sends a machine's
//! display, published to that machine's live recorder.
//!
//! QEMU never reports the pointer position over VNC (research 05), so the
//! inputs vmlab itself sends are the recorder's only pointer source. A human
//! viewer's input goes straight to QEMU and is not tracked (D12).

use std::sync::Mutex;
use std::time::Instant;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

/// One input the display sent, with absolute framebuffer pixel coordinates
/// where it had them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    /// A key chord such as `ctrl-alt-delete`.
    Keys { chord: String },
    /// Literal text typed one character at a time.
    Text { text: String },
    /// The pointer moved to `(x, y)`.
    Move { x: i64, y: i64 },
    /// A button clicked, at `(x, y)` when the caller named a point, else
    /// wherever the pointer already was.
    Click {
        button: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<i64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<i64>,
    },
    /// The left button pressed at `(x1, y1)`, dragged and released at
    /// `(x2, y2)`.
    Drag { x1: i64, y1: i64, x2: i64, y2: i64 },
}

/// An input event and when the display sent it.
#[derive(Debug, Clone)]
pub struct TimedInput {
    pub at: Instant,
    pub event: InputEvent,
}

/// Where a display publishes what it sends. One per machine, for the
/// machine's lifetime; a recorder attaches a channel while it runs and
/// detaches when it finalises. Publishing never blocks and never fails: with
/// no recorder attached an event is dropped.
#[derive(Default)]
pub struct InputTap {
    sink: Mutex<Option<mpsc::UnboundedSender<TimedInput>>>,
}

impl InputTap {
    /// Publish `event`, stamped now.
    pub fn publish(&self, event: InputEvent) {
        let sink = self.sink.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(tx) = sink.as_ref() {
            let _ = tx.send(TimedInput {
                at: Instant::now(),
                event,
            });
        }
    }

    /// Start receiving every input from now on, replacing any earlier
    /// receiver (whose channel closes).
    pub fn attach(&self) -> mpsc::UnboundedReceiver<TimedInput> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
        rx
    }

    /// Stop publishing.
    pub fn detach(&self) {
        *self.sink.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Whether a recorder is listening.
    pub fn attached(&self) -> bool {
        self.sink
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn events_reach_an_attached_receiver_and_vanish_otherwise() {
        let tap = InputTap::default();
        tap.publish(InputEvent::Move { x: 1, y: 2 });
        let mut rx = tap.attach();
        tap.publish(InputEvent::Move { x: 3, y: 4 });
        assert_eq!(
            rx.recv().await.unwrap().event,
            InputEvent::Move { x: 3, y: 4 }
        );
        tap.detach();
        tap.publish(InputEvent::Move { x: 5, y: 6 });
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn events_serialise_with_a_kind_tag() {
        let v = serde_json::to_value(InputEvent::Click {
            button: "left".into(),
            x: Some(10),
            y: None,
        })
        .unwrap();
        assert_eq!(
            v,
            serde_json::json!({"kind": "click", "button": "left", "x": 10})
        );
    }
}
