//! Recipe system integration: toolchain direct execution, CLI default
//! application (max_output/timeout), and --toolchain runner resolution.

use std::io::Write;
use std::process::{Command, Stdio};

fn tmp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("unirun-recipe-{}-{}", tag, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_recipe(dir: &std::path::Path, body: &str) {
    let d = dir.join(".unirun");
    std::fs::create_dir_all(&d).unwrap();
    std::fs::write(d.join("recipe.toml"), body).unwrap();
}

#[test]
fn toolchain_direct_argv_via_library() {
    let dir = tmp_dir("lib");
    let script = dir.join("hello.py");
    std::fs::write(&script, "print('toolchain-ok')\n").unwrap();
    let r = unirun::run(&unirun::spec::ExecSpec {
        command: String::new(),
        kind: unirun::spec::ExecKind::Script,
        workdir: Some(dir.clone()),
        direct: Some(vec![
            "python3".into(),
            script.to_string_lossy().into_owned(),
        ]),
        ..Default::default()
    });
    assert_eq!(r.exit_code, Some(0), "stderr: {}", r.stderr);
    assert_eq!(r.stdout.trim(), "toolchain-ok");
    assert_eq!(r.shell_used, "python3");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn recipe_toolchain_resolves_runner() {
    let dir = tmp_dir("resolve");
    write_recipe(
        &dir,
        r#"
schema = 1
[toolchains.python]
runner = "python3"
args = []
"#,
    );
    let recipe = unirun::recipe::Recipe::load_from_dir(&dir).unwrap();
    let (runner, args) = recipe.resolve_toolchain("python").expect("python resolves");
    assert_eq!(runner, "python3");
    assert!(args.is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cli_recipe_max_output_applied() {
    // Recipe caps output at 128 bytes → the CLI must mark the run truncated.
    let dir = tmp_dir("cap");
    write_recipe(&dir, "[conventions]\nmax_output_bytes = 128\n");
    let out = Command::new(env!("CARGO_BIN_EXE_unirun"))
        .args(["run", "seq 1 20000", "--workdir"])
        .arg(&dir)
        .arg("--json")
        .output()
        .unwrap();
    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        parsed["truncated"],
        serde_json::json!(true),
        "recipe cap not applied"
    );
    assert!(parsed["stdout"].as_str().unwrap().len() <= 256);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cli_toolchain_runner_executes_script() {
    let dir = tmp_dir("tc");
    write_recipe(
        &dir,
        r#"
schema = 1
[toolchains.python]
runner = "python3"
args = []
"#,
    );
    let script = dir.join("main.py");
    std::fs::write(&script, "print('ran-via-toolchain')\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_unirun"))
        .args(["script"])
        .arg(&script)
        .args(["--workdir"])
        .arg(&dir)
        .args(["--toolchain", "python", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let parsed: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(parsed["exit_code"], serde_json::json!(0));
    assert_eq!(
        parsed["stdout"].as_str().unwrap().trim(),
        "ran-via-toolchain"
    );
    assert_eq!(parsed["shell_used"].as_str().unwrap(), "python3");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cli_toolchain_missing_runner_fails_cleanly() {
    let dir = tmp_dir("missing");
    write_recipe(
        &dir,
        r#"
schema = 1
[toolchains.nope]
runner = "this_runner_does_not_exist_xyz"
"#,
    );
    let script = dir.join("x.py");
    std::fs::write(&script, "print(1)\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_unirun"))
        .args(["script"])
        .arg(&script)
        .args(["--workdir"])
        .arg(&dir)
        .args(["--toolchain", "nope", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "usage error expected");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not resolvable"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cli_mcp_stdin_streams_one_line_per_request() {
    // Sanity: `unirun mcp` speaks newline-delimited JSON (already covered in
    // tests/mcp.rs); here we just verify the binary exposes the subcommand.
    let mut child = Command::new(env!("CARGO_BIN_EXE_unirun"))
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    {
        let stdin = child.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"ping","params":{{}}}}"#
        )
        .unwrap();
        stdin.flush().unwrap();
    }
    let mut line = String::new();
    {
        use std::io::BufRead;
        let mut r = std::io::BufReader::new(child.stdout.take().unwrap());
        r.read_line(&mut line).unwrap();
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(line.contains("\"result\""), "ping response: {}", line);
    let _ = std::fs::remove_dir_all(tmp_dir("mcp"));
}
