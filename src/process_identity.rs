//! Process identity validation — prevent killing a reused pid.
//!
//! Borrowed from grok-bot's `local-exec-native` (防 pid 复用误杀): a pid alone
//! is not an identity — the kernel recycles pids, so a stale handle can end
//! up pointing at an innocent process. Before terminating a process tree we
//! verify the target is still *the process we spawned*:
//!
//! - every spawned child receives a random **generation token**, embedded in
//!   its command text (argv-visible on every platform, including Windows
//!   `CommandLine`) and its environment (inherited by the whole tree);
//! - the child's **start epoch** is snapshotted right after spawn;
//! - at kill time the observed identity must carry the token (when it is
//!   observable on this platform) and match the snapshotted start epoch —
//!   otherwise the pid was reused and the kill is refused (classified
//!   `PID_REUSED`). When *neither* signal is available the verdict is
//!   `Unverifiable` (`IDENTITY_UNVERIFIABLE`): a probe-less environment must
//!   not be mistaken for a verified identity.
//!
//! Zombies are not alive: a process in `Z` state has already exited and its
//! pid is one step from reuse, so it is treated as gone (nothing to kill).

use crate::spec::Shell;
use std::process::Command;

/// Env var carrying the generation token to the child (inherited tree-wide;
/// readable on unix via `/proc/<pid>/environ` or `ps eww`).
pub const GENERATION_TOKEN_ENV: &str = "UNIRUN_GENERATION_TOKEN";
/// Assignment prefix used when the token is embedded in shell command text
/// (argv-visible on every platform, including Windows `CommandLine`).
const TOKEN_ASSIGN_PREFIX: &str = "UNIRUN_GENERATION_TOKEN=";

/// Start-epoch comparison tolerance (ms). Sources are second-granular on some
/// platforms (`ps lstart`), so a small skew between two reads of the *same*
/// process must not false-positive.
pub const IDENTITY_EPOCH_TOLERANCE_MS: u64 = 2_000;

/// Observed identity of a live process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub start_epoch_ms: u64,
    /// argv (NUL/space-joined) plus, where the platform exposes it, the
    /// environment — so `command_carries_token` can find the token.
    pub command: String,
}

/// What we expect the pid to refer to at kill time.
#[derive(Debug, Clone)]
pub struct ExpectedIdentity {
    pub pid: u32,
    pub generation_token: String,
    /// Start epoch observed right after spawn; `None` when the snapshot
    /// failed (child exited instantly). When `None` only the token is checked.
    pub start_epoch_ms: Option<u64>,
    /// Whether the token is observable in the target's command on this
    /// platform for this kind of spawn. Direct-argv spawns on Windows cannot
    /// be verified by token (their env is not exposed) — the epoch check
    /// still applies.
    pub token_observable: bool,
}

/// Why a kill was (or was not) authorized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityVerdict {
    /// The pid is exactly the process we spawned — safe to kill.
    Matches,
    /// Not running (gone, zombie, or unreadable) — nothing to kill.
    NotRunning,
    /// The pid now belongs to a different process (start epoch differs).
    StartEpochMismatch { observed: u64, expected: u64 },
    /// Running, but its command does not carry our generation token.
    TokenMismatch,
    /// The pid exists, but this environment could observe neither a start
    /// epoch (the probe failed at spawn) nor the generation token (not
    /// observable for this kind of spawn), so nothing ties the pid to our run.
    /// Callers must fail closed: never signal a pid we cannot identify.
    Unverifiable,
}

impl IdentityVerdict {
    /// Human-readable description used in kill refusals and hints.
    pub fn describe(&self) -> String {
        match self {
            IdentityVerdict::Matches => "process identity matches the spawned process".to_string(),
            IdentityVerdict::NotRunning => "process is not running (exited or zombie)".to_string(),
            IdentityVerdict::StartEpochMismatch { observed, expected } => format!(
                "process start epoch {} does not match expected {} — the pid was reused",
                observed, expected
            ),
            IdentityVerdict::TokenMismatch => {
                "process command does not carry the run's generation token — the pid was reused"
                    .to_string()
            }
            IdentityVerdict::Unverifiable => {
                "process identity is unverifiable here: no start epoch was captured and the \
                 command does not expose the generation token"
                    .to_string()
            }
        }
    }
}

