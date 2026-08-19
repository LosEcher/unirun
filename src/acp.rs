//! ACP (Agent Client Protocol) v1 server — stdio transport.
//!
//! Baseline surface of the current ACP spec (agentclientprotocol.com v1,
//! JSON-RPC 2.0, newline-delimited):
//!   `initialize`         → protocolVersion + agentCapabilities + authMethods
//!   `session/new`        → sessionId (cwd is honored)
//!   `session/prompt`     → streams `session/update` notifications
//!                          (`agent_message_chunk`) while the command runs,
//!                          then responds `{stopReason}` ("end_turn" |
//!                          "cancelled")
//!   `session/cancel`     → notification; aborts the in-flight command tree
//!   `session/update`     → the notification method this server uses to push
//!                          output to the client
//!
//! Prompt → command mapping: the text blocks of `prompt` are joined into a
//! command line (`ExecKind::Run`). A single text block whose content is a
//! JSON object `{"command"|"script", "shell", "timeout", "workdir", "env"}`
//! is interpreted as a structured `ExecSpec`. The final normalized
//! `ExecResult` is streamed as the last chunk — the same content contract
//! agents get from the MCP server.
//!
//! unirun is an executor, not an LLM agent: `agentCapabilities` advertises
//! only the baseline (`sessionCapabilities: {}`, no auth, no tools), so any
//! ACP v1 client (Zed, Cursor, …) can drive it as a "run this command" agent
//! without capability negotiation surprises.

use crate::exec::{run_with_abort_streaming, StreamChunk, StreamKind};
use crate::spec::{ExecKind, ExecSpec, Shell};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};

/// ACP protocol version we speak (integer per the v1 spec).
const PROTOCOL_VERSION: u64 = 1;
/// Max characters per streamed text chunk (keeps stdio lines bounded).
const MAX_CHUNK_CHARS: usize = 8192;
/// JSON-RPC application error: session-level errors (busy / unknown).
const ERR_SESSION: i64 = -32000;
const ERR_UNKNOWN_SESSION: i64 = -32001;
const ERR_INVALID_PARAMS: i64 = -32602;

struct Session {
    cwd: PathBuf,
    /// Set by `session/cancel`, read by the prompt worker's exec loop.
    abort: Arc<AtomicBool>,
    /// One prompt at a time per session; cleared by the worker on exit.
    busy: Arc<AtomicBool>,
}

/// Drop guard: clears the session's busy flag when the prompt worker exits
/// (including on panic).
struct ClearBusy(Arc<AtomicBool>);

