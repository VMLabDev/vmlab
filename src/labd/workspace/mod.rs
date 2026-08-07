//! The workspace syncer (PRD §19.6, ADR-0014).
//!
//! **The workspace is a guest-local working copy on the machine's own disk;
//! the host directory is canonical; this keeps them in step.** Neither share
//! transport can carry a watched source tree and both fail *silently*, which
//! is the failure the whole design exists to avoid — so nothing here may fail
//! the same way. Every skip, every refusal and every conflict is named.
//!
//! The pieces, in the order one sync pass uses them:
//!
//! - [`ignore`] — the layered rule set: a built-in floor, the repo's
//!   `.gitignore`, then `.vmlabignore` for the delta including negations. An
//!   ignored path is **guest-owned**, never touched in either direction.
//! - [`scan`] — each side's state: the host-side walk, which never follows a
//!   symlink and digests only what the ledger's pre-filter cannot vouch for,
//!   and the guest probe that asks about those same paths.
//! - [`guest`] — the seam every guest-side effect goes through, so "what does
//!   the syncer do to a guest" is one page, and so the applies can be tested
//!   as behaviour rather than inspected as code.
//! - [`ledger`] — the agreement point: a content digest per path plus **each
//!   side's own** size and mtime. Lab-local per (machine, workspace), so
//!   `destroy` wipes it.
//! - [`plan`] — reconciliation as a value (ADR-0003): host state, guest state
//!   and the ledger in, actions and conflicts out, no I/O anywhere in it.
//! - [`apply`] — the executor: temp-name-then-rename in the same directory,
//!   with the ledger written only **after** the rename.
//! - [`watcher`] — the host-side watcher whose per-path debounce keeps the
//!   syncer from reading a file mid-write.
//! - [`windows`] — the three actions a Windows guest costs vmlab, resolved as
//!   a value before the loop starts: the NTFS case-sensitivity flag every
//!   `mkdir` carries, the symlink attempt that warns by name, and the guest's
//!   line-ending setting. Also the two degradations a login declared
//!   `elevated = false` brings, said up front.
//! - [`syncer`] — the lab-daemon-owned task that runs the loop, as the
//!   machine's default login, from after provisioning until the machine stops.
//!
//! **The seed is the first sync pass**, not a separate mechanism: an empty
//! guest tree is simply the case where every host path has no counterpart.

pub mod apply;
pub mod guest;
pub mod ignore;
pub mod ledger;
pub mod plan;
pub mod scan;
pub mod syncer;
pub mod watcher;
pub mod windows;

pub use syncer::{Workspace, WorkspaceSyncers};
pub use windows::preconditions;
