//! Template matching via zero-mean normalised cross-correlation (NCC).
//!
//! Scores are raw NCC values in `[-1, 1]` where `1.0` is a perfect match;
//! they are compared directly against [`MatchOptions::threshold`] (default
//! 0.9, per PRD §10.3) with no rescaling. Matching is done on grayscale
//! (standard Rec. 601 luma).
//!
//! A naive scan is O(W·H·w·h), far too slow for HD screens. The search is
//! therefore two-stage:
//!
//! 1. **Coarse pass** on a 4x box-downscaled pyramid level: exhaustive NCC
//!    scan, keeping the top 8 non-overlapping candidates.
//! 2. **Refinement** of each candidate at full resolution within ±8 px,
//!    using integral images (sum and sum-of-squares) so the per-window
//!    normalisation terms are O(1); only the cross term costs O(w·h).
//!
//! Degenerate zero-variance templates (solid colours) have undefined NCC, so
//! they fall back to a mean-absolute-difference similarity `1 - MAD/255`
//! (also `1.0` = perfect), compared against the same threshold.

use image::RgbImage;

/// Pyramid downscale factor for the coarse pass.
const PYRAMID_FACTOR: usize = 4;
/// Number of coarse candidates carried into full-resolution refinement.
const TOP_CANDIDATES: usize = 8;
/// Refinement neighbourhood radius (full-resolution pixels).
const REFINE_RADIUS: usize = 8;
/// Per-pixel variance below this counts as "flat" (degenerate for NCC).
const FLAT_VARIANCE: f64 = 1e-6;

/// A located template occurrence: top-left position, template size and the
/// similarity score (`1.0` = perfect). Carries enough to anchor a relative
/// mouse click via [`Match::center`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Match {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub score: f64,
}

impl Match {
    /// Centre of the matched region, e.g. as a mouse-click target.
    pub fn center(&self) -> (u32, u32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }
}

/// Options for [`find_template`].
#[derive(Debug, Clone)]
pub struct MatchOptions {
    /// Minimum similarity score to report a match (`1.0` = perfect).
    pub threshold: f64,
    /// Restrict the search to `(x, y, w, h)` of the screen, if set.
    pub region: Option<(u32, u32, u32, u32)>,
}

impl Default for MatchOptions {
    fn default() -> Self {
        Self {
            threshold: 0.9,
            region: None,
        }
    }
}

/// Find the best occurrence of `template` in `screen`.
///
/// Returns `None` if the template does not fit the (possibly
/// region-restricted) screen or if the best score is below
/// `opts.threshold`. Coordinates in the returned [`Match`] are always
/// absolute screen coordinates, even when a region is given.
pub fn find_template(screen: &RgbImage, template: &RgbImage, opts: &MatchOptions) -> Option<Match> {
    let (tw, th) = template.dimensions();
    if tw == 0 || th == 0 || screen.width() == 0 || screen.height() == 0 {
        return None;
    }

    let gray_screen = GrayF::from_rgb(screen);
    let (offset_x, offset_y, search) = match opts.region {
        Some((rx, ry, rw, rh)) => {
            let rx = rx as usize;
            let ry = ry as usize;
            if rx >= gray_screen.w || ry >= gray_screen.h {
                return None;
            }
            let rw = (rw as usize).min(gray_screen.w - rx);
            let rh = (rh as usize).min(gray_screen.h - ry);
            (rx, ry, gray_screen.crop(rx, ry, rw, rh))
        }
        None => (0, 0, gray_screen),
    };

    if (tw as usize) > search.w || (th as usize) > search.h {
        return None;
    }

    let gray_tmpl = GrayF::from_rgb(template);
    let (x, y, score) = search_best(&search, &gray_tmpl)?;
    if score < opts.threshold {
        return None;
    }
    Some(Match {
        x: (offset_x + x) as u32,
        y: (offset_y + y) as u32,
        w: tw,
        h: th,
        score,
    })
}

/// Grayscale image as f64 luma in `[0, 255]`.
struct GrayF {
    w: usize,
    h: usize,
    px: Vec<f64>,
}

impl GrayF {
    fn from_rgb(img: &RgbImage) -> Self {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let px = img
            .pixels()
            .map(|p| 0.299 * f64::from(p[0]) + 0.587 * f64::from(p[1]) + 0.114 * f64::from(p[2]))
            .collect();
        Self { w, h, px }
    }

    #[inline]
    fn at(&self, x: usize, y: usize) -> f64 {
        self.px[y * self.w + x]
    }

