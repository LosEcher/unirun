//! MCP server integration test — drives the real `unirun mcp` binary over
//! stdio with the JSON-RPC 2.0 protocol, exactly like an agent client would.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

struct McpSession {
    child: std::process::Child,
    reader: BufReader<std::process::ChildStdout>,
    stdin: std::process::ChildStdin,
    next_id: u64,
}

impl McpSession {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_unirun"))
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn unirun mcp");
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        McpSession {
            child,
            reader,
            stdin,
            next_id: 1,
        }
    }

    /// Send a request, read exactly one response line, return parsed JSON.
    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{}", serde_json::to_string(&msg).unwrap()).unwrap();
        self.stdin.flush().unwrap();
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        let v: serde_json::Value = serde_json::from_str(&line)
            .unwrap_or_else(|e| panic!("invalid JSON response `{}`: {}", line, e));
        assert_eq!(v["id"], serde_json::json!(id), "response id mismatch");
        v
    }

    fn request_ok(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let v = self.request(method, params);
        assert!(v.get("error").is_none(), "unexpected error: {}", v);
        v["result"].clone()
    }

    fn close(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_initialize_and_list_tools() {
    let mut s = McpSession::start();
    let result = s.request_ok(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "test", "version": "0" }
        }),
    );
    assert_eq!(result["serverInfo"]["name"], "unirun");
    assert_eq!(result["capabilities"]["tools"], serde_json::json!({}));

    let tools = s.request_ok("tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"exec.run"));
    assert!(names.contains(&"exec.script"));
    assert!(names.contains(&"exec.probe"));
    assert!(
        names.contains(&"exec.capabilities"),
        "agents must be able to ask what this build can do: {names:?}"
    );
    s.close();
}

