//! CLI argument contract.
//!
//! The rule these tests pin down: an argument unirun does not recognise is a
//! **usage error**, never something that gets appended to the command that
//! runs. Before this contract, `unirun ssh host 'script' --cwd /srv` sent
//! `--cwd /srv` to the remote as part of the script (see
//! `los/packages/gateway/src/unirun-capabilities.ts:1-6`, written to work
//! around exactly that), and `--workdir=dir` silently became a positional.
//!
//! Platform-neutral on purpose: the failing paths never reach a shell, and the
//! one command that does run (`echo`) behaves the same on Windows.

use std::process::{Command, Output};

fn unirun(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_unirun"))
        .args(args)
        .output()
        .expect("unirun binary runs")
}

#[test]
fn unknown_long_flag_is_a_usage_error() {
    let out = unirun(&["run", "echo hi", "--nope"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {:?}", out.stderr);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("unknown flag `--nope`"), "{stderr}");
    assert!(
        stderr.contains("`--`"),
        "the error must point at the escape hatch: {stderr}"
    );
    assert!(
        out.stdout.is_empty(),
        "a usage error must not run anything: {:?}",
        out.stdout
    );
}

#[test]
fn unknown_flag_is_not_appended_to_the_command() {
    // The old behaviour: positional.join(" ") → `echo hi --nope` actually ran.
    let out = unirun(&["run", "echo hi", "--nope", "--json"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "{:?}", out.stdout);
}

#[test]
fn ssh_unknown_flag_never_reaches_the_script() {
    // `--cwd` is the historical typo: it used to land inside the remote script.
    // The usage error fires before any connection attempt.
    let out = unirun(&["ssh", "host.invalid", "echo hi", "--cwd", "/srv"]);
    assert_eq!(out.status.code(), Some(2), "stderr: {:?}", out.stderr);
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown flag `--cwd`"));
}

#[test]
fn equals_form_is_accepted() {
    let out = unirun(&["run", "echo hi", "--timeout=30", "--json"]);
    assert!(out.status.success(), "stderr: {:?}", out.stderr);
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed["stdout"], serde_json::json!("hi\n"));
}

#[test]
fn double_dash_ends_flag_parsing() {
    // Everything after `--` is positional and reaches the shell intact —
    // including something that looks like a unirun flag. (Flags must come
    // before `--`, since they are positionals afterwards.)
    let out = unirun(&["run", "echo", "--json", "--", "--not-a-unirun-flag"]);
    assert!(out.status.success(), "stderr: {:?}", out.stderr);
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed["stdout"], serde_json::json!("--not-a-unirun-flag\n"));
}

#[test]
fn single_dash_arguments_stay_positional() {
    // `-la` is an argument to the command, not a unirun flag.
    let out = unirun(&["run", "echo", "-la", "--json"]);
    assert!(out.status.success(), "stderr: {:?}", out.stderr);
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed["stdout"], serde_json::json!("-la\n"));
}

#[test]
fn probe_rejects_unknown_arguments() {
    let out = unirun(&["probe", "--wat"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown flag `--wat`"));
}

#[test]
fn known_flags_still_work_after_the_contract() {
    let out = unirun(&["probe", "--json"]);
    assert!(out.status.success(), "stderr: {:?}", out.stderr);
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(parsed["platform"].is_string());
}
