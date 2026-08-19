//! P0 acceptance matrix — the win-exec A–L empirical matrix (2026-08-18),
//! ported to local POSIX semantics, plus the P0-specific guarantees:
//! timeout, whole-tree kill, tail truncation, taxonomy, encoding.

use std::process::Command;
use unirun::spec::{ExecResult, ExecSpec};

fn run_cmd(cmd: &str) -> ExecResult {
    unirun::run(&ExecSpec {
        command: cmd.to_string(),
        ..Default::default()
    })
}

// ---- win-exec matrix, local POSIX counterparts ----

#[test]
fn t_d_rc_propagation_explicit_exit() {
    // win-exec D: `powershell -Command "exit 42"` → rc 42
    let r = run_cmd("exit 42");
    assert_eq!(r.exit_code, Some(42));
    assert_eq!(r.error_class.as_deref(), None);
}

#[test]
fn t_e_nested_exit_code() {
    // win-exec E: `cmd /c "exit 7"` → rc 7
    let r = run_cmd("sh -c 'exit 7'");
    assert_eq!(r.exit_code, Some(7));
}

#[test]
fn t_f_quoted_metachars_safe() {
    // win-exec F (negative on Windows: cmd eats `>`). Locally, a quoted `>`
    // must NOT create a redirect side effect.
    let dir = std::env::temp_dir().join(format!("unirun-tf-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let r = unirun::run(&ExecSpec {
        command: "echo 'a > b' && echo done".into(),
        workdir: Some(dir.clone()),
        ..Default::default()
    });
    assert_eq!(r.exit_code, Some(0));
    assert!(r.stdout.contains("a > b"));
    assert!(r.stdout.contains("done"));
    // No stray file named "b" in the workdir.
    assert!(!dir.join("b").exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn t_g_h_multiline_script() {
    // win-exec G/H: multi-line scripts, line-by-line output
    let r = run_cmd("echo line1\necho line2\necho line3");
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.stdout, "line1\nline2\nline3\n");
}

#[test]
fn t_i_pipes_and_metachars() {
    // win-exec I: `$null`/`;`/pipes inside payload are safe
    let r = run_cmd("printf 'x\\n' | tr 'x' 'y'; echo end");
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.stdout, "y\nend\n");
}

#[test]
fn t_j_implicit_rc_is_last_command() {
    // win-exec J/L: without explicit exit, rc = last command's rc
    let ok = run_cmd("echo hi");
    assert_eq!(ok.exit_code, Some(0));
    let fail = run_cmd("false");
    assert_eq!(fail.exit_code, Some(1));
    let after_fail = run_cmd("false; echo survived");
    assert_eq!(after_fail.exit_code, Some(0));
    assert!(after_fail.stdout.contains("survived"));
}

#[test]
fn t_unicode_chinese_utf8() {
    // win-exec A/B/C: Chinese output must be clean UTF-8 (local: trivially,
    // but pins the pipeline so the remote provider can't regress it later)
    let r = run_cmd("echo '中文OK'");
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.stdout, "中文OK\n");
    assert_eq!(r.encoding, "utf-8");
}

#[test]
fn t_stdout_stderr_split() {
    let r = run_cmd("echo out; echo err >&2");
    assert_eq!(r.stdout, "out\n");
    assert_eq!(r.stderr, "err\n");
}

// ---- P0-specific guarantees ----

#[test]
fn t_timeout_kills_and_classifies() {
    let r = unirun::run(&ExecSpec {
        command: "sleep 5".into(),
        timeout_ms: 1000,
        ..Default::default()
    });
    assert!(r.timed_out, "expected timeout, got {:?}", r);
    assert!(r.duration_ms < 3_000, "took {}ms", r.duration_ms);
    assert_eq!(r.error_class.as_deref(), Some("TIMEOUT"));
}

