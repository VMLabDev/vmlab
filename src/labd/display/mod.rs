//! The **Display** capability: a machine's framebuffer, and the operations
//! that read and drive it — screen capture, keyboard, pointer, OCR and image
//! matching (PRD §10.3).
//!
//! Obtained from [`super::machine::Machine::display`], so a caller holding a
//! [`Display`] has already established the machine reports one. Absence is
//! reported, never inferred: nothing here asks whether a machine is a VM or a
//! container, and a container that one day runs a display server reports a
//! `Display` like anything else.
//!
//! What a display needs from the machine underneath it is [`DisplayHost`] —
//! the machine's input and frames capabilities, a VNC socket, somewhere to drop a capture, which of the two
//! input transports the guest actually listens to, and whether its scanout is
//! a GL texture that only the VNC path can capture. That is the whole
//! contract, and it is why this module lives beside the machine rather than
//! inside the wscript host: the lab daemon takes screenshots for
//! `machine.screenshot` with no script anywhere in sight, and the dependency
//! arrow from `labd` to `scripting` that the previous home created now points
//! the other way.

pub mod keymap;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow};
use image::RgbImage;

use super::hypervisor::{Frames, Input};
use crate::profiles::InputTransport;
use crate::record::{InputEvent, InputTap};
use crate::vision::{self, Match, MatchOptions};

/// What a [`Display`] needs from the machine behind it.
///
/// Implemented by [`crate::labd::vm::VmInstance`]. A machine kind that grows a
/// display implements this and returns `Some` from
/// [`Machine::display`](super::machine::Machine::display); nothing else has to
/// change.
#[async_trait::async_trait]
pub trait DisplayHost: Send + Sync + 'static {
    /// The machine's name, for error messages.
    fn name(&self) -> &str;
    /// The machine's **input** capability (ADR-0020), for guests whose
    /// input rides the control channel. An error when it has none.
    async fn input(&self) -> Result<Arc<dyn Input>>;
    /// The machine's **frames** capability, for captures that do not need
    /// the VNC path. An error when it has none.
    async fn frames(&self) -> Result<Arc<dyn Frames>>;
    /// The machine's VNC socket, for guests whose input rides RFB.
    fn vnc_sock(&self) -> PathBuf;
    /// Where a screendump may be written and immediately deleted.
    fn capture_dir(&self) -> PathBuf;
    /// Whether input goes over QMP or VNC — a resolved hardware fact, not a
    /// preference (the profile's `input_transport`).
    fn input_transport(&self) -> InputTransport;
    /// Whether the scanout is a host GL texture (`gpu.mode` virgl or vulkan
    /// under egl-headless). QMP `screendump` answers `no surface` there, so
    /// captures go over VNC, which reads the texture back.
    fn gl_display(&self) -> bool;
    /// Where every input this display sends is published, for the machine's
    /// console recorder (§11.1, D12): one per machine, for its lifetime.
    fn input_tap(&self) -> Arc<InputTap>;
}

/// A machine's display, with the operations that read and drive it.
///
/// Concrete rather than a trait: QEMU's is the only one that ever satisfies
/// it, and the part that *does* vary per machine sits behind [`DisplayHost`].
#[derive(Clone)]
pub struct Display {
    host: Arc<dyn DisplayHost>,
}

impl Display {
    pub fn new(host: Arc<dyn DisplayHost>) -> Self {
        Self { host }
    }

    /// True when input should go over VNC instead of QMP (the profile's
    /// `input_transport`).
    fn input_vnc(&self) -> bool {
        matches!(self.host.input_transport(), InputTransport::Vnc)
    }

    /// The machine's VNC socket — what a console recorder connects to on its
    /// own, separate connection (§11.1).
    pub fn vnc_sock(&self) -> PathBuf {
        self.host.vnc_sock()
    }

    /// The feed a recorder attaches to for every input this display sends.
    pub fn input_tap(&self) -> Arc<InputTap> {
        self.host.input_tap()
    }

    /// Open a fresh RFB connection. A long-lived connection that never drains
    /// the server's messages can desync and drop later input on real-mode
    /// guests (DOS/9x TUIs); a fresh connection per op mirrors an external
    /// viewer's reliable behaviour.
    async fn vnc(&self) -> Result<crate::vnc::VncClient> {
        crate::vnc::VncClient::connect(&self.host.vnc_sock()).await
    }

