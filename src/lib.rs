//! unirun — cross-platform command execution normalization for AI agents.
//!
//! Library surface: `ExecSpec` in → `ExecResult` out, same shape on every
//! platform. Binaries: `unirun run|script|probe` (see `main.rs`) and, from
//! P1, the MCP server.

pub mod encoding;
pub mod exec;
pub mod probe;
pub mod spec;
pub mod taxonomy;

pub use exec::{install_sigint_handler, reset_abort, run};
pub use probe::{probe, Capabilities};
pub use spec::{ExecKind, ExecResult, ExecSpec, Shell};
