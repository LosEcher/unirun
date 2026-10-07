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
use crate::spec::{ExecKind, ExecResult, ExecSpec, ExitCodeConfidence, KillStatus, Shell};
use crate::taxonomy::classify_with_maps;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// Local PowerShell: the UTF-8 "golden recipe", so stdout and stderr are clean
/// UTF-8 instead of CLIXML/OEM mojibake — the same normalization the SSH
/// transport applies remotely.
///
/// Platform difference (Windows, PowerShell 5.1): **each setter must be
/// try/catch-guarded**. In a no-console (piped) environment
/// `[Console]::OutputEncoding` throws a non-terminating "handle is invalid"
/// error that would otherwise land in the agent's stderr; and
/// `$ProgressPreference` must be silenced because progress records are written
/// to the same streams.
pub(crate) const POWERSHELL_UTF8_RECIPE: &str = "$ProgressPreference='SilentlyContinue';try{[Console]::OutputEncoding=[Text.Encoding]::UTF8}catch{};try{$OutputEncoding=[Text.Encoding]::UTF8}catch{};";

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

/// Has the caller asked to cancel? Consulted by every transport's wait loop,
/// not just the local one: a Ctrl-C during a remote run must cancel it too.
pub fn abort_requested() -> bool {
    ABORT.load(Ordering::SeqCst)
}

/// Why a wait loop stopped, when it did not stop because the child exited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteStop {
    Aborted,
    TimedOut,
}

/// Which reason wins when both are true.
///
/// Cancellation is checked **before** the deadline: a Ctrl-C that lands at the
/// same moment as the timeout is a caller cancellation, and reporting `TIMEOUT`
/// there would invite a retry the caller explicitly did not ask for.
pub fn remote_stop(
    abort_requested: bool,
    elapsed: Duration,
    timeout: Duration,
) -> Option<RemoteStop> {
    if abort_requested {
        return Some(RemoteStop::Aborted);
    }
    if elapsed >= timeout {
        return Some(RemoteStop::TimedOut);
    }
    None
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
            // Nothing was handed to a shell: this is the one local case where
            // resubmitting is unambiguously safe.
            r.dispatched = false;
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
    let (stdout_done_tx, stdout_done_rx) = mpsc::channel::<Captured>();
    let (stderr_done_tx, stderr_done_rx) = mpsc::channel::<Captured>();
    let stdout_shared = PartialCapture::new();
    let stderr_shared = PartialCapture::new();
    if let Some(s) = child.stdout.take() {
        let tx = tx.clone();
        let cfg = coalesce_cfg;
        let done = stdout_done_tx;
        let shared = stdout_shared.clone();
        thread::spawn(move || {
            let c = read_capped_maybe_stream(s, max, StreamKind::Stdout, tx, cfg, shared);
            let _ = done.send(c);
        });
    }
    if let Some(s) = child.stderr.take() {
        let tx = tx.clone();
        let cfg = coalesce_cfg.filter(|c| c.coalesce_stderr);
        let done = stderr_done_tx;
        let shared = stderr_shared.clone();
        thread::spawn(move || {
            let c = read_capped_maybe_stream(s, max, StreamKind::Stderr, tx, cfg, shared);
            let _ = done.send(c);
        });
    }

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
    let mut kill_attempt = KillAttempt {
        outcome: None,
        identity_unconfirmed: None,
    };

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
                    kill_attempt = kill_tree_of_owned_child(&mut child, grace, &identity);
                    break;
                }
                if start.elapsed() >= timeout {
                    timed_out = true;
                    kill_attempt = kill_tree_of_owned_child(&mut child, grace, &identity);
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

    // Bounded drain: the child is gone, but a grandchild may still hold the
    // pipe. Wait for the readers, but never forever.
    // One budget for both streams: a grandchild that holds the pipes holds
    // both, and two sequential deadlines would double the wait.
    let drain_deadline = Instant::now() + Duration::from_millis(spec.effective_drain_ms());
    let remaining = || drain_remaining(drain_deadline);
    let (stdout_raw, stdout_trunc, stdout_drain_timeout) =
        collect_capture(Some(stdout_done_rx), Some(stdout_shared), remaining());
    let (stderr_raw, stderr_trunc, stderr_drain_timeout) =
        collect_capture(Some(stderr_done_rx), Some(stderr_shared), remaining());
    let drain_timeout = stdout_drain_timeout || stderr_drain_timeout;
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
        dispatched: true,
        // Filled in once the run ends: both are properties of how *this* run
        // terminated, not of the spec.
        kill_status: None,
        exit_code_confidence: ExitCodeConfidence::Observed,
        drain_timeout,
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
    if let Some(verdict) = kill_attempt.identity_unconfirmed.take() {
        let note = format!("tree kill not identity-confirmed: {}", verdict.describe());
        result.hint = Some(match result.hint.take() {
            Some(existing) => format!("{}; {}", existing, note),
            None => note,
        });
    }
    result.kill_status = kill_attempt.status();
    result.exit_code_confidence = local_exit_code_confidence(spec, &result);
    result
}