impl Drop for ClearBusy {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

enum Out {
    Response {
        id: Value,
        result: Result<Value, Value>,
    },
    Notification {
        method: String,
        params: Value,
    },
}

/// Serve ACP until stdin closes.
pub fn serve() -> std::io::Result<()> {
    let (tx, rx) = mpsc::channel::<Out>();
    let writer = std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut w = stdout.lock();
        while let Ok(msg) = rx.recv() {
            let line = match msg {
                Out::Response { id, result } => match result {
                    Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
                    Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
                },
                Out::Notification { method, params } => {
                    json!({ "jsonrpc": "2.0", "method": method, "params": params })
                }
            };
            if let Ok(s) = serde_json::to_string(&line) {
                let _ = writeln!(w, "{}", s);
                let _ = w.flush();
            }
        }
    });

    let sessions: Arc<Mutex<HashMap<String, Session>>> =
        Arc::new(Mutex::new(HashMap::new()));

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let id = msg.get("id").cloned();
        let params = msg.get("params").cloned().unwrap_or(json!({}));

        match method {
            "initialize" => {
                respond(&tx, id, Ok(initialize_result()));
            }
            "session/new" => {
                let cwd = params.get("cwd").and_then(|v| v.as_str());
                match cwd {
                    Some(cwd) => {
                        let sid = new_id();
                        sessions.lock().unwrap().insert(
                            sid.clone(),
                            Session {
                                cwd: PathBuf::from(cwd),
                                abort: Arc::new(AtomicBool::new(false)),
                                busy: Arc::new(AtomicBool::new(false)),
                            },
                        );
                        respond(&tx, id, Ok(json!({ "sessionId": sid })));
                    }
                    None => respond(
                        &tx,
                        id,
                        Err(json!({
                            "code": ERR_INVALID_PARAMS,
                            "message": "session/new: missing required `cwd`"
                        })),
                    ),
                }
            }
            "session/prompt" => {
                let sid = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let (cwd, abort, busy) = {
                    let guard = sessions.lock().unwrap();
                    match guard.get(sid) {
                        Some(s) => (s.cwd.clone(), s.abort.clone(), s.busy.clone()),
                        None => {
                            respond(
                                &tx,
                                id,
                                Err(json!({
                                    "code": ERR_UNKNOWN_SESSION,
                                    "message": format!("unknown session `{}`", sid)
                                })),
                            );
                            continue;
                        }
                    }
                };
                // One in-flight prompt per session.
                if busy.swap(true, Ordering::AcqRel) {
                    respond(
                        &tx,
                        id,
                        Err(json!({
                            "code": ERR_SESSION,
                            "message": format!(
                                "session `{}` already has a prompt in flight",
                                sid
                            )
                        })),
                    );
                    continue;
                }
                abort.store(false, Ordering::SeqCst); // fresh turn
                let text = prompt_text(&params);
                if text.trim().is_empty() {
                    busy.store(false, Ordering::Release);
                    respond(
                        &tx,
                        id,
                        Err(json!({
                            "code": ERR_INVALID_PARAMS,
                            "message": "session/prompt: prompt has no text blocks"
                        })),
                    );
                    continue;
                }
                let spec = spec_from_prompt(&text, &cwd);
                let request_id = id.clone();
                let has_id = id.is_some();
                let sid_owned = sid.to_string();
                let message_id = new_id();
                let worker_tx = tx.clone();
                std::thread::spawn(move || {
                    let _clear = ClearBusy(busy.clone());
                    run_prompt(
                        &worker_tx,
                        &sid_owned,
                        &message_id,
                        request_id,
                        has_id,
                        spec,
                        &abort,
                    );
                });
            }
            "session/cancel" => {
                let sid = params
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if let Some(s) = sessions.lock().unwrap().get(sid) {
                    s.abort.store(true, Ordering::SeqCst);
                }
                // Notifications get no response; requests get a plain ok.
                if let Some(id) = id {
                    respond(&tx, Some(id), Ok(json!({})));
                }
            }
            "session/update" => {
                // Agent → client notification channel; a client should never
                // call it. Tolerate silently (it may carry an id).
                if let Some(id) = id {
                    respond(
                        &tx,
                        Some(id),
                        Err(json!({
                            "code": -32601,
                            "message": "method not found: session/update"
                        })),
                    );
                }
            }
            "ping" => {
                respond(&tx, id, Ok(json!({})));
            }
            other => {
                if let Some(id) = id {
                    respond(
                        &tx,
                        Some(id),
                        Err(json!({
                            "code": -32601,
                            "message": format!("method not found: {}", other)
                        })),
                    );
                }
            }
        }
    }

    drop(tx);
    let _ = writer.join();
    Ok(())
}

/// The prompt worker: stream output, then send the final result + stopReason.
fn run_prompt(
    tx: &mpsc::Sender<Out>,
    sid: &str,
    message_id: &str,
    request_id: Option<Value>,
    has_id: bool,
    spec: ExecSpec,
    abort: &AtomicBool,
) {
    let (chunk_tx, chunk_rx) = mpsc::channel::<StreamChunk>();
    let forward_tx = tx.clone();
    let forward_sid = sid.to_string();
    let forward_mid = message_id.to_string();
    let forward = std::thread::spawn(move || {
        for chunk in chunk_rx {
            for piece in split_chars(&chunk.text, MAX_CHUNK_CHARS) {
                let text = match chunk.stream {
                    StreamKind::Stdout => piece,
                    StreamKind::Stderr => format!("[stderr] {}", piece),
                };
                let _ = forward_tx.send(Out::Notification {
                    method: "session/update".into(),
                    params: json!({
                        "sessionId": forward_sid,
                        "update": {
                            "sessionUpdate": "agent_message_chunk",
                            "messageId": forward_mid,
                            "content": { "type": "text", "text": text }
                        }
                    }),
                });
            }
        }
    });

    let result = run_with_abort_streaming(&spec, abort, Some(chunk_tx));
    let _ = forward.join();

    // Final chunk: the normalized ExecResult (the MCP content contract).
    let final_text = serde_json::to_string(&result).unwrap_or_else(|_| "{}".into());
    let _ = tx.send(Out::Notification {
        method: "session/update".into(),
        params: json!({
            "sessionId": sid,
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "messageId": message_id,
                "content": { "type": "text", "text": final_text }
            }
        }),
    });

    if has_id {
        let stop = if result.aborted { "cancelled" } else { "end_turn" };
        let _ = tx.send(Out::Response {
            id: request_id.unwrap_or(Value::Null),
            result: Ok(json!({ "stopReason": stop })),
        });
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": { "audio": false, "embeddedContext": false, "image": false },
            "mcpCapabilities": { "http": false, "sse": false },
            "sessionCapabilities": {},
            "auth": {}
        },
        "authMethods": []
    })
}

