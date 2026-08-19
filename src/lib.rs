//! unirun — cross-platform command execution normalization for AI agents.
//!
//! Library surface: `ExecSpec` in → `ExecResult` out, same shape on every
//! platform. Binaries: `unirun run|script|probe` (see `main.rs`) and, from
//! P1, the MCP server.

pub mod acp;
pub mod encoding;
pub mod error_maps;
pub mod exec;
pub mod mcp;
pub mod probe;
pub mod recipe;
pub mod session;
pub mod spec;
pub mod taxonomy;
pub mod transport;

pub use exec::{install_sigint_handler, reset_abort, run};
pub use probe::{probe, Capabilities};
pub use spec::{ExecKind, ExecResult, ExecSpec, Shell};
pub use transport::{ssh_run, SshTarget};
