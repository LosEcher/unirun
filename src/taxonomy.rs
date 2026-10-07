//! Error taxonomy: stable, actionable classes agents can branch on.
//!
//! `classify` inspects a completed `ExecResult` (exit code, timed-out flag,
//! captured stderr) and returns an `(error_class, hint)` pair. The taxonomy
//! is part of the public schema — classes are semver-stable:
//!
//!   TIMEOUT · ABORTED · COMMAND_NOT_FOUND · PERMISSION · EXEC_FORMAT
//!   NOT_FOUND · DEPENDENCY_MISSING · SYNTAX · UNKNOWN_FAILURE
//!   NETWORK · COMPILE_ERROR          (added with the P2 error-map library)
//!   TRANSPORT                        (the transport failed before the command
//!                                    ran: ssh connect/auth/host-key/DNS, a
//!                                    failed scp upload, a WinRM/PSRP failure.
//!                                    Structural — set from
//!                                    `ExecResult::transport_error`, never
//!                                    inferred from output)
//!   PID_REUSED                       (kill refused — the pid no longer refers
//!                                    to the process we started; set by the
//!                                    background-session kill path, which
//!                                    signals a pid it does not own, never
//!                                    inferred from output evidence. `exec.run`
//!                                    does not emit it: it signals the child it
//!                                    still holds unreaped, whose pid cannot
//!                                    have been recycled)
//!   IDENTITY_UNVERIFIABLE            (kill refused — this environment could
//!                                    observe neither the start epoch nor the
//!                                    generation token, so a detached pid could
//!                                    not be tied to the session; fail-closed,
//!                                    same path)
//!   PROCESS_UNKILLABLE               (the tree survived an escalated kill —
//!                                    `kill_status: "survived"`, structural)
//!
//! Matching order (first hit wins):
//!   1. structural cases handled here (timeout / abort / POSIX exit codes)
//!   2. project recipe `[error_maps]` patterns (`classify_with_maps`)
//!   3. the built-in pattern library (`error_maps` module)
//!   4. `UNKNOWN_FAILURE` / no-evidence fallback

use crate::error_maps;
use crate::recipe::ErrorMapEntry;
use crate::spec::ExecResult;
use std::collections::BTreeMap;

/// Classify a finished run using only the built-in error-map library.
/// `None` class means no confirmed error — either success, an explicit
/// non-zero exit without error evidence, or an aborted run (unirun never
/// invents a class without evidence).
pub fn classify(r: &ExecResult) -> (Option<String>, Option<String>) {
    classify_with_maps(r, None)
}

