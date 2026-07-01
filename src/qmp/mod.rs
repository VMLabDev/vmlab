//! QMP (QEMU Machine Protocol) client.
//!
//! Async client for the QMP control socket QEMU exposes per VM (PRD §3:
//! the lab daemon owns one QMP channel per QEMU process). The protocol is
//! newline-delimited JSON over a unix socket: QEMU sends a greeting
//! (`{"QMP": {...}}`), the client negotiates with `qmp_capabilities`, and
//! from then on commands are `{"execute": ..., "arguments": ..., "id": n}`
//! with responses matched by `id`. Asynchronous events
//! (`{"event": ..., "data": ..., "timestamp": ...}`) arrive interleaved
//! with responses and are fanned out on a broadcast channel.

mod client;
mod error;
mod types;

pub use client::QmpClient;

#[cfg(test)]
mod tests;
