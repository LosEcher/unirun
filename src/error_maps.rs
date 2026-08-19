//! Error-map library — curated, evidence-based stderr patterns for common
//! toolchains, plus the project-recipe extension point.
//!
//! Classification order (first match wins, applied by `taxonomy`):
//!   1. structural cases (timeout / abort / POSIX exit-code semantics)
//!   2. project recipe `[error_maps]` patterns — project knowledge beats
//!      generic heuristics, so it is consulted first
//!   3. the built-in library below
//!   4. `UNKNOWN_FAILURE` / no-evidence fallback
//!
//! Patterns are **case-insensitive substrings** matched against the flattened
//! stderr (all whitespace collapsed to single spaces), so console line-wrap
//! can never break a match. Recipe patterns may carry leading/trailing `*`
//! wildcards (e.g. `"ModuleNotFoundError: *"`), which are stripped before
//! substring matching.
//!
//! The library is conservative on purpose (matching the taxonomy's
//! evidence-based rule): a pattern ships only when the class and hint are
//! confident. `exit 42` with no matching evidence stays unclassified — the
//! exit code is the signal.

use crate::recipe::ErrorMapEntry;
use std::collections::BTreeMap;

/// One curated pattern: when it matches, the run gets `class` + `hint`.
pub struct ErrorPattern {
    /// Lowercase substring searched in the flattened stderr.
    pub pattern: &'static str,
    /// Stable error class (see taxonomy docs).
    pub class: &'static str,
    /// Actionable remediation hint for an agent's next step.
    pub hint: &'static str,
}

