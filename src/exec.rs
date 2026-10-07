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

use crate::coalesce::{CoalesceConfig, CoalescePolicy, OutputCoalescer};
use crate::encoding::decode_with;
use crate::probe::which;
use crate::process_identity::{self, ExpectedIdentity, IdentityVerdict};
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
    // Every child receives a random generation token: embedded in its command
    // text (argv-visible on every platform) and its environment (inherited by
    // the whole tree). The tree-kill path verifies the pid still carries this
    // identity before signalling, so a recycled pid can never be mis-killed.
    let generation_token = process_identity::generate_generation_token();
    // Direct argv (toolchain runner) bypasses shell interpretation entirely.
    let (shell, argv) = match &spec.direct {
        Some(a) => (a.first().cloned().unwrap_or_default(), a.clone()),
        None => {
            let shell = resolve_shell(spec);
            let mut argv = shell_argv(shell, spec, &generation_token);
            // Resolve the shell binary through `which()` so Windows never
            // spawns the WSL launcher (`System32\bash.exe` — prints a
            // UTF-16LE "no distributions" message and exits 1) instead of a
            // real bash. which() excludes that shim; when no real shell is
            // found the bare name is kept and the spawn-error path reports
            // COMMAND_NOT_FOUND.
            if let Some(found) = which(&argv[0]) {
                argv[0] = found;
            }
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
    cmd.env(process_identity::GENERATION_TOKEN_ENV, &generation_token);
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

    // Snapshot the child's identity at spawn. If the child exited instantly
    // (snapshot fails) the epoch is `None` and the kill-time token check
    // alone decides — which still catches a recycled pid.
    //
    // On Windows the snapshot costs a PowerShell spawn (~1 s), so it is only
    // taken where the token cannot be verified at kill time: direct-argv runs
    // (their env is not exposed) — and on unix where it is cheap. Shell runs
    // carry the token in their argv (visible in `CommandLine`), which already
    // proves identity, so Windows shell runs skip the snapshot.
    let token_observable = spec.direct.is_none() || cfg!(not(windows));
    let epoch_snapshot_needed = cfg!(not(windows)) || spec.direct.is_some();
    let identity = ExpectedIdentity {
        pid: child.id(),
        generation_token: generation_token.clone(),
        start_epoch_ms: if epoch_snapshot_needed {
            process_identity::read_start_epoch_ms(child.id())
        } else {
            None
        },
        token_observable,
    };

    let coalesce_cfg: Option<CoalesceConfig> = match spec.coalesce {
        CoalescePolicy::Off => None,
        CoalescePolicy::Default => Some(CoalesceConfig::default()),
        CoalescePolicy::Custom(c) => Some(c),
    };
    let max = spec.effective_max_output();
    let stdout_thread = child.stdout.take().map(|s| {
        let tx = tx.clone();
        let cfg = coalesce_cfg;
        thread::spawn(move || read_capped_maybe_stream(s, max, StreamKind::Stdout, tx, cfg))
    });
    let stderr_thread = child.stderr.take().map(|s| {
        let tx = tx.clone();
        let cfg = coalesce_cfg.filter(|c| c.coalesce_stderr);
        thread::spawn(move || read_capped_maybe_stream(s, max, StreamKind::Stderr, tx, cfg))
    });

    let timeout = Duration::from_millis(spec.effective_timeout_ms());
    let grace = Duration::from_millis(spec.effective_grace_ms());
    let mut exit_code: Option<i32> = None;
    let mut signal: Option<i32> = None;
    let mut timed_out = false;
    let mut aborted = false;
    // Set when the tree-kill went ahead without the identity probe confirming
    // the pid (unreadable probe, or a child that re-exec'd away its token).
    // The kill still happens — see `kill_gate` — so this only annotates the
    // result instead of replacing its classification.
    let mut identity_unconfirmed: Option<IdentityVerdict> = None;

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
                    identity_unconfirmed = kill_tree_of_owned_child(&mut child, grace, &identity);
                    break;
                }
                if start.elapsed() >= timeout {
                    timed_out = true;
                    identity_unconfirmed = kill_tree_of_owned_child(&mut child, grace, &identity);
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
    let stdout_decoded = decode_with(&stdout_raw, spec.output_encoding.as_deref());
    let stderr_decoded = decode_with(&stderr_raw, spec.output_encoding.as_deref());
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
        transport_error: false,
        transport_stderr: None,
    };
    let recipe_maps = if spec.error_maps.is_empty() {
        None
    } else {
        Some(&spec.error_maps)
    };
    let (class, hint) = classify_with_maps(&result, recipe_maps);
    result.error_class = class;
    result.hint = hint;
    // The tree was signalled on the owned-child guarantee (see `kill_gate`),
    // not on a verified pid: keep that visible instead of silently claiming
    // the identity layer confirmed it.
    if let Some(verdict) = identity_unconfirmed {
        let note = format!("tree kill not identity-confirmed: {}", verdict.describe());
        result.hint = Some(match result.hint.take() {
            Some(existing) => format!("{}; {}", existing, note),
            None => note,
        });
    }
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

/// Build the exact argv handed to `Command` — no string interpolation. The
/// generation token is embedded in the command text so it is argv-visible on
/// every platform (see `process_identity::inject_generation_token`).
fn shell_argv(shell: Shell, spec: &ExecSpec, generation_token: &str) -> Vec<String> {
    let command = spec.command.clone();
    match shell {
        Shell::Bash | Shell::Sh | Shell::Zsh => {
            vec![
                shell.as_str().to_string(),
                "-c".into(),
                process_identity::inject_generation_token(shell, &command, generation_token),
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
                format!(
                    "{} {}",
                    recipe,
                    process_identity::inject_generation_token(shell, &command, generation_token)
                ),
            ]
        }
        Shell::Cmd => {
            vec![
                shell.as_str().to_string(),
                "/C".into(),
                process_identity::inject_generation_token(shell, &command, generation_token),
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
/// actually need. When `tx` is `Some`, decoded chunks are streamed live —
/// through an `OutputCoalescer` when `coalesce` is `Some` (adjacent same-type
/// chunks merged; forwarded on byte threshold / timer; final flush at EOF, so
/// content is never lost).
fn read_capped_maybe_stream<R: Read>(
    mut reader: R,
    max: usize,
    kind: StreamKind,
    tx: Option<mpsc::Sender<StreamChunk>>,
    coalesce: Option<CoalesceConfig>,
) -> Captured {
    let mut tail: Vec<u8> = Vec::with_capacity(max.saturating_add(8192));
    let mut total: usize = 0;
    let mut chunk = [0u8; 8192];
    let mut dec = tx.as_ref().map(|_| IncrementalDecoder::new());
    let coalescer = tx
        .as_ref()
        .zip(coalesce)
        .map(|(tx, cfg)| OutputCoalescer::new(tx.clone(), cfg));
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
                        let text = crate::encoding::normalize_line_endings(&text);
                        match &coalescer {
                            Some(co) => co.push(kind, text),
                            None => {
                                let _ = tx
                                    .as_ref()
                                    .unwrap()
                                    .send(StreamChunk { stream: kind, text });
                            }
                        }
                    }
                }
            }
            Err(_) => break,
        }
    }
    if let Some(d) = &mut dec {
        let rest = d.finish();
        if !rest.is_empty() {
            let rest = crate::encoding::normalize_line_endings(&rest);
            match &coalescer {
                Some(co) => co.push(kind, rest),
                None => {
                    let _ = tx.as_ref().unwrap().send(StreamChunk {
                        stream: kind,
                        text: rest,
                    });
                }
            }
        }
    }
    if let Some(co) = coalescer {
        // EOF: forward whatever is buffered and stop the timer thread.
        co.close();
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

/// Bounded grace for a freshly spawned child to `exec`, used by
/// `verify_before_kill`; poll interval while waiting.
const KILL_IDENTITY_SETTLE: Duration = Duration::from_millis(250);
const KILL_IDENTITY_POLL: Duration = Duration::from_millis(10);

/// `verify`, tolerant of the `fork` → `execve` window. Between the two the
/// child still carries its *parent's* argv and environment — only `execve`
/// installs ours — so the generation token is not observable yet and a read
/// calls a perfectly owned child unconfirmed. A mismatch is therefore re-read
/// for a bounded grace before it is believed; `Matches`, `NotRunning` and
/// `Unverifiable` are returned as they are.
///
/// This is what made the same window show up as a rare `PID_REUSED` on the
/// ubuntu runner (start epoch matching, only the token missing) back when a
/// mismatch was allowed to veto the kill.
fn verify_before_kill(identity: &ExpectedIdentity, settle: Duration) -> IdentityVerdict {
    let deadline = Instant::now() + settle;
    loop {
        let verdict =
            process_identity::verify(identity, process_identity::IDENTITY_EPOCH_TOLERANCE_MS);
        match verdict {
            IdentityVerdict::StartEpochMismatch { .. } | IdentityVerdict::TokenMismatch => {
                if Instant::now() >= deadline {
                    return verdict;
                }
                thread::sleep(KILL_IDENTITY_POLL);
            }
            other => return other,
        }
    }
}

/// What to do about a tree-kill once the identity probe has reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KillGate {
    /// Signal the tree — identity confirmed.
    Signal,
    /// Signal the tree anyway, and report the verdict: the probe could not
    /// confirm a child we hold unreaped, which is not a reason to leave a tree
    /// running.
    SignalUnconfirmed,
    /// Do not signal: the pid is gone.
    Skip,
}

/// Policy for the tree-kill gate (I/O lives in `kill_tree_of_owned_child`).
///
/// This gate only ever handles a child **we still own**: it runs from the poll
/// loop, which reaches it only when `try_wait()` reports the child unreaped.
/// An unreaped child's pid cannot have been recycled — POSIX keeps a pid
/// allocated until it is waited for, and Rust keeps the process handle open on
/// Windows until then — so the probe is *corroboration, never a veto*:
///
/// - `NotRunning` → nothing to signal.
/// - `Matches` → confirmed; signal.
/// - anything else → signal and report. Refusing here is what turned a
///   blocked probe (`ps` unavailable) into a silently skipped kill, and an
///   immediately aborted run into a `PID_REUSED` claim about a pid that was
///   never reused. A child that re-exec'd away its token (`env -i`, `exec`)
///   or whose command cannot be read is still ours.
fn kill_gate(verdict: &IdentityVerdict) -> KillGate {
    match verdict {
        IdentityVerdict::NotRunning => KillGate::Skip,
        IdentityVerdict::Matches => KillGate::Signal,
        IdentityVerdict::Unverifiable
        | IdentityVerdict::StartEpochMismatch { .. }
        | IdentityVerdict::TokenMismatch => KillGate::SignalUnconfirmed,
    }
}

/// Terminate the tree of the child we own, per [`kill_gate`]. Returns the
/// verdict to report when the probe did not confirm the identity, and `None`
/// when it matched or there was nothing to kill.
fn kill_tree_of_owned_child(
    child: &mut Child,
    grace: Duration,
    identity: &ExpectedIdentity,
) -> Option<IdentityVerdict> {
    let verdict = verify_before_kill(identity, KILL_IDENTITY_SETTLE);
    match kill_gate(&verdict) {
        KillGate::Skip => None,
        KillGate::Signal => {
            kill_tree(child, grace);
            None
        }
        KillGate::SignalUnconfirmed => {
            kill_tree(child, grace);
            Some(verdict)
        }
    }
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
        assert_eq!(
            buffered.exit_code,
            Some(0),
            "buffered result: {:?}",
            buffered
        );
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
        assert!(r.aborted, "result: {:?}", r);
        assert_eq!(r.error_class.as_deref(), Some("ABORTED"), "result: {:?}", r);
    }

    /// The identity gate must not refuse a child it cannot see *yet*: between
    /// `fork` and `execve` the child still shows its parent's argv and
    /// environment, so an abort landing immediately after spawn observes a
    /// token-less command. That surfaced once as `PID_REUSED` instead of
    /// `ABORTED` on the ubuntu runner.
    #[test]
    fn verify_before_kill_waits_out_the_fork_exec_window() {
        if !process_identity::platform_probe_available() {
            eprintln!("skipping: this environment cannot report process identities");
            return;
        }
        // Our own process carries neither our random token nor its start epoch,
        // so after the grace the refusal must still be the honest one: waiting
        // must not turn a mismatch into a match.
        let expected = ExpectedIdentity {
            pid: std::process::id(),
            generation_token: "ur-not-ours".into(),
            start_epoch_ms: None,
            token_observable: true,
        };
        let settle = Duration::from_millis(30);
        let started = Instant::now();
        assert_eq!(
            verify_before_kill(&expected, settle),
            IdentityVerdict::TokenMismatch
        );
        assert!(
            started.elapsed() >= settle,
            "the fork/exec grace was not honoured"
        );
    }

    #[test]
    fn generation_token_injection_does_not_change_output() {
        if which("bash").is_none() {
            return;
        }
        // Identity injection is default-on; the prefix `export
        // UNIRUN_GENERATION_TOKEN=…;` must not leak into captured output.
        let spec = sh_ok("echo hi");
        let r = run(&spec);
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.stdout, "hi\n", "output polluted by token: {:?}", r.stdout);
        assert!(!r.stdout.contains("UNIRUN_GENERATION_TOKEN"));
        assert!(!r.stderr.contains("UNIRUN_GENERATION_TOKEN"));
    }

    /// The default shell on Windows is PowerShell, where the right-hand side of
    /// `=` is parsed as a *statement*: an unquoted `$env:NAME=<token>` runs the
    /// token as a command, pollutes stderr and the taxonomy reports
    /// `COMMAND_NOT_FOUND` for every default-shell run. The other identity
    /// tests pin `shell: Some(Shell::Bash)` (and skip when bash is absent), so
    /// this one deliberately exercises the resolved default shell end to end.
    #[cfg(windows)]
    #[test]
    fn default_shell_run_reports_no_spurious_error() {
        let spec = ExecSpec {
            command: "echo unirun-default-shell".into(),
            ..Default::default()
        };
        let r = run(&spec);
        assert_eq!(r.exit_code, Some(0), "result: {:?}", r);
        assert_eq!(r.error_class, None, "result: {:?}", r);
        assert_eq!(r.stdout, "unirun-default-shell\n", "result: {:?}", r);
        assert!(r.stderr.is_empty(), "stderr polluted: {:?}", r.stderr);
    }

    /// Policy pin for the tree-kill gate: the child is ours by construction
    /// (we hold it unreaped, so its pid cannot have been recycled), therefore
    /// only "gone" may skip the kill. A blocked probe or a child that re-exec'd
    /// away its token must still be terminated — the verdict is reported.
    #[test]
    fn kill_gate_never_refuses_an_owned_child() {
        assert_eq!(kill_gate(&IdentityVerdict::NotRunning), KillGate::Skip);
        assert_eq!(kill_gate(&IdentityVerdict::Matches), KillGate::Signal);
        let unconfirmed = [
            IdentityVerdict::Unverifiable,
            IdentityVerdict::TokenMismatch,
            IdentityVerdict::StartEpochMismatch {
                observed: 2,
                expected: 1,
            },
        ];
        for verdict in unconfirmed {
            assert_eq!(
                kill_gate(&verdict),
                KillGate::SignalUnconfirmed,
                "verdict: {:?}",
                verdict
            );
        }
    }

    /// End to end: `exec env -i sleep 30` replaces the shell with a process
    /// whose argv and environment carry no generation token, so the probe can
    /// never confirm it. It is still our unreaped child, so the tree must die
    /// and the run must report its real classification (TIMEOUT) with the
    /// missing confirmation noted — not a `PID_REUSED` claim about a pid that
    /// was never reused, and not a silently skipped kill.
    #[cfg(unix)]
    #[test]
    fn cleared_environment_child_is_still_killed_and_reported() {
        if which("bash").is_none() || which("env").is_none() {
            return;
        }
        let marker = std::env::temp_dir().join(format!(
            "unirun-killgate-{}-{}.pid",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        // `exec` keeps the pid, so the marker names the process that survives
        // as `sleep` with an empty environment.
        let mut spec = sh_ok(&format!(
            "printf '%s' $$ > \"{}\"; exec env -i sleep 30",
            marker.display()
        ));
        spec.timeout_ms = 700;
        let r = run(&spec);
        assert!(r.timed_out, "result: {:?}", r);
        assert_eq!(r.error_class.as_deref(), Some("TIMEOUT"), "result: {:?}", r);
        let hint = r.hint.clone().unwrap_or_default();
        assert!(
            hint.contains("not identity-confirmed"),
            "expected an unconfirmed-identity note, got {:?} (result: {:?})",
            hint,
            r
        );
        let pid: u32 = std::fs::read_to_string(&marker)
            .expect("child wrote its pid")
            .trim()
            .parse()
            .expect("pid");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && process_identity::is_alive(pid) {
            thread::sleep(Duration::from_millis(20));
        }
        let alive = process_identity::is_alive(pid);
        let _ = std::fs::remove_file(&marker);
        assert!(
            !alive,
            "cleared-environment child {} survived the tree kill",
            pid
        );
    }

    #[test]
    fn generation_token_visible_in_child_env() {
        if which("bash").is_none() {
            return;
        }
        // The injected token must be readable from inside the child (it was
        // both prefixed into the command text and passed via env).
        let r = run(&sh_ok(
            "test -n \"$UNIRUN_GENERATION_TOKEN\" && printf '%s' \"$UNIRUN_GENERATION_TOKEN\"",
        ));
        assert_eq!(r.exit_code, Some(0), "result: {:?}", r);
        assert!(r.stdout.starts_with("ur"), "stdout: {:?}", r.stdout);
    }

    #[test]
    fn no_coalesce_streams_same_content() {
        if which("bash").is_none() {
            return;
        }
        let mut spec = sh_ok("printf 'abc'; printf '中文'; echo boom >&2");
        spec.coalesce = crate::coalesce::CoalescePolicy::Off;
        let buffered = run(&spec);
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
    }

    #[test]
    fn direct_argv_runs_are_unaffected_by_injection() {
        if cfg!(windows) {
            return; // no standalone `echo` binary on Windows
        }
        let spec = ExecSpec {
            direct: Some(vec!["echo".into(), "direct-ok".into()]),
            ..Default::default()
        };
        let r = run(&spec);
        assert_eq!(r.exit_code, Some(0), "result: {:?}", r);
        assert_eq!(r.stdout, "direct-ok\n");
    }
}
