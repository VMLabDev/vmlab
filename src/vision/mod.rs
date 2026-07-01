//! Vision support for VM automation (PRD §10.3 "Screen").
//!
//! Provides the host-side primitives behind `vm.wait_for_image`,
//! `vm.find_image`, `vm.ocr` and `vm.wait_for_text`:
//!
//! - [`load_screen`] / [`save_png`] — read QMP `screendump` output (PPM) or
//!   PNG reference images, write PNG screenshots.
//! - [`find_template`] — normalised cross-correlation template matching,
//!   returning a [`Match`] (location + score) that can anchor a relative
//!   mouse click.
//! - [`ocr`] — Tesseract-backed text extraction (`vm.wait_for_text` applies
//!   its regex in the scripting layer).
//!
//! Wait/retry loops and lab-relative path resolution live in the scripting
//! layer; this module is pure image-in, result-out.

mod matching;
mod ocr;
mod screenshot;

pub use matching::{Match, MatchOptions, find_template};
pub use ocr::ocr;
pub use screenshot::{load_screen, save_png};