/// A fresh random-looking generation token. Not cryptographic: it only needs
/// to be unique across process instances on this host.
pub fn generate_generation_token() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mix = counter.wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(17);
    format!(
        "ur{:016x}{:016x}",
        nanos ^ mix,
        pid ^ counter.rotate_left(31)
    )
}

/// Wrap a shell command so the generation token appears in the child's argv
/// (visible on every platform, including Windows `CommandLine`) and — via the
/// assignment — in the child's environment, inherited by the whole tree:
///
/// - POSIX shells:  `export UNIRUN_GENERATION_TOKEN=<tok>; <cmd>`
/// - PowerShell:    `$env:UNIRUN_GENERATION_TOKEN='<tok>'; <cmd>` — the value
///   **must** be quoted. PowerShell parses the right-hand side of `=` as a
///   *statement*, so a bare word is run as a command: `$env:X=ur7` executes
///   `ur7`, prints "not recognized as the name of a cmdlet" on stderr and
///   leaves the variable unset. Numeric literals happen to work, so only
///   unquoted word-like tokens (ours) expose it. Verified on Windows
///   PowerShell 5.1 for `-Command`, script blocks and `.ps1` files alike.
/// - cmd.exe:       `set UNIRUN_GENERATION_TOKEN=<tok>&& <cmd>`
///
/// The assignment only (re)sets the same env var unirun already passes to the
/// child, so execution semantics are unchanged.
pub fn inject_generation_token(shell: Shell, command: &str, token: &str) -> String {
    match shell {
        Shell::Cmd => format!("set {}{}&& {}", TOKEN_ASSIGN_PREFIX, token, command),
        Shell::Powershell | Shell::Pwsh => {
            format!("$env:{}'{}'; {}", TOKEN_ASSIGN_PREFIX, token, command)
        }
        _ => format!("export {}{}; {}", TOKEN_ASSIGN_PREFIX, token, command),
    }
}

/// True when `command` carries `UNIRUN_GENERATION_TOKEN=<token>` — optionally
/// quoted (`UNIRUN_GENERATION_TOKEN='<token>'`, the PowerShell spelling) — with
/// non-alphanumeric neighbors. The injected token can be surrounded by shell
/// syntax (`;`, `&`, `'`, whitespace), so the boundary rule only guards the
/// real false-positive: a longer token value (`ur1234`) must not match a
/// shorter one (`ur123`). A recycled pid would have to reproduce the whole
/// random token, quoted or not.
pub fn command_carries_token(command: &str, token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    let mut offset = 0;
    while let Some(pos) = command[offset..].find(TOKEN_ASSIGN_PREFIX) {
        let abs = offset + pos;
        let value_start = abs + TOKEN_ASSIGN_PREFIX.len();
        let before_ok = abs == 0
            || !command[..abs]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_ascii_alphanumeric());
        // Shells that need a string expression quote the value; skip one quote
        // so the identical token matches in either spelling.
        let token_start = match command[value_start..].chars().next() {
            Some(quote @ ('\'' | '"')) => value_start + quote.len_utf8(),
            _ => value_start,
        };
        if before_ok && command[token_start..].starts_with(token) {
            let end = token_start + token.len();
            let after_ok = end == command.len()
                || !command[end..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphanumeric());
            if after_ok {
                return true;
            }
        }
        offset = value_start;
    }
    false
}

/// True when the process exists and is not a zombie.
pub fn is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        let signalable = unsafe { libc::kill(pid as i32, 0) } == 0
            || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
        if !signalable {
            return false;
        }
        !is_zombie(pid)
    }
    #[cfg(windows)]
    {
        // Windows has no signal-0 probe. Without this branch every non-zero
        // pid counted as alive, so a crashed background session never settled
        // as `interrupted` and `kill` saw no reason to stop. The process list
        // is the probe (one PowerShell call, the same cost macOS already pays
        // for `ps`).
        read_windows_identity(pid).is_some()
    }
}

/// True when this environment can report process identities at all (start
/// epoch and command). The probe shells out to `ps`/CIM, which some sandboxes
/// and stripped-down containers block; the identity tests skip when it is
/// unavailable instead of failing for a missing capability. Note that a
/// `false` result also makes `verify` return `Unverifiable` for spawns whose
/// token is not observable, which is a fail-closed refusal, not a silent match.
#[cfg(test)]
pub(crate) fn platform_probe_available() -> bool {
    read_start_epoch_ms(std::process::id()).is_some()
}