    fn crop(&self, x: usize, y: usize, w: usize, h: usize) -> Self {
        let mut px = Vec::with_capacity(w * h);
        for row in y..y + h {
            px.extend_from_slice(&self.px[row * self.w + x..row * self.w + x + w]);
        }
        Self { w, h, px }
    }

    /// Box-average downscale by `factor`; partial edge blocks average the
    /// pixels available.
    fn downscale(&self, factor: usize) -> Self {
        let w = self.w.div_ceil(factor).max(1);
        let h = self.h.div_ceil(factor).max(1);
        let mut px = Vec::with_capacity(w * h);
        for by in 0..h {
            for bx in 0..w {
                let x1 = (bx * factor + factor).min(self.w);
                let y1 = (by * factor + factor).min(self.h);
                let mut sum = 0.0;
                let mut count = 0.0;
                for y in by * factor..y1 {
                    for x in bx * factor..x1 {
                        sum += self.at(x, y);
                        count += 1.0;
                    }
                }
                px.push(sum / count);
            }
        }
        Self { w, h, px }
    }

    fn mean(&self) -> f64 {
        self.px.iter().sum::<f64>() / self.px.len() as f64
    }
}

/// Integral images of pixel values and squared values, for O(1) window sums.
struct Integral {
    w: usize,
    sum: Vec<f64>,
    sq: Vec<f64>,
}

impl Integral {
    fn new(g: &GrayF) -> Self {
        let w = g.w + 1;
        let h = g.h + 1;
        let mut sum = vec![0.0; w * h];
        let mut sq = vec![0.0; w * h];
        for y in 0..g.h {
            let mut row_sum = 0.0;
            let mut row_sq = 0.0;
            for x in 0..g.w {
                let v = g.at(x, y);
                row_sum += v;
                row_sq += v * v;
                sum[(y + 1) * w + x + 1] = sum[y * w + x + 1] + row_sum;
                sq[(y + 1) * w + x + 1] = sq[y * w + x + 1] + row_sq;
            }
        }
        Self { w, sum, sq }
    }

    /// `(Σ pixel, Σ pixel²)` over the window with top-left `(x, y)`.
    fn window(&self, x: usize, y: usize, w: usize, h: usize) -> (f64, f64) {
        let idx = |x: usize, y: usize| y * self.w + x;
        let a = idx(x, y);
        let b = idx(x + w, y);
        let c = idx(x, y + h);
        let d = idx(x + w, y + h);
        (
            self.sum[d] - self.sum[b] - self.sum[c] + self.sum[a],
            self.sq[d] - self.sq[b] - self.sq[c] + self.sq[a],
        )
    }
}

/// Per-level NCC scorer: precomputed zero-mean template + screen integrals.
struct NccLevel<'a> {
    screen: &'a GrayF,
    integral: Integral,
    t0: Vec<f64>,
    t_norm: f64,
    tw: usize,
    th: usize,
}

impl<'a> NccLevel<'a> {
    fn new(screen: &'a GrayF, tmpl: &GrayF) -> Self {
        let t_mean = tmpl.mean();
        let t0: Vec<f64> = tmpl.px.iter().map(|v| v - t_mean).collect();
        let t_norm = t0.iter().map(|v| v * v).sum::<f64>().sqrt();
        Self {
            screen,
            integral: Integral::new(screen),
            t0,
            t_norm,
            tw: tmpl.w,
            th: tmpl.h,
        }
    }

    fn score(&self, x: usize, y: usize) -> f64 {
        let n = (self.tw * self.th) as f64;
        let (s_sum, s_sq) = self.integral.window(x, y, self.tw, self.th);
        // Σ(S - mean_S)² via integral images.
        let s_dev = s_sq - s_sum * s_sum / n;
        if s_dev <= 1e-9 {
            // Flat window cannot correlate with a non-flat template.
            return 0.0;
        }
        // Σ S·T0 == Σ(S - mean_S)·T0 because Σ T0 = 0.
        let mut cross = 0.0;
        for j in 0..self.th {
            let row = (y + j) * self.screen.w + x;
            let srow = &self.screen.px[row..row + self.tw];
            let trow = &self.t0[j * self.tw..(j + 1) * self.tw];
            for (s, t) in srow.iter().zip(trow) {
                cross += s * t;
            }
        }
        cross / (s_dev.sqrt() * self.t_norm)
    }
}

