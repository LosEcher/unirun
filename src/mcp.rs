//! MCP (Model Context Protocol) server — stdio transport.
//!
//! Minimal, dependency-free implementation of the MCP 2024-11-05 tool
//! surface: `exec.run`, `exec.script`, `exec.probe`. Newline-delimited
//! JSON-RPC 2.0 over stdin/stdout — the standard every MCP-capable agent
//! (Claude Code, Cursor, DSH, …) speaks. `unirun mcp` is a long-lived
//! stdio process; run it once per agent session.

use crate::probe;
use crate::spec::{ExecKind, ExecResult, ExecSpec, Shell};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// Serve the MCP protocol until stdin closes.
pub fn serve() -> std::io::Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let method = msg.get("method").and_then(|m| m.as_str());
        let id = msg.get("id").cloned();
        match method {
            Some("initialize") => {
                let result = json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": { "tools": {} },
                    "serverInfo": { "name": "unirun", "version": env!("CARGO_PKG_VERSION") }
                });
                respond(&mut stdout, id, Ok(result))?;
            }
            Some("notifications/initialized") | Some("notifications/cancelled") => {
                // Notifications get no reply.
            }
            Some("ping") => {
                respond(&mut stdout, id, Ok(json!({})))?;
            }
            Some("tools/list") => {
                let tools = json!([exec_run_tool(), exec_script_tool(), exec_probe_tool()]);
                respond(&mut stdout, id, Ok(json!({ "tools": tools })))?;
            }
            Some("tools/call") => {
                let name = msg
                    .pointer("/params/name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let args = msg
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                let (text, is_error) = call_tool(name, &args);
                respond(
                    &mut stdout,
                    id,
                    Ok(json!({
                        "content": [{ "type": "text", "text": text }],
                        "isError": is_error
                    })),
                )?;
            }
            _ => {
                if let Some(id) = id {
                    respond(
                        &mut stdout,
                        Some(id),
                        Err(json!({
                            "code": -32601,
                            "message": format!("method not found: {}", method.unwrap_or("?"))
                        })),
                    )?;
                }
            }
        }
    }
    Ok(())
}

fn respond(
    w: &mut impl Write,
    id: Option<Value>,
    result: Result<Value, Value>,
) -> std::io::Result<()> {
    let msg = match (id, result) {
        (Some(id), Ok(r)) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        (Some(id), Err(e)) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
        (None, _) => return Ok(()),
    };
    let s = serde_json::to_string(&msg)?;
    writeln!(w, "{}", s)?;
    w.flush()
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

fn call_tool(name: &str, args: &Value) -> (String, bool) {
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
            unirun_run(&ExecSpec {
                command,
                ..spec_from_args(args)
            })
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
            unirun_run(&spec)
        }
        "exec.probe" => {
            let caps = probe::probe();
            (
                serde_json::to_string(&caps).unwrap_or_else(|_| "{}".into()),
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

fn unirun_run(spec: &ExecSpec) -> (String, bool) {
    let result = crate::exec::run(spec);
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
