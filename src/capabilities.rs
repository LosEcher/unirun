//! Self-description: what *this build* of unirun can do.
//!
//! `unirun --version` answers "which release is this?", which forces every
//! consumer to maintain a version→capability table of its own. los does exactly
//! that (`packages/gateway/src/unirun-capabilities.ts:83-94`, mapping `0.4.0`
//! to "`ssh --workdir/--env` works"), and its comment explains why it cannot
//! simply trust the binary:
//!
//! > Pre-0.3.0 builds do not reject unknown flags — they append them to the
//! > remote script.
//!
//! `unirun capabilities --json` replaces that table with a list of stable
//! capability keys. Consumers ask for a capability, not a version.
//!
//! **Compatibility contract**: keys are added, never renamed or removed; the
//! `schema` field is bumped only if the *shape* changes. A consumer that sees
//! an unknown key ignores it; a consumer that misses a key it needs falls back
//! to its own precedent behaviour.

use serde::Serialize;

/// Capability-key schema version (the shape, not the crate version).
pub const CAPABILITIES_SCHEMA: u32 = 1;

/// Capability keys this build answers for, in the order they were introduced.
///
/// Each key names a *behaviour*, not a flag, so a consumer can branch on what
/// it needs without reading release notes:
pub const FEATURE_KEYS: &[&str] = &[
    // Execution surfaces.
    "local-exec",
    "probe",
    "mcp",
    "mcp-cancel", // notifications/cancelled stops a running exec.*
    "acp",
    "bg-session",
    "recipe-registry",
    "recipe-toolchain",
    // Remote execution.
    "ssh",
    "ssh-identity",    // --user / --port / --identity
    "ssh-workdir-env", // remote --workdir / --env
    "winrm",           // only when the `winrm` feature is compiled in
    // Result-shape capabilities.
    "error-taxonomy",
    "output-cap",           // --max-output, per stream, all transports
    "truncation-flag",      // `truncated` is truthful on every transport
    "transport-error",      // `transport_error` + `transport_stderr`
    "dispatched",           // `dispatched`: did the command possibly run?
    "kill-status",          // `kill_status`: clean / escalated / survived / unconfirmed
    "exit-code-confidence", // `exit_code_confidence`: is a zero evidence?
    "drain-timeout",        // `drain_timeout`: output may be incomplete
    "legacy-codepage",      // GBK auto-detection
    "encoding-hint",        // --output-encoding / recipe [conventions] encoding
    "strict-flags",         // unknown `--flags` are a usage error, never script text
];

#[derive(Debug, Serialize)]
pub struct UnirunInfo {
    /// The crate version (`CARGO_PKG_VERSION`).
    pub version: &'static str,
    /// The shape of this document.
    pub schema: u32,
}

#[derive(Debug, Serialize)]
pub struct PlatformInfo {
    pub os: &'static str,
    pub arch: &'static str,
}

#[derive(Debug, Serialize)]
pub struct BuildCapabilities {
    pub unirun: UnirunInfo,
    pub platform: PlatformInfo,
    pub features: Vec<&'static str>,
}

/// Describe this build. `features` reflects compile-time facts (`winrm`) as
/// well as behaviour that is always present.
pub fn capabilities() -> BuildCapabilities {
    BuildCapabilities {
        unirun: UnirunInfo {
            version: env!("CARGO_PKG_VERSION"),
            schema: CAPABILITIES_SCHEMA,
        },
        platform: PlatformInfo {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
        },
        features: feature_keys(),
    }
}

/// The keys this build actually answers for.
pub fn feature_keys() -> Vec<&'static str> {
    FEATURE_KEYS
        .iter()
        .copied()
        .filter(|key| *key != "winrm" || cfg!(feature = "winrm"))
        .collect()
}

/// Does this build answer for `key`?
pub fn supports(key: &str) -> bool {
    feature_keys().contains(&key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_and_schema_are_reported() {
        let caps = capabilities();
        assert_eq!(caps.unirun.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(caps.unirun.schema, CAPABILITIES_SCHEMA);
        assert!(!caps.platform.os.is_empty());
        assert!(!caps.platform.arch.is_empty());
    }

    #[test]
    fn keys_are_unique_and_kebab_cased() {
        let mut seen = std::collections::BTreeSet::new();
        for key in FEATURE_KEYS {
            assert!(seen.insert(*key), "duplicate capability key `{key}`");
            assert!(
                key.chars().all(|c| c.is_ascii_lowercase() || c == '-'),
                "`{key}` must be kebab-case"
            );
        }
    }

    /// The keys los needs to stop parsing `--version`.
    #[test]
    fn the_capabilities_consumers_gate_on_are_present() {
        for key in [
            "ssh",
            "ssh-identity",
            "ssh-workdir-env",
            "strict-flags",
            "transport-error",
            "dispatched",
            "output-cap",
            "truncation-flag",
            "legacy-codepage",
            "encoding-hint",
        ] {
            assert!(supports(key), "missing capability `{key}`");
        }
    }

    #[test]
    fn winrm_follows_the_compile_time_feature() {
        assert_eq!(
            supports("winrm"),
            cfg!(feature = "winrm"),
            "the winrm key must track the feature flag, not be hard-coded"
        );
    }

    #[test]
    fn unknown_keys_are_simply_unsupported() {
        assert!(!supports("teleport"));
        assert!(!supports(""));
    }
}
