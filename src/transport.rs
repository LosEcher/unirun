//! SSH remote transport — the win-exec knowledge ported to Rust, plus a
//! Unix remote branch.
//!
//! Kills the `bash → ssh → cmd.exe → PowerShell` escaping chain the same way
//! win-exec does: script content travels as a **payload** (UTF-16LE base64
//! `-EncodedCommand`, or a scp-uploaded temp file for large scripts), never
//! as a hand-quoted command string. The "golden recipe" is auto-injected so
//! PowerShell 5.1 emits clean UTF-8 (no CLIXML/OEM/GBK mojibake), and the
//! `exit $LASTEXITCODE` contract propagates exact remote exit codes.
//!
//! Unix remotes (bash / sh / zsh) get the same payload philosophy: the script
//! travels over stdin to `<shell> -s`, so no outer quoting layer can corrupt
//! it, and the script's own exit code propagates exactly.

use crate::exec::{Captured, PartialCapture};
use crate::spec::{ExecResult, ExitCodeConfidence, KillStatus, Shell, DEFAULT_MAX_OUTPUT_BYTES};
use crate::taxonomy::classify;
use base64::Engine;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const GOLDEN_PREFIX: &str = "$ProgressPreference='SilentlyContinue'\n[Console]::OutputEncoding=[Text.Encoding]::UTF8\n$OutputEncoding=[Text.Encoding]::UTF8\n";
const EXIT_CONTRACT: &str = "\nexit $LASTEXITCODE\n";
/// EncodedCommand base64 length beyond which we fall back to scp + `-File`.
///
/// `CreateProcess` caps the whole command line at 32 767 UTF-16 units, and our
/// `-EncodedCommand` line is `powershell.exe -NoProfile -NonInteractive
/// -EncodedCommand <b64>` — so the base64 payload must leave room for the
/// prefix. 30 000 keeps ~2.7 KB of headroom and still keeps the common case
/// (inline EncodedCommand, no scp round-trip) fast. The previous value of
/// 60 000 *exceeded the limit it claimed to be conservative about*, so a
/// ~12–22 KB PowerShell body silently built an unusable remote command line.
const B64_THRESHOLD: usize = 30_000;
/// Length of the fixed part of the inline `-EncodedCommand` remote command
/// line: `<exe> -NoProfile -NonInteractive -EncodedCommand ` where `<exe>` is
/// the shorter of the two PowerShell names (`pwsh.exe`). Used to prove the
/// threshold leaves room inside `CreateProcess`'s 32 767-unit limit.
const ENCODED_COMMAND_PREFIX: usize = "pwsh.exe -NoProfile -NonInteractive -EncodedCommand ".len();
/// `CreateProcess` caps the entire command line at 32 767 UTF-16 units.
const CREATEPROCESS_COMMAND_LINE_LIMIT: usize = 32_767;
/// Compile-time proof that an accepted inline payload always fits.
const _: () = assert!(ENCODED_COMMAND_PREFIX + B64_THRESHOLD < CREATEPROCESS_COMMAND_LINE_LIMIT);

/// Can this base64 payload travel inline, or must it go over scp + `-File`?
fn inline_encoded_command_fits(b64_len: usize) -> bool {
    b64_len <= B64_THRESHOLD && ENCODED_COMMAND_PREFIX + b64_len < CREATEPROCESS_COMMAND_LINE_LIMIT
}

/// Remote host + shell selection for SSH execution.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    /// Remote shell: `Bash` | `Sh` | `Zsh` (Unix) | `Powershell` (PS 5.1) |
    /// `Pwsh` (7) | `Cmd`.
    pub shell: Shell,
    pub timeout_ms: u64,
    pub connect_timeout: u64,
    /// Optional SSH user (`user@host`); falls back to the local user / ssh config.
    pub user: Option<String>,
    /// Optional SSH port (`-p`); falls back to 22 / ssh config.
    pub port: Option<u16>,
    /// Optional identity file (`-i`).
    pub identity_file: Option<PathBuf>,
    /// Optional remote working directory.
    pub workdir: Option<PathBuf>,
    /// Optional remote environment overrides.
    pub env: Vec<(String, String)>,
    /// Per-stream output cap in bytes; `0` → `DEFAULT_MAX_OUTPUT_BYTES`.
    /// Overflow is drained and only the tail is kept, flagged `truncated`.
    pub max_output_bytes: usize,
    /// Explicit code page for the captured output (`--output-encoding`).
    /// `None` = auto-detect (see `encoding::decode_with`).
    pub output_encoding: Option<String>,
    /// Bounded drain after the ssh child exits, in ms; `0` → the shared default.
    pub drain_ms: u64,
}

impl Default for SshTarget {
    fn default() -> Self {
        SshTarget {
            host: "win-los".into(),
            shell: Shell::Powershell,
            timeout_ms: 120_000,
            connect_timeout: 15,
            user: None,
            port: None,
            identity_file: None,
            workdir: None,
            env: Vec::new(),
            max_output_bytes: 0,
            output_encoding: None,
            drain_ms: 0,
        }
    }
}

/// Run a script on a remote host over SSH. Returns a normalized `ExecResult`
/// with exact remote exit code and clean UTF-8 output. Windows targets use
/// the win-exec payload machinery; Unix targets stream the script over stdin.
pub fn ssh_run(target: &SshTarget, script: &str) -> ExecResult {
    let script = prepare_script(target, script);
    match target.shell {
        Shell::Powershell | Shell::Pwsh => ssh_powershell(target, &script),
        Shell::Cmd => ssh_cmd_file(target, &script),
        Shell::Bash | Shell::Sh | Shell::Zsh => ssh_unix(target, &script),
    }
}

