//! Background sessions — detached command execution with a durable session
//! record agents can poll and inspect.
//!
//! Lifecycle:
//!   `session::start` writes `<sessions>/<id>/` (spec.json + empty logs +
//!   state.json "running"), then re-execs this same binary as a detached
//!   `__bg-runner` child (new session on POSIX via `setsid`, detached process
//!   group on Windows). The CLI parent exits immediately; the runner streams
//!   decoded output into `stdout.log`/`stderr.log` and writes a terminal
//!   `state.json` when done (completed / aborted / timed_out).
//!
//!   `bg kill` sends SIGTERM to the runner (POSIX), which treats it as an
//!   abort (exec's tree-kill runs); Windows force-kills via `taskkill /T /F`.
//!   A stale "running" state whose runner pid is gone is reported as
//!   `interrupted` (host crash / reboot).
//!
//! Storage root: `$UNIRUN_HOME/sessions` (default `~/.unirun/sessions`).

use crate::process_identity::{self, ExpectedIdentity, IdentityVerdict};
use crate::recipe::unirun_home;
use crate::spec::{ExecResult, ExecSpec, Shell};
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Hard cap on each log file: beyond this the runner stops appending and
/// flags `truncated_log` (disk-bounded; the terminal `ExecResult` in
/// state.json still carries the tail-kept output).
const LOG_CAP: u64 = 1024 * 1024;
/// How long `kill` waits for the runner to write a terminal state itself.
const KILL_WAIT: Duration = Duration::from_millis(3000);

/// Current session record. Written atomically (tmp + rename) by the runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub id: String,
    pub label: String,
    /// running | completed | aborted | timed_out | failed | killed | interrupted
    pub status: String,
    /// Runner process id (the process supervising the command tree).
    pub pid: Option<u32>,
    pub started_at: u64,
    pub finished_at: Option<u64>,
    pub exit_code: Option<i32>,
    pub error_class: Option<String>,
    pub hint: Option<String>,
    pub truncated: bool,
    pub truncated_log: bool,
    pub duration_ms: u64,
    pub encoding: String,
    pub shell_used: String,
}

/// Snapshot of the executed spec (for inspection; not an execution resume).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    pub command: String,
    pub shell: Option<String>,
    pub workdir: Option<String>,
    pub timeout_ms: u64,
}

/// The runner's process identity at spawn (sidecar `identity.json`), used by
/// `kill` to refuse killing a recycled pid.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIdentity {
    pub pid: u32,
    pub generation_token: String,
    pub start_epoch_ms: Option<u64>,
    pub token_observable: bool,
}

impl SessionState {
    pub fn is_terminal(&self) -> bool {
        self.status != "running"
    }
}

/// Sessions storage directory.
pub fn sessions_dir() -> PathBuf {
    unirun_home().join("sessions")
}

