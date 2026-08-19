//! Execution engine: normalized local command/script execution.
//!
//! Guarantees (the product's reason to exist, implemented locally in P0):
//! - **argv, never hand-quoted strings** — script content reaches the shell
//!   as a single `-c` argument (or an explicit shell argv), so no outer
//!   quoting layer can corrupt it (the `cmd`-eats-`>` class of bugs).
//! - **in-process deadline** — no dependency on a GNU `timeout` binary
//!   (which does not exist on stock macOS); timeout is a wall-clock deadline
//!   enforced by this process.
//! - **whole-tree termination** — POSIX: negative pgid SIGTERM → SIGKILL
//!   (own process group via `process_group(0)`); Windows: `taskkill /T /F`.
//! - **capped tail-keeping streams** — output is bounded, drained past the
//!   cap (no pipe deadlock), and only the tail is kept, marked `truncated`.
//! - **SIGINT = abort** — a SIGINT installs a flag checked by the deadline
//!   loop; an in-flight tree is terminated and the result reports
//!   `aborted: true` (agent-safe retry semantics).

use crate::encoding::decode;
use crate::probe::which;
use crate::spec::{ExecKind, ExecResult, ExecSpec, Shell};
use crate::taxonomy::classify_with_maps;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Set by the SIGINT handler; checked by the deadline loop.
static ABORT: AtomicBool = AtomicBool::new(false);

extern "C" fn on_sigint(_: libc::c_int) {
    ABORT.store(true, Ordering::SeqCst);
}

/// Install the SIGINT → abort handler (call once, from main).
pub fn install_sigint_handler() {
    // signal() with a static, non-capturing handler is safe here.
    unsafe { libc::signal(libc::SIGINT, on_sigint as *const () as libc::sighandler_t) };
}

/// Reset the abort flag (mostly for tests that want clean state).
pub fn reset_abort() {
    ABORT.store(false, Ordering::SeqCst);
}

/// Set the abort flag programmatically (used by signal handlers in other
/// processes — e.g. the background-session runner treats SIGTERM as abort).
pub fn signal_abort() {
    ABORT.store(true, Ordering::SeqCst);
}

/// Which stream a chunk came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

/// One decoded output chunk (incremental UTF-8, CRLF normalized).
#[derive(Debug, Clone)]
pub struct StreamChunk {
    pub stream: StreamKind,
    pub text: String,
}

/// Run a command/script and return the normalized result (buffered).
pub fn run(spec: &ExecSpec) -> ExecResult {
    run_inner(spec, &ABORT, None)
}

/// Run and stream decoded output chunks as they arrive. `tx` receives
/// `StreamChunk`s (per-stream, incremental UTF-8 decoding, CRLF normalized);
/// the returned `ExecResult` is identical to `run`'s (same tail-keeping,
/// same classification). Use for live tails (background sessions) and
/// protocol streaming (ACP). When the receiver is dropped, streaming silently
/// degrades to buffered mode.
pub fn run_streaming(spec: &ExecSpec, tx: mpsc::Sender<StreamChunk>) -> ExecResult {
    run_inner(spec, &ABORT, Some(tx))
}

/// Streaming variant with a caller-owned abort flag (per-session cancel,
/// e.g. ACP `session/cancel`).
pub(crate) fn run_with_abort_streaming(
    spec: &ExecSpec,
    abort: &AtomicBool,
    tx: Option<mpsc::Sender<StreamChunk>>,
) -> ExecResult {
    run_inner(spec, abort, tx)
}