/// Built-in error-map library, grouped by ecosystem.
///
/// New classes beyond the original set are additive and semver-stable:
/// `NETWORK` (unreachable hosts/repos/registries) and `COMPILE_ERROR`
/// (compiler/toolchain diagnostics) were added with the P2 library.
pub const BUILTIN: &[ErrorPattern] = &[
    // --- Generic / shell ---
    ErrorPattern {
        pattern: "command not found",
        class: "COMMAND_NOT_FOUND",
        hint: "a command is missing; install it, or check PATH on the target",
    },
    ErrorPattern {
        pattern: "is not recognized as the name of a cmdlet",
        class: "COMMAND_NOT_FOUND",
        hint: "the PowerShell command does not exist; check the module/command name",
    },
    ErrorPattern {
        pattern: "is not recognized as an internal or external command",
        class: "COMMAND_NOT_FOUND",
        hint: "the cmd.exe command does not exist; check the name or PATH",
    },
    ErrorPattern {
        pattern: "permission denied (publickey",
        class: "PERMISSION",
        hint: "SSH public-key authentication failed; check your key, ssh-agent, or remote authorized_keys",
    },
    ErrorPattern {
        pattern: "permission denied",
        class: "PERMISSION",
        hint: "the process lacks permission; check file ownership, mode bits, or policy",
    },
    ErrorPattern {
        pattern: "denied: requested access to the resource is denied",
        class: "PERMISSION",
        hint: "the registry/repository denied access; check credentials and repository permissions",
    },
    ErrorPattern {
        pattern: "no such file or directory",
        class: "NOT_FOUND",
        hint: "a referenced file or directory does not exist; verify paths before retrying",
    },
    ErrorPattern {
        pattern: "cannot find the path",
        class: "NOT_FOUND",
        hint: "a referenced path does not exist on the target; verify it before retrying",
    },
    ErrorPattern {
        pattern: "cannot find the file",
        class: "NOT_FOUND",
        hint: "a referenced file does not exist on the target; verify it before retrying",
    },
    ErrorPattern {
        pattern: "syntax error",
        class: "SYNTAX",
        hint: "the script has a syntax error; review quoting and line structure",
    },
    ErrorPattern {
        pattern: "unexpected token",
        class: "SYNTAX",
        hint: "the script has a shell syntax error; review quoting and line structure",
    },
    ErrorPattern {
        pattern: "syntaxerror",
        class: "SYNTAX",
        hint: "the Python source has a syntax error; review the flagged line",
    },
    ErrorPattern {
        pattern: "indentationerror",
        class: "SYNTAX",
        hint: "the Python source has an indentation error; check mixed tabs/spaces",
    },
    // --- Python ---
    ErrorPattern {
        pattern: "windows subsystem for linux has no installed distributions",
        class: "COMMAND_NOT_FOUND",
        hint: "the WSL bash shim was invoked with no distro installed; install a WSL distribution or use Git Bash / PowerShell",
    },
    ErrorPattern {
        pattern: "modulenotfounderror",
        class: "DEPENDENCY_MISSING",
        hint: "a Python module is missing; sync the project environment (e.g. `uv sync` / `pip install -r requirements.txt`)",
    },
    ErrorPattern {
        pattern: "no module named",
        class: "DEPENDENCY_MISSING",
        hint: "a Python module is missing; install it into the active environment",
    },
    ErrorPattern {
        pattern: "importerror",
        class: "DEPENDENCY_MISSING",
        hint: "a Python import failed; check the installed packages and environment",
    },
    ErrorPattern {
        pattern: "no matching distribution found",
        class: "DEPENDENCY_MISSING",
        hint: "the package is not available on the configured index; check the package name or index",
    },
    ErrorPattern {
        pattern: "externally-managed-environment",
        class: "PERMISSION",
        hint: "system Python is externally managed (PEP 668); use a virtualenv or `uv` instead of system pip",
    },
    ErrorPattern {
        pattern: "can't open file",
        class: "NOT_FOUND",
        hint: "Python cannot open the script file; check the path passed to the interpreter",
    },
    // --- Node / npm / pnpm ---
    ErrorPattern {
        pattern: "cannot find module",
        class: "DEPENDENCY_MISSING",
        hint: "a Node module is missing; run `npm install` / `pnpm install` / `yarn`",
    },
    ErrorPattern {
        pattern: "enoent",
        class: "NOT_FOUND",
        hint: "a file or module referenced by Node does not exist; check the path",
    },
    // --- Rust / cargo ---
    ErrorPattern {
        pattern: "error[e0432]",
        class: "DEPENDENCY_MISSING",
        hint: "a Rust crate import could not be resolved; add the dependency to Cargo.toml",
    },
    ErrorPattern {
        pattern: "error[e0425]",
        class: "DEPENDENCY_MISSING",
        hint: "a Rust item could not be found; check imports, feature flags, and crate versions",
    },
    ErrorPattern {
        pattern: "error[e",
        class: "COMPILE_ERROR",
        hint: "the Rust compiler rejected the code; fix the flagged diagnostics before retrying",
    },
    ErrorPattern {
        pattern: "error: could not compile",
        class: "COMPILE_ERROR",
        hint: "cargo could not compile the crate; fix the compiler diagnostics",
    },
    // --- Go ---
    ErrorPattern {
        pattern: "no required module provides package",
        class: "DEPENDENCY_MISSING",
        hint: "a Go module dependency is missing; run `go mod tidy`",
    },
    // --- TypeScript / tsc ---
    ErrorPattern {
        pattern: "error ts",
        class: "COMPILE_ERROR",
        hint: "the TypeScript compiler rejected the code; fix the flagged diagnostics",
    },
    ErrorPattern {
        pattern: "tsc: error",
        class: "COMPILE_ERROR",
        hint: "the TypeScript compiler rejected the code; fix the flagged diagnostics",
    },
    // --- Git ---
    ErrorPattern {
        pattern: "fatal: not a git repository",
        class: "NOT_FOUND",
        hint: "the current directory is not inside a git repository; run from the repo root",
    },
    ErrorPattern {
        pattern: "fatal: authentication failed",
        class: "PERMISSION",
        hint: "git authentication failed; refresh credentials (credential helper, token, or SSH key)",
    },
    ErrorPattern {
        pattern: "fatal: unable to access",
        class: "NETWORK",
        hint: "git could not reach the remote; check network, proxy, and remote URL",
    },
    ErrorPattern {
        pattern: "remote: repository not found",
        class: "NOT_FOUND",
        hint: "the remote repository does not exist or is private; check the remote URL",
    },
    // --- Network (generic) ---
    ErrorPattern {
        pattern: "could not resolve host",
        class: "NETWORK",
        hint: "DNS resolution failed; check the hostname and network/DNS configuration",
    },
    ErrorPattern {
        pattern: "connection refused",
        class: "NETWORK",
        hint: "the remote endpoint refused the connection; check that the service is running and the port is right",
    },
    ErrorPattern {
        pattern: "connection timed out",
        class: "NETWORK",
        hint: "the connection timed out; check the endpoint reachability and firewall",
    },
    ErrorPattern {
        pattern: "network is unreachable",
        class: "NETWORK",
        hint: "the network is unreachable; check connectivity, VPN, or sandbox restrictions",
    },
    ErrorPattern {
        pattern: "temporary failure in name resolution",
        class: "NETWORK",
        hint: "DNS resolution temporarily failed; retry or check the DNS configuration",
    },
    ErrorPattern {
        pattern: "failed to connect to",
        class: "NETWORK",
        hint: "a connection to a remote service failed; check reachability and credentials",
    },
];

