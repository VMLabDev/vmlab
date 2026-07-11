//! Transport-aware VM interaction primitives (PRD §10.3): screen capture,
//! keyboard, pointer, OCR, and image search over a [`VmInstance`]. The
//! QMP-vs-VNC choice (the resolved `input_transport`) lives here so the wscript
//! `VmHandle` methods and the `vmlab vm` CLI subcommands share one
//! implementation.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};
use image::RgbImage;

use super::keymap;
use crate::labd::vm::VmInstance;
use crate::vision::{self, Match, MatchOptions};

/// True when input should go over VNC instead of QMP (for USB-HID-only
/// guests like macOS where QMP `send-key` is ignored).
fn input_vnc(vm: &VmInstance) -> bool {
    matches!(
        vm.template().resolved.input_transport,
        crate::profiles::InputTransport::Vnc
    )
}

/// Open a fresh RFB connection. A long-lived connection that never drains
/// the server's messages can desync and drop later input on real-mode
/// guests (DOS/9x TUIs); a fresh connection per op mirrors an external
/// viewer's reliable behaviour.
async fn vnc(vm: &VmInstance) -> Result<crate::vnc::VncInput> {
    crate::vnc::VncInput::connect(&vm.dirs.vnc_sock()).await
}

/// RFB button mask for a button name.
pub fn vnc_button(button: &str) -> Result<u8> {
    match button {
        "left" => Ok(crate::vnc::BTN_LEFT),
        "middle" => Ok(crate::vnc::BTN_MIDDLE),
        "right" => Ok(crate::vnc::BTN_RIGHT),
        other => Err(anyhow!("unknown mouse button `{other}`")),
    }
}

/// QMP screendump → decoded image.
pub async fn grab_screen(vm: &VmInstance) -> Result<RgbImage> {
    let qmp = vm.qmp().await?;
    let tmp = vm.dirs.run.join(format!(".grab-{}.ppm", vm.cfg.name));
    qmp.screendump(&tmp).await?;
    let img = vision::load_screen(&tmp)?;
    let _ = std::fs::remove_file(&tmp);
    Ok(img)
}

/// Current screen dimensions, needed to scale absolute mouse coordinates.
async fn screen_size(vm: &VmInstance) -> Result<(u32, u32)> {
    let img = grab_screen(vm).await?;
    Ok((img.width(), img.height()))
}

/// Capture the screen to a PNG at `out`.
pub async fn screenshot(vm: &VmInstance, out: &Path) -> Result<()> {
    let img = grab_screen(vm).await?;
    if let Some(parent) = out.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    vision::save_png(&img, out)
}

/// Send a key chord (e.g. `ctrl-alt-delete`).
pub async fn send_keys(vm: &VmInstance, chord: &str) -> Result<()> {
    let keys = keymap::parse_chord(chord).map_err(|e| anyhow!(e))?;
    if input_vnc(vm) {
        let syms: Vec<u32> = keys
            .iter()
            .map(|q| keymap::keysym(q))
            .collect::<Result<_, String>>()
            .map_err(|e| anyhow!(e))?;
        let mut c = vnc(vm).await?;
        return c.chord(&syms).await;
    }
    let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
    let qmp = vm.qmp().await?;
    qmp.send_key(&refs, None).await?;
    Ok(())
}

/// Type literal text, one character at a time, pausing `delay_ms` between.
pub async fn type_text(vm: &VmInstance, text: &str, delay_ms: u64) -> Result<()> {
    if input_vnc(vm) {
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
        let mut c = vnc(vm).await?;
        for syms in &per_char {
            c.chord(syms).await?;
            tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        }
        return Ok(());
    }
    let qmp = vm.qmp().await?;
    for ch in text.chars() {
        let keys = keymap::char_keys(ch).map_err(|e| anyhow!(e))?;
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        qmp.send_key(&refs, None).await?;
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }
    Ok(())
}

/// Move the pointer to absolute screen coordinates.
pub async fn mouse_move(vm: &VmInstance, x: i64, y: i64) -> Result<()> {
    if input_vnc(vm) {
        let mut c = vnc(vm).await?;
        return c.mouse_move(x, y).await;
    }
    let (w, h) = screen_size(vm).await?;
    let qmp = vm.qmp().await?;
    qmp.mouse_move_abs(x.max(0) as u32, y.max(0) as u32, w, h)
        .await?;
    Ok(())
}

/// Click a mouse button. When `at` is `Some`, move there first (correct for
/// an explicit one-shot click); when `None`, QMP clicks at the pointer's
/// current position and VNC errors (it needs coordinates).
pub async fn mouse_click(vm: &VmInstance, button: &str, at: Option<(i64, i64)>) -> Result<()> {
    if input_vnc(vm) {
        let mask = vnc_button(button)?;
        let (x, y) = at.ok_or_else(|| anyhow!("VNC input needs coordinates for a click"))?;
        let mut c = vnc(vm).await?;
        return c.click(x, y, mask).await;
    }
    if let Some((x, y)) = at {
        mouse_move(vm, x, y).await?;
    }
    let qmp = vm.qmp().await?;
    qmp.mouse_button(button, true).await?;
    tokio::time::sleep(Duration::from_millis(60)).await;
    qmp.mouse_button(button, false).await?;
    Ok(())
}

/// Press the left button at `(x1,y1)`, drag to `(x2,y2)` in a few steps,
/// then release.
pub async fn mouse_drag(vm: &VmInstance, x1: i64, y1: i64, x2: i64, y2: i64) -> Result<()> {
    if input_vnc(vm) {
        let mut c = vnc(vm).await?;
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
    let (w, h) = screen_size(vm).await?;
    let qmp = vm.qmp().await?;
    qmp.mouse_move_abs(x1.max(0) as u32, y1.max(0) as u32, w, h)
        .await?;
    qmp.mouse_button("left", true).await?;
    for step in 1..=8 {
        let x = x1 + (x2 - x1) * step / 8;
        let y = y1 + (y2 - y1) * step / 8;
        qmp.mouse_move_abs(x.max(0) as u32, y.max(0) as u32, w, h)
            .await?;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    qmp.mouse_button("left", false).await?;
    Ok(())
}

/// OCR the screen, optionally restricted to a `(x, y, w, h)` region.
pub async fn ocr(vm: &VmInstance, region: Option<(u32, u32, u32, u32)>) -> Result<String> {
    let img = grab_screen(vm).await?;
    vision::ocr(&img, region).await
}

/// Search the screen for the first matching template image.
pub async fn find_image(
    vm: &VmInstance,
    templates: &[PathBuf],
    opts: &MatchOptions,
) -> Result<Option<Match>> {
    let screen = grab_screen(vm).await?;
    for path in templates {
        let template = vision::load_screen(path)
            .map_err(|e| anyhow!("reference image {}: {e:#}", path.display()))?;
        if let Some(m) = vision::find_template(&screen, &template, opts) {
            return Ok(Some(m));
        }
    }
    Ok(None)
}
