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
    write_json(&dir.join("state.json"), &SessionState {
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
    })?;

    // The runner is this same binary re-exec'd. `UNIRUN_BIN` overrides the
    // executable path for embedding hosts and tests (where current_exe() is
    // the test binary, not unirun).
    let exe = std::env::var_os("UNIRUN_BIN")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_exe().unwrap_or_default());
    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("__bg-runner")
        .arg(&dir)
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
        shell: spec_rec
            .shell
            .as_deref()
            .and_then(Shell::from_name),
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

/// Read the current state of a session (with stale-detection).
pub fn status(id: &str) -> Result<SessionState, String> {
    let mut st = load_state(id)?;
    if st.status == "running" {
        if let Some(pid) = st.pid {
            if !pid_alive(pid) {
                st.status = "interrupted".into();
                st.finished_at = Some(now_millis());
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
pub fn kill(id: &str) -> Result<SessionState, String> {
    let st = status(id)?;
    if st.is_terminal() {
        return Ok(st);
    }
    let pid = st.pid.ok_or("session has no runner pid")?;
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
                if let Ok(mut st) = status(id) {
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
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("no such session `{}`: {}", id, e))?;
    serde_json::from_str(&text).map_err(|e| format!("session `{}` state corrupt: {}", id, e))
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
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }
    #[cfg(windows)]
    {
        // No FFI surface for OpenProcess without a windows-sys dependency:
        // treat unknown pids as alive so stale-detection degrades to
        // "running" (bg kill still works via taskkill /T /F).
        let _ = pid;
        true
    }
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
}
