//! Error taxonomy: stable, actionable classes agents can branch on.
//!
//! `classify` inspects a completed `ExecResult` (exit code, timed-out flag,
//! captured stderr) and returns an `(error_class, hint)` pair. The taxonomy
//! is part of the public schema — classes are semver-stable:
//!
//!   TIMEOUT · ABORTED · COMMAND_NOT_FOUND · PERMISSION · EXEC_FORMAT
//!   NOT_FOUND · DEPENDENCY_MISSING · SYNTAX · UNKNOWN_FAILURE

use crate::spec::ExecResult;

/// Classify a finished run. `None` class means no confirmed error — either
/// success, an explicit non-zero exit without error evidence, or an aborted
/// run (unirun never invents a class without evidence).
pub fn classify(r: &ExecResult) -> (Option<String>, Option<String>) {
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

    let stderr = r.stderr.to_lowercase();
    // Collapse all whitespace to single spaces: consoles wrap error text at
    // ~80 columns (PowerShell's "…the name of a\ncmdlet…" is real), and a
    // wrapped pattern must still match.
    let flat: String = stderr.split_whitespace().collect::<Vec<_>>().join(" ");

    // POSIX shell exit-code semantics (bash/sh/zsh).
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

    // stderr pattern matching (cross-platform heuristics).
    if flat.contains("command not found")
        || flat.contains("is not recognized as the name of a cmdlet")
        || flat.contains("is not recognized as an internal or external command")
    {
        return (
            Some("COMMAND_NOT_FOUND".into()),
            Some("a command is missing; install it, or check PATH on the target".into()),
        );
    }
    if flat.contains("permission denied") {
        return (
            Some("PERMISSION".into()),
            Some("the process lacks permission; check file ownership, mode bits, or policy".into()),
        );
    }
    if flat.contains("module not found") || flat.contains("modulenotfounderror") {
        return (
            Some("DEPENDENCY_MISSING".into()),
            Some("a Python dependency is missing; sync the project environment (e.g. `uv sync` / `pip install -r requirements.txt`)".into()),
        );
    }
    if flat.contains("no such file or directory") || flat.contains("cannot find the path") {
        return (
            Some("NOT_FOUND".into()),
            Some(
                "a referenced file or directory does not exist; verify paths before retrying"
                    .into(),
            ),
        );
    }
    if flat.contains("syntax error") || flat.contains("unexpected token") {
        return (
            Some("SYNTAX".into()),
            Some("the script has a shell syntax error; review quoting and line structure".into()),
        );
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
}
