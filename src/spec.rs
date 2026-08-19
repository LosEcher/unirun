//! Core data structures: execution spec in, normalized result out.

use crate::recipe::ErrorMapEntry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Supported shells. `Run` defaults to the best POSIX shell found
/// (`bash` → `sh`); `Script` infers from the file extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Shell {
    Bash,
    Sh,
    Zsh,
    Cmd,
    Powershell,
    Pwsh,
}

impl Shell {
    pub fn as_str(&self) -> &'static str {
        match self {
            Shell::Bash => "bash",
            Shell::Sh => "sh",
            Shell::Zsh => "zsh",
            Shell::Cmd => "cmd",
            Shell::Powershell => "powershell",
            Shell::Pwsh => "pwsh",
        }
    }

    pub fn from_name(name: &str) -> Option<Shell> {
        match name.to_ascii_lowercase().as_str() {
            "bash" => Some(Shell::Bash),
            "sh" => Some(Shell::Sh),
            "zsh" => Some(Shell::Zsh),
            "cmd" | "cmd.exe" => Some(Shell::Cmd),
            "powershell" | "powershell.exe" => Some(Shell::Powershell),
            "pwsh" | "pwsh.exe" => Some(Shell::Pwsh),
            _ => None,
        }
    }

    /// Best-effort guess from a script file extension.
    pub fn from_path(path: &std::path::Path) -> Option<Shell> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "sh" => Some(Shell::Bash),
            "zsh" => Some(Shell::Zsh),
            "ps1" => Some(Shell::Pwsh),
            "bat" | "cmd" => Some(Shell::Cmd),
            _ => None,
        }
    }
}

/// What kind of execution the caller wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecKind {
    /// A single command line run through a shell (`bash -c …`).
    Run,
    /// A multi-line script, shell inferred from extension unless overridden.
    Script,
}

/// Defaults (used when the caller leaves a field at its zero value).
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;
pub const DEFAULT_GRACE_MS: u64 = 2_000;
pub const DEFAULT_MAX_OUTPUT_BYTES: usize = 256 * 1024;

/// Everything unirun needs to run one command or script.
#[derive(Debug, Clone)]
pub struct ExecSpec {
    /// Command line or script body.
    pub command: String,
    pub kind: ExecKind,
    /// Explicit shell; `None` = auto-detect.
    pub shell: Option<Shell>,
    /// Working directory; `None` = current process cwd.
    pub workdir: Option<PathBuf>,
    /// Extra environment overrides applied on top of the ambient env.
    pub env: Vec<(String, String)>,
    /// Deadline in ms; `0` → `DEFAULT_TIMEOUT_MS`.
    pub timeout_ms: u64,
    /// SIGTERM → SIGKILL grace in ms; `0` → `DEFAULT_GRACE_MS`.
    pub grace_ms: u64,
    /// Per-stream output cap in bytes; `0` → `DEFAULT_MAX_OUTPUT_BYTES`.
    /// Overflow is drained (no pipe deadlock) and marked `truncated`.
    pub max_output_bytes: usize,
    /// Exact argv to execute directly (no shell). When set, bypasses
    /// `shell`/`command` interpretation — used by toolchain runners
    /// (e.g. `["uv", "run", "script.py"]`).
    pub direct: Option<Vec<String>>,
    /// Project recipe `[error_maps]` patterns, consulted before the built-in
    /// error-map library during classification (project knowledge wins).
    pub error_maps: BTreeMap<String, ErrorMapEntry>,
}

impl Default for ExecSpec {
    fn default() -> Self {
        ExecSpec {
            command: String::new(),
            kind: ExecKind::Run,
            shell: None,
            workdir: None,
            env: Vec::new(),
            timeout_ms: 0,
            grace_ms: 0,
            max_output_bytes: 0,
            direct: None,
            error_maps: BTreeMap::new(),
        }
    }
}

impl ExecSpec {
    pub fn effective_timeout_ms(&self) -> u64 {
        if self.timeout_ms == 0 {
            DEFAULT_TIMEOUT_MS
        } else {
            self.timeout_ms
        }
    }
    pub fn effective_grace_ms(&self) -> u64 {
        if self.grace_ms == 0 {
            DEFAULT_GRACE_MS
        } else {
            self.grace_ms
        }
    }
    pub fn effective_max_output(&self) -> usize {
        if self.max_output_bytes == 0 {
            DEFAULT_MAX_OUTPUT_BYTES
        } else {
            self.max_output_bytes
        }
    }
}

/// The normalized result — same shape on every platform, stable schema (semver).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecResult {
    /// Process exit code; `None` when killed by a signal (or timed out).
    pub exit_code: Option<i32>,
    /// Terminating signal, when the process was killed by one.
    pub signal: Option<i32>,
    /// Captured stdout (decoded, tail-kept on truncation).
    pub stdout: String,
    /// Captured stderr (decoded, tail-kept on truncation).
    pub stderr: String,
    /// True when the deadline elapsed and the process tree was terminated.
    pub timed_out: bool,
    /// True when the caller (or SIGINT) aborted the run.
    pub aborted: bool,
    /// Wall-clock duration of the run.
    pub duration_ms: u64,
    /// Stable error class (see taxonomy) — `None` on success.
    pub error_class: Option<String>,
    /// Actionable remediation hint for the error class.
    pub hint: Option<String>,
    /// Encoding the decoded output was produced in ("utf-8" | "utf-8-lossy" | "utf-16le" | "utf-16be").
    pub encoding: String,
    /// True when a stream exceeded the cap and only its tail was kept.
    pub truncated: bool,
    /// Shell actually used (after resolution/fallback).
    pub shell_used: String,
}

impl ExecResult {
    pub fn success(stdout: String, stderr: String, shell_used: &str) -> Self {
        ExecResult {
            exit_code: Some(0),
            signal: None,
            stdout,
            stderr,
            timed_out: false,
            aborted: false,
            duration_ms: 0,
            error_class: None,
            hint: None,
            encoding: "utf-8".to_string(),
            truncated: false,
            shell_used: shell_used.to_string(),
        }
    }
}