/// Start a command in the background. Returns the initial (running) state.
pub fn start(spec: &ExecSpec, label: &str) -> Result<SessionState, String> {
    let id = new_id();
    let dir = session_dir(&id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create session dir: {}", e))?;

    let spec_rec = SessionSpec {
        command: spec.command.clone(),
        shell: spec.shell.map(|s| s.as_str().to_string()),
        workdir: spec
            .workdir
            .as_ref()
            .map(|p| p.to_string_lossy().into_owned()),
        timeout_ms: spec.effective_timeout_ms(),
    };
    let spec_path = dir.join("spec.json");
    write_json(&spec_path, &spec_rec)?;
    write_json(
        &dir.join("state.json"),
        &SessionState {
            id: id.clone(),
            label: label.to_string(),
            status: "running".into(),
            pid: None,
            started_at: now_millis(),
            finished_at: None,
            exit_code: None,
            error_class: None,
            hint: None,
            truncated: false,
            truncated_log: false,
            duration_ms: 0,
            encoding: String::new(),
            shell_used: String::new(),
        },
    )?;

    // The runner is this same binary re-exec'd. `UNIRUN_BIN` overrides the
    // executable path for embedding hosts and tests (where current_exe() is
    // the test binary, not unirun).
    let exe = std::env::var_os("UNIRUN_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_exe().unwrap_or_default());
    // Generation token for the runner's own identity: `bg kill` verifies the
    // stored pid still carries it before signalling (anti pid-reuse).
    let generation_token = process_identity::generate_generation_token();
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("__bg-runner")
        .arg(&dir)
        .env(process_identity::GENERATION_TOKEN_ENV, &generation_token)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid(); // new session: survive parent exit, own process group
                Ok(())
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
    }
    let child = cmd
        .spawn()
        .map_err(|e| format!("cannot spawn background runner: {}", e))?;

    let mut st = load_state(&id).unwrap_or_else(|_| SessionState {
        id: id.clone(),
        label: label.to_string(),
        status: "running".into(),
        pid: None,
        started_at: now_millis(),
        finished_at: None,
        exit_code: None,
        error_class: None,
        hint: None,
        truncated: false,
        truncated_log: false,
        duration_ms: 0,
        encoding: String::new(),
        shell_used: String::new(),
    });
    st.pid = Some(child.id());
    write_json(&dir.join("state.json"), &st).map_err(|e| format!("cannot write state: {}", e))?;

    // Record the runner's identity at spawn so `kill` can refuse a recycled
    // pid. The runner's argv is `unirun __bg-runner <dir>` — the token lives
    // in its env, which unix exposes (`/proc`/`ps eww`) but Windows does not.
    let identity = SessionIdentity {
        pid: child.id(),
        generation_token: generation_token.clone(),
        start_epoch_ms: process_identity::read_start_epoch_ms(child.id()),
        token_observable: cfg!(not(windows)),
    };
    write_json(&dir.join("identity.json"), &identity)
        .map_err(|e| format!("cannot write identity: {}", e))?;
    Ok(st)
}

/// The detached runner entry point: load the spec, run with streaming output
/// into the log files, write the terminal state.
pub fn run_runner(session_dir: &Path) -> i32 {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGTERM, on_sigterm as *const () as libc::sighandler_t);
    }
    let id = session_dir
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?")
        .to_string();
    let spec_path = session_dir.join("spec.json");
    let spec_rec: SessionSpec = match std::fs::read_to_string(&spec_path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
    {
        Some(s) => s,
        None => {
            eprintln!("unirun __bg-runner: cannot read {}", spec_path.display());
            return 1;
        }
    };
    let spec = ExecSpec {
        command: spec_rec.command.clone(),
        kind: crate::spec::ExecKind::Run,
        shell: spec_rec.shell.as_deref().and_then(Shell::from_name),
        workdir: spec_rec.workdir.as_ref().map(PathBuf::from),
        timeout_ms: spec_rec.timeout_ms,
        ..Default::default()
    };

    let (tx, rx) = std::sync::mpsc::channel::<crate::exec::StreamChunk>();
    let log_dir = session_dir.to_path_buf();
    let drain = std::thread::spawn(move || {
        let mut so = open_append(log_dir.join("stdout.log"));
        let mut se = open_append(log_dir.join("stderr.log"));
        let mut so_bytes: u64 = 0;
        let mut se_bytes: u64 = 0;
        let mut truncated_log = false;
        for chunk in rx {
            let (f, bytes) = match chunk.stream {
                crate::exec::StreamKind::Stdout => (&mut so, &mut so_bytes),
                crate::exec::StreamKind::Stderr => (&mut se, &mut se_bytes),
            };
            if *bytes < LOG_CAP {
                let room = LOG_CAP - *bytes;
                let text: String = chunk.text.chars().take(room as usize).collect();
                if let Some(f) = f {
                    let _ = f.write_all(text.as_bytes());
                }
                *bytes += text.len() as u64;
                if *bytes >= LOG_CAP {
                    truncated_log = true;
                }
            } else {
                truncated_log = true;
            }
        }
        truncated_log
    });

    let result = crate::exec::run_streaming(&spec, tx);
    let truncated_log = drain.join().unwrap_or(true);

    let mut st = SessionState {
        id,
        label: String::new(),
        status: status_of(&result),
        pid: Some(std::process::id()),
        started_at: now_millis().saturating_sub(result.duration_ms),
        finished_at: Some(now_millis()),
        exit_code: result.exit_code,
        error_class: result.error_class.clone(),
        hint: result.hint.clone(),
        truncated: result.truncated,
        truncated_log,
        duration_ms: result.duration_ms,
        encoding: result.encoding.clone(),
        shell_used: result.shell_used.clone(),
    };
    if let Some(prev) = std::fs::read_to_string(session_dir.join("state.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<SessionState>(&t).ok())
    {
        st.label = prev.label;
        st.started_at = prev.started_at;
        if st.pid.is_none() {
            st.pid = prev.pid;
        }
    }
    let _ = write_json(&session_dir.join("state.json"), &st);
    0
}

fn status_of(r: &ExecResult) -> String {
    if r.timed_out {
        "timed_out".into()
    } else if r.aborted {
        "aborted".into()
    } else if r.exit_code == Some(0) {
        "completed".into()
    } else if r.error_class.is_some() {
        "failed".into()
    } else {
        "completed".into()
    }
}

#[cfg(unix)]
extern "C" fn on_sigterm(_: libc::c_int) {
    crate::exec::signal_abort();
}

/// Grace period for a runner's terminal state to become visible after its pid
/// is gone, and the poll step used while waiting for it.
///
/// The runner publishes `state.json` itself before exiting, so a dead pid with
/// a still-`running` state is either (a) that write in flight or (b) a hard
/// crash. The window is not academic: the liveness probe shells out to `ps` on
/// macOS, which is wide enough for a fast command to finish and publish while
/// `status()` is still deciding — that race reported a successful background
/// command as `interrupted` (caught on macOS CI by
/// `tests/mcp.rs::mcp_session_start_wait_output`).
const RUNNER_SETTLE_GRACE: Duration = Duration::from_millis(1_000);
const RUNNER_SETTLE_POLL: Duration = Duration::from_millis(20);

/// Read the current state of a session (with stale-detection).
pub fn status(id: &str) -> Result<SessionState, String> {
    status_with_settle(id, true)
}

/// `settle = false` is the cheap path used by `list()`: it still re-reads
/// before writing (so it can never clobber a terminal state) but never sleeps,
/// keeping `bg list` fast no matter how many interrupted sessions exist.
fn status_with_settle(id: &str, settle: bool) -> Result<SessionState, String> {
    let mut st = load_state(id)?;
    if st.status == "running" {
        if let Some(pid) = st.pid {
            if !pid_alive(pid) {
                let deadline = Instant::now() + RUNNER_SETTLE_GRACE;
                loop {
                    let fresh = load_state(id)?;
                    if fresh.is_terminal() {
                        // The runner won the race: its verdict is authoritative.
                        return Ok(fresh);
                    }
                    st = fresh;
                    if !settle || Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(RUNNER_SETTLE_POLL);
                }
                st.status = "interrupted".into();
                let finished_at = now_millis();
                st.finished_at = Some(finished_at);
                st.duration_ms = finished_at.saturating_sub(st.started_at);
                let _ = write_json(&session_dir(id).join("state.json"), &st);
            }
        }
    }
    Ok(st)
}

/// stdout/stderr log tails for a session. Returns `(stdout, stderr, truncated_log)`.
pub fn output(id: &str, tail_bytes: usize) -> Result<(String, String, bool), String> {
    let st = status(id)?;
    let dir = session_dir(id);
    let so = read_tail(&dir.join("stdout.log"), tail_bytes);
    let se = read_tail(&dir.join("stderr.log"), tail_bytes);
    Ok((so, se, st.truncated_log))
}

/// Kill a running session: SIGTERM the runner (POSIX) / taskkill the tree
/// (Windows), wait briefly for the runner to record a terminal state, then
/// force-mark `killed` if it did not.
///
/// Before signalling, the stored runner identity (generation token + start
/// epoch, captured at spawn) is verified: a stale session whose runner pid
/// was recycled must not kill an innocent process — the kill is refused with
/// a `PID_REUSED` classification. Sessions started before identity tracking
/// (no `identity.json`) keep the legacy unverified behavior.
pub fn kill(id: &str) -> Result<SessionState, String> {
    let mut st = status(id)?;
    if st.is_terminal() {
        return Ok(st);
    }
    let pid = st.pid.ok_or("session has no runner pid")?;

    if let Some(identity) = load_identity(id)? {
        let expected = ExpectedIdentity {
            pid,
            generation_token: identity.generation_token,
            start_epoch_ms: identity.start_epoch_ms,
            token_observable: identity.token_observable,
        };
        let verdict =
            process_identity::verify(&expected, process_identity::IDENTITY_EPOCH_TOLERANCE_MS);
        match &verdict {
            IdentityVerdict::Matches => {}
            IdentityVerdict::NotRunning => {
                // Runner already gone (exited or zombie): mark interrupted.
                st.status = "interrupted".into();
                let finished_at = now_millis();
                st.finished_at = Some(finished_at);
                st.duration_ms = finished_at.saturating_sub(st.started_at);
                write_json(&session_dir(id).join("state.json"), &st)?;
                return Ok(st);
            }
            IdentityVerdict::StartEpochMismatch { .. } | IdentityVerdict::TokenMismatch => {
                return Err(format!(
                    "refusing to kill session {}: PID_REUSED — {}",
                    id,
                    verdict.describe()
                ));
            }
            IdentityVerdict::Unverifiable => {
                // Fail closed: the runner pid exists but nothing ties it to the
                // stored identity (no start epoch, token not observable), so
                // signalling it could hit an innocent process.
                return Err(format!(
                    "refusing to kill session {}: IDENTITY_UNVERIFIABLE — {}",
                    id,
                    verdict.describe()
                ));
            }
        }
    }

    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
    #[cfg(windows)]
    {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }
    let deadline = Instant::now() + KILL_WAIT;
    loop {
        if let Ok(cur) = load_state(id) {
            if cur.is_terminal() {
                return Ok(cur);
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // The runner did not write a terminal state in time: mark it ourselves.
    let mut final_st = load_state(id)?;
    final_st.status = "killed".into();
    final_st.finished_at = Some(now_millis());
    write_json(&session_dir(id).join("state.json"), &final_st)?;
    Ok(final_st)
}

/// List all sessions, newest first.
pub fn list() -> Vec<SessionState> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(sessions_dir()) {
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(id) = path.file_name().and_then(|s| s.to_str()) {
                if let Ok(mut st) = status_with_settle(id, false) {
                    // re-check staleness was handled by status()
                    let _ = &mut st;
                    out.push(st);
                }
            }
        }
    }
    out.sort_by_key(|s| std::cmp::Reverse(s.started_at));
    out
}

/// Poll until the session reaches a terminal state (or the timeout elapses).
pub fn wait(id: &str, timeout_ms: u64) -> Result<SessionState, String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        let st = status(id)?;
        if st.is_terminal() {
            return Ok(st);
        }
        if Instant::now() >= deadline {
            return Ok(st);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

// --- internals ---

fn session_dir(id: &str) -> PathBuf {
    sessions_dir().join(id)
}

fn load_state(id: &str) -> Result<SessionState, String> {
    let path = session_dir(id).join("state.json");
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("no such session `{}`: {}", id, e))?;
    serde_json::from_str(&text).map_err(|e| format!("session `{}` state corrupt: {}", id, e))
}

/// The runner identity sidecar, if present (sessions started before identity
/// tracking have none and keep the legacy kill behavior).
fn load_identity(id: &str) -> Result<Option<SessionIdentity>, String> {
    let path = session_dir(id).join("identity.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return Ok(None),
    };
    serde_json::from_str(&text)
        .map(Some)
        .map_err(|e| format!("session `{}` identity corrupt: {}", id, e))
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), String> {
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string(value).map_err(|e| e.to_string())?;
    std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

fn open_append(path: PathBuf) -> Option<std::fs::File> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .ok()
}

fn read_tail(path: &Path, tail_bytes: usize) -> String {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return String::new(),
    };
    if bytes.len() <= tail_bytes {
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    // Trim to a char boundary so the tail never starts mid-codepoint.
    let mut start = bytes.len() - tail_bytes;
    while start < bytes.len() && (bytes[start] & 0xC0) == 0x80 {
        start += 1;
    }
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!(
        "{:x}{:x}{:x}",
        std::process::id(),
        t,
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn pid_alive(pid: u32) -> bool {
    // Zombie-aware: a zombie has already exited and its pid is one step from
    // reuse, so it must not keep a session marked "running".
    process_identity::is_alive(pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("unirun-sess-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn read_tail_keeps_char_boundary() {
        let path = std::env::temp_dir().join(format!("unirun-tail-{}", std::process::id()));
        std::fs::write(&path, "abcdef中文".as_bytes()).unwrap();
        let t = read_tail(&path, 4);
        assert!(t.ends_with("文"), "tail: {:?}", t);
        assert!(!t.contains('\u{FFFD}'));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn status_of_maps_exec_result() {
        let mut r = ExecResult::success(String::new(), String::new(), "bash");
        assert_eq!(status_of(&r), "completed");
        r.exit_code = Some(3);
        r.error_class = Some("NOT_FOUND".into());
        assert_eq!(status_of(&r), "failed");
        r.timed_out = true;
        assert_eq!(status_of(&r), "timed_out");
        r.timed_out = false;
        r.aborted = true;
        assert_eq!(status_of(&r), "aborted");
    }

    #[test]
    fn sessions_dir_uses_unirun_home() {
        let _guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = temp_home("dir");
        std::env::set_var("UNIRUN_HOME", &home);
        assert_eq!(sessions_dir(), home.join("sessions"));
        std::env::remove_var("UNIRUN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn kill_refuses_recycled_identity() {
        // Needs the platform probe (ps/CIM) to see our own process; without it
        // the identity reads as `NotRunning` and this test would assert on the
        // wrong arm. Skip instead of failing for a missing capability.
        if !process_identity::platform_probe_available() {
            eprintln!("skipping: this environment cannot report process identities");
            return;
        }
        let _guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = temp_home("killreuse");
        std::env::set_var("UNIRUN_HOME", &home);
        // Fake a "running" session whose identity points at OUR live process
        // with a foreign token: kill must refuse (PID_REUSED) instead of
        // signalling an innocent pid.
        let id = "fakesession".to_string();
        let dir = session_dir(&id);
        std::fs::create_dir_all(&dir).unwrap();
        let state = SessionState {
            id: id.clone(),
            label: "fake".into(),
            status: "running".into(),
            pid: Some(std::process::id()),
            started_at: now_millis(),
            finished_at: None,
            exit_code: None,
            error_class: None,
            hint: None,
            truncated: false,
            truncated_log: false,
            duration_ms: 0,
            encoding: String::new(),
            shell_used: String::new(),
        };
        write_json(&dir.join("state.json"), &state).unwrap();
        write_json(
            &dir.join("identity.json"),
            &SessionIdentity {
                pid: std::process::id(),
                generation_token: "ur-not-ours".into(),
                start_epoch_ms: None,
                token_observable: true,
            },
        )
        .unwrap();
        let err = kill(&id).unwrap_err();
        assert!(err.contains("PID_REUSED"), "err: {}", err);
        // The innocent process must still be alive (kill was refused).
        assert!(process_identity::is_alive(std::process::id()));
        std::env::remove_var("UNIRUN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn kill_marks_not_running_session_interrupted() {
        let _guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = temp_home("killgone");
        std::env::set_var("UNIRUN_HOME", &home);
        let id = "gonesession".to_string();
        let dir = session_dir(&id);
        std::fs::create_dir_all(&dir).unwrap();
        let state = SessionState {
            id: id.clone(),
            label: "fake".into(),
            status: "running".into(),
            pid: Some(999_999_999), // certainly gone
            started_at: now_millis().saturating_sub(1_000),
            finished_at: None,
            exit_code: None,
            error_class: None,
            hint: None,
            truncated: false,
            truncated_log: false,
            duration_ms: 0,
            encoding: String::new(),
            shell_used: String::new(),
        };
        write_json(&dir.join("state.json"), &state).unwrap();
        let st = kill(&id).unwrap();
        assert_eq!(st.status, "interrupted", "state: {:?}", st);
        assert!(st.finished_at.is_some());
        assert!(st.duration_ms > 0);
        std::env::remove_var("UNIRUN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Regression: the runner publishes its own terminal state, and
    /// stale-detection used to overwrite it with `interrupted` whenever the
    /// runner finished during the liveness probe (macOS `ps` made that window
    /// wide enough to lose reliably). A terminal write that lands while
    /// `status` is waiting must win, both in the returned value and on disk.
    #[test]
    fn status_keeps_a_terminal_state_that_lands_during_stale_check() {
        let _guard = crate::ENV_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = temp_home("settle");
        std::env::set_var("UNIRUN_HOME", &home);
        let id = "settlesession".to_string();
        let dir = session_dir(&id);
        std::fs::create_dir_all(&dir).unwrap();

        // A "running" session whose runner pid is certainly gone.
        let running = SessionState {
            id: id.clone(),
            label: "fake".into(),
            status: "running".into(),
            pid: Some(999_999_999),
            started_at: now_millis().saturating_sub(1_000),
            finished_at: None,
            exit_code: None,
            error_class: None,
            hint: None,
            truncated: false,
            truncated_log: false,
            duration_ms: 0,
            encoding: String::new(),
            shell_used: String::new(),
        };
        write_json(&dir.join("state.json"), &running).unwrap();

        // Stand in for the runner: publish the real verdict shortly after
        // stale-detection has already seen the dead pid.
        let mut completed = running.clone();
        completed.status = "completed".into();
        completed.exit_code = Some(0);
        completed.encoding = "utf-8".into();
        let writer_dir = dir.clone();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            completed.finished_at = Some(now_millis());
            write_json(&writer_dir.join("state.json"), &completed).unwrap();
        });

        let st = status(&id).unwrap();
        writer.join().unwrap();

        assert_eq!(
            st.status, "completed",
            "stale-detection clobbered the runner's verdict: {:?}",
            st
        );
        assert_eq!(st.exit_code, Some(0));
        assert_eq!(
            load_state(&id).unwrap().status,
            "completed",
            "on-disk record must keep the runner's verdict"
        );

        std::env::remove_var("UNIRUN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
