//! unirun — cross-platform command execution normalization for AI agents.
//!
//! Library surface: `ExecSpec` in → `ExecResult` out, same shape on every
//! platform. Binaries: `unirun run|script|probe|ssh|mcp|acp|bg|recipe`
//! (see `main.rs`), plus the optional `winrm`-feature WinRM provider.

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

#[cfg(feature = "winrm")]
pub mod winrm;

pub use exec::{install_sigint_handler, reset_abort, run};
pub use probe::{probe, Capabilities};
pub use spec::{ExecKind, ExecResult, ExecSpec, Shell};
pub use transport::{ssh_run, SshTarget};

/// Serializes tests that mutate process-wide environment (`UNIRUN_HOME`,
/// `UNIRUN_BIN`, …) — the recipe-registry and session tests both touch it
/// and must never observe each other's value mid-mutation.
#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