/// True when the process exists but has already exited (POSIX zombie).
/// Zombies are not alive: they cannot be signalled meaningfully and their
/// pid is one step from reuse.
pub fn is_zombie(pid: u32) -> bool {
    #[cfg(unix)]
    {
        process_state_char(pid) == Some('Z')
    }
    #[cfg(windows)]
    {
        // Win32 has no zombie state: a dead process is simply gone.
        let _ = pid;
        false
    }
}

/// Start epoch (ms since the unix epoch) of `pid`, or `None` when the process
/// does not exist / is unreadable / is a zombie.
pub fn read_start_epoch_ms(pid: u32) -> Option<u64> {
    #[cfg(unix)]
    {
        read_start_epoch_unix(pid)
    }
    #[cfg(windows)]
    {
        read_windows_identity(pid).map(|id| id.start_epoch_ms)
    }
}

/// Read the full observable identity of `pid`. `None` when the process does
/// not exist, is a zombie, or cannot be queried.
pub fn read_process_identity(pid: u32) -> Option<ProcessIdentity> {
    #[cfg(unix)]
    {
        if !is_alive(pid) {
            return None;
        }
        let start_epoch_ms = read_start_epoch_unix(pid)?;
        let command = read_command(pid).unwrap_or_default();
        Some(ProcessIdentity {
            pid,
            start_epoch_ms,
            command,
        })
    }
    #[cfg(windows)]
    {
        // `read_windows_identity` is itself the liveness probe on Windows;
        // gating it behind `is_alive` first would shell out twice per read.
        read_windows_identity(pid)
    }
}

/// Verify that `pid` still refers to the process we spawned.
pub fn verify(expected: &ExpectedIdentity, tolerance_ms: u64) -> IdentityVerdict {
    verify_identity(expected, read_process_identity(expected.pid), tolerance_ms)
}

/// Pure decision over an observed identity (kept separate so the verdict
/// logic is unit-testable without real processes).
pub fn verify_identity(
    expected: &ExpectedIdentity,
    observed: Option<ProcessIdentity>,
    tolerance_ms: u64,
) -> IdentityVerdict {
    let Some(observed) = observed else {
        return IdentityVerdict::NotRunning;
    };
    if let Some(expected_epoch) = expected.start_epoch_ms {
        let diff = observed.start_epoch_ms.abs_diff(expected_epoch);
        if diff > tolerance_ms {
            return IdentityVerdict::StartEpochMismatch {
                observed: observed.start_epoch_ms,
                expected: expected_epoch,
            };
        }
    }
    if expected.token_observable {
        if !command_carries_token(&observed.command, &expected.generation_token) {
            return IdentityVerdict::TokenMismatch;
        }
    } else if expected.start_epoch_ms.is_none() {
        // The token is not observable for this kind of spawn (Windows direct
        // argv, Windows background sessions) and no start epoch was captured
        // either — because the probe failed at spawn. There is nothing left to
        // compare, so say so instead of reporting a match: the caller must not
        // signal a pid it cannot tie to this run.
        return IdentityVerdict::Unverifiable;
    }
    IdentityVerdict::Matches
}

// --- platform identity queries ---

#[cfg(unix)]
fn read_start_epoch_unix(pid: u32) -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        if let Some(epoch) = read_proc_start_epoch(pid) {
            return Some(epoch);
        }
    }
    read_ps_start_epoch(pid)
}

#[cfg(target_os = "linux")]
fn read_proc_start_epoch(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let after = stat.rsplit(')').next()?;
    // A zombie has no usable identity: its pid is one step from reuse.
    if after.split_whitespace().next() == Some("Z") {
        return None;
    }
    let start_ticks = starttime_ticks_from_stat(&stat)?;
    let btime_sec = btime_secs()?;
    let clk = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let clk = if clk <= 0 { 100 } else { clk as u64 };
    Some(
        btime_sec
            .saturating_mul(1000)
            .saturating_add(start_ticks.saturating_mul(1000) / clk.max(1)),
    )
}

