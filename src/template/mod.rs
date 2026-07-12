//! Template store and supporting machinery (PRD §4, §6, §7.1).
//!
//! - [`meta`] — `template.wcl` metadata read/written beside each disk image.
//! - [`qimg`] — async `qemu-img` wrappers (blank disks, linked clones, info).
//! - [`store`] — the on-disk store at `~/.local/share/vmlab/templates`.

pub mod artefact;
pub mod build;
pub mod cli;
pub mod meta;
pub mod oci_bridge;
pub mod qimg;
pub mod registries;
pub mod store;

pub use meta::{META_FILE, TemplateMeta};
pub use store::TemplateStore;
