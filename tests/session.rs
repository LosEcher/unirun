//! Background session integration — real detached runner via the library
//! (UNIRUN_BIN points at the actual binary) and the `unirun bg` CLI.

use std::process::{Command, Stdio};
use std::time::Duration;

fn tmp_home(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("unirun-sess-it-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

static SESS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Serialized env-scoped session test; returns the session id.
fn with_home(tag: &str, f: impl FnOnce(&std::path::Path)) {
    let _guard = SESS_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let home = tmp_home(tag);
    std::env::set_var("UNIRUN_HOME", &home);
    std::env::set_var("UNIRUN_BIN", env!("CARGO_BIN_EXE_unirun"));
    f(&home);
    std::env::remove_var("UNIRUN_HOME");
    std::env::remove_var("UNIRUN_BIN");
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn library_start_wait_output_roundtrip() {
    with_home("lib", |_| {
        let spec = unirun::spec::ExecSpec {
            command: "printf 'one\\n'; printf 'two' ; echo warn >&2".into(),
            ..Default::default()
        };
        let st = unirun::session::start(&spec, "lib-test").expect("start");
        assert_eq!(st.status, "running");
        let id = st.id.clone();
        let done = unirun::session::wait(&id, 15_000).expect("wait");
        assert_eq!(done.status, "completed");
        assert_eq!(done.exit_code, Some(0));
        assert!(done.duration_ms > 0);
        let (so, se, _) = unirun::session::output(&id, 4096).expect("output");
        assert_eq!(so, "one\ntwo");
        assert!(se.contains("warn"));
    });
}

#[test]
fn library_kill_terminates_long_run() {
    with_home("kill", |_| {
        let cmd = if cfg!(windows) {
            "ping -n 60 127.0.0.1 >nul"
        } else {
            "sleep 60"
        };
        let spec = unirun::spec::ExecSpec {
            command: cmd.into(),
            ..Default::default()
        };
        let st = unirun::session::start(&spec, "kill-test").expect("start");
        let id = st.id.clone();
        std::thread::sleep(Duration::from_millis(400));
        let killed = unirun::session::kill(&id).expect("kill");
        assert!(killed.is_terminal());
        let after = unirun::session::status(&id).expect("status");
        assert!(after.is_terminal());
    });
}

#[test]
fn library_timeout_marks_session_timed_out() {
    with_home("to", |_| {
        let cmd = if cfg!(windows) {
            "ping -n 60 127.0.0.1 >nul"
        } else {
            "sleep 60"
        };
        let spec = unirun::spec::ExecSpec {
            command: cmd.into(),
            timeout_ms: 500,
            ..Default::default()
        };
        let st = unirun::session::start(&spec, "to-test").expect("start");
        let id = st.id.clone();
        let done = unirun::session::wait(&id, 15_000).expect("wait");
        assert_eq!(done.status, "timed_out");
    });
}

#[test]
fn cli_bg_start_status_list() {
    with_home("cli", |home| {
        let child = Command::new(env!("CARGO_BIN_EXE_unirun"))
            .args(["bg", "start", "echo cli-bg", "--label", "cli-test"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn bg start");
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("session ") && text.contains("cli-test"), "{}", text);
        // Extract the id (first hex token after "session ").
        let id = text
            .split_whitespace()
            .nth(1)
            .expect("session id")
            .to_string();

        // Wait for completion via the CLI.
        let mut ok = false;
        for _ in 0..100 {
            let st = Command::new(env!("CARGO_BIN_EXE_unirun"))
                .args(["bg", "wait", &id, "--timeout", "2", "--json"])
                .output()
                .unwrap();
            let v: serde_json::Value =
                serde_json::from_slice(&st.stdout).unwrap_or_else(|_| serde_json::json!({}));
            if v["status"] == "completed" {
                ok = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(ok, "session never completed");

        let list = Command::new(env!("CARGO_BIN_EXE_unirun"))
            .args(["bg", "list", "--json"])
            .output()
            .unwrap();
        let arr: serde_json::Value = serde_json::from_slice(&list.stdout).unwrap();
        assert!(
            arr.as_array().unwrap().iter().any(|s| s["id"] == id),
            "{}",
            arr
        );

        let out = Command::new(env!("CARGO_BIN_EXE_unirun"))
            .args(["bg", "output", &id])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(text.contains("cli-bg"), "{}", text);

        let _ = home.join("sessions");
    });
}

#[test]
fn cli_bg_missing_id_usage_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_unirun"))
        .args(["bg", "status"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}