fn prepare_script(target: &SshTarget, script: &str) -> String {
    let mut prefix = String::new();
    match target.shell {
        Shell::Powershell | Shell::Pwsh => {
            if let Some(dir) = &target.workdir {
                prefix.push_str("Set-Location -LiteralPath ");
                prefix.push_str(&powershell_quote(&dir.to_string_lossy()));
                prefix.push('\n');
            }
            for (key, value) in &target.env {
                if valid_env_key(key) {
                    prefix.push_str("$env:");
                    prefix.push_str(key);
                    prefix.push_str(" = ");
                    prefix.push_str(&powershell_quote(value));
                    prefix.push('\n');
                }
            }
        }
        Shell::Cmd => {
            if let Some(dir) = &target.workdir {
                prefix.push_str("cd /d \"");
                prefix.push_str(&cmd_quote(&dir.to_string_lossy()));
                prefix.push_str("\"\r\n");
            }
            for (key, value) in &target.env {
                if valid_env_key(key) {
                    prefix.push_str("set \"");
                    prefix.push_str(key);
                    prefix.push('=');
                    prefix.push_str(&cmd_quote(value));
                    prefix.push_str("\"\r\n");
                }
            }
        }
        Shell::Bash | Shell::Sh | Shell::Zsh => {
            if let Some(dir) = &target.workdir {
                prefix.push_str("cd ");
                prefix.push_str(&posix_quote(&dir.to_string_lossy()));
                prefix.push_str(" || exit $?");
                prefix.push('\n');
            }
            for (key, value) in &target.env {
                if valid_env_key(key) {
                    prefix.push_str("export ");
                    prefix.push_str(key);
                    prefix.push('=');
                    prefix.push_str(&posix_quote(value));
                    prefix.push('\n');
                }
            }
        }
    }
    prefix.push_str(script);
    prefix
}

fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn posix_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn cmd_quote(value: &str) -> String {
    value.replace('"', "\"\"")
}

/// Unix remote: script travels over stdin to `<shell> -s`, so no outer
/// quoting layer can corrupt it (same payload philosophy as the Windows
/// branch, minus the encoding dance — UTF-8 everywhere). The script's own
/// exit code propagates exactly (`exit N` → rc N), and stderr reaches the
/// classifier so "command not found" and friends get the stable taxonomy.
fn ssh_unix(target: &SshTarget, script: &str) -> ExecResult {
    let shell_bin = match target.shell {
        Shell::Sh => "sh",
        Shell::Zsh => "zsh",
        _ => "bash",
    };
    run_ssh(target, &format!("{} -s", shell_bin), Some(script))
}

fn ssh_powershell(target: &SshTarget, script: &str) -> ExecResult {
    let exe = if target.shell == Shell::Pwsh {
        "pwsh.exe"
    } else {
        "powershell.exe"
    };
    let payload = ps_payload(script);
    let b64 = base64_utf16le(&payload);
    if inline_encoded_command_fits(b64.len()) {
        let remote_cmd = format!("{} -NoProfile -NonInteractive -EncodedCommand {}", exe, b64);
        run_ssh(target, &remote_cmd, None)
    } else {
        // Large payload: scp a UTF-8-BOM temp .ps1, run with -File, clean up.
        let remote_path = format!(r"C:\Windows\Temp\unirun-{}.ps1", nonce());
        let uploaded = upload_scp(target, &remote_path, &ps_file_payload(&payload));
        if let Err(e) = uploaded {
            return upload_failed(exe, e);
        }
        let remote_cmd = format!(
            "{} -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {}",
            exe, remote_path
        );
        let r = run_ssh(target, &remote_cmd, None);
        let _ = run_ssh(target, &format!("del /q {}", remote_path), None);
        r
    }
}

/// cmd.exe: always file mode (a `.bat` avoids cmd eating metacharacters;
/// content stays ASCII-safe per win-exec guidance).
fn ssh_cmd_file(target: &SshTarget, script: &str) -> ExecResult {
    let remote_path = format!(r"C:\Windows\Temp\unirun-{}.bat", nonce());
    if let Err(e) = upload_scp(target, &remote_path, &cmd_payload(script)) {
        return upload_failed("cmd", e);
    }
    let remote_cmd = format!("cmd.exe /C \"{}\"", remote_path);
    let r = run_ssh(target, &remote_cmd, None);
    // Clean the temp file best-effort.
    let _ = run_ssh(target, &format!("del /q {}", remote_path), None);
    r
}

/// Spawn ssh, capture output with a deadline, filter the OpenSSH banner.
/// `stdin_payload` is streamed to the remote command's stdin (Unix `-s`
/// shells); `None` closes stdin immediately (Windows branches).
fn run_ssh(target: &SshTarget, remote_cmd: &str, stdin_payload: Option<&str>) -> ExecResult {
    let start = Instant::now();
    let mut cmd = Command::new("ssh");
    cmd.args(ssh_argv(target, remote_cmd));
    if stdin_payload.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let mut r =
                ExecResult::success(String::new(), format!("ssh spawn failed: {}", e), "ssh");
            r.error_class = Some("COMMAND_NOT_FOUND".into());
            r.hint = Some("ssh binary not available on this host".into());
            return r;
        }
    };

    if let Some(payload) = stdin_payload {
        let mut stdin = child.stdin.take().unwrap();
        let payload = payload.to_string();
        // Write in a thread so a large payload cannot deadlock against the
        // remote's output readers. Dropping stdin closes it → EOF on the
        // remote, which ends the `-s` script.
        thread::spawn(move || {
            let _ = stdin.write_all(payload.as_bytes());
        });
    }

    let so = child.stdout.take().unwrap();
    let se = child.stderr.take().unwrap();
    let max = output_cap(target);
    let (out_tx, out_rx) = mpsc::channel::<Captured>();
    let (err_tx, err_rx) = mpsc::channel::<Captured>();
    let out_shared = PartialCapture::new();
    let err_shared = PartialCapture::new();
    {
        let shared = out_shared.clone();
        thread::spawn(move || {
            let (bytes, truncated) = read_capped(so, max, &shared);
            let _ = out_tx.send(Captured { bytes, truncated });
        });
    }
    {
        let shared = err_shared.clone();
        thread::spawn(move || {
            let (bytes, truncated) = read_capped(se, max, &shared);
            let _ = err_tx.send(Captured { bytes, truncated });
        });
    }

    let timeout = Duration::from_millis(if target.timeout_ms == 0 {
        120_000
    } else {
        target.timeout_ms
    });
    let grace = Duration::from_millis(2_000);
    let mut exit_code = None;
    let mut timed_out = false;
    let mut aborted = false;
    let mut kill_status: Option<KillStatus> = None;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                break;
            }
            Ok(None) => {
                // Ctrl-C cancels a remote run exactly as it cancels a local one:
                // the ssh client's tree is signalled, so the remote command does
                // not keep running behind a returned prompt.
                match crate::exec::remote_stop(
                    crate::exec::abort_requested(),
                    start.elapsed(),
                    timeout,
                ) {
                    Some(crate::exec::RemoteStop::Aborted) => {
                        aborted = true;
                        kill_status = Some(kill_ssh_tree(&mut child, grace));
                        let _ = child.wait();
                        break;
                    }
                    Some(crate::exec::RemoteStop::TimedOut) => {
                        timed_out = true;
                        kill_status = Some(kill_ssh_tree(&mut child, grace));
                        let _ = child.wait();
                        break;
                    }
                    None => {}
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.wait();
                break;
            }
        }
    }
    // Bounded drain: a remote grandchild can hold the pipe after the ssh child
    // is gone, exactly as locally.
    let drain = std::time::Duration::from_millis(if target.drain_ms == 0 {
        crate::spec::DEFAULT_DRAIN_MS
    } else {
        target.drain_ms
    });
    let drain_deadline = Instant::now() + drain;
    let remaining = || drain_deadline.saturating_duration_since(Instant::now());
    let (out_bytes, out_truncated, out_drain_timeout) =
        crate::exec::collect_capture(Some(out_rx), Some(out_shared), remaining());
    let (err_bytes, err_truncated, err_drain_timeout) =
        crate::exec::collect_capture(Some(err_rx), Some(err_shared), remaining());
    let drain_timeout = out_drain_timeout || err_drain_timeout;
    assemble_ssh_result(
        target,
        exit_code,
        timed_out,
        aborted,
        kill_status,
        drain_timeout,
        StreamCapture {
            bytes: out_bytes,
            truncated: out_truncated,
        },
        StreamCapture {
            bytes: err_bytes,
            truncated: err_truncated,
        },
        start.elapsed().as_millis() as u64,
    )
}