    /// Capture the framebuffer. A GL display is read over VNC; anything else
    /// through QMP `screendump`, falling back to VNC once if QEMU answers
    /// `no surface` (a GL scanout the host did not report).
    pub async fn grab(&self) -> Result<RgbImage> {
        if self.host.gl_display() {
            return self.grab_vnc().await;
        }
        match self.grab_frames().await {
            Err(e) if is_no_surface(&e) => self
                .grab_vnc()
                .await
                .map_err(|vnc| e.context(format!("VNC capture fallback failed too: {vnc:#}"))),
            other => other,
        }
    }

    /// The frames capability's screendump → decoded image.
    async fn grab_frames(&self) -> Result<RgbImage> {
        let frames = self.host.frames().await?;
        let tmp = self
            .host
            .capture_dir()
            .join(format!(".grab-{}.ppm", self.host.name()));
        frames.screendump(&tmp).await?;
        let img = vision::load_screen(&tmp)?;
        let _ = std::fs::remove_file(&tmp);
        Ok(img)
    }

    /// One full framebuffer update over RFB.
    async fn grab_vnc(&self) -> Result<RgbImage> {
        self.vnc().await?.capture().await
    }

    /// Current screen dimensions, needed to scale absolute mouse coordinates.
    async fn size(&self) -> Result<(u32, u32)> {
        let img = self.grab().await?;
        Ok((img.width(), img.height()))
    }

    /// Capture the screen to a PNG at `out`.
    pub async fn screenshot(&self, out: &Path) -> Result<()> {
        let img = self.grab().await?;
        if let Some(parent) = out.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        vision::save_png(&img, out)
    }

    /// Send a key chord (e.g. `ctrl-alt-delete`).
    pub async fn send_keys(&self, chord: &str) -> Result<()> {
        let keys = keymap::parse_chord(chord).map_err(|e| anyhow!(e))?;
        self.host.input_tap().publish(InputEvent::Keys {
            chord: chord.to_string(),
        });
        if self.input_vnc() {
            let syms: Vec<u32> = keys
                .iter()
                .map(|q| keymap::keysym(q))
                .collect::<Result<_, String>>()
                .map_err(|e| anyhow!(e))?;
            let mut c = self.vnc().await?;
            return c.chord(&syms).await;
        }
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let input = self.host.input().await?;
        input.send_keys(&refs).await?;
        Ok(())
    }

