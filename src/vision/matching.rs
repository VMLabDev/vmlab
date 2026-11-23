//! Full-resolution template matching in RGB.
//!
//! The score is one minus the root mean square channel difference divided by
//! 255. It ranges from 0 to 1, with 1 for identical pixels. The search visits
//! placements in row-major order and stops scoring a window once its partial
//! error rules out meeting the threshold or improving the current match.

use image::RgbImage;

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

    let (left, top, width, height) = opts
        .region
        .unwrap_or((0, 0, screen.width(), screen.height()));
    let width = width.min(screen.width().saturating_sub(left));
    let height = height.min(screen.height().saturating_sub(top));
    if tw > width || th > height {
        return None;
    }

    let (x, y, score) = search_best(
        screen,
        template,
        left as usize,
        top as usize,
        width as usize,
        height as usize,
        opts.threshold,
    )?;
    Some(Match {
        x: x as u32,
        y: y as u32,
        w: tw,
        h: th,
        score,
    })
}

fn similarity(error: u64, channels: usize) -> f64 {
    1.0 - (error as f64 / channels as f64).sqrt() / 255.0
}

/// Invert the score using the same rounding as the final comparison.
fn error_budget(channels: usize, threshold: f64) -> Option<u64> {
    if threshold > 1.0 {
        return None;
    }
    let mut low = 0;
    let mut high = (channels as u64).saturating_mul(255 * 255);
    while low < high {
        let middle = high - (high - low) / 2;
        if similarity(middle, channels) < threshold {
            high = middle - 1;
        } else {
            low = middle;
        }
    }
    Some(low)
}

/// Sum over at most 4096 channels, so the total fits in a u32.
fn squared_error(actual: &[u8], expected: &[u8]) -> u32 {
    actual
        .iter()
        .zip(expected)
        .map(|(&a, &b)| {
            let delta = i16::from(a) - i16::from(b);
            // The squared difference fits in an unsigned 16-bit lane.
            u32::from(delta.wrapping_mul(delta) as u16)
        })
        .sum()
}


/// Find the smallest sum of squared RGB channel differences in budget.
fn search_best(
    screen: &RgbImage,
    template: &RgbImage,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    threshold: f64,
) -> Option<(usize, usize, f64)> {
    let screen_stride = screen.width() as usize * 3;
    let template_stride = template.width() as usize * 3;
    let template_rows = template.as_raw().chunks_exact(template_stride);
    let pixels = screen.as_raw();
    let channels = template.as_raw().len();
    let mut budget = error_budget(channels, threshold)?;
    let mut best = None;

    for y in top..=top + height - template.height() as usize {
        'placement: for x in left..=left + width - template.width() as usize {
            let mut error = 0;
            for (row, expected) in template_rows.clone().enumerate() {
                let start = (y + row) * screen_stride + x * 3;
                let actual = &pixels[start..start + template_stride];
                // A u32 sum vectorises more efficiently. Each chunk's
                // maximum error fits even for very wide templates.
                for (actual, expected) in actual.chunks(4096).zip(expected.chunks(4096)) {
                    error += u64::from(squared_error(actual, expected));
                    if error > budget {
                        continue 'placement;
                    }
                }
            }
            if error == 0 {
                return Some((x, y, 1.0));
            }
            best = Some((x, y, error));
            budget = error - 1;
        }
    }

    best.map(|(x, y, error)| (x, y, similarity(error, channels)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, imageops};

    /// Deterministic test screen: per-axis gradients plus a hash-indexed
    /// (no RNG) green level per 4x4 px block. The texture distinguishes
    /// patches of the screen even under moderate pixel noise.
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

    /// Deterministic hash-based pixel noise.
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
    fn solid_color_template_is_matched() {
        let mut screen = test_screen(640, 480);
        // Paint a solid white block larger than the template.
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
    fn small_template_matches_exactly() {
        let screen = test_screen(200, 150);
        let tmpl = cut(&screen, 77, 53, 6, 5);
        let m = find_template(&screen, &tmpl, &MatchOptions::default()).expect("should match");
        assert_eq!((m.x, m.y, m.w, m.h), (77, 53, 6, 5));
        assert_eq!(m.score, 1.0);
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
        // The alternating changes cancel in each coarse block, while
        // lowering the old full-resolution correlation below 0.9.
        let strongly_noisy = RgbImage::from_fn(16, 16, |x, y| {
            let base = tmpl.get_pixel(x, y)[0] / 2 + 64;
            Rgb([if x % 2 == 0 { base - 24 } else { base + 24 }; 3])
        });
        for decoy in [&noisy, &strongly_noisy] {
            let mut screen = RgbImage::from_pixel(320, 96, Rgb([128; 3]));
            for i in 0..8 {
                imageops::replace(&mut screen, decoy, 4 + i * 36, 4);
            }
            imageops::replace(&mut screen, &tmpl, 150, 62);
            let m = find_template(&screen, &tmpl, &MatchOptions::default()).expect("exact match");
            assert_eq!((m.x, m.y), (150, 62));
            assert!(m.score > 0.999);
        }
    }

    #[test]
    fn a_dimmed_copy_is_not_a_full_match() {
        let tmpl = test_screen(32, 32);
        let mut screen = tmpl.clone();
        for pixel in screen.pixels_mut() {
            *pixel = Rgb(pixel.0.map(|value| value / 2 + 20));
        }
        assert_eq!(
            find_template(&screen, &tmpl, &MatchOptions::default()),
            None
        );
    }

    #[test]
    fn a_colour_swap_is_not_a_full_match() {
        let shape =
            |colour| RgbImage::from_fn(16, 16, |x, _| if x < 8 { colour } else { Rgb([0; 3]) });
        let tmpl = shape(Rgb([255, 0, 0]));
        let screen = shape(Rgb([0, 130, 0]));
        assert_eq!(
            find_template(&screen, &tmpl, &MatchOptions::default()),
            None
        );
    }

    #[test]
    fn hd_search_rejects_absent_template_and_finds_off_grid_occurrence() {
        let mut screen = test_screen(1920, 1080);
        let template = RgbImage::from_fn(64, 64, |x, y| {
            let hash = (x + 1).wrapping_mul(0x9E37_79B1) ^ (y + 1).wrapping_mul(0x85EB_CA77);
            let hash = (hash ^ (hash >> 13)).wrapping_mul(0xC2B2_AE3D);
            Rgb([hash as u8, (hash >> 8) as u8, (hash >> 16) as u8])
        });
        let opts = MatchOptions::default();
        assert_eq!(find_template(&screen, &template, &opts), None);
        imageops::replace(&mut screen, &template, 1731, 951);
        let found = find_template(&screen, &template, &opts).expect("exact HD match");
        assert_eq!((found.x, found.y, found.score), (1731, 951, 1.0));
    }

    #[test]
    fn a_match_at_the_threshold_is_accepted() {
        let screen = RgbImage::from_pixel(2, 1, Rgb([0, 0, 0]));
        let template = RgbImage::from_pixel(1, 1, Rgb([13, 10, 1]));
        let opts = MatchOptions {
            threshold: similarity(270, 3),
            ..Default::default()
        };
        let found = find_template(&screen, &template, &opts).expect("match at threshold");
        assert_eq!((found.x, found.score), (0, opts.threshold));
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