/// Run a command/script and return the normalized result.
fn run_inner(
    spec: &ExecSpec,
    abort: &AtomicBool,
    tx: Option<mpsc::Sender<StreamChunk>>,
) -> ExecResult {
    let start = Instant::now();
    // Direct argv (toolchain runner) bypasses shell interpretation entirely.
    let (shell, argv) = match &spec.direct {
        Some(a) => (a.first().cloned().unwrap_or_default(), a.clone()),
        None => {
            let shell = resolve_shell(spec);
            let argv = shell_argv(shell, spec);
            (shell.as_str().to_string(), argv)
        }
    };

    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(dir) = &spec.workdir {
        cmd.current_dir(dir);
    }
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    // Own process group so we can kill the whole tree.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Spawn failure (e.g. shell binary missing): surface as a
            // normalized result with a taxonomy class instead of panicking.
            let mut r = ExecResult::success(String::new(), String::new(), &shell);
            r.exit_code = None;
            r.error_class = Some("COMMAND_NOT_FOUND".into());
            r.hint = Some(format!("could not spawn `{}`: {}", shell, e));
            r.stderr = format!("unirun: spawn failed: {}", e);
            r.duration_ms = start.elapsed().as_millis() as u64;
            return r;
        }
    };

    let max = spec.effective_max_output();
    let stdout_thread = child.stdout.take().map(|s| {
        let tx = tx.clone();
        thread::spawn(move || read_capped_maybe_stream(s, max, StreamKind::Stdout, tx))
    });
    let stderr_thread = child
        .stderr
        .take()
        .map(|s| thread::spawn(move || read_capped_maybe_stream(s, max, StreamKind::Stderr, tx)));

    let timeout = Duration::from_millis(spec.effective_timeout_ms());
    let grace = Duration::from_millis(spec.effective_grace_ms());
    let mut exit_code: Option<i32> = None;
    let mut signal: Option<i32> = None;
    let mut timed_out = false;
    let mut aborted = false;

    // Deadline + abort polling loop.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                signal = signal_of(&status);
                break;
            }
            Ok(None) => {
                if abort.load(Ordering::SeqCst) {
                    aborted = true;
                    #[cfg(unix)]
                    kill_tree(&mut child, grace);
                    #[cfg(windows)]
                    kill_tree(&child, grace);
                    let _ = child.wait();
                    break;
                }
                if start.elapsed() >= timeout {
                    timed_out = true;
                    #[cfg(unix)]
                    kill_tree(&mut child, grace);
                    #[cfg(windows)]
                    kill_tree(&child, grace);
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(_) => {
                // ESRCH-like races: reap and move on.
                let _ = child.wait();
                break;
            }
        }
    }

    let (stdout_raw, stdout_trunc) = join_capture(stdout_thread);
    let (stderr_raw, stderr_trunc) = join_capture(stderr_thread);
    let stdout_decoded = decode(&stdout_raw);
    let stderr_decoded = decode(&stderr_raw);
    let stdout = crate::encoding::normalize_line_endings(&stdout_decoded.text);
    let stderr = crate::encoding::normalize_line_endings(&stderr_decoded.text);

    let mut result = ExecResult {
        exit_code,
        signal,
        stdout,
        stderr,
        timed_out,
        aborted,
        duration_ms: start.elapsed().as_millis() as u64,
        error_class: None,
        hint: None,
        encoding: stdout_decoded.encoding.to_string(),
        truncated: stdout_trunc || stderr_trunc,
        shell_used: shell,
    };
    let recipe_maps = if spec.error_maps.is_empty() {
        None
    } else {
        Some(&spec.error_maps)
    };
    let (class, hint) = classify_with_maps(&result, recipe_maps);
    result.error_class = class;
    result.hint = hint;
    result
}

/// Resolve which shell to use: explicit wins, else kind/extension-aware default.
fn resolve_shell(spec: &ExecSpec) -> Shell {
    if let Some(s) = spec.shell {
        return s;
    }
    if let Some(path) = &spec.workdir {
        // (kind-aware default below; workdir doesn't influence shell choice)
        let _ = path;
    }
    match spec.kind {
        ExecKind::Script => {
            // Extension inference happens in the CLI layer (it owns the file
            // path); by the time we get here a Script spec without an explicit
            // shell defaults to the POSIX default like Run.
            default_posix_shell()
        }
        ExecKind::Run => default_posix_shell(),
    }
}

fn default_posix_shell() -> Shell {
    if cfg!(windows) {
        // Windows has no bash/sh by default: PowerShell if present, else cmd.
        if which("powershell").is_some() || which("pwsh").is_some() {
            Shell::Powershell
        } else {
            Shell::Cmd
        }
    } else if which("bash").is_some() {
        Shell::Bash
    } else {
        Shell::Sh
    }
}

