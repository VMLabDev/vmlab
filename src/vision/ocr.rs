//! OCR via the `tesseract` CLI.
//!
//! Tesseract is driven as a subprocess: the frame is cropped, turned to
//! greyscale and upscaled with a Lanczos3 filter, written to a temporary PNG,
//! then `tesseract <png> stdout --psm <n>` is run with stderr discarded.
//!
//! The upscale is what makes console fonts readable. Tesseract's model is
//! trained on text far larger than an 8x16 cell, and on an unscaled frame it
//! reads OpenCore's builtin font as "Dperating Systen" and Terminal's as
//! "VHLAB"; three times the size, smoothed rather than blocky, it reads them
//! as written. Light-on-dark text needs nothing extra: tesseract 5 inverts
//! each line it finds itself, and inverting or thresholding up front measured
//! no better or worse.
//!
//! Every decision — the crop, the output size, the page segmentation mode —
//! is made by [`plan`] as a value before any pixel is touched (ADR-0003).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail};
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::{GrayImage, RgbImage, imageops};
use serde::{Deserialize, Serialize};

/// The upscale applied when a caller names none.
pub const DEFAULT_SCALE: u32 = 3;
/// The largest upscale a caller may ask for. Beyond four, tesseract's time
/// grows with the area and its accuracy no longer does.
pub const MAX_SCALE: u32 = 4;
/// The most pixels an upscaled image may have: a 1280x800 frame at
/// [`MAX_SCALE`]. A frame whose requested scale would exceed it is scaled
/// less — never below its own size — so a 4K display costs about what a
/// 1280x800 one does.
pub const MAX_SCALED_PIXELS: u64 = 1280 * 800 * 16;

/// How tesseract segments the page: its `--psm`, by what the text looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Layout {
    /// One uniform block of text (psm 6): consoles, terminals, dialogs. The
    /// lines come back in order.
    #[default]
    Block,
    /// Scattered text in no particular order (psm 11): desktops, lock
    /// screens, menus with columns. Finds more of a busy screen, and returns
    /// it a word group per line.
    Sparse,
    /// A single line of text (psm 7), for a region cropped to one line.
    Line,
    /// Tesseract's own page analysis (psm 3): columns and paragraphs.
    Auto,
}

impl Layout {
    /// Every layout, in the order the docs and errors list them.
    pub const ALL: [Layout; 4] = [Layout::Block, Layout::Sparse, Layout::Line, Layout::Auto];

    /// Tesseract's page segmentation mode for this layout.
    pub fn psm(self) -> u8 {
        match self {
            Layout::Block => 6,
            Layout::Sparse => 11,
            Layout::Line => 7,
            Layout::Auto => 3,
        }
    }

    /// The name every surface spells this layout with.
    pub fn name(self) -> &'static str {
        match self {
            Layout::Block => "block",
            Layout::Sparse => "sparse",
            Layout::Line => "line",
            Layout::Auto => "auto",
        }
    }
}

impl std::fmt::Display for Layout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl std::str::FromStr for Layout {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        Layout::ALL
            .into_iter()
            .find(|l| l.name() == s)
            .ok_or_else(|| {
                format!("unknown OCR layout `{s}`; the layouts are block, sparse, line and auto")
            })
    }
}

/// What a caller may ask of one OCR, as every surface spells it: the
/// `vmlab vm ocr` flags, wscript's `opts` map and the wire. Every field
/// optional; [`plan`] fills the defaults and validates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OcrOptions {
    /// Upscale factor, 1 to [`MAX_SCALE`]; [`DEFAULT_SCALE`] when absent. One
    /// reads the frame at its own size, as vmlab did before the upscale.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<u32>,
    /// Page segmentation; [`Layout::Block`] when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
}