#[cfg(target_os = "linux")]
fn btime_secs() -> Option<u64> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    for line in stat.lines() {
        if let Some(rest) = line.strip_prefix("btime ") {
            return rest.trim().parse().ok();
        }
    }
    None
}

/// `/proc/<pid>/stat`: `pid (comm) state ppid ... starttime` — starttime is
/// field 22 overall, i.e. the 20th whitespace token after the `(comm)` part.
#[cfg(target_os = "linux")]
fn starttime_ticks_from_stat(stat: &str) -> Option<u64> {
    let after = stat.rsplit(')').next()?;
    let mut fields = after.split_whitespace();
    fields.next()?; // state
    for _ in 0..18 {
        fields.next()?; // ppid .. itrealvalue
    }
    fields.next()?.parse().ok()
}

#[cfg(unix)]
fn read_ps_start_epoch(pid: u32) -> Option<u64> {
    // One call for both the start time and the state (a zombie has no usable
    // identity — its pid is one step from reuse).
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart=", "-o", "state="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    // "Tue Aug 20 12:00:00 2026 S" — the lstart field is fixed-width 24 chars.
    if text.len() < 25 {
        return None;
    }
    let state = text[24..].trim();
    if state.starts_with('Z') {
        return None;
    }
    parse_lstart_epoch(&text[..24])
}

/// Parse `ps lstart` ("Tue Aug 20 12:00:00 2026", local time) into epoch ms.
#[cfg(unix)]
fn parse_lstart_epoch(lstart: &str) -> Option<u64> {
    let mut it = lstart.split_whitespace();
    let _dow = it.next()?;
    let mon = month_index(it.next()?)?;
    let mday: i32 = it.next()?.parse().ok()?;
    let time = it.next()?;
    let year: i32 = it.next()?.parse().ok()?;
    let mut hms = time.split(':');
    let hour: i32 = hms.next()?.parse().ok()?;
    let min: i32 = hms.next()?.parse().ok()?;
    let sec: i32 = hms.next()?.parse().ok()?;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = year - 1900;
    tm.tm_mon = mon;
    tm.tm_mday = mday;
    tm.tm_hour = hour;
    tm.tm_min = min;
    tm.tm_sec = sec;
    tm.tm_isdst = -1; // let the local zone decide
    let t = unsafe { libc::mktime(&mut tm) };
    if t < 0 {
        return None;
    }
    Some(t as u64 * 1000)
}

#[cfg(unix)]
fn month_index(name: &str) -> Option<i32> {
    match name {
        "Jan" => Some(0),
        "Feb" => Some(1),
        "Mar" => Some(2),
        "Apr" => Some(3),
        "May" => Some(4),
        "Jun" => Some(5),
        "Jul" => Some(6),
        "Aug" => Some(7),
        "Sep" => Some(8),
        "Oct" => Some(9),
        "Nov" => Some(10),
        "Dec" => Some(11),
        _ => None,
    }
}

#[cfg(unix)]
fn process_state_char(pid: u32) -> Option<char> {
    #[cfg(target_os = "linux")]
    {
        if let Ok(stat) = std::fs::read_to_string(format!("/proc/{}/stat", pid)) {
            if let Some(after) = stat.rsplit(')').next() {
                if let Some(state) = after.split_whitespace().next() {
                    return state.chars().next();
                }
            }
        }
    }
    let out = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "state="])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().chars().next()
}

#[cfg(unix)]
fn read_command(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let mut parts = Vec::new();
        if let Ok(cmdline) = std::fs::read(format!("/proc/{}/cmdline", pid)) {
            parts.push(nul_join(&cmdline));
        }
        if let Ok(environ) = std::fs::read(format!("/proc/{}/environ", pid)) {
            parts.push(nul_join(&environ));
        }
        let joined = parts.join(" ");
        if !joined.trim().is_empty() {
            return Some(joined);
        }
    }
    // Fallback (macOS and other unix without /proc): `ps eww` prints the
    // command line followed by the environment (verified on macOS).
    let out = Command::new("ps")
        .args(["eww", "-p", &pid.to_string()])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    if text.is_empty() {
        None
    } else {
        Some(text.to_string())
    }
}

#[cfg(target_os = "linux")]
fn nul_join(bytes: &[u8]) -> String {
    let mut s = String::new();
    for part in bytes.split(|b| *b == 0) {
        if part.is_empty() {
            continue;
        }
        if !s.is_empty() {
            s.push(' ');
        }
        s.push_str(&String::from_utf8_lossy(part));
    }
    s
}