/// `exec.capabilities` is the MCP half of `unirun capabilities --json`: a
/// consumer gates on behaviour keys instead of parsing `--version`.
#[test]
fn mcp_exec_capabilities_describes_the_build() {
    let mut s = McpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let result = s.request_ok(
        "tools/call",
        serde_json::json!({ "name": "exec.capabilities", "arguments": {} }),
    );
    assert_eq!(result["isError"], serde_json::json!(false), "{}", result);
    let text = result["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["unirun"]["version"], env!("CARGO_PKG_VERSION"));
    let features: Vec<&str> = parsed["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    for key in ["ssh", "ssh-workdir-env", "strict-flags", "dispatched"] {
        assert!(features.contains(&key), "missing `{key}` in {features:?}");
    }
    s.close();
}

#[test]
fn mcp_exec_run_ok() {
    let mut s = McpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let result = s.request_ok(
        "tools/call",
        serde_json::json!({
            "name": "exec.run",
            "arguments": { "command": "echo hello-mcp" }
        }),
    );
    assert_eq!(
        result["isError"],
        serde_json::json!(false),
        "exec.run reported an error; full result: {}",
        result
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(
        parsed["exit_code"],
        serde_json::json!(0),
        "result: {}",
        parsed
    );
    assert_eq!(parsed["stdout"], "hello-mcp\n", "result: {}", parsed);
    s.close();
}

#[test]
fn mcp_exec_run_unicode() {
    let mut s = McpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let result = s.request_ok(
        "tools/call",
        serde_json::json!({
            "name": "exec.run",
            "arguments": { "command": "echo '中文MCP'" }
        }),
    );
    let text = result["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["stdout"], "中文MCP\n");
    assert_eq!(parsed["encoding"], "utf-8");
    s.close();
}

#[test]
fn mcp_exec_run_error_flag_and_class() {
    let mut s = McpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let result = s.request_ok(
        "tools/call",
        serde_json::json!({
            "name": "exec.run",
            "arguments": { "command": "definitely_not_a_real_cmd_mcp_xyz" }
        }),
    );
    assert_eq!(result["isError"], serde_json::json!(true));
    let parsed: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        parsed["error_class"], "COMMAND_NOT_FOUND",
        "full result: {}",
        parsed
    );
    s.close();
}

#[test]
fn mcp_exec_run_timeout() {
    let mut s = McpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let result = s.request_ok(
        "tools/call",
        serde_json::json!({
            "name": "exec.run",
            "arguments": { "command": "sleep 5", "timeout": 1 }
        }),
    );
    assert_eq!(result["isError"], serde_json::json!(true));
    let parsed: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(parsed["error_class"], "TIMEOUT");
    assert_eq!(parsed["timed_out"], serde_json::json!(true));
    s.close();
}

#[test]
fn mcp_exec_script() {
    let mut s = McpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let result = s.request_ok(
        "tools/call",
        serde_json::json!({
            "name": "exec.script",
            "arguments": { "script": "echo one\necho two" }
        }),
    );
    let parsed: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(parsed["stdout"], "one\ntwo\n");
    s.close();
}

#[test]
fn mcp_probe() {
    let mut s = McpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let result = s.request_ok(
        "tools/call",
        serde_json::json!({
            "name": "exec.probe",
            "arguments": {}
        }),
    );
    let parsed: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(!parsed["platform"].as_str().unwrap().is_empty());
    assert!(parsed["shells"].is_array());
    s.close();
}

#[test]
fn mcp_unknown_method_errors() {
    let mut s = McpSession::start();
    let v = s.request("bogus/method", serde_json::json!({}));
    assert_eq!(v["error"]["code"], serde_json::json!(-32601));
    s.close();
}

#[test]
fn mcp_session_start_wait_output() {
    // Background sessions need an isolated UNIRUN_HOME + the real binary for
    // the detached runner; other tests here don't depend on these vars.
    let home = std::env::temp_dir().join(format!("unirun-mcp-sess-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let prev = std::env::var_os("UNIRUN_HOME");
    std::env::set_var("UNIRUN_HOME", &home);
    std::env::set_var("UNIRUN_BIN", env!("CARGO_BIN_EXE_unirun"));

    let mut s = McpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let tools = s.request_ok("tools/list", serde_json::json!({}));
    let names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"session.start"));
    assert!(names.contains(&"session.list"));

    let result = s.request_ok(
        "tools/call",
        serde_json::json!({
            "name": "session.start",
            "arguments": { "command": "echo mcp-bg", "label": "mcp-test" }
        }),
    );
    let st: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    let id = st["id"].as_str().unwrap().to_string();
    assert_eq!(st["status"], "running");

    let result = s.request_ok(
        "tools/call",
        serde_json::json!({
            "name": "session.wait",
            "arguments": { "id": id, "timeout": 15 }
        }),
    );
    let st: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(st["status"], "completed", "{}", st);

    let result = s.request_ok(
        "tools/call",
        serde_json::json!({
            "name": "session.output",
            "arguments": { "id": id }
        }),
    );
    let out: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(
        out["stdout"].as_str().unwrap().contains("mcp-bg"),
        "{}",
        out
    );

    let result = s.request_ok(
        "tools/call",
        serde_json::json!({ "name": "session.list", "arguments": {} }),
    );
    let all: serde_json::Value =
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap();
    assert!(all.as_array().unwrap().iter().any(|x| x["id"] == id));

    s.close();
    match prev {
        Some(v) => std::env::set_var("UNIRUN_HOME", v),
        None => std::env::remove_var("UNIRUN_HOME"),
    }
    std::env::remove_var("UNIRUN_BIN");
    let _ = std::fs::remove_dir_all(&home);
}

/// The cancellation notification must actually stop a running call. Before
/// this, `notifications/cancelled` was a no-op and the call ran to completion —
/// an agent that cancels a long command got nothing back until it finished.
#[test]
fn mcp_cancelled_call_reports_aborted() {
    let mut s = McpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let id = s.next_id;
    s.next_id += 1;
    writeln!(
        s.stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "exec.run", "arguments": { "command": "sleep 5" } }
        })
    )
    .unwrap();
    s.stdin.flush().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(200));
    writeln!(
        s.stdin,
        "{}",
        serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": id }
        })
    )
    .unwrap();
    s.stdin.flush().unwrap();

    let started = std::time::Instant::now();
    let mut line = String::new();
    s.reader.read_line(&mut line).unwrap();
    let elapsed = started.elapsed();
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(v["id"], serde_json::json!(id), "{}", v);
    let text = v["result"]["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["aborted"], serde_json::json!(true), "{}", parsed);
    assert_eq!(parsed["error_class"], serde_json::json!("ABORTED"));
    assert!(
        elapsed < std::time::Duration::from_secs(3),
        "the cancel must take effect promptly, not after the command: {elapsed:?}"
    );
    s.close();
}

/// Ctrl-C must stop the server. The installed SIGINT handler used to swallow
/// it: the process stayed alive with the flag set and nobody reading it.
#[cfg(unix)]
#[test]
fn mcp_server_exits_on_sigint() {
    let mut s = McpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    // SAFETY: signalling a child we started.
    unsafe { libc::kill(s.child.id() as i32, libc::SIGINT) };
    let started = std::time::Instant::now();
    loop {
        if let Some(status) = s.child.try_wait().unwrap() {
            assert_eq!(
                status.code(),
                Some(130),
                "SIGINT must map to the abort exit code"
            );
            break;
        }
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "the MCP server ignored SIGINT"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}