#[test]
fn t_timeout_kills_whole_tree() {
    // Foreground `sleep 30` plus a backgrounded one. If the tree (negative
    // pgid) is not killed, wait blocks ~30s and this test fails by timing out.
    let r = unirun::run(&ExecSpec {
        command: "sleep 30 & sleep 30".into(),
        timeout_ms: 1000,
        ..Default::default()
    });
    assert!(r.timed_out);
    assert!(
        r.duration_ms < 5_000,
        "process tree not killed, took {}ms",
        r.duration_ms
    );
}

#[test]
fn t_output_tail_truncated_not_deadlocked() {
    let r = unirun::run(&ExecSpec {
        command: "seq 1 200000".into(),
        timeout_ms: 15_000,
        max_output_bytes: 4096,
        ..Default::default()
    });
    assert_eq!(r.exit_code, Some(0));
    assert!(r.truncated, "expected truncation");
    assert!(r.stdout.len() <= 4096 + 64, "stdout len {}", r.stdout.len());
    // Tail kept: the last line must survive.
    assert!(
        r.stdout.trim_end().ends_with("200000"),
        "tail not kept: ...{}",
        &r.stdout[r.stdout.len().saturating_sub(40)..]
    );
}

#[test]
fn t_command_not_found_classified() {
    let r = run_cmd("definitely_not_a_real_cmd_xyz_123");
    assert_eq!(r.exit_code, Some(127));
    assert_eq!(r.error_class.as_deref(), Some("COMMAND_NOT_FOUND"));
    assert!(r.hint.as_deref().unwrap_or("").contains("probe"));
}

#[cfg(unix)]
#[test]
fn t_permission_denied_classified() {
    use std::os::unix::fs::PermissionsExt;
    let dir = std::env::temp_dir().join(format!("unirun-tp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("noperm.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o000)).unwrap();
    let r = unirun::run(&ExecSpec {
        command: script.to_string_lossy().into_owned(),
        ..Default::default()
    });
    // Executing a 0o000 script through `bash -c <path>` yields 126 + Permission denied.
    assert_eq!(r.exit_code, Some(126), "stderr: {}", r.stderr);
    assert_eq!(r.error_class.as_deref(), Some("PERMISSION"));
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn t_invalid_utf8_lossy_labeled() {
    let r = run_cmd("printf '\\377\\376'"); // 0xFF 0xFE — invalid UTF-8
    assert!(
        r.encoding == "utf-8-lossy" || r.encoding == "utf-16le",
        "got {}",
        r.encoding
    );
    // Must not panic, must still return a classified result.
    assert_eq!(r.exit_code, Some(0));
}

#[test]
fn t_syntax_error_classified() {
    let r = run_cmd("if then fi");
    assert_eq!(r.error_class.as_deref(), Some("SYNTAX"));
    assert_ne!(r.exit_code, Some(0));
}

#[test]
fn t_explicit_shell_override() {
    // Explicit sh instead of default bash.
    let r = unirun::run(&ExecSpec {
        command: "echo $0".into(),
        shell: Some(unirun::spec::Shell::Sh),
        ..Default::default()
    });
    assert_eq!(r.exit_code, Some(0));
    assert_eq!(r.shell_used, "sh");
    assert!(r.stdout.trim().ends_with("sh"), "got: {}", r.stdout);
}

#[test]
fn t_env_override_applied() {
    let r = unirun::run(&ExecSpec {
        command: "echo \"$UNIRUN_TEST_VAR\"".into(),
        env: vec![("UNIRUN_TEST_VAR".into(), "hello-unirun".into())],
        ..Default::default()
    });
    assert_eq!(r.stdout.trim(), "hello-unirun");
}

#[test]
fn t_missing_shell_binary_reports_gracefully() {
    // A shell name with no binary must not panic; it returns a classified result.
    let r = unirun::run(&ExecSpec {
        command: "echo hi".into(),
        shell: Some(unirun::spec::Shell::Zsh),
        timeout_ms: 5_000,
        ..Default::default()
    });
    if Command::new("zsh").arg("--version").output().is_err() {
        assert_eq!(r.error_class.as_deref(), Some("COMMAND_NOT_FOUND"));
    } else {
        assert_eq!(r.exit_code, Some(0));
    }
}