#[cfg(windows)]
fn read_windows_identity(pid: u32) -> Option<ProcessIdentity> {
    // Emit epoch milliseconds directly. `CreationDate` is a PowerShell
    // `DateTime`, and its JSON rendering is not the raw CIM string this code
    // used to parse: Windows PowerShell 5.1 (what windows-latest runs) emits
    // `/Date(1791162553132)/`, so every Windows read returned `None` and the
    // identity/liveness layer silently degraded. `CommandLine` can also be
    // null for some processes, which must not read as "process gone".
    let script = format!(
        "$p=Get-CimInstance Win32_Process -Filter 'ProcessId = {}'; \
         if ($null -ne $p) {{ \
         $ms=[int64](($p.CreationDate.ToUniversalTime() - [datetime]'1970-01-01').TotalMilliseconds); \
         @{{EpochMs=$ms;CommandLine=$p.CommandLine}} | ConvertTo-Json -Compress }}",
        pid
    );
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    let start_epoch_ms = match v.get("EpochMs").and_then(|n| n.as_i64()) {
        Some(ms) if ms >= 0 => ms as u64,
        // Older/alternate renderings still carry the creation stamp.
        _ => parse_creation_date(v.get("CreationDate").and_then(|s| s.as_str())?)?,
    };
    let command = v
        .get("CommandLine")
        .and_then(|s| s.as_str())
        .unwrap_or_default()
        .trim()
        .to_string();
    Some(ProcessIdentity {
        pid,
        start_epoch_ms,
        command,
    })
}

/// Parse a creation stamp in either shape it reaches us: the raw CIM form
/// (`20260820120000.000000+480`) or PowerShell's JSON rendering of a
/// `DateTime` (`/Date(1791162553132)/`, Windows PowerShell 5.1).
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_creation_date(s: &str) -> Option<u64> {
    match s.strip_prefix("/Date(").and_then(|r| r.strip_suffix(")/")) {
        // Milliseconds since the epoch, optionally followed by an offset.
        Some(inner) => inner
            .split(['+', '-'])
            .next()
            .unwrap_or(inner)
            .trim()
            .parse::<i64>()
            .ok()
            .filter(|n| *n >= 0)
            .map(|n| n as u64),
        // The raw CIM form (`20260820120000.000000+480`).
        None => parse_wmi_datetime(s),
    }
}

/// Parse a WMI CIM datetime (`20260820120000.000000+480`) into epoch ms.
/// Platform-independent (used by the Windows identity reader; tested
/// everywhere — hence the dead-code allowance on non-Windows builds).
#[cfg_attr(not(windows), allow(dead_code))]
fn parse_wmi_datetime(s: &str) -> Option<u64> {
    if s.len() < 14 {
        return None;
    }
    let year: i64 = s[0..4].parse().ok()?;
    let month: i64 = s[4..6].parse().ok()?;
    let day: i64 = s[6..8].parse().ok()?;
    let hour: i64 = s[8..10].parse().ok()?;
    let minute: i64 = s[10..12].parse().ok()?;
    let second: i64 = s[12..14].parse().ok()?;
    // UTC offset in minutes ("+480"/"-300"), right after the fraction.
    let offset_min: i64 = {
        let rest = &s[14..];
        match rest.find(['+', '-']) {
            Some(pos) => {
                let sign = if rest.as_bytes()[pos] == b'-' { -1 } else { 1 };
                let num: i64 = rest[pos + 1..].trim_end().parse().unwrap_or(0);
                sign * num
            }
            None => 0,
        }
    };
    let epoch_sec =
        days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second
            - offset_min * 60;
    if epoch_sec < 0 {
        return None;
    }
    Some(epoch_sec as u64 * 1000)
}

