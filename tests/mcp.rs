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
    assert_eq!(result["isError"], serde_json::json!(false));
    let text = result["content"][0]["text"].as_str().unwrap();
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(parsed["exit_code"], serde_json::json!(0));
    assert_eq!(parsed["stdout"], "hello-mcp\n");
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
    assert_eq!(parsed["error_class"], "COMMAND_NOT_FOUND");
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