/// Build the exact argv handed to `Command` — no string interpolation.
fn shell_argv(shell: Shell, spec: &ExecSpec) -> Vec<String> {
    match shell {
        Shell::Bash | Shell::Sh | Shell::Zsh => {
            vec![
                shell.as_str().to_string(),
                "-c".into(),
                spec.command.clone(),
            ]
        }
        Shell::Pwsh | Shell::Powershell => {
            // Local PowerShell: inject the UTF-8 "golden recipe" so stdout and
            // stderr are clean UTF-8 instead of CLIXML/OEM mojibake — the same
            // normalization the SSH transport applies remotely. Each setter is
            // try/catch-guarded: in a no-console (piped) environment,
            // [Console]::OutputEncoding can throw a non-terminating "handle is
            // invalid" error that would otherwise pollute stderr.
            let recipe = "$ProgressPreference='SilentlyContinue';try{[Console]::OutputEncoding=[Text.Encoding]::UTF8}catch{};try{$OutputEncoding=[Text.Encoding]::UTF8}catch{};";
            vec![
                shell.as_str().to_string(),
                "-NoProfile".into(),
                "-Command".into(),
                format!("{} {}", recipe, spec.command),
            ]
        }
        Shell::Cmd => {
            vec![
                shell.as_str().to_string(),
                "/C".into(),
                spec.command.clone(),
            ]
        }
    }
}

struct Captured {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Read a stream to EOF, keeping only the **tail** `max` bytes but draining
/// the rest so the child never blocks on a full pipe. Errors and results
/// cluster at the end of output, so the tail is the diagnostic part agents
/// actually need. When `tx` is `Some`, decoded chunks are streamed live.
fn read_capped_maybe_stream<R: Read>(
    mut reader: R,
    max: usize,
    kind: StreamKind,
    tx: Option<mpsc::Sender<StreamChunk>>,
) -> Captured {
    let mut tail: Vec<u8> = Vec::with_capacity(max.saturating_add(8192));
    let mut total: usize = 0;
    let mut chunk = [0u8; 8192];
    let mut dec = tx.as_ref().map(|_| IncrementalDecoder::new());
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
                if let Some(d) = &mut dec {
                    let text = d.push(&chunk[..n]);
                    if !text.is_empty() {
                        let _ = tx.as_ref().unwrap().send(StreamChunk {
                            stream: kind,
                            text: crate::encoding::normalize_line_endings(&text),
                        });
                    }
                }
            }
            Err(_) => break,
        }
    }
    if let Some(d) = &mut dec {
        let rest = d.finish();
        if !rest.is_empty() {
            let _ = tx.as_ref().unwrap().send(StreamChunk {
                stream: kind,
                text: crate::encoding::normalize_line_endings(&rest),
            });
        }
    }
    Captured {
        bytes: tail,
        truncated: total > max,
    }
}

/// Incremental UTF-8 decoder: emits valid text per push and carries an
/// incomplete trailing sequence (≤3 bytes) to the next push. Invalid bytes
/// are replaced with U+FFFD (the stream is labeled lossy by the caller via
/// the final `ExecResult` encoding when the buffered decode detects it).
struct IncrementalDecoder {
    buf: Vec<u8>,
    lossy: bool,
}

impl IncrementalDecoder {
    fn new() -> Self {
        IncrementalDecoder {
            buf: Vec::with_capacity(8),
            lossy: false,
        }
    }

    /// Decode `bytes`; return the text completed so far. Anything that could
    /// still be the head of a multi-byte sequence is carried over.
    fn push(&mut self, bytes: &[u8]) -> String {
        self.buf.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.buf) {
                Ok(s) => {
                    out.push_str(s);
                    self.buf.clear();
                    break;
                }
                Err(e) => {
                    let valid = e.valid_up_to();
                    if valid > 0 {
                        // Safety: from_utf8 verified `valid` bytes are valid.
                        out.push_str(unsafe { std::str::from_utf8_unchecked(&self.buf[..valid]) });
                        self.buf.drain(..valid);
                        continue;
                    }
                    match e.error_len() {
                        Some(n) => {
                            self.lossy = true;
                            out.push('\u{FFFD}');
                            self.buf.drain(..n);
                        }
                        None => break, // incomplete tail: wait for more bytes
                    }
                }
            }
        }
        out
    }

    /// Flush whatever is still buffered (lossy).
    fn finish(&mut self) -> String {
        if self.buf.is_empty() {
            return String::new();
        }
        self.lossy = true;
        let out = String::from_utf8_lossy(&self.buf).into_owned();
        self.buf.clear();
        out
    }
}