    /// Type literal text, one character at a time, pausing `delay_ms` between.
    pub async fn type_text(&self, text: &str, delay_ms: u64) -> Result<()> {
        self.host.input_tap().publish(InputEvent::Text {
            text: text.to_string(),
        });
        if self.input_vnc() {
            // Resolve all keysyms up front so the input loop owns plain data.
            let mut per_char: Vec<Vec<u32>> = Vec::with_capacity(text.len());
            for ch in text.chars() {
                let keys = keymap::char_keys(ch).map_err(|e| anyhow!(e))?;
                per_char.push(
                    keys.iter()
                        .map(|q| keymap::keysym(q))
                        .collect::<Result<_, String>>()
                        .map_err(|e| anyhow!(e))?,
                );
            }
            let mut c = self.vnc().await?;
            for syms in &per_char {
                c.chord(syms).await?;
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
            return Ok(());
        }
        let input = self.host.input().await?;
        for ch in text.chars() {
            let keys = keymap::char_keys(ch).map_err(|e| anyhow!(e))?;
            let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            input.send_keys(&refs).await?;
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        Ok(())
    }

    /// Move the pointer to absolute screen coordinates.
    pub async fn mouse_move(&self, x: i64, y: i64) -> Result<()> {
        self.host.input_tap().publish(InputEvent::Move { x, y });
        self.move_pointer(x, y).await
    }

    /// The move itself, unpublished: a click that names a point moves first,
    /// and the recorder hears about the click, not a move it did not ask for.
    async fn move_pointer(&self, x: i64, y: i64) -> Result<()> {
        if self.input_vnc() {
            let mut c = self.vnc().await?;
            return c.mouse_move(x, y).await;
        }
        let (w, h) = self.size().await?;
        let input = self.host.input().await?;
        input
            .pointer_to(x.max(0) as u32, y.max(0) as u32, w, h)
            .await?;
        Ok(())
    }

    /// Click a mouse button. When `at` is `Some`, move there first (correct
    /// for an explicit one-shot click); when `None`, QMP clicks at the
    /// pointer's current position and VNC errors (it needs coordinates).
    pub async fn mouse_click(&self, button: &str, at: Option<(i64, i64)>) -> Result<()> {
        self.host.input_tap().publish(InputEvent::Click {
            button: button.to_string(),
            x: at.map(|p| p.0),
            y: at.map(|p| p.1),
        });
        if self.input_vnc() {
            let mask = vnc_button(button)?;
            let (x, y) = at.ok_or_else(|| anyhow!("VNC input needs coordinates for a click"))?;
            let mut c = self.vnc().await?;
            return c.click(x, y, mask).await;
        }
        if let Some((x, y)) = at {
            self.move_pointer(x, y).await?;
        }
        let input = self.host.input().await?;
        input.button(button, true).await?;
        tokio::time::sleep(Duration::from_millis(60)).await;
        input.button(button, false).await?;
        Ok(())
    }

    /// Press the left button at `(x1,y1)`, drag to `(x2,y2)` in a few steps,
    /// then release.
    pub async fn mouse_drag(&self, x1: i64, y1: i64, x2: i64, y2: i64) -> Result<()> {
        self.host
            .input_tap()
            .publish(InputEvent::Drag { x1, y1, x2, y2 });
        if self.input_vnc() {
            let mut c = self.vnc().await?;
            c.pointer(x1, y1, 0).await?;
            c.pointer(x1, y1, crate::vnc::BTN_LEFT).await?;
            for step in 1..=8 {
                let x = x1 + (x2 - x1) * step / 8;
                let y = y1 + (y2 - y1) * step / 8;
                c.pointer(x, y, crate::vnc::BTN_LEFT).await?;
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
            return c.pointer(x2, y2, 0).await;
        }
        let (w, h) = self.size().await?;
        let input = self.host.input().await?;
        input
            .pointer_to(x1.max(0) as u32, y1.max(0) as u32, w, h)
            .await?;
        input.button("left", true).await?;
        for step in 1..=8 {
            let x = x1 + (x2 - x1) * step / 8;
            let y = y1 + (y2 - y1) * step / 8;
            input
                .pointer_to(x.max(0) as u32, y.max(0) as u32, w, h)
                .await?;
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        input.button("left", false).await?;
        Ok(())
    }

    /// OCR the screen, optionally restricted to a `(x, y, w, h)` region.
    pub async fn ocr(&self, region: Option<(u32, u32, u32, u32)>) -> Result<String> {
        let img = self.grab().await?;
        vision::ocr(&img, region).await
    }

    /// Search the screen for the first matching template image.
    pub async fn find_image(
        &self,
        templates: &[PathBuf],
        opts: &MatchOptions,
    ) -> Result<Option<Match>> {
        let current = self.grab().await?;
        for path in templates {
            let template = vision::load_screen(path)
                .map_err(|e| anyhow!("reference image {}: {e:#}", path.display()))?;
            if let Some(m) = vision::find_template(&current, &template, opts) {
                return Ok(Some(m));
            }
        }
        Ok(None)
    }
}

/// Whether a capture failed because QEMU's console has no 2D surface — the
/// answer `screendump` gives for a GL scanout. The frames capability keeps
/// QEMU's error inside the one it returns, so it still reads here.
fn is_no_surface(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<crate::qmp::QmpError>(),
        Some(crate::qmp::QmpError::Command { desc, .. }) if desc == "no surface"
    )
}

/// RFB button mask for a button name.
fn vnc_button(button: &str) -> Result<u8> {
    match button {
        "left" => Ok(crate::vnc::BTN_LEFT),
        "middle" => Ok(crate::vnc::BTN_MIDDLE),
        "right" => Ok(crate::vnc::BTN_RIGHT),
        other => Err(anyhow!("unknown mouse button `{other}`")),
    }
}

#[cfg(test)]
mod tests;
