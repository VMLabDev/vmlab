//! Script fault types (trappable runtime errors), shared between the VM
//! (which raises them) and the host boundary (which can carry them
//! through `HostError` when a script callback faults inside a host
//! function). The VM re-exports [`ScriptFault`] as `RuntimeError`.

use std::fmt;

use crate::span::Span;

/// One frame of a runtime stack trace, innermost first.
#[derive(Debug, Clone)]
pub struct TraceFrame {
    /// Name of the function this frame is executing.
    pub function: String,
    /// Span of the instruction the frame was executing when the fault
    /// propagated through it — the fault site for the innermost frame,
    /// the call site for outer frames. `None` when no span is available
    /// (e.g. a synthetic `<host function>` frame).
    pub span: Option<Span>,
}

/// A trappable runtime fault. Carries the source span of the faulting
/// instruction and a script-level stack trace.
#[derive(Debug, Clone)]
pub struct ScriptFault {
    pub message: String,
    /// Span of the faulting instruction. Equal to `trace[0].span`; kept
    /// as a convenience for callers that only want the fault site.
    pub span: Option<Span>,
    /// Stack trace, innermost frame first.
    pub trace: Vec<TraceFrame>,
    /// Set when the fault is a requested process exit (`process::exit`),
    /// not a failure — honor it by terminating with this code instead of
    /// rendering an error.
    pub exit_code: Option<i32>,
}

impl fmt::Display for ScriptFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Source-free fallback: callers with the source (CLI/REPL) render
        // a richer trace with line numbers via `diag_render`.
        write!(f, "runtime error: {}", self.message)?;
        for frame in &self.trace {
            write!(f, "\n  in {}", frame.function)?;
        }
        Ok(())
    }
}

impl std::error::Error for ScriptFault {}
