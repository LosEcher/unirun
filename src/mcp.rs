//! MCP (Model Context Protocol) server — stdio transport.
//!
//! Minimal, dependency-free implementation of the MCP 2024-11-05 tool
//! surface: `exec.run`, `exec.script`, `exec.probe`, `exec.capabilities` and
//! the `session.*` background tools. Newline-delimited JSON-RPC 2.0 over
//! stdin/stdout — the standard every MCP-capable agent (Claude Code, Cursor,
//! DSH, …) speaks. `unirun mcp` is a long-lived stdio process; run it once per
//! agent session.
//!
//! Concurrency: `tools/call` runs on its own thread so the reader keeps
//! draining stdin. That is what makes `notifications/cancelled` real — a
//! synchronous server cannot see the cancellation until the call it wants to
//! cancel has already finished (the previous implementation no-op'd the
//! notification, so a cancelled `exec.run` ran to completion). Cancellation is
//! per request: each call carries its own abort flag, so cancelling one call
//! does not disturb another.
//!
//! SIGINT is honoured too: the reader polls the process-wide abort flag, so
//! Ctrl-C stops the server instead of being swallowed by the installed handler
//! (measured before this change: the process stayed alive).

use crate::probe;
use crate::spec::{ExecKind, ExecResult, ExecSpec, Shell};
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::Duration;

/// How often the reader wakes up to notice a SIGINT.
const ABORT_POLL: Duration = Duration::from_millis(100);

/// One in-flight `tools/call`.
struct InFlight {
    /// The JSON-RPC request id, as sent (echoed in `notifications/cancelled`).
    request_id: Value,
    cancel: Arc<AtomicBool>,
    handle: std::thread::JoinHandle<()>,
}

/// Serve the MCP protocol until stdin closes (or SIGINT).
pub fn serve() -> std::io::Result<()> {
    // Reader thread: stdin → channel. All JSON parsing happens on the main
    // loop so a malformed line cannot desynchronise the stream.
    let (line_tx, line_rx) = mpsc::channel::<Option<Value>>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => {
                    if line_tx.send(Some(v)).is_err() {
                        break;
                    }
                }
                Err(_) => continue,
            }
        }
        let _ = line_tx.send(None);
    });

    // Writer thread: responses arrive from worker threads and are written one
    // line at a time, so two calls can never interleave inside a message.
    let (out_tx, out_rx) = mpsc::channel::<String>();
    let writer = std::thread::spawn(move || {
        let stdout = std::io::stdout();
        for line in out_rx {
            let mut lock = stdout.lock();
            if writeln!(lock, "{}", line).is_err() {
                break;
            }
            let _ = lock.flush();
        }
    });

    let mut inflight: Vec<InFlight> = Vec::new();
    loop {
        match line_rx.recv_timeout(ABORT_POLL) {
            Ok(Some(msg)) => {
                handle_message(&msg, &out_tx, &mut inflight);
            }
            Ok(None) => break, // stdin closed
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if crate::exec::abort_requested() {
                    // Ctrl-C: cancel what is running and let the workers unwind.
                    for req in &inflight {
                        req.cancel.store(true, Ordering::SeqCst);
                    }
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Cancel anything still running, then give the workers a moment to report.
    for req in &inflight {
        req.cancel.store(true, Ordering::SeqCst);
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(1_000);
    while inflight.iter().any(|r| !r.handle.is_finished()) {
        if std::time::Instant::now() >= deadline {
            break;
        }
        inflight.retain(|r| !r.handle.is_finished());
        std::thread::sleep(Duration::from_millis(10));
    }
    drop(out_tx);
    let _ = writer.join();
    Ok(())
}

fn handle_message(msg: &Value, out: &mpsc::Sender<String>, inflight: &mut Vec<InFlight>) {
    inflight.retain(|r| !r.handle.is_finished());
    let method = msg.get("method").and_then(|m| m.as_str());
    let id = msg.get("id").cloned();
    match method {
        Some("initialize") => {
            let result = json!({
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "unirun", "version": env!("CARGO_PKG_VERSION") }
            });
            send(out, response(id, Ok(result)));
        }
        Some("notifications/initialized") => {}
        Some("notifications/cancelled") => {
            // Per MCP: params.requestId identifies the call to cancel. An id we
            // do not know is ignored (the call already finished).
            let target = msg.pointer("/params/requestId").cloned();
            if let Some(target) = target {
                for req in inflight.iter() {
                    if req.request_id == target {
                        req.cancel.store(true, Ordering::SeqCst);
                    }
                }
            }
        }
        Some("ping") => send(out, response(id, Ok(json!({})))),
        Some("tools/list") => {
            let tools = json!([
                exec_run_tool(),
                exec_script_tool(),
                exec_probe_tool(),
                exec_capabilities_tool(),
                session_start_tool(),
                session_status_tool(),
                session_output_tool(),
                session_kill_tool(),
                session_list_tool(),
                session_wait_tool()
            ]);
            send(out, response(id, Ok(json!({ "tools": tools }))));
        }
        Some("tools/call") => {
            let name = msg
                .pointer("/params/name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let args = msg
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or(json!({}));
            let cancel = Arc::new(AtomicBool::new(false));
            let worker_cancel = cancel.clone();
            let out = out.clone();
            let handle = std::thread::spawn(move || {
                let (text, is_error) = call_tool(&name, &args, &worker_cancel);
                send(
                    &out,
                    response(
                        id,
                        Ok(json!({
                            "content": [{ "type": "text", "text": text }],
                            "isError": is_error
                        })),
                    ),
                );
            });
            inflight.push(InFlight {
                request_id: msg.get("id").cloned().unwrap_or(Value::Null),
                cancel,
                handle,
            });
        }
        _ => {
            send(
                out,
                response(
                    id,
                    Err(json!({
                        "code": -32601,
                        "message": format!("method not found: {}", method.unwrap_or("?"))
                    })),
                ),
            );
        }
    }
}

fn send(out: &mpsc::Sender<String>, msg: Value) {
    if msg.is_null() {
        // A notification gets no reply.
        return;
    }
    if let Ok(s) = serde_json::to_string(&msg) {
        let _ = out.send(s);
    }
}

fn response(id: Option<Value>, result: Result<Value, Value>) -> Value {
    match (id, result) {
        (Some(id), Ok(r)) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        (Some(id), Err(e)) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        // A notification gets no reply.
        (None, _) => Value::Null,
    }
}

fn tool_schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {
            "type": "object",
            "properties": properties,
            "required": required,
            "additionalProperties": false
        }
    })
}