/// One captured stream: the (tail-kept) bytes plus whether the cap was hit.
struct StreamCapture {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Per-stream output cap for this target (`0` → the shared default).
fn output_cap(target: &SshTarget) -> usize {
    if target.max_output_bytes == 0 {
        DEFAULT_MAX_OUTPUT_BYTES
    } else {
        target.max_output_bytes
    }
}

/// Assemble the normalized result from the captured streams.
///
/// Extracted from `run_ssh` so the truncation flags `read_capped` produces are
/// provably *not* dropped on the way into `ExecResult` — the bug this function
/// exists to prevent — without needing a live host in the test suite.
#[allow(clippy::too_many_arguments)]
fn assemble_ssh_result(
    target: &SshTarget,
    exit_code: Option<i32>,
    timed_out: bool,
    aborted: bool,
    kill_status: Option<KillStatus>,
    drain_timeout: bool,
    stdout_capture: StreamCapture,
    stderr_capture: StreamCapture,
    duration_ms: u64,
) -> ExecResult {
    let hint = target.output_encoding.as_deref();
    let stdout_decoded = crate::encoding::decode_with(&stdout_capture.bytes, hint);
    let stderr_raw = filter_banner(&stderr_capture.bytes);
    let stderr_decoded = crate::encoding::decode_with(&stderr_raw, hint);
    let stdout = crate::encoding::normalize_line_endings(&stdout_decoded.text);
    let stderr = crate::encoding::normalize_line_endings(&stderr_decoded.text);

    // ssh reports its own failures with status 255; when its diagnostics are
    // present, the command never produced a result. Anything else is a remote
    // failure that happens to share the code.
    let (stderr, transport_stderr, transport_error) =
        if exit_code == Some(SSH_CLIENT_FAILURE) && !timed_out {
            let (remote, transport) = split_transport_diagnostics(&stderr);
            let transport_error = transport.is_some();
            (remote, transport, transport_error)
        } else {
            (stderr, None, false)
        };
    // The ssh child spawned, so execution *may* have started; only a clean
    // pre-dispatch signature (connect/auth/upload) clears the flag.
    let dispatched = !transport_stderr
        .as_deref()
        .is_some_and(provably_not_dispatched);

    let mut result = ExecResult {
        exit_code,
        signal: None,
        stdout,
        stderr,
        timed_out,
        aborted,
        duration_ms,
        error_class: None,
        hint: None,
        encoding: stdout_decoded.encoding.to_string(),
        truncated: stdout_capture.truncated || stderr_capture.truncated,
        shell_used: target.shell.as_str().to_string(),
        transport_error,
        transport_stderr,
        dispatched,
        kill_status,
        exit_code_confidence: ExitCodeConfidence::Observed,
        drain_timeout,
    };
    let (class, hint) = classify(&result);
    result.error_class = class;
    result.hint = hint;
    result
}

/// The line the detach wrapper prints once the remote process is running.
pub const DETACHED_SENTINEL: &str = "__UNIRUN_DETACHED__";
/// Printed instead when the wrapper could not establish the remote pid.
pub const DETACHED_ERROR_SENTINEL: &str = "__UNIRUN_DETACHED_ERROR__";

/// A remote process unirun started detached, plus the handles to poll it.
///
/// The remote side owns everything durable here (pid, log, exit code file); the
/// local session record just remembers where to look, which is what makes a
/// detached run survive both the ssh session and this machine.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DetachedRun {
    pub host: String,
    /// Remote process-group leader (the script's own pid).
    pub pid: u32,
    /// Remote log file (`stdout` and `stderr` are merged into it).
    pub log: String,
    /// Remote file the script's exit status is written to when it finishes.
    pub rc_file: String,
    /// Remote path of the uploaded script (kept for debugging/re-running).
    pub script_file: String,
}

/// The POSIX detach wrapper.
///
/// `setsid` is preferred (a teardown cannot reach a different session) with a
/// `nohup`-only fallback for minimal hosts such as a NAS busybox. The inner
/// shell is single-quoted and reads its paths from the **environment**, so no
/// path is ever interpolated into quoted shell text.
fn detached_wrapper(script_file: &str, log: &str, rc_file: &str, pid_file: &str) -> String {
    format!(
        "UNIRUN_SCRIPT={}\nUNIRUN_LOG={}\nUNIRUN_RC={}\nUNIRUN_PIDFILE={}\nexport UNIRUN_SCRIPT UNIRUN_LOG UNIRUN_RC UNIRUN_PIDFILE\nrm -f \"$UNIRUN_PIDFILE\" \"$UNIRUN_RC\"\nif command -v setsid >/dev/null 2>&1; then\n  setsid sh -c 'echo $$ > \"$UNIRUN_PIDFILE\"; sh \"$UNIRUN_SCRIPT\"; echo $? > \"$UNIRUN_RC\"' >\"$UNIRUN_LOG\" 2>&1 </dev/null &\nelse\n  nohup sh -c 'echo $$ > \"$UNIRUN_PIDFILE\"; sh \"$UNIRUN_SCRIPT\"; echo $? > \"$UNIRUN_RC\"' >\"$UNIRUN_LOG\" 2>&1 </dev/null &\nfi\ni=0\nwhile [ ! -s \"$UNIRUN_PIDFILE\" ] && [ $i -lt 50 ]; do i=$((i+1)); sleep 0.1; done\npid=$(cat \"$UNIRUN_PIDFILE\" 2>/dev/null)\nif [ -z \"$pid\" ]; then echo \"{} pidfile not written\"; exit 1; fi\necho \"{} $pid $UNIRUN_LOG $UNIRUN_RC\"\n",
        posix_quote(script_file),
        posix_quote(log),
        posix_quote(rc_file),
        posix_quote(pid_file),
        DETACHED_ERROR_SENTINEL,
        DETACHED_SENTINEL,
    )
}