/// One OCR, decided: what to crop, the size to scale the crop to, and the
/// page segmentation mode to run tesseract with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OcrPlan {
    /// The `(x, y, w, h)` rectangle of the frame to read, clipped to it.
    pub crop: (u32, u32, u32, u32),
    /// The size the crop is resampled to before tesseract reads it.
    pub scaled: (u32, u32),
    /// Tesseract's `--psm`.
    pub psm: u8,
}

/// Decide one OCR of a `width` x `height` frame. A region whose origin lies
/// outside the frame is an error; one that runs past the edge is clipped. A
/// scale outside 1 to [`MAX_SCALE`] is an error; one that would take the
/// image past [`MAX_SCALED_PIXELS`] is lowered to fit, never below 1.
pub fn plan(
    width: u32,
    height: u32,
    region: Option<(u32, u32, u32, u32)>,
    opts: &OcrOptions,
) -> Result<OcrPlan> {
    let crop = match region {
        Some((x, y, w, h)) => {
            if x >= width || y >= height {
                bail!("OCR region origin ({x}, {y}) lies outside the {width}x{height} image");
            }
            (x, y, w.min(width - x), h.min(height - y))
        }
        None => (0, 0, width, height),
    };
    let scale = opts.scale.unwrap_or(DEFAULT_SCALE);
    if !(1..=MAX_SCALE).contains(&scale) {
        bail!("OCR scale {scale} is outside 1 to {MAX_SCALE}");
    }
    let (w, h) = (crop.2, crop.3);
    let area = u64::from(w) * u64::from(h);
    let factor = if area * u64::from(scale * scale) <= MAX_SCALED_PIXELS {
        f64::from(scale)
    } else {
        (MAX_SCALED_PIXELS as f64 / area as f64).sqrt().max(1.0)
    };
    let scaled = (
        (f64::from(w) * factor).floor().max(1.0) as u32,
        (f64::from(h) * factor).floor().max(1.0) as u32,
    );
    Ok(OcrPlan {
        crop,
        scaled,
        psm: opts.layout.unwrap_or_default().psm(),
    })
}

/// The image tesseract reads for `plan`: the crop, in greyscale, resampled
/// to the planned size with a Lanczos3 filter.
pub fn prepare(img: &RgbImage, plan: &OcrPlan) -> GrayImage {
    let (x, y, w, h) = plan.crop;
    let grey = imageops::grayscale(&*imageops::crop_imm(img, x, y, w, h));
    if plan.scaled == (w, h) {
        return grey;
    }
    imageops::resize(
        &grey,
        plan.scaled.0,
        plan.scaled.1,
        imageops::FilterType::Lanczos3,
    )
}

/// Run OCR over `img`, optionally restricted to a `(x, y, w, h)` region.
///
/// Returns tesseract's stdout text verbatim (trailing whitespace and all).
/// A missing `tesseract` binary is reported as a clear, actionable error.
pub async fn ocr(
    img: &RgbImage,
    region: Option<(u32, u32, u32, u32)>,
    opts: &OcrOptions,
) -> Result<String> {
    let plan = plan(img.width(), img.height(), region, opts)?;
    let prepared = prepare(img, &plan);

    let png_path = temp_png_path();
    write_png(&prepared, &png_path)
        .with_context(|| format!("failed to write OCR scratch PNG {}", png_path.display()))?;

    let result = run_tesseract(&png_path, plan.psm).await;
    let _ = std::fs::remove_file(&png_path); // best effort
    result
}

/// Write the scratch PNG with fast compression: it lives for one tesseract
/// run, and the default level spends longer encoding a 9 MP frame than
/// tesseract spends reading it.
fn write_png(img: &GrayImage, path: &std::path::Path) -> Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let encoder = PngEncoder::new_with_quality(file, CompressionType::Fast, PngFilter::NoFilter);
    img.write_with_encoder(encoder)?;
    Ok(())
}

