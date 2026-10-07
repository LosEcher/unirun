#![cfg(unix)]
//! Library ↔ CLI parity.
//!
//! The library and the CLI are two doors to one normalizer. If they drift, a
//! consumer's decision (retry? show the hint? trust `truncated`?) depends on
//! which door it came through — the exact class of bug this project exists to
//! remove. Each case runs the same spec both ways and compares every normalized
//! field.

use std::process::Command;
use unirun::spec::{ExecResult, ExecSpec, Shell};

/// Fields that must agree between the library and the CLI.
fn assert_same(lib: &ExecResult, cli: &serde_json::Value, what: &str) {
    let json = serde_json::to_value(lib).expect("lib result serializes");
    for field in [
        "exit_code",
        "stdout",
        "stderr",
        "timed_out",
        "aborted",
        "error_class",
        "hint",
        "encoding",
        "truncated",
        "shell_used",
        "transport_error",
        "dispatched",
        "kill_status",
        "exit_code_confidence",
        "drain_timeout",
    ] {
        assert_eq!(
            json.get(field),
            cli.get(field),
            "{what}: field `{field}` differs\nlib: {json}\ncli: {cli}"
        );
    }
    // `duration_ms` is wall clock: it must exist and be plausible, not equal.
    assert!(cli["duration_ms"].is_u64(), "{what}: {cli}");
}

fn cli_run(args: &[&str]) -> serde_json::Value {
    let out = Command::new(env!("CARGO_BIN_EXE_unirun"))
        .args(args)
        .output()
        .expect("run unirun");
    assert!(out.status.success(), "cli failed: {:?}", out);
    serde_json::from_slice(&out.stdout).expect("normalized JSON on stdout")
}

fn parity(spec: ExecSpec, cli_args: &[&str]) {
    let lib = unirun::run(&spec);
    let cli = cli_run(cli_args);
    assert_same(&lib, &cli, cli_args.join(" ").leak());
}

#[test]
fn parity_success_unicode_and_explicit_rc() {
    parity(
        ExecSpec {
            command: "printf '中文 OK\\n'; exit 42".into(),
            shell: Some(Shell::Bash),
            ..Default::default()
        },
        &["run", "printf '中文 OK\\n'; exit 42", "--json"],
    );
}

#[test]
fn parity_failure_with_evidence() {
    parity(
        ExecSpec {
            command: "echo 'ModuleNotFoundError: no module named x' >&2; exit 1".into(),
            shell: Some(Shell::Bash),
            ..Default::default()
        },
        &[
            "run",
            "echo 'ModuleNotFoundError: no module named x' >&2; exit 1",
            "--json",
        ],
    );
}

#[test]
fn parity_timeout_and_kill_status() {
    parity(
        ExecSpec {
            command: "sleep 30".into(),
            shell: Some(Shell::Bash),
            timeout_ms: 300,
            grace_ms: 300,
            ..Default::default()
        },
        &["run", "sleep 30", "--timeout", "1", "--json"],
    );
}

#[test]
fn parity_truncation() {
    parity(
        ExecSpec {
            command: "seq 1 20000".into(),
            shell: Some(Shell::Bash),
            max_output_bytes: 512,
            ..Default::default()
        },
        &["run", "seq 1 20000", "--max-output", "512", "--json"],
    );
}

/// The embedding API: a caller-owned abort flag cancels without a signal, and
/// reports the same `aborted`/`ABORTED` pair the CLI produces on SIGINT.
#[test]
fn caller_owned_abort_matches_the_signal_path() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let flag = Arc::new(AtomicBool::new(false));
    let setter = flag.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(150));
        setter.store(true, Ordering::SeqCst);
    });
    let r = unirun::run_with_abort(
        &ExecSpec {
            command: "sleep 30".into(),
            shell: Some(Shell::Bash),
            ..Default::default()
        },
        &flag,
    );
    assert!(r.aborted, "{r:?}");
    assert_eq!(r.error_class.as_deref(), Some("ABORTED"));
    assert_eq!(r.exit_code, None);
    assert!(r.dispatched, "it did start: {r:?}");
    // Cancelled, not timed out: the flag outranks the deadline.
    assert!(!r.timed_out);
}
