#![cfg(feature = "winrm")]
//! WinRM provider smoke tests — `#[ignore]` by default because they need a
//! live Windows host with WinRM enabled. Run with:
//!
//!   UNIRUN_TEST_WINRM_HOST=<host> \
//!   UNIRUN_TEST_WINRM_USER=<user> \
//!   UNIRUN_TEST_WINRM_PASS=<password> \
//!   cargo test --features winrm -- --ignored
//!
//! The POC contract verified end-to-end: CLIXML-decoded UTF-8 output, exact
//! `$LASTEXITCODE` propagation via the sentinel, and error-stream
//! classification. (Top-level `exit N` in the script terminates the PSRP
//! runspace before the sentinel runs — use native commands to set
//! `$LASTEXITCODE`, mirroring what agents do.)

use unirun::winrm::{winrm_run, WinrmAuth, WinrmTarget};

fn target() -> WinrmTarget {
    WinrmTarget {
        host: std::env::var("UNIRUN_TEST_WINRM_HOST").unwrap_or_else(|_| "win-los".into()),
        username: std::env::var("UNIRUN_TEST_WINRM_USER").unwrap_or_default(),
        password: std::env::var("UNIRUN_TEST_WINRM_PASS").unwrap_or_default(),
        timeout_ms: 60_000,
        ..Default::default()
    }
}

#[test]
#[ignore]
fn winrm_unicode_and_exact_exit_code() {
    let t = target();
    let r = winrm_run(&t, "Write-Output '远程OK'\ncmd /c exit 42");
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
fn winrm_native_failure_classified() {
    let t = target();
    let r = winrm_run(&t, "definitely_not_a_real_cmdlet_winrm");
    assert_eq!(r.exit_code, Some(1), "stderr: {}", r.stderr);
    assert_eq!(
        r.error_class.as_deref(),
        Some("COMMAND_NOT_FOUND"),
        "stderr: {}",
        r.stderr
    );
}

#[test]
#[ignore]
fn winrm_https_basic_auth() {
    let mut t = target();
    t.use_tls = true;
    t.port = 5986;
    t.auth = WinrmAuth::Basic;
    t.accept_invalid_certs = true; // test hosts rarely have a trusted cert
    let r = winrm_run(&t, "Write-Output 'tls-ok'\ncmd /c exit 0");
    assert_eq!(r.exit_code, Some(0), "stderr: {}", r.stderr);
    assert!(r.stdout.contains("tls-ok"), "stdout: {}", r.stdout);
}