/// Concatenate the text of all `ContentBlock::Text` entries in `prompt`.
fn prompt_text(params: &Value) -> String {
    let blocks = params
        .get("prompt")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let texts: Vec<String> = blocks
        .iter()
        .filter_map(|b| b.get("text").and_then(|t| t.as_str()).map(String::from))
        .collect();
    texts.join("\n")
}

/// Prompt → ExecSpec: JSON object (command/script + options) or plain command.
fn spec_from_prompt(text: &str, cwd: &std::path::Path) -> ExecSpec {
    let mut spec = ExecSpec {
        command: text.trim().to_string(),
        kind: ExecKind::Run,
        workdir: Some(cwd.to_path_buf()),
        ..Default::default()
    };
    if let Ok(v) = serde_json::from_str::<Value>(text.trim()) {
        if let Some(obj) = v.as_object() {
            if let Some(cmd) = obj.get("command").and_then(|c| c.as_str()) {
                spec.command = cmd.to_string();
                apply_spec_json(&mut spec, &v);
                return spec;
            }
            if let Some(script) = obj.get("script").and_then(|c| c.as_str()) {
                spec.command = script.to_string();
                spec.kind = ExecKind::Script;
                apply_spec_json(&mut spec, &v);
                return spec;
            }
        }
    }
    spec
}

fn apply_spec_json(spec: &mut ExecSpec, v: &Value) {
    if let Some(s) = v.get("shell").and_then(|x| x.as_str()) {
        spec.shell = Shell::from_name(s);
    }
    if let Some(t) = v.get("timeout").and_then(|x| x.as_f64()) {
        spec.timeout_ms = (t * 1000.0) as u64;
    }
    if let Some(d) = v.get("workdir").and_then(|x| x.as_str()) {
        spec.workdir = Some(d.into());
    }
    if let Some(env) = v.get("env").and_then(|x| x.as_object()) {
        spec.env = env
            .iter()
            .map(|(k, val)| (k.clone(), val.as_str().unwrap_or("").to_string()))
            .collect();
    }
}

fn split_chars(text: &str, max: usize) -> Vec<String> {
    if text.chars().count() <= max {
        return vec![text.to_string()];
    }
    let mut out = Vec::new();
    let mut buf = String::with_capacity(max);
    for c in text.chars() {
        buf.push(c);
        if buf.chars().count() >= max {
            out.push(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        out.push(buf);
    }
    out
}

fn respond(tx: &mpsc::Sender<Out>, id: Option<Value>, result: Result<Value, Value>) {
    if let Some(id) = id {
        let _ = tx.send(Out::Response { id, result });
    }
}

fn new_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        "{:x}{:x}{:x}",
        std::process::id(),
        t,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_text_joins_blocks() {
        let params = json!({
            "prompt": [
                { "type": "text", "text": "echo a" },
                { "type": "text", "text": "echo b" }
            ]
        });
        assert_eq!(prompt_text(&params), "echo a\necho b");
    }

    #[test]
    fn prompt_text_skips_non_text_blocks() {
        let params = json!({
            "prompt": [
                { "type": "image", "data": "x" },
                { "type": "text", "text": "ls -la" }
            ]
        });
        assert_eq!(prompt_text(&params), "ls -la");
    }

    #[test]
    fn plain_prompt_is_run_command() {
        let spec = spec_from_prompt("echo hi", std::path::Path::new("/tmp"));
        assert_eq!(spec.command, "echo hi");
        assert_eq!(spec.kind, ExecKind::Run);
        assert_eq!(spec.workdir.as_deref(), Some(std::path::Path::new("/tmp")));
    }

    #[test]
    fn json_prompt_is_structured_spec() {
        let spec = spec_from_prompt(
            r#"{"command": "ls", "shell": "bash", "timeout": 5, "env": {"A": "1"}}"#,
            std::path::Path::new("/"),
        );
        assert_eq!(spec.command, "ls");
        assert_eq!(spec.shell, Some(Shell::Bash));
        assert_eq!(spec.timeout_ms, 5000);
        assert_eq!(spec.env, vec![("A".to_string(), "1".to_string())]);
    }

    #[test]
    fn split_chars_respects_max() {
        let pieces = split_chars("abcdef", 2);
        assert_eq!(pieces, vec!["ab", "cd", "ef"]);
        let one = split_chars("hi", 10);
        assert_eq!(one, vec!["hi"]);
        // Multibyte safety: pieces must rejoin to the original.
        let s = "中文输出内容".to_string();
        let rejoined: String = split_chars(&s, 3).concat();
        assert_eq!(rejoined, s);
    }
}
