//! unirun — cross-platform command execution normalization for AI agents.
//!
//! Library surface: `ExecSpec` in → `ExecResult` out, same shape on every
//! platform. Binaries: `unirun run|script|probe|ssh|mcp|acp|bg|recipe`
//! (see `main.rs`), plus the optional `winrm`-feature WinRM provider.
//!
//! # Stability
//!
//! The **stable** surface is:
//!
//! - [`spec`]: [`ExecSpec`], [`ExecResult`], [`Shell`], [`ExecKind`],
//!   [`KillStatus`], [`ExitCodeConfidence`] and the field set of `ExecResult`
//!   (fields are added, never removed or repurposed; a removed field would be a
//!   major version);
//! - [`exec::run`], [`exec::run_streaming`], [`exec::run_with_abort`],
//!   [`exec::run_with_abort_streaming`] and [`exec::StreamChunk`];
//! - [`transport`]: [`SshTarget`], [`transport::ssh_run`],
//!   [`transport::ssh_run_detached`], [`DetachedRun`];
//! - [`probe::probe`], [`capabilities`], [`session`], [`recipe`],
//!   [`taxonomy`] (the `error_class` vocabulary), [`encoding`].
//!
//! Everything else is internal and may change in a minor release. The CLI's
//! `--json` payload and the library types are the **same** data: a parity test
//! (`tests/lib_cli_parity.rs`) asserts that a run produces identical normalized
//! fields through both, so a change cannot drift one from the other.
//!
//! Cancellation is explicit, never implicit: [`exec::run`] honours the process
//! SIGINT flag, [`exec::run_with_abort`] honours a flag the caller owns.

pub mod acp;
pub mod capabilities;
pub mod coalesce;
pub mod encoding;
pub mod error_maps;
pub mod exec;
pub mod mcp;
pub mod probe;
pub mod process_identity;
pub mod recipe;
pub mod session;
pub mod spec;
pub mod taxonomy;
pub mod transport;

#[cfg(feature = "winrm")]
pub mod winrm;

pub use exec::{
    install_sigint_handler, reset_abort, run, run_streaming, run_with_abort,
    run_with_abort_streaming,
};
// NB: `probe::Capabilities` is the *host* matrix (shells/tools); the build's
// own capability list is `capabilities::BuildCapabilities`, deliberately not
// re-exported here so the two can never be confused at a call site.
pub use probe::{probe, Capabilities};
pub use spec::{ExecKind, ExecResult, ExecSpec, ExitCodeConfidence, KillStatus, Shell};
pub use transport::{ssh_run, ssh_run_detached, DetachedRun, SshTarget};

/// Serializes tests that mutate process-wide environment (`UNIRUN_HOME`,
/// `UNIRUN_BIN`, …) — the recipe-registry and session tests both touch it
/// and must never observe each other's value mid-mutation.
#[cfg(test)]
pub(crate) static ENV_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
