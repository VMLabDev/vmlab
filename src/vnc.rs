//! Minimal RFB (VNC) input client for scripted keyboard/pointer events.
//!
//! vmlab normally injects input via QMP (`send-key` / `input-send-event`),
//! but `send-key` only drives QEMU's PS/2 keyboard. USB-HID-only guests —
//! notably macOS booted through OpenCore — ignore PS/2, so scripted keys
//! never land. A real VNC viewer's input *does* reach them because it flows
//! through QEMU's VNC server to the USB devices. This client speaks just
//! enough of RFB 3.8 to do the same from a provision/`vmlab script`: it
//! connects to the VM's `vnc.sock`, completes the (auth-less) handshake, and
//! sends `KeyEvent`/`PointerEvent` messages.
//!
//! Selected per VM via `input_transport = "vnc"` (profile). Pointer
//! coordinates are framebuffer pixels — the same space screenshots use, so
//! no scaling is needed (unlike the QMP abs path).

use std::path::Path;

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Button-mask bits for `PointerEvent` (RFB 6.4.4).
pub const BTN_LEFT: u8 = 1;
pub const BTN_MIDDLE: u8 = 1 << 1;
pub const BTN_RIGHT: u8 = 1 << 2;

pub struct VncInput {
    stream: UnixStream,
    pub width: u16,
    pub height: u16,
}

impl VncInput {
    /// Connect to a VM's VNC unix socket and complete the RFB 3.8 handshake
    /// (Security type "None" — QEMU's default on these sockets).
    pub async fn connect(sock: &Path) -> Result<Self> {
        let mut stream = UnixStream::connect(sock)
            .await
            .with_context(|| format!("connecting VNC socket {}", sock.display()))?;

        // ProtocolVersion exchange.
        let mut server_ver = [0u8; 12];
        stream.read_exact(&mut server_ver).await?;
        stream.write_all(b"RFB 003.008\n").await?;

        // Security handshake (3.7+): server lists types, we pick None (1).
        let count = stream.read_u8().await?;
        if count == 0 {
            let reason_len = stream.read_u32().await? as usize;
            let mut reason = vec![0u8; reason_len];
            stream.read_exact(&mut reason).await?;
            bail!(
                "VNC connection refused: {}",
                String::from_utf8_lossy(&reason)
            );
        }
        let mut types = vec![0u8; count as usize];
        stream.read_exact(&mut types).await?;
        if !types.contains(&1) {
            bail!("VNC server requires authentication (unsupported); offered {types:?}");
        }
        stream.write_u8(1).await?; // chosen security: None
        let security_result = stream.read_u32().await?;
        if security_result != 0 {
            bail!("VNC security handshake failed");
        }

        // ClientInit (shared) → ServerInit.
        stream.write_u8(1).await?;
        let width = stream.read_u16().await?;
        let height = stream.read_u16().await?;
        let mut pixel_format = [0u8; 16];
        stream.read_exact(&mut pixel_format).await?;
        let name_len = stream.read_u32().await? as usize;
        let mut name = vec![0u8; name_len];
        stream.read_exact(&mut name).await?;

        Ok(Self {
            stream,
            width,
            height,
        })
    }

    /// Press or release one key (X11 keysym), RFB `KeyEvent` (type 4).
    pub async fn key(&mut self, keysym: u32, down: bool) -> Result<()> {
        let mut msg = [0u8; 8];
        msg[0] = 4;
        msg[1] = u8::from(down);
        msg[4..8].copy_from_slice(&keysym.to_be_bytes());
        self.stream.write_all(&msg).await?;
        Ok(())
    }

    /// Press all keysyms down (in order) then release them (reverse) — the
    /// chord semantics of a single QMP `send-key`.
    pub async fn chord(&mut self, keysyms: &[u32]) -> Result<()> {
        for &k in keysyms {
            self.key(k, true).await?;
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
        // Hold briefly before release so slow real-mode guests (DOS/9x TUIs
        // polling the BIOS keyboard) reliably latch the keystroke; an 8ms hold
        // was dropped between menu redraws.
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        for &k in keysyms.iter().rev() {
            self.key(k, false).await?;
            tokio::time::sleep(std::time::Duration::from_millis(15)).await;
        }
        Ok(())
    }

    /// Pointer state at (x, y) with the given button mask, RFB `PointerEvent`
    /// (type 5). Coordinates are framebuffer pixels, clamped to the screen.
    pub async fn pointer(&mut self, x: i64, y: i64, mask: u8) -> Result<()> {
        let cx = x.clamp(0, self.width.saturating_sub(1) as i64) as u16;
        let cy = y.clamp(0, self.height.saturating_sub(1) as i64) as u16;
        let mut msg = [0u8; 6];
        msg[0] = 5;
        msg[1] = mask;
        msg[2..4].copy_from_slice(&cx.to_be_bytes());
        msg[4..6].copy_from_slice(&cy.to_be_bytes());
        self.stream.write_all(&msg).await?;
        Ok(())
    }

    /// Move with no buttons pressed.
    pub async fn mouse_move(&mut self, x: i64, y: i64) -> Result<()> {
        self.pointer(x, y, 0).await
    }

    /// Press + release a button at (x, y).
    pub async fn click(&mut self, x: i64, y: i64, button: u8) -> Result<()> {
        self.pointer(x, y, 0).await?;
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        self.pointer(x, y, button).await?;
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        self.pointer(x, y, 0).await?;
        Ok(())
    }
}