async fn run_tesseract(png_path: &std::path::Path, psm: u8) -> Result<String> {
    let output = tokio::process::Command::new("tesseract")
        .arg(png_path)
        .arg("stdout")
        .args(["--psm", &psm.to_string()])
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

    fn opts(scale: Option<u32>, layout: Option<Layout>) -> OcrOptions {
        OcrOptions { scale, layout }
    }

    #[test]
    fn the_default_plan_triples_the_whole_frame_as_one_block() {
        let p = plan(1280, 800, None, &OcrOptions::default()).unwrap();
        assert_eq!(
            p,
            OcrPlan {
                crop: (0, 0, 1280, 800),
                scaled: (3840, 2400),
                psm: 6,
            }
        );
    }

    #[test]
    fn scale_one_reads_the_frame_at_its_own_size() {
        let p = plan(1280, 800, None, &opts(Some(1), None)).unwrap();
        assert_eq!(p.scaled, (1280, 800));
    }

    #[test]
    fn a_scale_outside_one_to_four_is_refused() {
        for bad in [0, 5, 100] {
            let err = plan(100, 100, None, &opts(Some(bad), None)).unwrap_err();
            assert!(err.to_string().contains("outside 1 to 4"), "{err}");
        }
        assert!(plan(100, 100, None, &opts(Some(4), None)).is_ok());
    }

    #[test]
    fn a_large_frame_is_scaled_down_to_the_pixel_budget() {
        // 1920x1080 at 3x is 18.7 MP; the budget holds it to 16.4 MP.
        let p = plan(1920, 1080, None, &OcrOptions::default()).unwrap();
        let px = u64::from(p.scaled.0) * u64::from(p.scaled.1);
        assert!(px <= MAX_SCALED_PIXELS, "{px}");
        assert!(p.scaled.0 > 1920 * 2, "{:?}", p.scaled);
        // The aspect ratio survives the lowered factor.
        let ratio = f64::from(p.scaled.0) / f64::from(p.scaled.1);
        assert!((ratio - 1920.0 / 1080.0).abs() < 0.01, "{ratio}");
    }

    #[test]
    fn a_frame_already_past_the_budget_is_never_shrunk() {
        let p = plan(7680, 4320, None, &OcrOptions::default()).unwrap();
        assert_eq!(p.scaled, (7680, 4320));
    }

    #[test]
    fn a_region_is_clipped_and_scaled_alone() {
        let p = plan(
            1280,
            800,
            Some((1200, 700, 500, 500)),
            &OcrOptions::default(),
        )
        .unwrap();
        assert_eq!(p.crop, (1200, 700, 80, 100));
        assert_eq!(p.scaled, (240, 300));
    }

    #[test]
    fn a_region_origin_outside_the_frame_is_refused() {
        assert!(plan(10, 10, Some((20, 0, 5, 5)), &OcrOptions::default()).is_err());
        assert!(plan(10, 10, Some((0, 10, 5, 5)), &OcrOptions::default()).is_err());
    }

    #[test]
    fn each_layout_picks_its_page_segmentation_mode() {
        let psm = |l| plan(10, 10, None, &opts(None, Some(l))).unwrap().psm;
        assert_eq!(psm(Layout::Block), 6);
        assert_eq!(psm(Layout::Sparse), 11);
        assert_eq!(psm(Layout::Line), 7);
        assert_eq!(psm(Layout::Auto), 3);
    }

    #[test]
    fn layouts_parse_by_name_and_refuse_anything_else() {
        for l in Layout::ALL {
            assert_eq!(l.name().parse::<Layout>().unwrap(), l);
        }
        let err = "column".parse::<Layout>().unwrap_err();
        assert!(err.contains("block, sparse, line and auto"), "{err}");
    }

    #[test]
    fn options_round_trip_the_wire_and_default_when_absent() {
        let o = opts(Some(2), Some(Layout::Sparse));
        let v = serde_json::to_value(o).unwrap();
        assert_eq!(v, serde_json::json!({"scale": 2, "layout": "sparse"}));
        assert_eq!(serde_json::from_value::<OcrOptions>(v).unwrap(), o);
        assert_eq!(
            serde_json::from_value::<OcrOptions>(serde_json::json!({})).unwrap(),
            OcrOptions::default()
        );
    }

    #[test]
    fn prepare_crops_greys_and_resamples_to_the_plan() {
        let mut img = RgbImage::from_pixel(8, 6, Rgb([255, 0, 0]));
        img.put_pixel(5, 4, Rgb([255, 255, 255]));
        let p = plan(8, 6, Some((4, 3, 4, 3)), &OcrOptions::default()).unwrap();
        let out = prepare(&img, &p);
        assert_eq!(out.dimensions(), (12, 9));
        // Pure red is mid-dark grey; the white pixel stays the brightest.
        let max = out.pixels().map(|p| p.0[0]).max().unwrap();
        let corner = out.get_pixel(11, 0).0[0];
        assert!(max > corner, "{max} vs {corner}");
        let unscaled = prepare(&img, &plan(8, 6, None, &opts(Some(1), None)).unwrap());
        assert_eq!(unscaled.dimensions(), (8, 6));
        assert_eq!(unscaled.get_pixel(5, 4).0[0], 255);
    }

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

    /// Render glyphs in `ink` at `scale`, starting at (ox, oy).
    fn draw_text(
        img: &mut RgbImage,
        glyphs: &[&[&str]],
        ox: u32,
        oy: u32,
        scale: u32,
        ink: Rgb<u8>,
    ) {
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
                                ink,
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

    /// Uppercased letters and digits only, so the assertions are about the
    /// glyphs and not tesseract's spacing.
    fn letters(text: &str) -> String {
        text.to_uppercase()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .collect()
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
        draw_text(&mut img, HELLO, 32, 32, scale, Rgb([0; 3]));
        let img = smooth_half(&img);

        let text = ocr(&img, None, &OcrOptions::default()).await.unwrap();
        // OCR is fuzzy and tesseract versions differ on edge glyphs (some read
        // "HELL", dropping the trailing O), so accept 4 of the 5 letters in
        // order — enough to prove the render -> tesseract -> text pipeline works.
        let got = letters(&text);
        assert!(
            got.contains("HELL") || got.contains("ELLO"),
            "tesseract output was: {text:?}"
        );
    }

    #[tokio::test]
    async fn ocr_reads_a_small_light_on_dark_console_font() {
        if !tesseract_available() {
            eprintln!("skipping ocr_reads_a_small_light_on_dark_console_font: not installed");
            return;
        }
        // "HELLO" at 2x — a 14-pixel-high glyph, the size of a console cell —
        // grey on black. The upscale is what makes this readable.
        let scale = 2;
        let mut img =
            RgbImage::from_pixel(line_cols(HELLO) * scale + 40, 7 * scale + 40, Rgb([0; 3]));
        draw_text(&mut img, HELLO, 20, 20, scale, Rgb([200; 3]));

        let text = ocr(&img, None, &OcrOptions::default()).await.unwrap();
        let got = letters(&text);
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
        draw_text(&mut img, HELLO, 32, 32, scale, Rgb([0; 3]));
        draw_text(
            &mut img,
            &[GLYPH_E, GLYPH_H],
            32,
            row_h + 32,
            scale,
            Rgb([0; 3]),
        );
        let img = smooth_half(&img);

        let region = Some((0, row_h / 2, img.width(), row_h / 2));
        let text = ocr(&img, region, &OcrOptions::default())
            .await
            .unwrap()
            .to_uppercase();
        assert!(text.contains("EH"), "tesseract output was: {text:?}");
        assert!(!text.contains("HELLO"), "region not respected: {text:?}");
    }

    #[tokio::test]
    async fn ocr_region_outside_image_is_error() {
        let img = RgbImage::from_pixel(10, 10, Rgb([255; 3]));
        assert!(
            ocr(&img, Some((20, 0, 5, 5)), &OcrOptions::default())
                .await
                .is_err()
        );
    }
}
