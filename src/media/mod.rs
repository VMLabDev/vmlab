//! Media building (PRD §6.3).
//!
//! Builds ISO and floppy images from folders on disk — unattend files,
//! driver bundles, agent installers, offline payload delivery — with a
//! content-addressed cache so unchanged folders never rebuild.

mod cache;
mod floppy;
mod hash;
mod iso;

pub use cache::MediaCache;
