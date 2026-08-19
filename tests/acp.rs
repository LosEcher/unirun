//! ACP (Agent Client Protocol) v1 integration — drives the real `unirun acp`
//! binary over stdio exactly like an ACP client (Zed, Cursor, …) would:
//! initialize → session/new → session/prompt (streaming session/update
//! notifications) → stopReason; plus session/cancel and error paths.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

struct AcpSession {
    child: std::process::Child,
    reader: BufReader<std::process::ChildStdout>,
    stdin: std::process::ChildStdin,
    next_id: u64,
}

impl AcpSession {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_unirun"))
            .arg("acp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn unirun acp");
        let stdin = child.stdin.take().unwrap();
        let reader = BufReader::new(child.stdout.take().unwrap());
        AcpSession {
            child,
            reader,
            stdin,
            next_id: 1,
        }
    }

    fn send(&mut self, method: &str, params: serde_json::Value) -> u64 {
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
        id
    }

    /// Send a notification (no id — never gets a response).
    fn notify(&mut self, method: &str, params: serde_json::Value) {
        let msg = serde_json::json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{}", serde_json::to_string(&msg).unwrap()).unwrap();
        self.stdin.flush().unwrap();
    }

    fn read_line(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "acp server closed stdout unexpectedly");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("invalid JSON `{}`: {}", line, e))
    }

    /// Read lines until the response with `id` arrives; return
    /// `(response, notifications-seen-before-it)`.
    fn wait_response(&mut self, id: u64) -> (serde_json::Value, Vec<serde_json::Value>) {
        let mut notifications = Vec::new();
        loop {
            let v = self.read_line();
            if v.get("id").and_then(|x| x.as_u64()) == Some(id) {
                return (v, notifications);
            }
            if v.get("method").and_then(|m| m.as_str()) == Some("session/update") {
                notifications.push(v);
            }
        }
    }

    fn request_ok(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.send(method, params);
        let (resp, _) = self.wait_response(id);
        assert!(resp.get("error").is_none(), "unexpected error: {}", resp);
        resp["result"].clone()
    }

    fn close(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn new_session_id(s: &mut AcpSession) -> String {
    let result = s.request_ok(
        "session/new",
        serde_json::json!({ "cwd": std::env::temp_dir(), "mcpServers": [] }),
    );
    result["sessionId"].as_str().unwrap().to_string()
}

fn collect_text(notifications: &[serde_json::Value]) -> String {
    let mut out = String::new();
    for n in notifications {
        if let Some(text) = n
            .pointer("/params/update/content/text")
            .and_then(|t| t.as_str())
        {
            out.push_str(text);
            out.push('\n');
        }
    }
    out
}

#[test]
fn acp_initialize_negotiates_baseline() {
    let mut s = AcpSession::start();
    let result = s.request_ok(
        "initialize",
        serde_json::json!({ "protocolVersion": 1, "clientCapabilities": {} }),
    );
    assert_eq!(result["protocolVersion"], serde_json::json!(1));
    assert_eq!(
        result["agentCapabilities"]["sessionCapabilities"],
        serde_json::json!({})
    );
    assert_eq!(result["authMethods"], serde_json::json!([]));
    s.close();
}

#[test]
fn acp_prompt_streams_output_and_ends_turn() {
    let mut s = AcpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let sid = new_session_id(&mut s);
    let id = s.send(
        "session/prompt",
        serde_json::json!({
            "sessionId": sid,
            "prompt": [{ "type": "text", "text": "echo acp-ok" }]
        }),
    );
    let (resp, notifications) = s.wait_response(id);
    assert_eq!(resp["result"]["stopReason"], "end_turn");
    let text = collect_text(&notifications);
    assert!(text.contains("acp-ok"), "updates: {}", text);
    // The final chunk carries the normalized ExecResult.
    let last: serde_json::Value =
        serde_json::from_str(text.lines().last().unwrap()).expect("ExecResult JSON chunk");
    assert_eq!(last["exit_code"], serde_json::json!(0));
    s.close();
}

#[test]
fn acp_json_prompt_is_structured_spec() {
    let mut s = AcpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let sid = new_session_id(&mut s);
    let id = s.send(
        "session/prompt",
        serde_json::json!({
            "sessionId": sid,
            "prompt": [{
                "type": "text",
                "text": "{\"command\": \"echo structured-ok\", \"timeout\": 30}"
            }]
        }),
    );
    let (resp, notifications) = s.wait_response(id);
    assert_eq!(resp["result"]["stopReason"], "end_turn");
    assert!(collect_text(&notifications).contains("structured-ok"));
    s.close();
}

#[test]
fn acp_cancel_aborts_prompt() {
    let mut s = AcpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let sid = new_session_id(&mut s);
    let cmd = if cfg!(windows) {
        "ping -n 60 127.0.0.1 >nul"
    } else {
        "sleep 30"
    };
    let id = s.send(
        "session/prompt",
        serde_json::json!({
            "sessionId": sid,
            "prompt": [{ "type": "text", "text": cmd }]
        }),
    );
    // Give the worker a moment to start, then cancel.
    std::thread::sleep(std::time::Duration::from_millis(200));
    s.notify("session/cancel", serde_json::json!({ "sessionId": sid }));
    let (resp, _) = s.wait_response(id);
    assert_eq!(
        resp["result"]["stopReason"], "cancelled",
        "response: {}",
        resp
    );
    s.close();
}

#[test]
fn acp_busy_session_rejects_second_prompt() {
    let mut s = AcpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let sid = new_session_id(&mut s);
    let cmd = if cfg!(windows) {
        "ping -n 60 127.0.0.1 >nul"
    } else {
        "sleep 30"
    };
    let first = s.send(
        "session/prompt",
        serde_json::json!({
            "sessionId": sid,
            "prompt": [{ "type": "text", "text": cmd }]
        }),
    );
    let second = s.send(
        "session/prompt",
        serde_json::json!({
            "sessionId": sid,
            "prompt": [{ "type": "text", "text": "echo nope" }]
        }),
    );
    let (resp2, _) = s.wait_response(second);
    assert_eq!(resp2["error"]["code"], serde_json::json!(-32000));
    s.notify("session/cancel", serde_json::json!({ "sessionId": sid }));
    let (resp1, _) = s.wait_response(first);
    assert_eq!(resp1["result"]["stopReason"], "cancelled");
    s.close();
}

#[test]
fn acp_unknown_session_and_method_error() {
    let mut s = AcpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let id = s.send(
        "session/prompt",
        serde_json::json!({
            "sessionId": "does-not-exist",
            "prompt": [{ "type": "text", "text": "echo x" }]
        }),
    );
    let (resp, _) = s.wait_response(id);
    assert_eq!(resp["error"]["code"], serde_json::json!(-32001));

    let id = s.send("bogus/method", serde_json::json!({}));
    let (resp, _) = s.wait_response(id);
    assert_eq!(resp["error"]["code"], serde_json::json!(-32601));
    s.close();
}

#[test]
fn acp_multi_block_prompt_joins_lines() {
    let mut s = AcpSession::start();
    s.request_ok("initialize", serde_json::json!({}));
    let sid = new_session_id(&mut s);
    let id = s.send(
        "session/prompt",
        serde_json::json!({
            "sessionId": sid,
            "prompt": [
                { "type": "text", "text": "echo line-one" },
                { "type": "text", "text": "echo line-two" }
            ]
        }),
    );
    let (resp, notifications) = s.wait_response(id);
    assert_eq!(resp["result"]["stopReason"], "end_turn");
    let text = collect_text(&notifications);
    assert!(
        text.contains("line-one") && text.contains("line-two"),
        "{}",
        text
    );
    s.close();
}