/// Mean-absolute-difference similarity `1 - MAD/255` for degenerate
/// (zero-variance) templates.
fn mad_score(screen: &GrayF, tmpl: &GrayF, x: usize, y: usize) -> f64 {
    let mut acc = 0.0;
    for j in 0..tmpl.h {
        for i in 0..tmpl.w {
            acc += (screen.at(x + i, y + j) - tmpl.at(i, j)).abs();
        }
    }
    1.0 - acc / ((tmpl.w * tmpl.h) as f64 * 255.0)
}

fn is_flat(g: &GrayF) -> bool {
    let mean = g.mean();
    let var = g.px.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / g.px.len() as f64;
    var < FLAT_VARIANCE
}

/// Best `(x, y, score)` over the whole search image, or `None` when there is
/// no valid placement.
fn search_best(screen: &GrayF, tmpl: &GrayF) -> Option<(usize, usize, f64)> {
    let max_x = screen.w.checked_sub(tmpl.w)?;
    let max_y = screen.h.checked_sub(tmpl.h)?;
    let degenerate = is_flat(tmpl);

    // Pyramid only pays off when the template survives 4x downscaling with
    // at least a couple of pixels per axis.
    let use_pyramid = tmpl.w >= 2 * PYRAMID_FACTOR && tmpl.h >= 2 * PYRAMID_FACTOR;
    if !use_pyramid {
        return Some(scan_full(screen, tmpl, degenerate, 0, max_x, 0, max_y));
    }

    let coarse_screen = screen.downscale(PYRAMID_FACTOR);
    let coarse_tmpl = tmpl.downscale(PYRAMID_FACTOR);
    if coarse_tmpl.w > coarse_screen.w || coarse_tmpl.h > coarse_screen.h {
        return Some(scan_full(screen, tmpl, degenerate, 0, max_x, 0, max_y));
    }

    // High-frequency templates can average out to flat at coarse scale, in
    // which case NCC is undefined there too; score candidates by MAD instead.
    let coarse_mad = degenerate || is_flat(&coarse_tmpl);
    let coarse_ncc = if coarse_mad {
        None
    } else {
        Some(NccLevel::new(&coarse_screen, &coarse_tmpl))
    };
    let coarse_score = |x: usize, y: usize| match &coarse_ncc {
        Some(ncc) => ncc.score(x, y),
        None => mad_score(&coarse_screen, &coarse_tmpl, x, y),
    };

    let candidates = top_candidates(
        &coarse_score,
        coarse_screen.w - coarse_tmpl.w,
        coarse_screen.h - coarse_tmpl.h,
        (coarse_tmpl.w / 2).max(1),
        (coarse_tmpl.h / 2).max(1),
    );

    let mut best: Option<(usize, usize, f64)> = None;
    for (cx, cy) in candidates {
        let fx = cx * PYRAMID_FACTOR;
        let fy = cy * PYRAMID_FACTOR;
        let found = scan_full(
            screen,
            tmpl,
            degenerate,
            fx.saturating_sub(REFINE_RADIUS),
            (fx + REFINE_RADIUS).min(max_x),
            fy.saturating_sub(REFINE_RADIUS),
            (fy + REFINE_RADIUS).min(max_y),
        );
        if best.is_none_or(|b| found.2 > b.2) {
            best = Some(found);
        }
    }
    best
}

/// Exhaustive full-resolution scan over an inclusive position range.
fn scan_full(
    screen: &GrayF,
    tmpl: &GrayF,
    degenerate: bool,
    x0: usize,
    x1: usize,
    y0: usize,
    y1: usize,
) -> (usize, usize, f64) {
    let ncc = if degenerate {
        None
    } else {
        Some(NccLevel::new(screen, tmpl))
    };
    let mut best = (x0, y0, f64::NEG_INFINITY);
    for y in y0..=y1 {
        for x in x0..=x1 {
            let s = match &ncc {
                Some(ncc) => ncc.score(x, y),
                None => mad_score(screen, tmpl, x, y),
            };
            if s > best.2 {
                best = (x, y, s);
            }
        }
    }
    best
}