/// Collapse all whitespace to single spaces and lowercase — the shared
/// normalization applied before pattern matching (console line-wrap must
/// never break a match; neither should case).
pub fn flatten(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Match the built-in library against flattened stderr. Returns `(class, hint)`.
pub fn match_builtin(flat: &str) -> Option<(&'static str, &'static str)> {
    for p in BUILTIN {
        if flat.contains(p.pattern) {
            return Some((p.class, p.hint));
        }
    }
    None
}

/// Match project recipe `[error_maps]` entries against flattened stderr.
/// Leading/trailing `*` wildcards are stripped; the remainder is a
/// case-insensitive substring. Returns `(class, hint)`.
pub fn match_recipe(
    flat: &str,
    maps: &BTreeMap<String, ErrorMapEntry>,
) -> Option<(String, String)> {
    for (pattern, entry) in maps {
        let needle = pattern.trim_matches('*').trim().to_lowercase();
        if needle.is_empty() {
            continue;
        }
        if flat.contains(&needle) {
            return Some((entry.class.clone(), entry.hint.clone().unwrap_or_default()));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flatten_collapses_wrapped_console_lines() {
        // PowerShell wraps cmdlet errors at ~80 columns; the flattened text
        // must still contain the full phrase on one line.
        let wrapped = "The term 'nope' is not recognized as the name of a\ncmdlet, function...";
        let flat = flatten(wrapped);
        assert!(flat.contains("is not recognized as the name of a cmdlet"));
        assert!(!flat.contains('\n'));
    }

    #[test]
    fn builtin_matches_common_ecosystems() {
        let cases: &[(&str, &str)] = &[
            (
                "ModuleNotFoundError: No module named 'requests'",
                "DEPENDENCY_MISSING",
            ),
            (
                "ERROR: No matching distribution found for foo",
                "DEPENDENCY_MISSING",
            ),
            ("Error: Cannot find module 'lodash'", "DEPENDENCY_MISSING"),
            (
                "error[E0432]: unresolved import `serde`",
                "DEPENDENCY_MISSING",
            ),
            ("error[E0308]: mismatched types", "COMPILE_ERROR"),
            (
                "no required module provides package github.com/x/y",
                "DEPENDENCY_MISSING",
            ),
            (
                "fatal: unable to access 'https://github.com/x/y.git/'",
                "NETWORK",
            ),
            ("curl: (6) Could not resolve host: example.com", "NETWORK"),
            ("bash: foo: command not found", "COMMAND_NOT_FOUND"),
            (
                "fatal: not a git repository (or any of the parent directories)",
                "NOT_FOUND",
            ),
            (
                "denied: requested access to the resource is denied",
                "PERMISSION",
            ),
        ];
        for (stderr, expected) in cases {
            let (class, hint) = match_builtin(&flatten(stderr)).expect(stderr);
            assert_eq!(class, *expected, "stderr: {}", stderr);
            assert!(!hint.is_empty());
        }
    }

    #[test]
    fn builtin_is_conservative_on_unknown_text() {
        let flat = flatten("some novel error nobody has seen before");
        assert!(match_builtin(&flat).is_none());
    }

    #[test]
    fn recipe_patterns_win_over_builtin() {
        let mut maps = BTreeMap::new();
        maps.insert(
            "ModuleNotFoundError: *".to_string(),
            ErrorMapEntry {
                class: "DEPENDENCY_MISSING".into(),
                hint: Some("run `uv sync` first".into()),
            },
        );
        let flat = flatten("ModuleNotFoundError: No module named 'x'");
        // Recipe consulted first → project hint wins over the builtin hint.
        let (class, hint) = match_recipe(&flat, &maps).unwrap();
        assert_eq!(class, "DEPENDENCY_MISSING");
        assert_eq!(hint, "run `uv sync` first");
    }

    #[test]
    fn recipe_wildcards_are_substrings() {
        let mut maps = BTreeMap::new();
        maps.insert(
            "*npm ERR!*".to_string(),
            ErrorMapEntry {
                class: "DEPENDENCY_MISSING".into(),
                hint: None,
            },
        );
        let flat = flatten("npm ERR! code ENOENT");
        assert!(match_recipe(&flat, &maps).is_some());
    }
}