/// Classify with project recipe `[error_maps]` patterns consulted before the
/// built-in library (project knowledge beats generic heuristics).
pub fn classify_with_maps(
    r: &ExecResult,
    recipe_maps: Option<&BTreeMap<String, ErrorMapEntry>>,
) -> (Option<String>, Option<String>) {
    // The tree survived even an escalated kill (D state, stuck driver): the
    // run is over but its processes are not, which outranks the timeout that
    // triggered the kill.
    if r.kill_status == Some(crate::spec::KillStatus::Survived) {
        return (
            Some("PROCESS_UNKILLABLE".into()),
            Some(
                "the process tree survived an escalated kill (SIGKILL / taskkill /F) and may still be running; do not assume its side effects stopped"
                    .into(),
            ),
        );
    }
    if r.timed_out {
        return (
            Some("TIMEOUT".into()),
            Some("deadline elapsed; the process tree was terminated. retry with a larger --timeout, or split the work".into()),
        );
    }
    if r.aborted {
        return (
            Some("ABORTED".into()),
            Some(
                "the run was cancelled by the caller; no partial state was committed by unirun"
                    .into(),
            ),
        );
    }
    // Structural, and ahead of every stderr pattern: the transport failed, so
    // the command never produced a result and no output evidence exists.
    if r.transport_error {
        return (
            Some("TRANSPORT".into()),
            Some(
                "the transport (ssh/scp/winrm) failed before the command ran; the remote command produced no result — retrying the submission is safe, retrying a partly-executed command is not"
                    .into(),
            ),
        );
    }

    let stderr = r.stderr.to_lowercase();
    // Collapse all whitespace to single spaces: consoles wrap error text at
    // ~80 columns (PowerShell's "…the name of a\ncmdlet…" is real), and a
    // wrapped pattern must still match.
    let flat = error_maps::flatten(&stderr);

    // POSIX shell exit-code semantics (bash/sh/zsh) — structural, not pattern.
    if r.exit_code == Some(127) {
        return (
            Some("COMMAND_NOT_FOUND".into()),
            Some("a command in the script does not exist on this platform, or is not on PATH. run `unirun probe` to see available tools".into()),
        );
    }
    if r.exit_code == Some(126) {
        let hint = if flat.contains("permission denied") {
            "the script file lacks execute permission, or the command cannot be executed".into()
        } else {
            "the script file exists but cannot be executed (wrong format or interpreter)".into()
        };
        return (Some("PERMISSION".into()), Some(hint));
    }

    // Project recipe patterns first, then the built-in library.
    if let Some(maps) = recipe_maps {
        if !maps.is_empty() {
            if let Some((class, hint)) = error_maps::match_recipe(&flat, maps) {
                return (Some(class), Some(hint));
            }
        }
    }
    if let Some((class, hint)) = error_maps::match_builtin(&flat) {
        return (Some(class.to_string()), Some(hint.to_string()));
    }

    if r.exit_code == Some(0) || r.stderr.trim().is_empty() {
        // No evidence: an explicit non-zero exit (e.g. `exit 42`) or a bare
        // `false` is the command's own signal — unirun does not invent a
        // class without evidence (fail-closed classification).
        (None, None)
    } else {
        // Carry a bounded stderr excerpt so agents (and humans) can see what
        // defeated classification without fetching the full output.
        let excerpt: String = r.stderr.chars().take(200).collect();
        (
            Some("UNKNOWN_FAILURE".into()),
            Some(format!("unrecognized stderr: {}", excerpt)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ExecResult;

    fn base() -> ExecResult {
        ExecResult::success(String::new(), String::new(), "bash")
    }

    #[test]
    fn success_is_none() {
        assert_eq!(classify(&base()), (None, None));
    }

    /// A tree that survived the escalated kill outranks the timeout that
    /// triggered it: the run is over, its processes are not.
    #[test]
    fn surviving_tree_beats_timeout() {
        let mut r = base();
        r.timed_out = true;
        r.kill_status = Some(crate::spec::KillStatus::Survived);
        let (class, hint) = classify(&r);
        assert_eq!(class.as_deref(), Some("PROCESS_UNKILLABLE"));
        assert!(hint.unwrap_or_default().contains("survived"));

        // An escalated-but-successful kill is still just a timeout.
        r.kill_status = Some(crate::spec::KillStatus::SigkillEscalated);
        assert_eq!(classify(&r).0.as_deref(), Some("TIMEOUT"));
    }

    #[test]
    fn transport_error_class_is_structural() {
        // Set from the transport, never inferred from stderr text: a remote
        // script that exits 255 while printing a connection error stays
        // whatever its own evidence says.
        let mut r = base();
        r.exit_code = Some(255);
        r.transport_error = true;
        r.transport_stderr = Some("ssh: connect to host x port 22: Connection refused\n".into());
        let (class, hint) = classify(&r);
        assert_eq!(class.as_deref(), Some("TRANSPORT"));
        let hint = hint.unwrap_or_default();
        assert!(
            hint.contains("never ran") || hint.contains("produced no result"),
            "the hint must say the command did not run: {hint}"
        );

        // Without the flag, the same shape is not a transport error.
        let mut plain = base();
        plain.exit_code = Some(255);
        plain.stderr = "some remote failure\n".into();
        assert_eq!(classify(&plain).0.as_deref(), Some("UNKNOWN_FAILURE"));
    }

    #[test]
    fn timeout_class() {
        let mut r = base();
        r.timed_out = true;
        let (class, _) = classify(&r);
        assert_eq!(class.as_deref(), Some("TIMEOUT"));
    }

    #[test]
    fn exit_127_command_not_found() {
        let mut r = base();
        r.exit_code = Some(127);
        let (class, hint) = classify(&r);
        assert_eq!(class.as_deref(), Some("COMMAND_NOT_FOUND"));
        assert!(hint.unwrap().contains("probe"));
    }

    #[test]
    fn exit_126_permission() {
        let mut r = base();
        r.exit_code = Some(126);
        r.stderr = "bash: ./x.sh: Permission denied".into();
        let (class, _) = classify(&r);
        assert_eq!(class.as_deref(), Some("PERMISSION"));
    }

    #[test]
    fn stderr_pattern_dependency() {
        let mut r = base();
        r.exit_code = Some(1);
        r.stderr = "ModuleNotFoundError: No module named 'requests'".into();
        let (class, hint) = classify(&r);
        assert_eq!(class.as_deref(), Some("DEPENDENCY_MISSING"));
        assert!(hint.unwrap().contains("uv sync"));
    }

    #[test]
    fn explicit_nonzero_exit_is_unclassified() {
        // `exit 42` with no stderr evidence: rc is the signal, no invented class.
        let mut r = base();
        r.exit_code = Some(42);
        assert_eq!(classify(&r), (None, None));
    }

    #[test]
    fn unknown_failure_only_with_stderr_evidence() {
        let mut r = base();
        r.exit_code = Some(3);
        r.stderr = "something odd happened\n".into();
        let (class, _) = classify(&r);
        assert_eq!(class.as_deref(), Some("UNKNOWN_FAILURE"));
    }

    #[test]
    fn builtin_library_classes_network_and_compile() {
        let mut r = base();
        r.exit_code = Some(128);
        r.stderr =
            "fatal: unable to access 'https://github.com/x/y.git/': Could not resolve host".into();
        let (class, _) = classify(&r);
        assert_eq!(class.as_deref(), Some("NETWORK"));

        let mut r2 = base();
        r2.exit_code = Some(101);
        r2.stderr = "error[E0432]: unresolved import `serde`".into();
        // E0432 has a specific DEPENDENCY_MISSING entry, checked first.
        let (class, _) = classify(&r2);
        assert_eq!(class.as_deref(), Some("DEPENDENCY_MISSING"));

        let mut r3 = base();
        r3.exit_code = Some(101);
        r3.stderr = "error[E0308]: mismatched types".into();
        let (class, _) = classify(&r3);
        assert_eq!(class.as_deref(), Some("COMPILE_ERROR"));
    }

    #[test]
    fn recipe_maps_override_builtin_hint() {
        let mut maps = std::collections::BTreeMap::new();
        maps.insert(
            "ModuleNotFoundError: *".to_string(),
            crate::recipe::ErrorMapEntry {
                class: "DEPENDENCY_MISSING".into(),
                hint: Some("project hint: uv sync".into()),
            },
        );
        let mut r = base();
        r.exit_code = Some(1);
        r.stderr = "ModuleNotFoundError: No module named 'requests'".into();
        let (class, hint) = classify_with_maps(&r, Some(&maps));
        assert_eq!(class.as_deref(), Some("DEPENDENCY_MISSING"));
        assert_eq!(hint.as_deref(), Some("project hint: uv sync"));

        // Without the maps, the builtin hint applies instead.
        let (_, hint2) = classify(&r);
        assert!(hint2.unwrap().contains("uv sync"));
    }
}