/// Days since 1970-01-01 (Howard Hinnant's civil algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    // Only the `#[cfg(unix)]` tests below use threads and timeouts; importing
    // them unconditionally made Windows clippy fail on unused imports.
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{Duration, Instant};

    fn identity(pid: u32, epoch: u64, command: &str) -> ProcessIdentity {
        ProcessIdentity {
            pid,
            start_epoch_ms: epoch,
            command: command.to_string(),
        }
    }

    #[test]
    fn command_carries_token_requires_exact_argument() {
        let cmd = "bash -c export UNIRUN_GENERATION_TOKEN=ur123; echo hi";
        assert!(command_carries_token(cmd, "ur123"));
        // Longer token must not match a prefix.
        assert!(!command_carries_token(cmd, "ur1234"));
        // Shorter token must not match a suffix.
        assert!(!command_carries_token(cmd, "ur12"));
        assert!(!command_carries_token("echo hi", "ur123"));
        assert!(!command_carries_token(cmd, ""));
        // cmd.exe and PowerShell forms.
        assert!(command_carries_token(
            "cmd /C set UNIRUN_GENERATION_TOKEN=ur9&& echo hi",
            "ur9"
        ));
        assert!(command_carries_token(
            "powershell -Command $env:UNIRUN_GENERATION_TOKEN=ur7; Write-Host hi",
            "ur7"
        ));
        // PowerShell quotes the value (a bare word after `=` is a command), so
        // the verifier must accept the quoted spelling…
        assert!(command_carries_token(
            "powershell -Command $env:UNIRUN_GENERATION_TOKEN='ur7'; Write-Host hi",
            "ur7"
        ));
        assert!(command_carries_token(
            "powershell -Command $env:UNIRUN_GENERATION_TOKEN=\"ur7\"; Write-Host hi",
            "ur7"
        ));
        // …without weakening the exact-token rule.
        assert!(!command_carries_token(
            "powershell -Command $env:UNIRUN_GENERATION_TOKEN='ur7'; Write-Host hi",
            "ur"
        ));
        assert!(!command_carries_token(
            "powershell -Command $env:UNIRUN_GENERATION_TOKEN='ur77'; Write-Host hi",
            "ur7"
        ));
    }

    /// The injector and the kill-time verifier are a matched pair: if the
    /// injected text stops being recognizable, every tree-kill turns into a
    /// `TokenMismatch` refusal (fail-closed but useless). Round-trip every shell
    /// so a change to either side has to keep the other one working.
    #[test]
    fn injected_token_is_recognizable_by_the_verifier_for_every_shell() {
        let shells = [
            Shell::Bash,
            Shell::Sh,
            Shell::Zsh,
            Shell::Powershell,
            Shell::Pwsh,
            Shell::Cmd,
        ];
        for shell in shells {
            let injected = inject_generation_token(shell, "echo hi", "ur7");
            assert!(
                command_carries_token(&injected, "ur7"),
                "{:?} injection is not recognized by the verifier: {}",
                shell,
                injected
            );
            // A different token must not be accepted for this command.
            assert!(
                !command_carries_token(&injected, "ur8"),
                "{:?} injection matched the wrong token: {}",
                shell,
                injected
            );
        }
    }

    #[test]
    fn inject_generation_token_forms() {
        assert_eq!(
            inject_generation_token(Shell::Bash, "echo hi", "tok1"),
            "export UNIRUN_GENERATION_TOKEN=tok1; echo hi"
        );
        assert_eq!(
            inject_generation_token(Shell::Sh, "echo hi", "tok1"),
            "export UNIRUN_GENERATION_TOKEN=tok1; echo hi"
        );
        assert_eq!(
            inject_generation_token(Shell::Powershell, "Write-Host hi", "tok2"),
            "$env:UNIRUN_GENERATION_TOKEN='tok2'; Write-Host hi"
        );
        assert_eq!(
            inject_generation_token(Shell::Pwsh, "Write-Host hi", "tok2"),
            "$env:UNIRUN_GENERATION_TOKEN='tok2'; Write-Host hi"
        );
        assert_eq!(
            inject_generation_token(Shell::Cmd, "echo hi", "tok3"),
            "set UNIRUN_GENERATION_TOKEN=tok3&& echo hi"
        );
    }

    #[test]
    fn generate_generation_token_is_unique_and_hex() {
        let a = generate_generation_token();
        let b = generate_generation_token();
        assert_ne!(a, b);
        assert!(a.starts_with("ur"));
        assert!(a.len() >= 32);
        assert!(a.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn verify_identity_pure_cases() {
        let expected = ExpectedIdentity {
            pid: 42,
            generation_token: "ur123".into(),
            start_epoch_ms: Some(1_000_000),
            token_observable: true,
        };
        // No process → not running.
        assert_eq!(
            verify_identity(&expected, None, 2_000),
            IdentityVerdict::NotRunning
        );
        // Same epoch + token → matches.
        assert_eq!(
            verify_identity(
                &expected,
                Some(identity(
                    42,
                    1_000_000,
                    "sh -c export UNIRUN_GENERATION_TOKEN=ur123; x"
                )),
                2_000,
            ),
            IdentityVerdict::Matches
        );
        // Epoch skew beyond tolerance → reused.
        assert_eq!(
            verify_identity(
                &expected,
                Some(identity(
                    42,
                    9_999_999,
                    "sh -c export UNIRUN_GENERATION_TOKEN=ur123; x"
                )),
                2_000,
            ),
            IdentityVerdict::StartEpochMismatch {
                observed: 9_999_999,
                expected: 1_000_000,
            }
        );
        // Wrong token → reused.
        assert_eq!(
            verify_identity(
                &expected,
                Some(identity(42, 1_000_000, "sh -c echo innocent")),
                2_000,
            ),
            IdentityVerdict::TokenMismatch
        );
        // Epoch within tolerance but token missing → still rejected.
        assert_eq!(
            verify_identity(
                &expected,
                Some(identity(42, 1_001_000, "sh -c echo innocent")),
                2_000,
            ),
            IdentityVerdict::TokenMismatch
        );
        // Epoch unknown (no snapshot): token alone decides.
        let no_epoch = ExpectedIdentity {
            start_epoch_ms: None,
            ..expected.clone()
        };
        assert_eq!(
            verify_identity(
                &no_epoch,
                Some(identity(
                    42,
                    0,
                    "sh -c export UNIRUN_GENERATION_TOKEN=ur123; x"
                )),
                2_000,
            ),
            IdentityVerdict::Matches
        );
        // Token unobservable: epoch alone decides.
        let unobservable = ExpectedIdentity {
            token_observable: false,
            ..expected.clone()
        };
        assert_eq!(
            verify_identity(
                &unobservable,
                Some(identity(42, 1_000_000, "whatever")),
                2_000,
            ),
            IdentityVerdict::Matches
        );
        // Token unobservable *and* no epoch captured (the probe failed at
        // spawn): nothing ties the pid to the run. Reporting `Matches` here
        // would let a recycled pid be signalled, so it must be `Unverifiable`
        // — the callers refuse and say so instead of claiming PID_REUSED.
        let nothing_to_check = ExpectedIdentity {
            start_epoch_ms: None,
            token_observable: false,
            ..expected.clone()
        };
        assert_eq!(
            verify_identity(
                &nothing_to_check,
                Some(identity(42, 1_000_000, "whatever")),
                2_000,
            ),
            IdentityVerdict::Unverifiable
        );
        // A present-but-wrong epoch still wins: that is a real reuse signal.
        assert_eq!(
            verify_identity(
                &ExpectedIdentity {
                    start_epoch_ms: Some(1_000_000),
                    token_observable: false,
                    ..expected.clone()
                },
                Some(identity(42, 9_999_999, "whatever")),
                2_000,
            ),
            IdentityVerdict::StartEpochMismatch {
                observed: 9_999_999,
                expected: 1_000_000,
            }
        );
    }

    #[test]
    fn fake_pid_is_not_running() {
        let pid = 999_999_999; // far beyond any real pid on this host
        assert!(read_process_identity(pid).is_none());
        assert!(read_start_epoch_ms(pid).is_none());
        let expected = ExpectedIdentity {
            pid,
            generation_token: "ur123".into(),
            start_epoch_ms: None,
            token_observable: true,
        };
        assert_eq!(verify(&expected, 2_000), IdentityVerdict::NotRunning);
    }

    #[test]
    fn self_start_epoch_is_stable_and_recent() {
        if !platform_probe_available() {
            eprintln!("skipping: this environment cannot report process start epochs");
            return;
        }
        let pid = std::process::id();
        let a = read_start_epoch_ms(pid).expect("our own start epoch");
        let b = read_start_epoch_ms(pid).expect("our own start epoch again");
        assert_eq!(a, b, "start epoch must be stable across reads");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // The test binary started within the last hour; allow a wide margin.
        assert!(
            now >= a,
            "start epoch {} must be in the past (now {})",
            a,
            now
        );
        assert!(now - a < 3_600_000, "start epoch must be recent");
    }

    /// The real verify() must refuse a mismatched identity (our own process
    /// does not carry a foreign generation token).
    #[test]
    fn verify_refuses_foreign_identity_on_self() {
        let pid = std::process::id();
        let expected = ExpectedIdentity {
            pid,
            generation_token: "ur-deadbeef-not-ours".into(),
            start_epoch_ms: None,
            token_observable: true,
        };
        let verdict = verify(&expected, 2_000);
        assert_ne!(verdict, IdentityVerdict::Matches, "verdict: {:?}", verdict);
    }

    /// Spawn a child with the token in its environment and confirm the
    /// observed identity carries it (unix: env is readable via /proc or ps eww).
    #[cfg(unix)]
    #[test]
    fn spawned_child_identity_carries_token() {
        if !platform_probe_available() {
            eprintln!("skipping: this environment cannot report process identities");
            return;
        }
        let token = generate_generation_token();
        let child = Command::new("sh")
            .arg("-c")
            .arg("sleep 2")
            .env(GENERATION_TOKEN_ENV, &token)
            .spawn()
            .expect("spawn sh");
        let pid = child.id();
        let epoch = read_start_epoch_ms(pid).expect("child start epoch");
        let observed = read_process_identity(pid).expect("child identity");
        assert!(
            command_carries_token(&observed.command, &token),
            "command did not carry token: {:?}",
            observed.command
        );
        let expected = ExpectedIdentity {
            pid,
            generation_token: token,
            start_epoch_ms: Some(epoch),
            token_observable: true,
        };
        assert_eq!(verify(&expected, 2_000), IdentityVerdict::Matches);
        let mut child = child;
        let _ = child.wait();
    }

    /// A real POSIX zombie is not alive and has no identity.
    #[cfg(unix)]
    #[test]
    fn zombie_process_is_not_alive() {
        if !platform_probe_available() {
            eprintln!("skipping: this environment cannot report process states");
            return;
        }
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            // Child: exit immediately without any Rust cleanup.
            unsafe { libc::_exit(0) };
        }
        assert!(pid > 0, "fork failed");
        let pid = pid as u32;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if is_zombie(pid) {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(is_zombie(pid), "child never became a zombie");
        assert!(!is_alive(pid), "zombie must not be alive");
        assert!(read_process_identity(pid).is_none());
        assert!(read_start_epoch_ms(pid).is_none());
        // Reap so the test leaves no zombie behind.
        unsafe {
            libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
        }
    }

    #[test]
    fn wmi_datetime_parses_to_epoch() {
        // 2026-08-20 12:00:00 UTC → 1787227200 s
        assert_eq!(
            parse_wmi_datetime("20260820120000.000000+000"),
            Some(1_787_227_200_000)
        );
        // +480 min (UTC+8): 2026-08-20 12:00:00 +08:00 → 04:00 UTC
        assert_eq!(
            parse_wmi_datetime("20260820120000.000000+480"),
            Some(1_787_198_400_000)
        );
        // -300 min (UTC-5): 2026-08-20 12:00:00 -05:00 → 17:00 UTC
        assert_eq!(
            parse_wmi_datetime("20260820120000.000000-300"),
            Some(1_787_245_200_000)
        );
        assert!(parse_wmi_datetime("garbage").is_none());
        assert!(parse_wmi_datetime("20260820").is_none());
    }

    /// Regression for the Windows reader: `Get-CimInstance` hands back a
    /// `DateTime`, and Windows PowerShell 5.1 renders it as `/Date(<ms>)/`,
    /// which the CIM-string parser cannot read. Both shapes must parse.
    #[test]
    fn creation_date_parses_both_renderings() {
        assert_eq!(
            parse_creation_date("/Date(1787227200000)/"),
            Some(1_787_227_200_000)
        );
        assert_eq!(
            parse_creation_date("/Date(1787227200000+0800)/"),
            Some(1_787_227_200_000)
        );
        // The raw CIM form still works.
        assert_eq!(
            parse_creation_date("20260820120000.000000+000"),
            Some(1_787_227_200_000)
        );
        assert!(parse_creation_date("garbage").is_none());
        assert!(parse_creation_date("/Date()/").is_none());
    }
}
