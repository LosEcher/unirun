//! SSH transport smoke tests — `#[ignore]` by default because they need a
//! reachable Windows host. Run with:
//!
//!   UNIRUN_TEST_SSH_HOST=<ssh-alias-or-user@host> cargo test -- --ignored
//!
//! The default alias in CI-free local runs is `win-los` (our Win collection
//! node: PS 5.1 + Win32-OpenSSH). These verify the win-exec port end-to-end:
//! UTF-16LE EncodedCommand, golden recipe, exact exit codes, scp fallback.

use std::path::PathBuf;
use unirun::spec::Shell;
use unirun::transport::{ssh_run, SshTarget};

fn target(shell: Shell) -> SshTarget {
    let host = std::env::var("UNIRUN_TEST_SSH_HOST").unwrap_or_else(|_| "win-los".into());
    SshTarget {
        host,
        shell,
        timeout_ms: 60_000,
        connect_timeout: 15,
        ..Default::default()
    }
}

/// The local ssh client's own failure must be labelled `TRANSPORT` and split
/// out of `stderr`, so a caller can tell "never ran" from "the remote exited
/// 255". Uses a port nothing listens on: no host, no credentials, no network.
/// Skipped when there is no ssh binary.
#[test]
fn ssh_transport_failure_is_classified() {
    if std::process::Command::new("ssh")
        .arg("-V")
        .output()
        .is_err()
    {
        eprintln!("skipping: no ssh binary on PATH");
        return;
    }
    let t = SshTarget {
        host: "127.0.0.1".into(),
        port: Some(1),
        connect_timeout: 3,
        timeout_ms: 10_000,
        ..Default::default()
    };
    let r = ssh_run(&t, "echo hi");
    assert!(
        r.transport_error,
        "a refused connection is a transport failure: {:?}",
        r
    );
    assert_eq!(r.error_class.as_deref(), Some("TRANSPORT"));
    assert!(
        r.transport_stderr
            .as_deref()
            .unwrap_or("")
            .to_lowercase()
            .contains("ssh"),
        "the client's own diagnostic must be preserved: {:?}",
        r.transport_stderr
    );
    assert!(
        !r.stderr.to_lowercase().contains("ssh:"),
        "the client's diagnostic must not stay in the remote stderr: {:?}",
        r.stderr
    );
    assert!(
        !r.dispatched,
        "a refused connection proves the remote command never ran: {:?}",
        r
    );
}

/// Unix remote target; host from `UNIRUN_TEST_SSH_HOST`, user/port/identity
/// from `UNIRUN_TEST_SSH_USER` / `UNIRUN_TEST_SSH_PORT` / `UNIRUN_TEST_SSH_IDENTITY`.
fn unix_target(shell: Shell) -> SshTarget {
    let host = std::env::var("UNIRUN_TEST_SSH_HOST").unwrap_or_else(|_| "localhost".into());
    SshTarget {
        host,
        shell,
        timeout_ms: 60_000,
        connect_timeout: 15,
        user: std::env::var("UNIRUN_TEST_SSH_USER").ok(),
        port: std::env::var("UNIRUN_TEST_SSH_PORT")
            .ok()
            .and_then(|p| p.parse().ok()),
        identity_file: std::env::var("UNIRUN_TEST_SSH_IDENTITY")
            .ok()
            .map(PathBuf::from),
        workdir: None,
        env: Vec::new(),
        max_output_bytes: 0,
        output_encoding: None,
        drain_ms: 0,
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

// ── Unix remote branch ────────────────────────────────────────────────────
// Same `#[ignore]` convention, but point UNIRUN_TEST_SSH_HOST at a reachable
// Linux/macOS host (e.g. `tencent-sin-t` or `node34-executor-1`). The script
// travels over stdin to `bash -s`; exit codes propagate exactly.

#[test]
#[ignore]
fn ssh_unix_echo_and_exact_exit_code() {
    let t = unix_target(Shell::Bash);
    let r = ssh_run(&t, "echo '远程OK'\nexit 42");
    assert_eq!(r.exit_code, Some(42), "stderr: {}", r.stderr);
    assert!(r.stdout.contains("远程OK"), "stdout: {}", r.stdout);
}

#[test]
#[ignore]
fn ssh_unix_command_not_found_classified() {
    let t = unix_target(Shell::Bash);
    let r = ssh_run(&t, "definitely_not_a_command_xyz_123");
    assert_eq!(r.error_class.as_deref(), Some("COMMAND_NOT_FOUND"));
    assert_eq!(r.exit_code, Some(127), "stderr: {}", r.stderr);
}

#[test]
#[ignore]
fn ssh_unix_sh_shell() {
    let t = unix_target(Shell::Sh);
    let r = ssh_run(&t, "echo sh-ok\nexit 3");
    assert_eq!(r.exit_code, Some(3), "stderr: {}", r.stderr);
    assert!(r.stdout.contains("sh-ok"), "stdout: {}", r.stdout);
}

#[test]
#[ignore]
fn ssh_unix_timeout_kills_tree() {
    let t = unix_target(Shell::Bash);
    let mut slow = t.clone();
    slow.timeout_ms = 1_500;
    let r = ssh_run(&slow, "sleep 30; echo never");
    assert!(r.timed_out, "expected timeout, got: {:?}", r.exit_code);
    assert!(!r.stdout.contains("never"));
}

/// Ctrl-C during a remote run must cancel it: the ssh client's tree is
/// signalled, the result reports `aborted`, and the CLI exits 130.
///
/// `#[ignore]` because it needs a reachable host — run it with
/// `UNIRUN_TEST_SSH_HOST=<host> cargo test -- --ignored`.
#[cfg(unix)]
#[test]
#[ignore]
fn ssh_abort_cancels_the_remote_run() {
    let host = std::env::var("UNIRUN_TEST_SSH_HOST").unwrap_or_else(|_| "win-los".into());
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_unirun"))
        .args(["ssh", &host, "sleep 60", "--shell", "bash", "--json"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("spawn unirun ssh");
    std::thread::sleep(std::time::Duration::from_secs(2));
    // SAFETY: signalling a child we own.
    unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    let out = child.wait_with_output().expect("wait for unirun ssh");
    let parsed: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("normalized JSON result");
    assert_eq!(parsed["aborted"], serde_json::json!(true), "{parsed}");
    assert_eq!(parsed["error_class"], serde_json::json!("ABORTED"));
    assert_eq!(
        out.status.code(),
        Some(0),
        "json mode exits 0 when unirun ran"
    );
}