fn common_properties() -> Value {
    json!({
        "workdir": { "type": "string", "description": "working directory (default: current)" },
        "timeout": { "type": "number", "description": "deadline in seconds (default 120)" },
        "env": { "type": "object", "additionalProperties": { "type": "string" }, "description": "environment overrides" }
    })
}

fn exec_run_tool() -> Value {
    let mut props = common_properties();
    props["command"] =
        json!({ "type": "string", "description": "command line to run through a shell" });
    props["shell"] = json!({
        "type": "string",
        "enum": ["bash", "sh", "zsh", "cmd", "powershell", "pwsh"],
        "description": "explicit shell; default auto-detect"
    });
    tool_schema("exec.run", "Run a command through a shell with normalized output (stable error_class + hint). Returns JSON.", props, &["command"])
}

fn exec_script_tool() -> Value {
    let mut props = common_properties();
    props["script"] = json!({ "type": "string", "description": "script body (shell inferred from content; --shell overrides)" });
    props["shell"] = json!({
        "type": "string",
        "enum": ["bash", "sh", "zsh", "cmd", "powershell", "pwsh"],
        "description": "explicit shell; default auto-detect"
    });
    tool_schema(
        "exec.script",
        "Run a multi-line script with normalized output. Returns JSON.",
        props,
        &["script"],
    )
}

fn exec_probe_tool() -> Value {
    tool_schema("exec.probe", "Return host capabilities: platform, shells, coreutils (e.g. GNU timeout availability), tools.", json!({}), &[])
}

fn exec_capabilities_tool() -> Value {
    tool_schema(
        "exec.capabilities",
        "Return what this unirun build can do as stable capability keys (plus version and schema), so callers gate on behaviour instead of parsing --version.",
        json!({}),
        &[],
    )
}