/// Parse the wrapper's sentinel line.
fn parse_detached(stdout: &str, host: &str, script_file: &str) -> Option<DetachedRun> {
    for line in stdout.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix(DETACHED_SENTINEL) {
            let mut parts = rest.split_whitespace();
            let pid = parts.next()?.parse::<u32>().ok()?;
            let log = parts.next()?.to_string();
            let rc_file = parts.next()?.to_string();
            return Some(DetachedRun {
                host: host.to_string(),
                pid,
                log,
                rc_file,
                script_file: script_file.to_string(),
            });
        }
    }
    None
}

/// Start `script` detached on a POSIX remote, returning the handles that make it
/// pollable later. The remote process outlives this ssh session — which is the
/// whole point: an ssh-attached long task is silently reaped when the session
/// ends (`NETWORK-FLEET-AND-TRANSFER-DESIGN-ANALYSIS-2026-09-18.md:445`).
///
/// Windows targets are refused rather than half-supported: persistence there is
/// a scheduled task (`schtasks /run`), a different mechanism with different
/// handles, and pretending otherwise would produce runs that look started and
/// die with the session.
#[allow(clippy::result_large_err)] // the failure *is* a normalized ExecResult
pub fn ssh_run_detached(target: &SshTarget, script: &str) -> Result<DetachedRun, ExecResult> {
    if !matches!(target.shell, Shell::Bash | Shell::Sh | Shell::Zsh) {
        let mut r = ExecResult::success(String::new(), String::new(), target.shell.as_str());
        r.exit_code = None;
        r.dispatched = false;
        r.error_class = Some("UNSUPPORTED".into());
        r.hint = Some(
            "detached runs are implemented for POSIX remotes; on Windows use the scheduled-task mechanism (schtasks /run) directly"
                .into(),
        );
        return Err(r);
    }
    let nonce = nonce();
    let script_file = format!("/tmp/unirun-{}.sh", nonce);
    let log = format!("/tmp/unirun-{}.log", nonce);
    let rc_file = format!("/tmp/unirun-{}.rc", nonce);
    let pid_file = format!("/tmp/unirun-{}.pid", nonce);
    if let Err(e) = upload_scp(target, &script_file, script.as_bytes()) {
        return Err(upload_failed(target.shell.as_str(), e));
    }
    let wrapper = detached_wrapper(&script_file, &log, &rc_file, &pid_file);
    let result = ssh_run(target, &wrapper);
    if let Some(run) = parse_detached(&result.stdout, &target.host, &script_file) {
        return Ok(run);
    }
    // No sentinel: report the transport failure (or the wrapper's own error)
    // instead of inventing a session that cannot be polled.
    let mut r = result;
    if r.error_class.is_none() {
        r.error_class = Some("UNSUPPORTED".into());
    }
    r.hint = Some(format!(
        "could not establish the detached remote process: {}",
        r.stderr.trim()
    ));
    Err(r)
}

/// Build the `ssh` argv for a target + remote command. Pure and
/// unit-testable: host/user/port/identity + the strict batch options.
///
/// ControlMaster is explicitly disabled: a reused connection lives in a
/// detached master daemon, so killing this process group could not terminate
/// the remote command tree — breaking unirun's whole-tree deadline guarantee.
fn ssh_argv(target: &SshTarget, remote_cmd: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-o".into(),
        "BatchMode=yes".into(),
        "-o".into(),
        format!("ConnectTimeout={}", target.connect_timeout),
        "-o".into(),
        "ServerAliveInterval=30".into(),
        "-o".into(),
        "StrictHostKeyChecking=accept-new".into(),
        "-o".into(),
        "ControlMaster=no".into(),
        "-o".into(),
        "ControlPath=none".into(),
    ];
    if let Some(port) = target.port {
        args.push("-p".into());
        args.push(port.to_string());
    }
    if let Some(identity) = &target.identity_file {
        args.push("-i".into());
        args.push(identity.to_string_lossy().into_owned());
    }
    let host = match &target.user {
        Some(u) if !u.is_empty() => format!("{}@{}", u, target.host),
        _ => target.host.clone(),
    };
    args.push(host);
    args.push(remote_cmd.to_string());
    args
}

/// The batch payload: the script plus an explicit exit-code contract.
///
/// `cmd.exe /C file.bat` normally returns the script's last errorlevel, but a
/// trailing statement can reset it — win-exec appended the contract for exactly
/// that reason (`win-exec/win-exec.py:234`). With it, a cmd run's exit status is
/// evidence, so `exit_code_confidence` stays `observed`.
fn cmd_payload(script: &str) -> Vec<u8> {
    format!("{}\r\nexit /b %ERRORLEVEL%\r\n", script.trim_end()).into_bytes()
}

/// Upload bytes as a temp file on the remote host via scp.
fn upload_scp(target: &SshTarget, remote_path: &str, bytes: &[u8]) -> std::io::Result<()> {
    // Local temp file (0600), scp it over, remove locally.
    let local = std::env::temp_dir().join(format!("unirun-{}.up", nonce()));
    {
        let mut f = std::fs::File::create(&local)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        f.write_all(bytes)?;
    }
    let mut cmd = Command::new("scp");
    cmd.arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={}", target.connect_timeout))
        .arg("-o")
        .arg("StrictHostKeyChecking=accept-new")
        .arg("-o")
        .arg("ControlMaster=no")
        .arg("-o")
        .arg("ControlPath=none");
    // Note: scp uses uppercase `-P` for the port (ssh uses lowercase `-p`).
    if let Some(port) = target.port {
        cmd.arg("-P").arg(port.to_string());
    }
    if let Some(identity) = &target.identity_file {
        cmd.arg("-i").arg(identity);
    }
    let dest = match &target.user {
        Some(u) if !u.is_empty() => format!("{}@{}:{}", u, target.host, remote_path),
        _ => format!("{}:{}", target.host, remote_path),
    };
    cmd.arg(&local).arg(dest);
    let status = cmd.status()?;
    let _ = std::fs::remove_file(&local);
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("scp exited non-zero"))
    }
}