/// Is this run's exit status evidence for its outcome?
///
/// Everything but one case is `Observed`. The exception is a **zero** status
/// from a local PowerShell `-Command` run: PowerShell does not propagate native
/// exit codes, and unlike the ssh transport this path appends no
/// `exit $LASTEXITCODE`, so a failing native command inside the script can
/// still leave `0` behind. A non-zero status is itself the evidence (something
/// deliberately set it), so only zero is unverifiable.
fn local_exit_code_confidence(spec: &ExecSpec, result: &ExecResult) -> ExitCodeConfidence {
    if spec.direct.is_some() || result.exit_code != Some(0) {
        return ExitCodeConfidence::Observed;
    }
    match result.shell_used.as_str() {
        "powershell" | "pwsh" => ExitCodeConfidence::Unknown,
        _ => ExitCodeConfidence::Observed,
    }
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
            let recipe = POWERSHELL_UTF8_RECIPE;
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

pub(crate) struct Captured {
    pub(crate) bytes: Vec<u8>,
    pub(crate) truncated: bool,
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
    shared: std::sync::Arc<PartialCapture>,
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
                shared.publish(&tail, total > max);
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

/// A reader thread's progress, shared with the main thread so a **drain
/// deadline** can take what has arrived instead of blocking on EOF forever.
///
/// EOF is not guaranteed even after the direct child exits: a grandchild that
/// inherited the pipe (`sleep 5 &`, a daemonised helper) keeps the write end
/// open. Waiting for it used to hang unirun after its own kill — the failure
/// mode Codex's exec kernel bounds with an IO drain timeout
/// (CODEX-DSH-HARNESS-DESIGN-ANALYSIS-2026-08-21.md:248).
pub(crate) struct PartialCapture {
    bytes: std::sync::Mutex<Vec<u8>>,
    truncated: std::sync::atomic::AtomicBool,
}

impl PartialCapture {
    pub(crate) fn new() -> std::sync::Arc<Self> {
        std::sync::Arc::new(PartialCapture {
            bytes: std::sync::Mutex::new(Vec::new()),
            truncated: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Publish the tail kept so far (called after each read).
    pub(crate) fn publish(&self, tail: &[u8], truncated: bool) {
        if let Ok(mut buf) = self.bytes.lock() {
            buf.clear();
            buf.extend_from_slice(tail);
        }
        if truncated {
            self.truncated
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    pub(crate) fn snapshot(&self) -> (Vec<u8>, bool) {
        let bytes = self.bytes.lock().map(|b| b.clone()).unwrap_or_default();
        (
            bytes,
            self.truncated.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

/// The result of collecting one stream: bytes, whether the cap was hit, and
/// whether the drain deadline expired first (capture may be incomplete).
pub(crate) type Collected = (Vec<u8>, bool, bool);

/// What is left of the **shared** drain budget.
///
/// One budget covers both streams: a grandchild that holds the pipes holds both,
/// and two sequential deadlines would double the wait (measured: 4.03 s instead
/// of 2.03 s for `sleep 5 &` with a 2 s budget). Extracted so the sharing is
/// unit-tested rather than inferred from a stopwatch.
pub(crate) fn drain_remaining_at(deadline: Instant, now: Instant) -> Duration {
    deadline.saturating_duration_since(now)
}

/// [`drain_remaining_at`] against the current clock.
pub(crate) fn drain_remaining(deadline: Instant) -> Duration {
    drain_remaining_at(deadline, Instant::now())
}

/// Take a reader's output, waiting at most `deadline` past the child's exit.
pub(crate) fn collect_capture(
    rx: Option<mpsc::Receiver<Captured>>,
    shared: Option<std::sync::Arc<PartialCapture>>,
    deadline: Duration,
) -> Collected {
    let Some(rx) = rx else {
        return (Vec::new(), false, false);
    };
    // Already finished (the common case, including "the other stream used up
    // the shared drain budget"): take it without waiting.
    match rx.try_recv() {
        Ok(c) => return (c.bytes, c.truncated, false),
        Err(mpsc::TryRecvError::Disconnected) => return (Vec::new(), false, false),
        Err(mpsc::TryRecvError::Empty) => {}
    }
    match rx.recv_timeout(deadline) {
        Ok(c) => (c.bytes, c.truncated, false),
        Err(_) => match shared {
            // Partial data, flagged: the caller must not read this as complete.
            Some(shared) => {
                let (bytes, truncated) = shared.snapshot();
                (bytes, truncated, true)
            }
            None => (Vec::new(), false, true),
        },
    }
}

/// How long to wait for an escalated (SIGKILL / `taskkill /F`) kill to take
/// effect before calling the process unkillable.
pub(crate) const SIGKILL_SETTLE: Duration = Duration::from_millis(500);

/// Terminate the whole process tree: SIGTERM, then SIGKILL after grace.
#[cfg(unix)]
fn kill_tree(child: &mut Child, grace: Duration) -> KillStatus {
    let pid = child.id() as i32;
    // The child is its own process-group leader (process_group(0)); negative
    // pid signals the entire group, so pipelines/subshells die together.
    unsafe { libc::kill(-pid, libc::SIGTERM) };
    let deadline = Instant::now() + grace;
    loop {
        if let Ok(Some(_)) = child.try_wait() {
            return KillStatus::Clean;
        }
        if Instant::now() >= deadline {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            // SIGKILL cannot be caught, but a process in uninterruptible sleep
            // (D state — a blocked network filesystem, a stuck driver) survives
            // it. Say so instead of implying the tree is gone.
            return settle_after_kill(child);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Windows tree termination via `taskkill /T /F` (P0: contained best-effort).
#[cfg(windows)]
fn kill_tree(child: &mut Child, _grace: Duration) -> KillStatus {
    // taskkill writes "SUCCESS: ... terminated." to stdout — silence it so
    // protocol streams (e.g. MCP) stay clean.
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    // `/F` *is* the force path: there is no softer escalation to distinguish,
    // so a successful termination is reported as an escalated one.
    match settle_after_kill(child) {
        KillStatus::Clean => KillStatus::SigkillEscalated,
        other => other,
    }
}

/// Bounded wait after an escalated kill: exited → escalated, still running →
/// survived.
pub(crate) fn settle_after_kill(child: &mut Child) -> KillStatus {
    let deadline = Instant::now() + SIGKILL_SETTLE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return KillStatus::SigkillEscalated,
            Ok(None) => {
                if Instant::now() >= deadline {
                    return KillStatus::Survived;
                }
            }
            Err(_) => return KillStatus::SigkillEscalated,
        }
        thread::sleep(Duration::from_millis(10));
    }
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

/// The result of one kill attempt.
struct KillAttempt {
    /// `None` when no signal was sent.
    outcome: Option<KillStatus>,
    /// The verdict to report when the probe did not confirm the identity; the
    /// outcome is downgraded to [`KillStatus::Unconfirmed`] in that case.
    identity_unconfirmed: Option<IdentityVerdict>,
}

impl KillAttempt {
    fn status(&self) -> Option<KillStatus> {
        match (self.outcome, self.identity_unconfirmed.is_some()) {
            (None, _) => None,
            (Some(_), true) => Some(KillStatus::Unconfirmed),
            (Some(s), false) => Some(s),
        }
    }
}

/// Terminate the tree of the child we own, per [`kill_gate`].
fn kill_tree_of_owned_child(
    child: &mut Child,
    grace: Duration,
    identity: &ExpectedIdentity,
) -> KillAttempt {
    let verdict = verify_before_kill(identity, KILL_IDENTITY_SETTLE);
    match kill_gate(&verdict) {
        KillGate::Skip => KillAttempt {
            outcome: None,
            identity_unconfirmed: None,
        },
        KillGate::Signal => KillAttempt {
            outcome: Some(kill_tree(child, grace)),
            identity_unconfirmed: None,
        },
        KillGate::SignalUnconfirmed => KillAttempt {
            outcome: Some(kill_tree(child, grace)),
            identity_unconfirmed: Some(verdict),
        },
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

    /// Cancellation beats the deadline, in every transport's wait loop: a
    /// Ctrl-C landing at the same instant as the timeout is a caller
    /// cancellation, not something to invite a retry with `TIMEOUT`.
    #[test]
    fn abort_outranks_the_deadline() {
        let timeout = Duration::from_millis(1_000);
        assert_eq!(remote_stop(false, Duration::from_millis(10), timeout), None);
        assert_eq!(
            remote_stop(false, timeout, timeout),
            Some(RemoteStop::TimedOut)
        );
        assert_eq!(
            remote_stop(true, Duration::from_millis(10), timeout),
            Some(RemoteStop::Aborted)
        );
        assert_eq!(
            remote_stop(true, timeout, timeout),
            Some(RemoteStop::Aborted),
            "abort must win at the deadline"
        );
    }

    /// The drain budget is shared: the second stream gets what the first left,
    /// and never a fresh deadline of its own.
    #[test]
    fn the_drain_budget_is_shared_between_streams() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(2_000);
        assert_eq!(
            drain_remaining_at(deadline, now),
            Duration::from_millis(2_000)
        );
        // The first stream consumed 1.5 s of it.
        let after_first = now + Duration::from_millis(1_500);
        assert_eq!(
            drain_remaining_at(deadline, after_first),
            Duration::from_millis(500),
            "the second stream must inherit the remainder, not restart the budget"
        );
        // A stream that used it all leaves nothing (and never panics).
        let exhausted = now + Duration::from_millis(2_500);
        assert_eq!(drain_remaining_at(deadline, exhausted), Duration::ZERO);
    }

    /// The bug this bounds: a grandchild inherits the pipe, the shell exits, and
    /// the reader never sees EOF. Before the drain deadline, `unirun run
    /// 'sleep 5 &'` waited the full 5 seconds (or forever, for a daemon).
    #[cfg(unix)]
    #[test]
    fn a_grandchild_holding_the_pipe_does_not_hang_the_run() {
        let mut spec = sh_ok("sleep 5 & echo started");
        spec.drain_ms = 1_000;
        let start = Instant::now();
        let r = run(&spec);
        let elapsed = start.elapsed();

        assert_eq!(r.exit_code, Some(0), "{r:?}");
        assert!(r.stdout.contains("started"), "stdout: {:?}", r.stdout);
        assert!(
            r.drain_timeout,
            "the drain deadline must be reported: {r:?}"
        );
        // Smoke bound only: the shared-budget property is pinned by
        // `the_drain_budget_is_shared_between_streams` (no stopwatch), while this
        // proves the run did not wait for the grandchild at all. Generous on
        // purpose — CI runners are slow and loaded.
        assert!(
            elapsed < std::time::Duration::from_millis(3_000),
            "must not wait for the grandchild: {elapsed:?}"
        );
    }

    /// The normal path is unaffected: EOF arrives, nothing is flagged, and the
    /// capture is complete.
    #[test]
    fn a_normal_run_reports_no_drain_timeout() {
        let r = run(&sh_ok("echo quick"));
        assert!(!r.drain_timeout, "{r:?}");
        assert_eq!(r.stdout, "quick\n");
    }

    /// A run that ignores SIGTERM must be reported as an escalated kill, not as
    /// a clean termination (and not as unkillable — SIGKILL does work).
    #[cfg(unix)]
    #[test]
    fn a_term_ignoring_child_reports_an_escalated_kill() {
        let mut spec = sh_ok("trap '' TERM; sleep 30");
        spec.timeout_ms = 300;
        spec.grace_ms = 200;
        let r = run(&spec);
        assert!(r.timed_out, "deadline must have elapsed: {r:?}");
        assert_eq!(
            r.kill_status,
            Some(KillStatus::SigkillEscalated),
            "SIGKILL was required: {r:?}"
        );
        assert_eq!(r.error_class.as_deref(), Some("TIMEOUT"));
    }

    /// A run that exits on SIGTERM reports a clean kill.
    #[cfg(unix)]
    #[test]
    fn a_cooperative_child_reports_a_clean_kill() {
        let mut spec = sh_ok("sleep 30");
        spec.timeout_ms = 300;
        spec.grace_ms = 2_000;
        let r = run(&spec);
        assert!(r.timed_out);
        assert_eq!(r.kill_status, Some(KillStatus::Clean), "{r:?}");
    }

    /// No signal, no `kill_status`: the process ended on its own.
    #[test]
    fn a_completed_run_has_no_kill_status() {
        let r = run(&sh_ok("echo done"));
        assert_eq!(r.kill_status, None);
        assert_eq!(r.exit_code_confidence, ExitCodeConfidence::Observed);
    }

    /// The kill-outcome state machine, including the unkillable case a live test
    /// cannot produce portably (a process in D state).
    #[test]
    fn kill_status_maps_the_attempt() {
        let attempt = |outcome: Option<KillStatus>, unconfirmed: bool| KillAttempt {
            outcome,
            identity_unconfirmed: unconfirmed.then_some(IdentityVerdict::Unverifiable),
        };
        assert_eq!(attempt(None, false).status(), None);
        assert_eq!(
            attempt(Some(KillStatus::Clean), false).status(),
            Some(KillStatus::Clean)
        );
        assert_eq!(
            attempt(Some(KillStatus::Survived), false).status(),
            Some(KillStatus::Survived)
        );
        assert_eq!(
            attempt(Some(KillStatus::Clean), true).status(),
            Some(KillStatus::Unconfirmed),
            "an unconfirmed identity outranks the signal outcome"
        );
    }

    /// Only a **zero** exit status from a local PowerShell `-Command` run is
    /// unverifiable: PowerShell does not propagate native exit codes and this
    /// path appends no `exit $LASTEXITCODE`. Everything else is evidence.
    #[test]
    fn exit_code_confidence_is_unknown_only_for_powershell_zero() {
        let ps = ExecSpec {
            command: "Get-Item /nope".into(),
            shell: Some(Shell::Powershell),
            ..Default::default()
        };
        let mut zero = ExecResult::success(String::new(), String::new(), "powershell");
        assert_eq!(
            local_exit_code_confidence(&ps, &zero),
            ExitCodeConfidence::Unknown
        );

        zero.exit_code = Some(1);
        assert_eq!(
            local_exit_code_confidence(&ps, &zero),
            ExitCodeConfidence::Observed,
            "a non-zero status is its own evidence"
        );

        let bash = ExecSpec {
            command: "echo hi".into(),
            shell: Some(Shell::Bash),
            ..Default::default()
        };
        let mut ok = ExecResult::success(String::new(), String::new(), "bash");
        ok.exit_code = Some(0);
        assert_eq!(
            local_exit_code_confidence(&bash, &ok),
            ExitCodeConfidence::Observed
        );

        let direct = ExecSpec {
            direct: Some(vec!["/bin/true".into()]),
            ..Default::default()
        };
        assert_eq!(
            local_exit_code_confidence(&direct, &zero),
            ExitCodeConfidence::Observed
        );
    }

    /// Platform differences, part 3: every recipe setter is guarded, because an
    /// unguarded `[Console]::OutputEncoding` throws "handle is invalid" when the
    /// process has no console — and the throw lands in the agent's stderr.
    /// Part 5: the recipe is applied to both PowerShell flavours, so a Windows
    /// host never sees the CLIXML/OEM default.
    #[test]
    fn powershell_recipe_guards_every_setter() {
        assert_eq!(
            POWERSHELL_UTF8_RECIPE.matches("try{").count(),
            2,
            "both setters must be try/catch guarded: {POWERSHELL_UTF8_RECIPE}"
        );
        assert_eq!(POWERSHELL_UTF8_RECIPE.matches("catch{}").count(), 2);
        assert!(POWERSHELL_UTF8_RECIPE.contains("$ProgressPreference='SilentlyContinue'"));
        assert!(POWERSHELL_UTF8_RECIPE.contains("[Console]::OutputEncoding=[Text.Encoding]::UTF8"));
        assert!(POWERSHELL_UTF8_RECIPE.contains("$OutputEncoding=[Text.Encoding]::UTF8"));

        for shell in [Shell::Powershell, Shell::Pwsh] {
            let spec = ExecSpec {
                command: "Write-Output hi".into(),
                shell: Some(shell),
                ..Default::default()
            };
            let argv = shell_argv(shell, &spec, "TOKEN");
            assert_eq!(argv[0], shell.as_str());
            assert_eq!(argv[1], "-NoProfile");
            assert_eq!(argv[2], "-Command");
            assert!(
                argv[3].starts_with(POWERSHELL_UTF8_RECIPE),
                "{} must get the recipe first: {}",
                shell.as_str(),
                argv[3]
            );
        }
    }

    /// A spawn failure is the one local case where nothing ran, so a caller may
    /// safely resubmit; every completed run reports `dispatched: true`.
    #[test]
    fn dispatched_reflects_whether_the_command_could_have_run() {
        let missing = ExecSpec {
            kind: crate::spec::ExecKind::Run,
            direct: Some(vec!["/nonexistent/unirun-probe-binary".into()]),
            ..Default::default()
        };
        let failed = run(&missing);
        assert!(
            !failed.dispatched,
            "nothing was handed to a shell: {failed:?}"
        );
        assert_eq!(failed.error_class.as_deref(), Some("COMMAND_NOT_FOUND"));

        let ok = run(&sh_ok("echo hi"));
        assert_eq!(ok.exit_code, Some(0));
        assert!(ok.dispatched);
        assert!(!ok.transport_error);
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