fn session_start_tool() -> Value {
    let mut props = common_properties();
    props["command"] =
        json!({ "type": "string", "description": "command line to run in the background" });
    props["label"] = json!({ "type": "string", "description": "human-readable session label" });
    tool_schema(
        "session.start",
        "Start a command as a detached background session; returns the session record (status running). Poll with session.status/session.wait.",
        props,
        &["command"],
    )
}

fn session_status_tool() -> Value {
    tool_schema(
        "session.status",
        "Return the current state of a background session (running/completed/timed_out/aborted/failed/killed/interrupted).",
        json!({ "id": { "type": "string", "description": "session id from session.start" } }),
        &["id"],
    )
}

fn session_output_tool() -> Value {
    tool_schema(
        "session.output",
        "Return background-session output: a byte tail, or only what is new since a cursor (pass a previous `next_cursor` as `cursor`).",
        json!({
            "id": { "type": "string", "description": "session id from session.start" },
            "tail": { "type": "number", "description": "max bytes per stream to return (default 65536)" },
            "cursor": { "type": "number", "description": "return only output appended after this cursor (from a previous next_cursor); overrides tail" }
        }),
        &["id"],
    )
}

fn session_kill_tool() -> Value {
    tool_schema(
        "session.kill",
        "Terminate a running background session (aborts the whole process tree).",
        json!({ "id": { "type": "string", "description": "session id from session.start" } }),
        &["id"],
    )
}

fn session_list_tool() -> Value {
    tool_schema(
        "session.list",
        "List all background sessions, newest first (terminal + running).",
        json!({}),
        &[],
    )
}

fn session_wait_tool() -> Value {
    tool_schema(
        "session.wait",
        "Block until a background session reaches a terminal state or the timeout elapses; returns the session record.",
        json!({
            "id": { "type": "string", "description": "session id from session.start" },
            "timeout": { "type": "number", "description": "max wait in seconds (default 120)" }
        }),
        &["id"],
    )
}

/// Run one tool call. `cancel` is this call's own abort flag: `exec.run` and
/// `exec.script` honour it, everything else is short-lived by construction.
fn call_tool(name: &str, args: &Value, cancel: &AtomicBool) -> (String, bool) {
    let result = match name {
        "exec.run" => {
            let command = args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if command.is_empty() {
                return (
                    json_error("exec.run: missing required argument `command`"),
                    true,
                );
            }
            unirun_run_cancellable(
                &ExecSpec {
                    command,
                    ..spec_from_args(args)
                },
                cancel,
            )
        }
        "exec.script" => {
            let script = args
                .get("script")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if script.is_empty() {
                return (
                    json_error("exec.script: missing required argument `script`"),
                    true,
                );
            }
            let spec = ExecSpec {
                command: script,
                kind: ExecKind::Script,
                ..spec_from_args(args)
            };
            unirun_run_cancellable(&spec, cancel)
        }
        "exec.probe" => {
            let caps = probe::probe();
            (
                serde_json::to_string(&caps).unwrap_or_else(|_| "{}".into()),
                false,
            )
        }
        "exec.capabilities" => {
            let caps = crate::capabilities::capabilities();
            (
                serde_json::to_string(&caps).unwrap_or_else(|_| "{}".into()),
                false,
            )
        }
        "session.start" => {
            let command = args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if command.is_empty() {
                return (
                    json_error("session.start: missing required argument `command`"),
                    true,
                );
            }
            let label = args
                .get("label")
                .and_then(|v| v.as_str())
                .unwrap_or("mcp-session")
                .to_string();
            let spec = ExecSpec {
                command,
                kind: ExecKind::Run,
                ..spec_from_args(args)
            };
            match crate::session::start(&spec, &label) {
                Ok(st) => (
                    serde_json::to_string(&st).unwrap_or_else(|_| "{}".into()),
                    false,
                ),
                Err(e) => (json_error(&e), true),
            }
        }
        "session.status" | "session.kill" | "session.wait" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                return (
                    json_error(&format!("{}: missing required argument `id`", name)),
                    true,
                );
            }
            let result = match name {
                "session.status" => crate::session::status(&id),
                "session.kill" => crate::session::kill(&id),
                _ => {
                    let timeout_ms = args
                        .get("timeout")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(120.0)
                        * 1000.0;
                    crate::session::wait(&id, timeout_ms as u64)
                }
            };
            match result {
                Ok(st) => (
                    serde_json::to_string(&st).unwrap_or_else(|_| "{}".into()),
                    false,
                ),
                Err(e) => (json_error(&e), true),
            }
        }
        "session.output" => {
            let id = args
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if id.is_empty() {
                return (
                    json_error("session.output: missing required argument `id`"),
                    true,
                );
            }
            // `cursor` reads incrementally from a previous `next_cursor`;
            // without it, the `tail` behaviour is kept for compatibility.
            let page = if let Some(cursor) = args.get("cursor").and_then(|v| v.as_u64()) {
                crate::session::output_since(&id, cursor)
            } else {
                let tail = args
                    .get("tail")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(65_536.0) as usize;
                crate::session::output(&id, tail).map(|(so, se, truncated_log)| {
                    crate::session::OutputPage {
                        id: id.clone(),
                        stdout: so,
                        stderr: se,
                        next_cursor: crate::session::log_len(&id),
                        truncated_log,
                        reset: false,
                    }
                })
            };
            match page {
                Ok(page) => (
                    serde_json::to_string(&page).unwrap_or_else(|_| "{}".into()),
                    false,
                ),
                Err(e) => (json_error(&e), true),
            }
        }
        "session.list" => {
            let all = crate::session::list();
            (
                serde_json::to_string(&all).unwrap_or_else(|_| "[]".into()),
                false,
            )
        }
        _ => (json_error(&format!("unknown tool `{}`", name)), true),
    };
    let (text, is_error) = result;
    // Content contract: the normalized JSON is what agents parse.
    (text, is_error)
}

