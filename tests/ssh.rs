//! SSH transport smoke tests — `#[ignore]` by default because they need a
//! reachable Windows host. Run with:
//!
//!   UNIRUN_TEST_SSH_HOST=<ssh-alias-or-user@host> cargo test -- --ignored
//!
//! The default alias in CI-free local runs is `win-los` (our Win collection
//! node: PS 5.1 + Win32-OpenSSH). These verify the win-exec port end-to-end:
//! UTF-16LE EncodedCommand, golden recipe, exact exit codes, scp fallback.

use unirun::spec::Shell;
use unirun::transport::{ssh_run, SshTarget};

fn target(shell: Shell) -> SshTarget {
    let host = std::env::var("UNIRUN_TEST_SSH_HOST").unwrap_or_else(|_| "win-los".into());
    SshTarget {
        host,
        shell,
        timeout_ms: 60_000,
        connect_timeout: 15,
    }
}

#[test]
#[ignore]
fn ssh_unicode_and_exact_exit_code() {
    // win-exec K-verified behavior: golden recipe → clean UTF-8; exit contract
    // → exact remote rc through SSH.
    let t = target(Shell::Powershell);
    let r = ssh_run(&t, "Write-Output '远程OK'\nexit 42");
    assert_eq!(r.exit_code, Some(42), "stderr: {}", r.stderr);
    assert!(r.stdout.contains("远程OK"), "stdout: {}", r.stdout);
    assert!(
        !r.stdout.contains("CLIXML"),
        "CLIXML pollution: {}",
        r.stdout
    );
}

#[test]
#[ignore]
fn ssh_native_exit_code_propagation() {
    // PS native command failure must propagate via $LASTEXITCODE.
    let t = target(Shell::Powershell);
    let r = ssh_run(&t, "cmd /c exit 7");
    assert_eq!(r.exit_code, Some(7), "stderr: {}", r.stderr);
}

#[test]
#[ignore]
fn ssh_large_payload_scp_fallback() {
    // >60k base64 → scp + `-File` path; UTF-8 BOM keeps Chinese content valid.
    let t = target(Shell::Powershell);
    let big = format!(
        "$s = '{}'\nWrite-Output $s.Length\nWrite-Output '尾部中文'",
        "x".repeat(40_000)
    );
    let r = ssh_run(&t, &big);
    assert_eq!(r.exit_code, Some(0), "stderr: {}", r.stderr);
    assert!(
        r.stdout.contains("40000"),
        "stdout tail: …{}",
        &r.stdout[r.stdout.len().saturating_sub(160)..]
    );
    assert!(
        r.stdout.contains("尾部中文"),
        "BOM/UTF-8 failure: …{}",
        &r.stdout[r.stdout.len().saturating_sub(160)..]
    );
}

#[test]
#[ignore]
fn ssh_banner_filtered_from_stderr() {
    let t = target(Shell::Powershell);
    let r = ssh_run(&t, "Write-Output hi");
    assert!(r.stderr.is_empty(), "banner leaked to stderr: {}", r.stderr);
}

#[test]
#[ignore]
fn ssh_cmd_shell_bat_mode() {
    let t = target(Shell::Cmd);
    let r = ssh_run(&t, "echo hello-from-cmd\r\nexit /b 3");
    assert_eq!(r.exit_code, Some(3), "stderr: {}", r.stderr);
    assert!(r.stdout.contains("hello-from-cmd"), "stdout: {}", r.stdout);
}
