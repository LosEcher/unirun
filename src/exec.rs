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
use crate::taxonomy::classify;
use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Run a command/script and return the normalized result.
pub fn run(spec: &ExecSpec) -> ExecResult {
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
    let stdout_thread = child
        .stdout
        .take()
        .map(|s| thread::spawn(move || read_capped(s, max)));
    let stderr_thread = child
        .stderr
        .take()
        .map(|s| thread::spawn(move || read_capped(s, max)));

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
                if ABORT.load(Ordering::SeqCst) {
                    aborted = true;
                    kill_tree(&mut child, grace);
                    let _ = child.wait();
                    break;
                }
                if start.elapsed() >= timeout {
                    timed_out = true;
                    kill_tree(&mut child, grace);
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
    let stdout = decode(&stdout_raw);
    let stderr = decode(&stderr_raw);

    let mut result = ExecResult {
        exit_code,
        signal,
        stdout: stdout.text,
        stderr: stderr.text,
        timed_out,
        aborted,
        duration_ms: start.elapsed().as_millis() as u64,
        error_class: None,
        hint: None,
        encoding: stdout.encoding.to_string(),
        truncated: stdout_trunc || stderr_trunc,
        shell_used: shell,
    };
    let (class, hint) = classify(&result);
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
    if which("bash").is_some() {
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
            vec![
                shell.as_str().to_string(),
                "-NoProfile".into(),
                "-Command".into(),
                spec.command.clone(),
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
/// actually need.
fn read_capped<R: Read>(mut reader: R, max: usize) -> Captured {
    let mut tail: Vec<u8> = Vec::with_capacity(max.saturating_add(8192));
    let mut total: usize = 0;
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
            }
            Err(_) => break,
        }
    }
    Captured {
        bytes: tail,
        truncated: total > max,
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
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
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
