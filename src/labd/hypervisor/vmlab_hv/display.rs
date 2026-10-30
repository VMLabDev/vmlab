//! The **frames** and **input** capabilities of a vmlab-hv VM (ADR-0020;
//! PRD §22, phase 2b), over its native frame protocol rather than the
//! control socket: vmlab-hv serves `display.sock` beside the control socket,
//! and `vnc.sock` with it for human viewers (`vmlab console`, `gui = true`).
//!
//! Its handshake advertises the two as `display` and `input`; the wire
//! client is [`crate::hvdisplay`].

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use vmlab_hv_display::Message;

use super::super::{Frames, Input};
use crate::hvdisplay::{InputConn, button_bit};

/// The native frame socket vmlab-hv binds beside `control`.
pub(crate) fn native_sock(control: &Path) -> PathBuf {
    control.with_file_name("display.sock")
}

/// The RFB socket vmlab-hv binds beside `control` — the `vnc.sock` every
/// VNC consumer already looks for.
pub(crate) fn rfb_sock(control: &Path) -> PathBuf {
    control.with_file_name("vnc.sock")
}

/// How long a scripted key is held: QMP `send-key`'s default, so a guest
/// reads a chord the way it does under QEMU.
const KEY_HOLD: Duration = Duration::from_millis(100);

/// One machine's display socket, as the two capabilities.
pub(crate) struct HvDisplay {
    path: PathBuf,
    input: Arc<Mutex<InputConn>>,
    /// The buttons held, because the protocol sends the whole mask.
    buttons: Arc<Mutex<u32>>,
}

impl HvDisplay {
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            input: Arc::new(Mutex::new(InputConn::new(path.clone()))),
            path,
            buttons: Arc::default(),
        }
    }

    /// Send on the blocking pool, in order: the connection is shared and
    /// one sender at a time holds it.
    async fn send(&self, messages: Vec<Message>) -> Result<()> {
        let input = self.input.clone();
        tokio::task::spawn_blocking(move || {
            input
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .send(&messages)
        })
        .await
        .context("vmlab-hv input task")?
    }
}

#[async_trait::async_trait]
impl Frames for HvDisplay {
    /// The frame written as PNG, which the display's loader reads by its
    /// bytes whatever the file is called.
    async fn screendump(&self, to: &Path) -> Result<()> {
        let image = crate::hvdisplay::capture(&self.path).await?;
        crate::vision::save_png(&image, to)
    }
}

#[async_trait::async_trait]
impl Input for HvDisplay {
    async fn send_keys(&self, keys: &[&str]) -> Result<()> {
        let key = |qcode: &&str, down| Message::Key {
            qcode: qcode.to_string(),
            down,
        };
        self.send(keys.iter().map(|k| key(k, true)).collect())
            .await?;
        tokio::time::sleep(KEY_HOLD).await;
        self.send(keys.iter().rev().map(|k| key(k, false)).collect())
            .await
    }

    async fn pointer_to(&self, x: u32, y: u32, width: u32, height: u32) -> Result<()> {
        self.send(vec![Message::PointerAbs {
            x,
            y,
            width,
            height,
        }])
        .await
    }

    async fn button(&self, button: &str, down: bool) -> Result<()> {
        let bit = button_bit(button)?;
        let mask = {
            let mut held = self.buttons.lock().unwrap_or_else(|p| p.into_inner());
            if down {
                *held |= bit;
            } else {
                *held &= !bit;
            }
            *held
        };
        self.send(vec![Message::Buttons { mask }]).await
    }
}