/// The PowerShell payload: golden recipe, the script, then the exit contract.
///
/// This string is never interpolated into a command line; it travels base64
/// (`-EncodedCommand`) or as a UTF-8-BOM file, which is what keeps cmd.exe from
/// eating `>` before PowerShell sees it (platform difference, part 16).
fn ps_payload(script: &str) -> String {
    format!("{}{}{}", GOLDEN_PREFIX, script.trim_end(), EXIT_CONTRACT)
}

/// The scp fallback's bytes: UTF-8 **with BOM**.
///
/// Platform difference, part 17: PowerShell 5.1 parses a `.ps1` as ANSI unless
/// it starts with a BOM, so a script containing Chinese comments would fail to
/// parse remotely without these three bytes.
fn ps_file_payload(payload: &str) -> Vec<u8> {
    let mut bytes = vec![0xEF, 0xBB, 0xBF];
    bytes.extend_from_slice(payload.as_bytes());
    bytes
}

fn base64_utf16le(text: &str) -> String {
    use base64::engine::general_purpose::STANDARD;
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let mut bytes = Vec::with_capacity(utf16.len() * 2);
    for u in utf16 {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    STANDARD.encode(&bytes)
}

fn nonce() -> String {
    format!(
        "{:x}{:x}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0)
    )
}

/// A failed scp upload means the remote command never ran: report it as a
/// transport error, not as a missing command on the remote host.
fn upload_failed(shell_used: &str, e: std::io::Error) -> ExecResult {
    let mut r = ExecResult::success(String::new(), String::new(), shell_used);
    r.exit_code = None;
    r.transport_error = true;
    r.transport_stderr = Some(format!("upload failed: {}", e));
    r.error_class = Some("TRANSPORT".into());
    r.hint = Some("scp to the remote host failed; check host/credentials".into());
    r.dispatched = false;
    r
}

/// Strip the Win32-OpenSSH post-quantum banner from stderr.
fn filter_banner(data: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(data);
    let mut out = Vec::new();
    for line in text.lines() {
        let l = line.to_lowercase();
        if l.contains("post-quantum")
            || l.contains("pq.html")
            || l.contains("store now")
            || l.contains("decrypt")
        {
            continue;
        }
        out.extend_from_slice(line.as_bytes());
        out.push(b'\n');
    }
    out
}

fn read_capped<R: Read>(mut reader: R, max: usize, shared: &PartialCapture) -> (Vec<u8>, bool) {
    let mut tail: Vec<u8> = Vec::with_capacity(max.saturating_add(8192));
    let mut total = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                total += n;
                tail.extend_from_slice(&chunk[..n]);
                if tail.len() > max {
                    let excess = tail.len() - max;
                    tail.drain(..excess);
                }
                shared.publish(&tail, total > max);
            }
            Err(_) => break,
        }
    }
    (tail, total > max)
}