fn spec_from_args(args: &Value) -> ExecSpec {
    let mut spec = ExecSpec {
        command: String::new(),
        ..Default::default()
    };
    if let Some(d) = args.get("workdir").and_then(|v| v.as_str()) {
        spec.workdir = Some(d.into());
    }
    if let Some(t) = args.get("timeout").and_then(|v| v.as_f64()) {
        spec.timeout_ms = (t * 1000.0) as u64;
    }
    if let Some(s) = args.get("shell").and_then(|v| v.as_str()) {
        spec.shell = Shell::from_name(s);
    }
    if let Some(env) = args.get("env").and_then(|v| v.as_object()) {
        spec.env = env
            .iter()
            .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
            .collect();
    }
    // Per-project adaptation: apply the nearest recipe's error maps so
    // project-specific `[error_maps]` hints reach MCP clients too.
    let base = spec
        .workdir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if let Some(recipe) = crate::recipe::Recipe::load_from_dir(&base) {
        if spec.max_output_bytes == 0 {
            if let Some(m) = recipe.max_output_bytes() {
                spec.max_output_bytes = m as usize;
            }
        }
        if spec.error_maps.is_empty() {
            spec.error_maps = recipe.error_maps.clone();
        }
    }
    spec
}

/// `exec.run`/`exec.script` with a caller-owned abort flag, so
/// `notifications/cancelled` can stop one call without touching another.
fn unirun_run_cancellable(spec: &ExecSpec, cancel: &AtomicBool) -> (String, bool) {
    let result = crate::exec::run_with_abort_streaming(spec, cancel, None);
    tool_result(result)
}

fn tool_result(result: ExecResult) -> (String, bool) {
    let is_error = result.error_class.is_some() || result.timed_out || result.aborted;
    (
        serde_json::to_string(&result).unwrap_or_else(|_| "{}".into()),
        is_error,
    )
}

fn json_error(msg: &str) -> String {
    serde_json::to_string(&json!({ "error": msg }))
        .unwrap_or_else(|_| format!("{{\"error\":\"{}\"}}", msg))
}

#[allow(dead_code)]
fn result_human(r: &ExecResult) -> String {
    // For debugging; agents consume the JSON content.
    format!(
        "exit={:?} timed_out={} error_class={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
        r.exit_code, r.timed_out, r.error_class, r.stdout, r.stderr
    )
}
