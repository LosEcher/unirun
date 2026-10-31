//! `unirun lint` and the pre-exec heredoc guard.
//!
//! The contract these tests pin down: a provably risky inline heredoc is
//! refused **before spawn** (nothing ran), the escape hatch really runs it, and
//! the judgement itself is available without execution through `lint` with the
//! toolset's 0 / 1 / 2 exit codes.

use std::path::PathBuf;
use std::process::{Command, Output};

fn unirun(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unirun"))
        .args(args)
        .output()
        .expect("unirun binary runs")
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn temp_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "unirun-heredoc-{}-{}-{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    p
}

#[test]
fn lint_clean_is_exit_0() {
    let out = unirun(&["lint", "cat > f <<'UNIRUN_INPUT'\nplain\nUNIRUN_INPUT"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(String::from_utf8_lossy(&out.stdout).contains("clean"));
}

#[test]
fn lint_risky_is_exit_1_and_names_the_kind() {
    let out = unirun(&["lint", "cat > f <<EOF\n$HOME\nEOF"]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("unquoted-delimiter"), "{text}");
    assert!(text.contains("fix:"), "a finding must carry a fix: {text}");
}

#[test]
fn lint_json_carries_the_verdict() {
    let out = unirun(&[
        "lint",
        "--json",
        "printf x | python3 - <<'PY'\nprint(1)\nPY",
    ]);
    assert_eq!(out.status.code(), Some(1), "stderr: {}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"verdict\":\"risky\""), "{text}");
    assert!(text.contains("stdin-conflict"), "{text}");
}

#[test]
fn lint_without_input_is_exit_2() {
    let out = unirun(&["lint"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
}

#[test]
fn lint_unknown_flag_is_a_usage_error() {
    let out = unirun(&["lint", "--nope", "echo hi"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown flag `--nope`"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn lint_unreadable_file_is_exit_2_not_0() {
    let out = unirun(&["lint", "--file", "/nonexistent/unirun/script.sh"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
}

#[test]
fn run_refuses_risky_heredoc_and_runs_nothing() {
    let target = temp_path("refused");
    let cmd = format!("cat > {} <<EOF\n$HOME\nEOF", target.display());
    let out = unirun(&["run", &cmd]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("refusing"), "{}", stderr(&out));
    assert!(
        !target.exists(),
        "the guard must refuse before spawn: {} exists",
        target.display()
    );
}

#[test]
fn run_allow_really_runs_the_same_command() {
    let target = temp_path("allowed");
    let cmd = format!("cat > {} <<EOF\nliteral\nEOF", target.display());
    let out = unirun(&["run", &cmd, "--heredoc=allow"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(target.exists(), "{} was not written", target.display());
    let _ = std::fs::remove_file(&target);
}

#[test]
fn run_warn_runs_and_says_why() {
    let target = temp_path("warned");
    let cmd = format!("cat > {} <<EOF\n$HOME\nEOF", target.display());
    let out = unirun(&["run", &cmd, "--heredoc=warn"]);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("warning"), "{}", stderr(&out));
    assert!(target.exists(), "{} was not written", target.display());
    let _ = std::fs::remove_file(&target);
}

#[test]
fn json_refusal_is_machine_readable_and_says_nothing_ran() {
    let out = unirun(&["run", "--json", "cat > f <<EOF\n$HOME\nEOF"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("\"error_class\":\"HEREDOC_RISK\""), "{text}");
    assert!(text.contains("\"dispatched\":false"), "{text}");
}

#[test]
fn unknown_heredoc_mode_is_a_usage_error() {
    let out = unirun(&["run", "echo hi", "--heredoc=rewrite"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(
        stderr(&out).contains("unknown --heredoc mode"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn lint_stdin_is_how_a_host_hook_gates() {
    use std::io::Write;
    let mut child = Command::new(env!("CARGO_BIN_EXE_unirun"))
        .args(["lint", "--stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("unirun spawns");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"cat > f <<EOF\n$HOME\nEOF\n")
        .expect("write");
    let out = child.wait_with_output().expect("wait");
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("unquoted-delimiter"));
}