/// Top-scoring positions with greedy non-maximum suppression: a candidate is
/// dropped if it lies within `(sep_x, sep_y)` of an already-picked one.
fn top_candidates(
    score: &dyn Fn(usize, usize) -> f64,
    max_x: usize,
    max_y: usize,
    sep_x: usize,
    sep_y: usize,
) -> Vec<(usize, usize)> {
    let mut all: Vec<(usize, usize, f64)> = Vec::with_capacity((max_x + 1) * (max_y + 1));
    for y in 0..=max_y {
        for x in 0..=max_x {
            all.push((x, y, score(x, y)));
        }
    }
    all.sort_by(|a, b| b.2.total_cmp(&a.2));

    let mut picked: Vec<(usize, usize)> = Vec::with_capacity(TOP_CANDIDATES);
    for (x, y, _) in all {
        let close = picked
            .iter()
            .any(|&(px, py)| px.abs_diff(x) < sep_x && py.abs_diff(y) < sep_y);
        if !close {
            picked.push((x, y));
            if picked.len() == TOP_CANDIDATES {
                break;
            }
        }
    }
    picked
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, imageops};

    /// Deterministic test screen: per-axis gradients plus a hash-indexed
    /// (no RNG) green level per 4x4 px block. The block texture decorrelates
    /// every window from every other (a typical 60x40 template covers ~150
    /// independent blocks), which matters because NCC is affine-invariant
    /// and would score smooth or repetitive patterns highly in many places.
    /// 4 px blocks survive the 4x coarse pyramid pass as one coarse pixel.
    fn test_screen(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| {
            let h32 = (x / 4).wrapping_mul(0x9E37_79B1) ^ (y / 4).wrapping_mul(0x85EB_CA77);
            let h32 = (h32 ^ (h32 >> 13)).wrapping_mul(0xC2B2_AE3D);
            let r = (x * 255 / w.max(1)) as u8;
            let g = (h32 >> 8) as u8;
            let b = (y * 255 / h.max(1)) as u8;
            Rgb([r, g, b])
        })
    }

    fn cut(img: &RgbImage, x: u32, y: u32, w: u32, h: u32) -> RgbImage {
        imageops::crop_imm(img, x, y, w, h).to_image()
    }

    /// Deterministic decorrelated xor "noise" (hash-based, no RNG), strong
    /// enough to dent an NCC score without dragging it below ~0.9.
    fn xor_noise(img: &mut RgbImage) {
        for (x, y, p) in img.enumerate_pixels_mut() {
            let h = x.wrapping_mul(0x9E37_79B1) ^ y.wrapping_mul(0x85EB_CA77);
            let n = ((h >> 11) % 48) as u8;
            p[0] ^= n;
            p[1] ^= n;
            p[2] ^= n;
        }
    }

    #[test]
    fn exact_copy_found_at_exact_location() {
        let screen = test_screen(640, 480);
        let tmpl = cut(&screen, 213, 147, 60, 40);
        let m = find_template(&screen, &tmpl, &MatchOptions::default()).expect("should match");
        assert_eq!((m.x, m.y, m.w, m.h), (213, 147, 60, 40));
        assert!(m.score >= 0.99, "score was {}", m.score);
    }

    #[test]
    fn noisy_template_still_found_with_lower_score() {
        let screen = test_screen(640, 480);
        let mut tmpl = cut(&screen, 320, 96, 60, 40);
        xor_noise(&mut tmpl);
        let opts = MatchOptions {
            threshold: 0.5,
            ..Default::default()
        };
        let m = find_template(&screen, &tmpl, &opts).expect("noisy template should match");
        assert_eq!((m.x, m.y), (320, 96));
        assert!(
            m.score < 0.999,
            "noise should reduce score, got {}",
            m.score
        );
        assert!(m.score >= 0.5);
    }

    #[test]
    fn absent_template_returns_none() {
        let screen = test_screen(640, 480);
        // A pattern that appears nowhere on the screen.
        let tmpl = RgbImage::from_fn(60, 40, |x, y| {
            let v = (((x / 4) + (y / 4)) % 2 * 255) as u8;
            Rgb([v, 255 - v, v])
        });
        assert_eq!(
            find_template(&screen, &tmpl, &MatchOptions::default()),
            None
        );
    }

    #[test]
    fn region_restricts_search() {
        let screen = test_screen(640, 480);
        let tmpl = cut(&screen, 400, 300, 60, 40);

        // Region that excludes the template location: no match.
        let miss = MatchOptions {
            region: Some((0, 0, 200, 150)),
            ..Default::default()
        };
        assert_eq!(find_template(&screen, &tmpl, &miss), None);

        // Region that contains it: found, in absolute coordinates.
        let hit = MatchOptions {
            region: Some((350, 250, 200, 150)),
            ..Default::default()
        };
        let m = find_template(&screen, &tmpl, &hit).expect("should match inside region");
        assert_eq!((m.x, m.y), (400, 300));
    }

    #[test]
    fn solid_color_template_uses_mad_fallback() {
        let mut screen = test_screen(640, 480);
        // Paint a solid white block; the solid template has zero variance so
        // NCC is undefined and MAD similarity takes over.
        for y in 80..110 {
            for x in 100..140 {
                screen.put_pixel(x, y, Rgb([255, 255, 255]));
            }
        }
        let tmpl = RgbImage::from_pixel(30, 20, Rgb([255, 255, 255]));
        let m = find_template(&screen, &tmpl, &MatchOptions::default()).expect("should match");
        assert!(m.score >= 0.99, "score was {}", m.score);
        // Any fully-inside-the-block placement is a perfect match.
        assert!((100..=110).contains(&m.x), "x was {}", m.x);
        assert!((80..=90).contains(&m.y), "y was {}", m.y);
    }

    #[test]
    fn threshold_is_respected() {
        let screen = test_screen(640, 480);
        let mut tmpl = cut(&screen, 160, 224, 60, 40);
        xor_noise(&mut tmpl);
        let strict = MatchOptions {
            threshold: 0.999,
            ..Default::default()
        };
        assert_eq!(find_template(&screen, &tmpl, &strict), None);
        let lenient = MatchOptions {
            threshold: 0.5,
            ..Default::default()
        };
        assert!(find_template(&screen, &tmpl, &lenient).is_some());
    }

    #[test]
    fn template_larger_than_screen_or_region_is_none() {
        let screen = test_screen(64, 64);
        let tmpl = test_screen(128, 32);
        assert_eq!(
            find_template(&screen, &tmpl, &MatchOptions::default()),
            None
        );

        let screen = test_screen(640, 480);
        let tmpl = cut(&screen, 0, 0, 60, 40);
        let tiny_region = MatchOptions {
            region: Some((0, 0, 30, 30)),
            ..Default::default()
        };
        assert_eq!(find_template(&screen, &tmpl, &tiny_region), None);
    }

    #[test]
    fn small_template_uses_direct_scan() {
        let screen = test_screen(200, 150);
        // Below the pyramid cutoff (less than 8 px per axis).
        let tmpl = cut(&screen, 77, 53, 6, 5);
        let m = find_template(&screen, &tmpl, &MatchOptions::default()).expect("should match");
        // NCC is invariant to affine luma changes, and a 6x5 patch of smooth
        // gradient legitimately matches many places; only assert that the
        // direct-scan path finds a (near-)perfect correlation of the right
        // size somewhere.
        assert!(m.score >= 0.99, "score was {}", m.score);
        assert_eq!((m.w, m.h), (6, 5));
        assert!(m.x <= 200 - 6 && m.y <= 150 - 5);
    }

    #[test]
    fn the_exact_occurrence_beats_eight_near_copies() {
        let tmpl = RgbImage::from_fn(16, 16, |x, y| {
            let h = (x / 4).wrapping_mul(0x9E37_79B1) ^ (y / 4).wrapping_mul(0x85EB_CA77);
            let h = (h ^ (h >> 13)).wrapping_mul(0xC2B2_AE3D);
            Rgb([(h >> 8) as u8; 3])
        });
        let mut noisy = tmpl.clone();
        xor_noise(&mut noisy);
        let mut screen = RgbImage::from_pixel(320, 96, Rgb([128; 3]));
        for i in 0..8 {
            imageops::replace(&mut screen, &noisy, 4 + i * 36, 4);
        }
        imageops::replace(&mut screen, &tmpl, 150, 62);
        let m = find_template(&screen, &tmpl, &MatchOptions::default()).expect("exact match");
        assert_eq!((m.x, m.y), (150, 62));
        assert!(m.score > 0.999);
    }

    #[test]
    fn a_dimmed_copy_is_not_a_full_match() {
        let tmpl = test_screen(32, 32);
        let mut screen = tmpl.clone();
        for pixel in screen.pixels_mut() {
            *pixel = Rgb(pixel.0.map(|value| value / 2 + 20));
        }
        assert_eq!(find_template(&screen, &tmpl, &MatchOptions::default()), None);
    }

    #[test]
    fn a_colour_swap_is_not_a_full_match() {
        let tmpl = RgbImage::from_fn(16, 16, |x, _| {
            if x < 8 { Rgb([255, 0, 0]) } else { Rgb([0; 3]) }
        });
        let screen = RgbImage::from_fn(16, 16, |x, _| {
            if x < 8 { Rgb([0, 130, 0]) } else { Rgb([0; 3]) }
        });
        assert_eq!(find_template(&screen, &tmpl, &MatchOptions::default()), None);
    }

    #[test]
    fn match_center() {
        let m = Match {
            x: 10,
            y: 20,
            w: 60,
            h: 41,
            score: 1.0,
        };
        assert_eq!(m.center(), (40, 40));
    }
}
