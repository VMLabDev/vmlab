//! OCR via the `tesseract` CLI.
//!
//! Tesseract is driven as a subprocess (it ships in the official runtime
//! image, PRD §14): the image is written to a temporary PNG, then
//! `tesseract <png> stdout --psm 6` is run with stderr discarded. PSM 6
//! ("assume a single uniform block of text") suits screenshots of consoles
//! and dialogs.

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use image::{RgbImage, imageops};

/// Run OCR over `img`, optionally restricted to a `(x, y, w, h)` region.
///
/// Returns tesseract's stdout text verbatim (trailing whitespace and all).
/// A missing `tesseract` binary is reported as a clear, actionable error.
pub async fn ocr(img: &RgbImage, region: Option<(u32, u32, u32, u32)>) -> Result<String> {
    let cropped;
    let img = match region {
        Some((x, y, w, h)) => {
            if x >= img.width() || y >= img.height() {
                bail!(
                    "OCR region origin ({x}, {y}) lies outside the {}x{} image",
                    img.width(),
                    img.height()
                );
            }
            let w = w.min(img.width() - x);
            let h = h.min(img.height() - y);
            cropped = imageops::crop_imm(img, x, y, w, h).to_image();
            &cropped
        }
        None => img,
    };

    let png_path = temp_png_path();
    img.save_with_format(&png_path, image::ImageFormat::Png)
        .with_context(|| format!("failed to write OCR scratch PNG {}", png_path.display()))?;

    let result = run_tesseract(&png_path).await;
    let _ = std::fs::remove_file(&png_path); // best effort
    result
}

async fn run_tesseract(png_path: &std::path::Path) -> Result<String> {
    let output = tokio::process::Command::new("tesseract")
        .arg(png_path)
        .arg("stdout")
        .args(["--psm", "6"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .await;

    let output = match output {
        Ok(out) => out,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("tesseract binary not found in PATH; install tesseract-ocr to use OCR features")
        }
        Err(e) => return Err(e).context("failed to spawn tesseract"),
    };
    if !output.status.success() {
        bail!("tesseract failed with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Unique scratch path in the system temp dir (no tempfile crate at
/// runtime; uniqueness via pid + monotonic counter).
fn temp_png_path() -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("vmlab-ocr-{}-{n}.png", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgb;

    /// 7-row bitmap glyphs (variable width), one string per row, '#' = ink.
    /// The O gets octagonal corners so tesseract does not read it as D.
    const GLYPH_H: &[&str] = &[
        "#...#", "#...#", "#...#", "#####", "#...#", "#...#", "#...#",
    ];
    const GLYPH_E: &[&str] = &[
        "#####", "#....", "#....", "####.", "#....", "#....", "#####",
    ];
    const GLYPH_L: &[&str] = &[
        "#....", "#....", "#....", "#....", "#....", "#....", "#####",
    ];
    const GLYPH_O: &[&str] = &[
        "..###..", ".#...#.", "#.....#", "#.....#", "#.....#", ".#...#.", "..###..",
    ];
    const HELLO: &[&[&str]] = &[GLYPH_H, GLYPH_E, GLYPH_L, GLYPH_L, GLYPH_O];

    /// Total column count of a glyph line, including 1-column gaps.
    fn line_cols(glyphs: &[&[&str]]) -> u32 {
        glyphs.iter().map(|g| g[0].len() as u32 + 1).sum::<u32>() - 1
    }

    /// Render glyphs black-on-white at `scale`, starting at (ox, oy).
    fn draw_text(img: &mut RgbImage, glyphs: &[&[&str]], ox: u32, oy: u32, scale: u32) {
        let mut gx = ox;
        for glyph in glyphs {
            for (row, line) in glyph.iter().enumerate() {
                for (col, ch) in line.bytes().enumerate() {
                    if ch != b'#' {
                        continue;
                    }
                    for dy in 0..scale {
                        for dx in 0..scale {
                            img.put_pixel(
                                gx + col as u32 * scale + dx,
                                oy + row as u32 * scale + dy,
                                Rgb([0, 0, 0]),
                            );
                        }
                    }
                }
            }
            gx += (glyph[0].len() as u32 + 1) * scale;
        }
    }

    /// Halve the image with a triangle filter: anti-aliases the blocky
    /// bitmap glyphs, which tesseract reads much more reliably.
    fn smooth_half(img: &RgbImage) -> RgbImage {
        imageops::resize(
            img,
            img.width() / 2,
            img.height() / 2,
            imageops::FilterType::Triangle,
        )
    }

    fn tesseract_available() -> bool {
        std::process::Command::new("tesseract")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    }

    #[tokio::test]
    async fn ocr_reads_bitmap_text() {
        if !tesseract_available() {
            eprintln!("skipping ocr_reads_bitmap_text: tesseract not installed");
            return;
        }
        // "HELLO" rendered at 16x, smoothed down to 8x, black on white.
        let scale = 16;
        let mut img =
            RgbImage::from_pixel(line_cols(HELLO) * scale + 64, 7 * scale + 64, Rgb([255; 3]));
        draw_text(&mut img, HELLO, 32, 32, scale);
        let img = smooth_half(&img);

        let text = ocr(&img, None).await.unwrap();
        // OCR is fuzzy and tesseract versions differ on edge glyphs (some read
        // "HELL", dropping the trailing O), so accept 4 of the 5 letters in
        // order — enough to prove the render -> tesseract -> text pipeline works.
        let got: String = text
            .to_uppercase()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect();
        assert!(
            got.contains("HELL") || got.contains("ELLO"),
            "tesseract output was: {text:?}"
        );
    }

    #[tokio::test]
    async fn ocr_region_crops_to_target_text() {
        if !tesseract_available() {
            eprintln!("skipping ocr_region_crops_to_target_text: tesseract not installed");
            return;
        }
        // "HELLO" on the top row, "EH" on the bottom; OCR only the bottom.
        let scale = 16;
        let row_h = 7 * scale + 64;
        let mut img = RgbImage::from_pixel(line_cols(HELLO) * scale + 64, 2 * row_h, Rgb([255; 3]));
        draw_text(&mut img, HELLO, 32, 32, scale);
        draw_text(&mut img, &[GLYPH_E, GLYPH_H], 32, row_h + 32, scale);
        let img = smooth_half(&img);

        let region = Some((0, row_h / 2, img.width(), row_h / 2));
        let text = ocr(&img, region).await.unwrap().to_uppercase();
        assert!(text.contains("EH"), "tesseract output was: {text:?}");
        assert!(!text.contains("HELLO"), "region not respected: {text:?}");
    }

    #[tokio::test]
    async fn ocr_region_outside_image_is_error() {
        let img = RgbImage::from_pixel(10, 10, Rgb([255; 3]));
        assert!(ocr(&img, Some((20, 0, 5, 5))).await.is_err());
    }
}