fn join_capture(t: Option<thread::JoinHandle<Captured>>) -> (Vec<u8>, bool) {
    match t {
        Some(h) => {
            let c = h.join().unwrap_or(Captured {
                bytes: Vec::new(),
                truncated: false,
            });
            (c.bytes, c.truncated)
        }
        None => (Vec::new(), false),
    }
}

/// Terminate the whole process tree: SIGTERM, then SIGKILL after grace.
#[cfg(unix)]
fn kill_tree(child: &mut Child, grace: Duration) {
    let pid = child.id() as i32;
    // The child is its own process-group leader (process_group(0)); negative
    // pid signals the entire group, so pipelines/subshells die together.
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    let deadline = Instant::now() + grace;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        if Instant::now() >= deadline {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Windows tree termination via `taskkill /T /F` (P0: contained best-effort).
#[cfg(windows)]
fn kill_tree(child: &Child, _grace: Duration) {
    // taskkill writes "SUCCESS: ... terminated." to stdout — silence it so
    // protocol streams (e.g. MCP) stay clean.
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(unix)]
fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.signal()
}

#[cfg(windows)]
fn signal_of(_status: &std::process::ExitStatus) -> Option<i32> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::ExecSpec;

    fn sh_ok(command: &str) -> ExecSpec {
        ExecSpec {
            command: command.to_string(),
            shell: Some(Shell::Bash),
            ..Default::default()
        }
    }

    #[test]
    fn incremental_decoder_splits_multibyte_across_pushes() {
        let mut d = IncrementalDecoder::new();
        let bytes = "中文".as_bytes(); // 6 bytes: E4 B8 AD | E6 96 87
        let a = d.push(&bytes[..5]); // cut in the middle of 文's lead
        assert_eq!(a, "中");
        let b = d.push(&bytes[5..]);
        assert_eq!(b, "文");
        assert_eq!(d.finish(), "");
    }

    #[test]
    fn incremental_decoder_replaces_invalid_bytes() {
        let mut d = IncrementalDecoder::new();
        let out = d.push(b"ok\xFF\xFEnope");
        assert!(out.contains('\u{FFFD}'));
        assert!(out.contains("ok"));
        assert!(out.contains("nope"));
    }

    #[test]
    fn incremental_decoder_flushes_incomplete_tail_lossily() {
        let mut d = IncrementalDecoder::new();
        assert_eq!(d.push(&[0xE4]), ""); // incomplete lead byte
        let rest = d.finish();
        assert!(rest.contains('\u{FFFD}'));
    }

    #[test]
    fn run_streaming_matches_run_and_splits_streams() {
        if which("bash").is_none() {
            return; // windows CI images without bash
        }
        let spec = sh_ok("printf 'abc'; printf '中文'; echo boom >&2");
        let buffered = run(&spec);
        assert_eq!(buffered.exit_code, Some(0));
        assert_eq!(buffered.stdout, "abc中文");

        let (tx, rx) = mpsc::channel();
        let streamed = run_streaming(&spec, tx);
        let chunks: Vec<StreamChunk> = rx.try_iter().collect();
        let stdout_all: String = chunks
            .iter()
            .filter(|c| c.stream == StreamKind::Stdout)
            .map(|c| c.text.as_str())
            .collect();
        let stderr_all: String = chunks
            .iter()
            .filter(|c| c.stream == StreamKind::Stderr)
            .map(|c| c.text.as_str())
            .collect();
        assert_eq!(stdout_all, buffered.stdout);
        assert!(stderr_all.contains("boom"));
        assert_eq!(streamed.stdout, buffered.stdout);
        assert_eq!(streamed.exit_code, buffered.exit_code);
        assert_eq!(streamed.error_class, buffered.error_class);
    }

    #[test]
    fn run_with_custom_abort_flag_reports_aborted() {
        if which("bash").is_none() {
            return;
        }
        let abort = AtomicBool::new(true); // pre-set: run must abort immediately
        let r = run_with_abort_streaming(&sh_ok("sleep 5"), &abort, None);
        assert!(r.aborted);
        assert_eq!(r.error_class.as_deref(), Some("ABORTED"));
    }
}
