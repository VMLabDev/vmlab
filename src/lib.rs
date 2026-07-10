//! vmlab as a library: the CLI binary (`src/main.rs`) and the web binary
//! (`src/web/main.rs`) both build on these modules. Only the surface the web
//! binary needs is `pub` (`cli`, `proto`, `paths`, plus `config`, `profiles`
//! and `template` for the visual lab editor and its catalog pickers); the
//! rest stays crate-internal and is reached via `crate::…` as before.

pub mod cli;
pub mod config;
pub mod guest_asset;
pub mod lab_init;
mod labd;
mod lifecycle;
pub mod logs;
mod media;
mod net;
mod oci;
pub mod paths;
pub mod profiles;
pub mod proto;
mod qemu;
mod qga;
mod qmp;
mod scripting;
mod smb;
mod supervisor;
mod sync;
pub mod template;
mod viewer;
mod vision;
mod vnc;
