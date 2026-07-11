//! Pulling **container images** (as opposed to vmlab template artifacts)
//! and flattening them into single squashfs root filesystems.
//!
//! Labs will run OCI containers inside a helper VM; the host side only needs
//! to fetch an image and produce one immutable rootfs file per image:
//!
//! - [`reference`] — `[host/]name[:tag][@sha256:…]` parsing with Docker
//!   shorthand normalisation (`nginx` → `registry-1.docker.io/library/nginx`).
//! - [`model`] — serde model of the OCI/docker image config blob (runtime
//!   defaults like `Env`/`Cmd`, and the `rootfs.diff_ids` layer digests).
//! - [`cache`] — the digest-addressed on-disk cache under
//!   [`crate::paths::oci_cache_dir()`], with the template store's
//!   flock + stage-then-rename discipline.
//! - [`flatten`] — apply the layer stack (whiteouts, opaque dirs, last
//!   writer wins) and stream the merged tree into `sqfstar` → `rootfs.sqfs`.
//! - [`pull`] — [`ensure_container_image`]: resolve, download, verify,
//!   flatten, cache; fully offline when the image is already cached.

// The lab-runtime wiring that consumes this module lands separately; until it
// does, only the tests reach it, so allow dead_code for the subtree rather
// than annotating every item. Remove when the container runtime is wired in.
#![allow(dead_code)]

pub mod cache;
pub mod flatten;
pub mod model;
pub mod pull;
pub mod reference;

// Some of these re-exports have no in-crate consumer until the runtime
// wiring lands (see the allow above) — silence unused_imports the same way.
#[allow(unused_imports)]
pub use cache::ImageCache;
#[allow(unused_imports)]
pub use model::{ImageConfig, oci_arch};
#[allow(unused_imports)]
pub use pull::{ImagePullProgress, PulledImage, cached_container_image, ensure_container_image};
#[allow(unused_imports)]
pub use reference::{ImageReference, Selector, parse_image_reference};