#[cfg(unix)]
fn kill_ssh_tree(child: &mut Child, grace: Duration) -> KillStatus {
    let pid = child.id() as i32;
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    let deadline = Instant::now() + grace;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return KillStatus::Clean;
        }
        if Instant::now() >= deadline {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            return crate::exec::settle_after_kill(child);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
fn kill_ssh_tree(child: &mut Child, _grace: Duration) -> KillStatus {
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    match crate::exec::settle_after_kill(child) {
        KillStatus::Clean => KillStatus::SigkillEscalated,
        other => other,
    }
}

/// Lines the `ssh`/`scp` clients write about *themselves*. Matching one of
/// these (with ssh's own exit status 255) means the transport failed before the
/// remote command produced a result.
///
/// Deliberately narrow: `connect to host … Connection refused` is a curl
/// message too, and a remote script that exits 255 while printing it must stay
/// a remote failure. The prefixes below are the ssh client's own vocabulary.
const TRANSPORT_DIAGNOSTIC_PREFIXES: &[&str] = &[
    "ssh: ",
    "scp: ",
    "kex_exchange_identification:",
    "ssh_exchange_identification:",
    "channel 0: open failed:",
    "connection closed by ",
    "received disconnect from ",
    "host key verification failed",
    "permission denied (",
    "too many authentication failures",
    "no matching host key type found",
    "remote host identification has changed",
    "banner exchange: ",
];

/// Transport diagnostics that prove the remote command was never dispatched:
/// the connection, the authentication or the payload upload failed.
///
/// Anything else the client reports (`Connection closed by …`, `Received
/// disconnect from …`, `channel 0: open failed:`) can happen *after* the remote
/// started executing, so it must not clear `dispatched`.
const PRE_DISPATCH_PREFIXES: &[&str] = &[
    "ssh: connect to host",
    "ssh: could not resolve hostname",
    "ssh: unknown port",
    "permission denied (",
    "host key verification failed",
    "no matching host key type found",
    "too many authentication failures",
    "remote host identification has changed",
    "kex_exchange_identification:",
    "ssh_exchange_identification:",
    "banner exchange: ",
    "scp: ",
    "upload failed:",
];

/// Did the remote command never start, according to the transport's own
/// diagnostics? Conservative: every line must be a recognised pre-dispatch
/// signature, so a mixed or unrecognised report still means "it may have run".
fn provably_not_dispatched(transport_stderr: &str) -> bool {
    let mut lines = transport_stderr.lines().peekable();
    if lines.peek().is_none() {
        return false;
    }
    lines.all(|line| {
        let lower = line.trim_start().to_lowercase();
        PRE_DISPATCH_PREFIXES.iter().any(|p| lower.starts_with(p))
    })
}

/// Split the ssh client's own diagnostics out of the captured stderr.
///
/// Returns `(remote_stderr, transport_stderr)`. Only consulted when the ssh
/// child exited 255 — the client's own failure status.
fn split_transport_diagnostics(stderr: &str) -> (String, Option<String>) {
    let mut remote = String::new();
    let mut transport = String::new();
    for line in stderr.lines() {
        let trimmed = line.trim_start();
        let lower = trimmed.to_lowercase();
        if TRANSPORT_DIAGNOSTIC_PREFIXES
            .iter()
            .any(|p| lower.starts_with(p))
        {
            transport.push_str(line);
            transport.push('\n');
        } else {
            remote.push_str(line);
            remote.push('\n');
        }
    }
    let transport = if transport.is_empty() {
        None
    } else {
        Some(transport)
    };
    (remote, transport)
}

/// `ssh` reserves exit status 255 for its own failures (`ExitOnForwardFailure`
/// and friends aside, a remote command cannot produce it through `ssh` without
/// the client having reported the error first).
const SSH_CLIENT_FAILURE: i32 = 255;

#[cfg(test)]
mod tests {
    use super::*;

    fn target() -> SshTarget {
        SshTarget {
            host: "h.example".into(),
            shell: Shell::Bash,
            timeout_ms: 0,
            connect_timeout: 15,
            user: None,
            port: None,
            identity_file: None,
            workdir: None,
            env: Vec::new(),
            max_output_bytes: 0,
            output_encoding: None,
            drain_ms: 0,
        }
    }

    fn capture(bytes: &[u8], truncated: bool) -> StreamCapture {
        StreamCapture {
            bytes: bytes.to_vec(),
            truncated,
        }
    }

    #[test]
    fn output_cap_defaults_and_honours_override() {
        assert_eq!(output_cap(&target()), DEFAULT_MAX_OUTPUT_BYTES);
        let mut t = target();
        t.max_output_bytes = 4096;
        assert_eq!(output_cap(&t), 4096);
    }

    /// Regression: the per-stream truncation flags returned by `read_capped`
    /// used to be discarded (`let (out_raw, _) = …`) and `truncated` was
    /// hard-coded `false`, so a remote run could hand back a silently
    /// incomplete stdout while claiming it was complete.
    #[test]
    fn truncation_from_either_stream_reaches_the_result() {
        let t = target();
        let out_hit = assemble_ssh_result(
            &t,
            Some(0),
            false,
            false,
            None,
            false,
            capture(b"tail-of-stdout", true),
            capture(b"", false),
            7,
        );
        assert!(out_hit.truncated, "stdout truncation must be reported");
        assert_eq!(out_hit.exit_code, Some(0));
        assert_eq!(out_hit.duration_ms, 7);

        let err_hit = assemble_ssh_result(
            &t,
            Some(0),
            false,
            false,
            None,
            false,
            capture(b"", false),
            capture(b"tail-of-stderr", true),
            3,
        );
        assert!(err_hit.truncated, "stderr truncation must be reported");
        // `filter_banner` re-terminates each surviving line with `\n`.
        assert_eq!(err_hit.stderr, "tail-of-stderr\n");
    }

    #[test]
    fn untruncated_and_timed_out_results_stay_unmarked() {
        let t = target();
        let r = assemble_ssh_result(
            &t,
            None,
            true,
            false,
            None,
            false,
            capture(b"partial", false),
            capture(b"", false),
            1,
        );
        assert!(!r.truncated);
        assert!(r.timed_out);
        assert_eq!(r.exit_code, None);
        assert_eq!(r.shell_used, "bash");
    }

    /// ssh reports its own failures with status 255; its diagnostics are the
    /// evidence that the command never ran. They are split out of `stderr` so
    /// the remote's output stays readable, and the result is flagged
    /// `transport_error`.
    #[test]
    fn ssh_client_diagnostics_are_split_and_flagged() {
        let t = target();
        let r = assemble_ssh_result(
            &t,
            Some(255),
            false,
            false,
            None,
            false,
            capture(b"", false),
            capture(
                b"ssh: connect to host h.example port 22: Connection refused\n",
                false,
            ),
            4,
        );
        assert!(
            r.transport_error,
            "255 + ssh diagnostic must be a transport failure"
        );
        assert_eq!(r.stderr, "");
        assert_eq!(
            r.transport_stderr.as_deref(),
            Some("ssh: connect to host h.example port 22: Connection refused\n")
        );
        assert_eq!(r.error_class.as_deref(), Some("TRANSPORT"));

        // Auth failure: ssh's `Permission denied (…)` line, no `ssh:` prefix.
        let auth = assemble_ssh_result(
            &t,
            Some(255),
            false,
            false,
            None,
            false,
            capture(b"", false),
            capture(b"Permission denied (publickey).\n", false),
            4,
        );
        assert!(auth.transport_error);
        assert!(auth.transport_stderr.is_some());
    }

    /// A remote script that exits 255 itself stays a remote failure: same exit
    /// code, but the stderr is not the ssh client talking about itself. A curl
    /// "Connection refused" must not be mistaken for a transport error either.
    #[test]
    fn remote_exit_255_is_not_a_transport_error() {
        let t = target();
        let r = assemble_ssh_result(
            &t,
            Some(255),
            false,
            false,
            None,
            false,
            capture(b"", false),
            capture(
                b"Failed to connect to db port 5432: Connection refused\n",
                false,
            ),
            4,
        );
        assert!(
            !r.transport_error,
            "remote stderr must not be read as the ssh client's own failure"
        );
        assert!(r.transport_stderr.is_none());
        assert_eq!(
            r.stderr,
            "Failed to connect to db port 5432: Connection refused\n"
        );

        // Mixed: the client's line is split out, the remote's line stays.
        let mixed = assemble_ssh_result(
            &t,
            Some(255),
            false,
            false,
            None,
            false,
            capture(b"", false),
            capture(
                b"remote warning\nssh: connect to host h port 22: timed out\n",
                false,
            ),
            4,
        );
        assert!(mixed.transport_error);
        assert_eq!(mixed.stderr, "remote warning\n");
        assert_eq!(
            mixed.transport_stderr.as_deref(),
            Some("ssh: connect to host h port 22: timed out\n")
        );
    }

    /// A timed-out run keeps its diagnostics in `stderr`: the timeout explains
    /// itself, and re-labelling it as a transport error would hide that.
    #[test]
    fn timed_out_runs_are_not_transport_errors() {
        let t = target();
        let r = assemble_ssh_result(
            &t,
            Some(255),
            true,
            false,
            None,
            false,
            capture(b"", false),
            capture(b"ssh: connect to host h port 22: timed out\n", false),
            4,
        );
        assert!(!r.transport_error);
        assert!(r.transport_stderr.is_none());
    }

    #[test]
    fn upload_failures_are_transport_errors() {
        let r = upload_failed("cmd", std::io::Error::other("scp exited non-zero"));
        assert!(r.transport_error);
        assert_eq!(r.exit_code, None);
        assert_eq!(r.error_class.as_deref(), Some("TRANSPORT"));
        assert!(r
            .transport_stderr
            .as_deref()
            .unwrap_or("")
            .contains("scp exited non-zero"));
    }

    /// `dispatched` is the retry-safety signal: only a clean pre-dispatch
    /// signature (connect/auth/upload) may clear it.
    #[test]
    fn dispatched_is_cleared_only_by_evidence_of_no_dispatch() {
        let t = target();
        let refused = assemble_ssh_result(
            &t,
            Some(255),
            false,
            false,
            None,
            false,
            capture(b"", false),
            capture(
                b"ssh: connect to host h port 22: Connection refused\n",
                false,
            ),
            4,
        );
        assert!(refused.transport_error);
        assert!(
            !refused.dispatched,
            "a refused connection never reached the remote"
        );

        let auth = assemble_ssh_result(
            &t,
            Some(255),
            false,
            false,
            None,
            false,
            capture(b"", false),
            capture(b"Permission denied (publickey).\n", false),
            4,
        );
        assert!(!auth.dispatched);

        // Closed mid-run: the command may have executed (and left side effects).
        let dropped = assemble_ssh_result(
            &t,
            Some(255),
            false,
            false,
            None,
            false,
            capture(b"", false),
            capture(b"Connection closed by 10.0.0.1 port 22\n", false),
            4,
        );
        assert!(dropped.transport_error);
        assert!(
            dropped.dispatched,
            "a mid-run disconnect must not license a blind retry"
        );

        // Mixed evidence: conservative.
        let mixed = assemble_ssh_result(
            &t,
            Some(255),
            false,
            false,
            None,
            false,
            capture(b"", false),
            capture(
                b"ssh: connect to host h port 22: Connection refused\nConnection closed by 10.0.0.1\n",
                false,
            ),
            4,
        );
        assert!(mixed.dispatched);

        // A plain successful run is dispatched.
        let ok = assemble_ssh_result(
            &t,
            Some(0),
            false,
            false,
            None,
            false,
            capture(b"hi", false),
            capture(b"", false),
            2,
        );
        assert!(ok.dispatched);
    }

    /// The cap must actually bound the reader: 8 KiB chunks, tail kept.
    #[test]
    fn read_capped_reports_and_keeps_the_tail() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 251) as u8).collect();
        let shared = PartialCapture::new();
        let (tail, truncated) = read_capped(std::io::Cursor::new(data.clone()), 1024, &shared);
        assert!(truncated);
        assert_eq!(tail.len(), 1024);
        assert_eq!(tail, &data[data.len() - 1024..]);
        // The partial capture published the same tail, so a drain deadline has
        // something to take even if the reader has not reached EOF.
        let (published, published_truncated) = shared.snapshot();
        assert_eq!(published, tail);
        assert!(published_truncated);

        let (all, not_truncated) = read_capped(
            std::io::Cursor::new(data.clone()),
            20_000,
            &PartialCapture::new(),
        );
        assert!(!not_truncated);
        assert_eq!(all, data);
    }

    /// The wrapper must start the script in a way that outlives the ssh session
    /// (setsid preferred, nohup fallback), redirect both streams into the log,
    /// detach stdin, and hand back a pid the caller can poll.
    #[test]
    fn detach_wrapper_detaches_and_reports_a_pid() {
        let w = detached_wrapper("/tmp/s.sh", "/tmp/s.log", "/tmp/s.rc", "/tmp/s.pid");
        assert!(w.contains("UNIRUN_SCRIPT='/tmp/s.sh'"));
        assert!(w.contains("UNIRUN_LOG='/tmp/s.log'"));
        assert!(w.contains("UNIRUN_RC='/tmp/s.rc'"));
        assert!(w.contains("UNIRUN_PIDFILE='/tmp/s.pid'"));
        assert!(w.contains("command -v setsid"), "setsid preferred: {w}");
        assert!(w.contains("nohup sh -c"), "nohup fallback: {w}");
        assert!(w.contains("</dev/null"), "stdin must be detached: {w}");
        assert_eq!(
            w.matches(r#">"$UNIRUN_LOG" 2>&1 </dev/null"#).count(),
            2,
            "both launchers merge stderr and detach stdin: {w}"
        );
        assert!(
            w.contains(r#"echo $$ > "$UNIRUN_PIDFILE""#),
            "the remote script's own pid must be recorded: {w}"
        );
        assert!(
            w.contains(r#"echo $? > "$UNIRUN_RC""#),
            "the remote exit status must be recorded: {w}"
        );
        assert!(w.contains(DETACHED_SENTINEL));
    }

    /// Paths are quoted, so a target directory with a space or quote cannot
    /// break out of the wrapper.
    #[test]
    fn detach_wrapper_quotes_paths() {
        let w = detached_wrapper("/tmp/a b/it's.sh", "/tmp/l", "/tmp/r", "/tmp/p");
        assert!(w.contains(r"UNIRUN_SCRIPT='/tmp/a b/it'\''s.sh'"), "{w}");
        assert!(!w.contains("UNIRUN_SCRIPT=/tmp/a b/it's.sh"));
    }

    #[test]
    fn detached_sentinel_is_parsed_and_junk_is_ignored() {
        let stdout = "some ssh noise\n__UNIRUN_DETACHED__ 4242 /tmp/x.log /tmp/x.rc\n";
        let run = parse_detached(stdout, "h.example", "/tmp/x.sh").expect("parsed");
        assert_eq!(
            run,
            DetachedRun {
                host: "h.example".into(),
                pid: 4242,
                log: "/tmp/x.log".into(),
                rc_file: "/tmp/x.rc".into(),
                script_file: "/tmp/x.sh".into(),
            }
        );
        assert!(parse_detached("no sentinel here", "h", "/tmp/x.sh").is_none());
        assert!(
            parse_detached("__UNIRUN_DETACHED__ not-a-pid /l /r", "h", "/s").is_none(),
            "a malformed pid must not become a session"
        );
        assert!(
            parse_detached("__UNIRUN_DETACHED__ 1 /l", "h", "/s").is_none(),
            "a truncated sentinel must not become a session"
        );
    }

    /// Windows persistence is a different mechanism; refusing loudly beats a
    /// run that looks started and dies with the session.
    #[test]
    fn detach_refuses_non_posix_targets() {
        for shell in [Shell::Powershell, Shell::Pwsh, Shell::Cmd] {
            let t = SshTarget { shell, ..target() };
            let err = ssh_run_detached(&t, "echo hi").expect_err("must refuse");
            assert_eq!(err.error_class.as_deref(), Some("UNSUPPORTED"));
            assert!(!err.dispatched, "nothing was started");
            assert!(
                err.hint.as_deref().unwrap_or("").contains("schtasks"),
                "the hint must point at the supported mechanism: {:?}",
                err.hint
            );
        }
    }

    /// Platform differences, 16 and 17: the payload never appears literally in
    /// the remote command (cmd.exe would eat `>`), and the scp fallback carries
    /// the UTF-8 BOM PowerShell 5.1 needs to parse non-ASCII scripts.
    #[test]
    fn powershell_payload_is_encoded_and_bom_prefixed() {
        let script = "Get-Date > $null; Write-Output '中文'";
        let payload = ps_payload(script);
        assert!(payload.starts_with(GOLDEN_PREFIX));
        assert!(payload.ends_with(EXIT_CONTRACT));
        assert!(payload.contains(script));

        let b64 = base64_utf16le(&payload);
        for meta in ['>', '<', '|', '&', '"', '\''] {
            assert!(
                !b64.contains(meta),
                "base64 must carry no shell metacharacter, found {meta:?}"
            );
        }
        // Round-trip: UTF-16LE, so decode as such.
        let mut units: Vec<u16> = Vec::new();
        let raw = {
            use base64::engine::general_purpose::STANDARD;
            STANDARD.decode(&b64).unwrap()
        };
        for pair in raw.chunks_exact(2) {
            units.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
        assert_eq!(String::from_utf16(&units).unwrap(), payload);

        let file = ps_file_payload(&payload);
        assert_eq!(&file[..3], &[0xEF, 0xBB, 0xBF], "PS 5.1 needs the BOM");
        assert_eq!(&file[3..], payload.as_bytes());
    }

    /// The batch payload ends with the cmd exit contract (A5/A14 follow-up):
    /// win-exec did this and unirun did not, so a trailing cmd statement could
    /// reset the errorlevel and make the status meaningless.
    #[test]
    fn cmd_payload_appends_the_exit_contract() {
        let p = String::from_utf8(cmd_payload("echo hi")).unwrap();
        assert!(p.starts_with("echo hi\r\n"));
        assert!(p.ends_with("exit /b %ERRORLEVEL%\r\n"));
        // Idempotent enough: an existing trailing newline is not doubled.
        let p2 = String::from_utf8(cmd_payload("echo hi\n\n")).unwrap();
        assert_eq!(p, p2);
    }

    /// The threshold must leave room for the `-EncodedCommand` prefix inside
    /// `CreateProcess`'s 32 767-unit command-line limit (A14). A ~35 000-char
    /// base64 payload (≈13 KB of PowerShell) used to be sent inline by the old
    /// 60 000 threshold and could never have fit.
    #[test]
    fn inline_payloads_fit_the_createprocess_limit() {
        assert!(inline_encoded_command_fits(0));
        assert!(inline_encoded_command_fits(B64_THRESHOLD));
        assert!(!inline_encoded_command_fits(B64_THRESHOLD + 1));
        assert!(
            !inline_encoded_command_fits(35_000),
            "a 35k base64 payload must fall back to scp + -File"
        );
    }

    #[test]
    fn argv_defaults_include_batch_and_accept_new() {
        let a = ssh_argv(&target(), "bash -s");
        assert!(a.contains(&"-o".to_string()));
        assert!(a.contains(&"BatchMode=yes".to_string()));
        assert!(a.contains(&"StrictHostKeyChecking=accept-new".to_string()));
        assert!(a.contains(&"ControlMaster=no".to_string()));
        assert!(a.contains(&"ControlPath=none".to_string()));
        assert!(a.contains(&"ConnectTimeout=15".to_string()));
        assert!(a.contains(&"h.example".to_string()));
        assert!(a.contains(&"bash -s".to_string()));
    }

    #[test]
    fn argv_user_port_identity() {
        let mut t = target();
        t.user = Some("root".into());
        t.port = Some(2222);
        t.identity_file = Some(PathBuf::from("/tmp/key"));
        let a = ssh_argv(&t, "bash -s");
        assert!(a.contains(&"root@h.example".to_string()));
        let p = a.iter().position(|v| v == "-p").unwrap();
        assert_eq!(a[p + 1], "2222");
        let i = a.iter().position(|v| v == "-i").unwrap();
        assert_eq!(a[i + 1], "/tmp/key");
        assert!(
            !a.contains(&"h.example".to_string()),
            "bare host must not appear when user set"
        );
    }

    #[test]
    fn argv_empty_user_falls_back_to_bare_host() {
        let mut t = target();
        t.user = Some(String::new());
        let a = ssh_argv(&t, "bash -s");
        assert!(a.contains(&"h.example".to_string()));
        assert!(!a.contains(&"@h.example".to_string()));
    }

    #[test]
    fn prepare_script_unix_injects_cwd_and_env_safely() {
        let mut t = target();
        t.workdir = Some(PathBuf::from("/tmp/a path"));
        t.env = vec![
            ("A".into(), "one two".into()),
            ("BAD-KEY".into(), "x".into()),
        ];
        let script = prepare_script(&t, "printf '%s' \"$A\"");
        assert!(script.starts_with("cd '/tmp/a path' || exit $?\nexport A='one two'\n"));
        assert!(!script.contains("BAD-KEY"));
    }

    #[test]
    fn prepare_script_powershell_escapes_values() {
        let mut t = target();
        t.shell = Shell::Powershell;
        t.workdir = Some(PathBuf::from("C:\\tmp\\it's"));
        t.env = vec![("A".into(), "one's value".into())];
        let script = prepare_script(&t, "Write-Output $env:A");
        assert!(script.contains("Set-Location -LiteralPath 'C:\\tmp\\it''s'"));
        assert!(script.contains("$env:A = 'one''s value'"));
    }

    #[test]
    fn unix_shells_dispatch_to_unix_path() {
        for s in [Shell::Bash, Shell::Sh, Shell::Zsh] {
            let mut t = target();
            t.shell = s;
            t.host = "127.0.0.1".into();
            t.connect_timeout = 1;
            let r = ssh_run(&t, "echo hi");
            // Must NOT be the old "not a Windows remote shell" rejection;
            // any other outcome (connection refused etc.) is a transport fact.
            assert_ne!(
                r.error_class.as_deref(),
                Some("COMMAND_NOT_FOUND"),
                "shell {} must not be rejected",
                s.as_str()
            );
            assert!(
                !r.hint
                    .as_deref()
                    .unwrap_or("")
                    .contains("not a Windows remote shell"),
                "hint must not mention Windows for unix shell {}",
                s.as_str()
            );
        }
    }
}
