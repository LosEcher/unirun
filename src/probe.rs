//! Capability probing — what shells, coreutils and tools actually exist on
//! this host. `unirun probe` answers the agent's first question: "what can
//! I rely on here?" before it picks a strategy. This is the probe half of
//! the probe-then-degrade pattern.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// What a probe could establish about one name.
///
/// Three states, not two: "absent" and "could not check" are different facts,
/// and collapsing them made a blocked PATH scan read as "this host has no
/// python" (the same confusion `process_identity` fixed for process liveness —
/// `Observed::{Alive,Gone,Unreadable}`). Callers that only need "is it usable"
/// can treat `Unreadable` as `Absent`; callers that report on a fleet must not.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProbeState {
    /// Found at `path`.
    Found,
    /// Looked for it and it is genuinely not there.
    Absent,
    /// The check itself could not be completed (a PATH entry that could not be
    /// read), so nothing can be said either way.
    Unreadable,
}

impl ProbeState {
    pub fn as_str(self) -> &'static str {
        match self {
            ProbeState::Found => "found",
            ProbeState::Absent => "absent",
            ProbeState::Unreadable => "unreadable",
        }
    }
}

/// One probe result: the state, plus a path when there is one.
#[derive(Debug, Clone)]
pub struct ProbeOutcome {
    pub path: Option<String>,
    pub state: ProbeState,
}

impl ProbeOutcome {
    fn found(path: String) -> Self {
        ProbeOutcome {
            path: Some(path),
            state: ProbeState::Found,
        }
    }
    fn absent() -> Self {
        ProbeOutcome {
            path: None,
            state: ProbeState::Absent,
        }
    }
    fn unreadable() -> Self {
        ProbeOutcome {
            path: None,
            state: ProbeState::Unreadable,
        }
    }
}

/// A tool found (or not) on PATH.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub path: Option<String>,
    /// `found` | `absent` | `unreadable` — see [`ProbeState`].
    pub state: ProbeState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellInfo {
    pub name: String,
    pub path: Option<String>,
    /// `found` | `absent` | `unreadable` — see [`ProbeState`].
    pub state: ProbeState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoreutilsInfo {
    /// `timeout` (GNU coreutils) — absent on stock macOS/BSD.
    pub timeout: Option<String>,
    /// `gtimeout` (Homebrew coreutils) — the GNU timeout under its g-prefixed name.
    pub gtimeout: Option<String>,
    /// True when a GNU-compatible timeout binary is reachable.
    pub gnu_timeout_available: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub platform: String,
    pub arch: String,
    pub shells: Vec<ShellInfo>,
    pub coreutils: CoreutilsInfo,
    pub tools: Vec<ToolInfo>,
    /// Coverage honesty: the names this probe could **not** check. Empty means
    /// every lookup completed, so an `absent` really is absent. A non-empty list
    /// means the rest of the snapshot is fine but incomplete — report it instead
    /// of pretending the host is bare.
    pub unreadable: Vec<String>,
}

/// Resolve a command to its absolute path by scanning PATH, or `None`.
/// On Windows, executable extensions (`.exe/.cmd/.bat/.ps1`) are probed
/// because `cmd` exists as `cmd.exe` (CreateProcess resolves extensions,
/// but a PATH file-scan must do so explicitly).
pub fn which(name: &str) -> Option<String> {
    which_state(name).path
}

/// [`which`] with three-state semantics: `Found`, `Absent`, or `Unreadable`
/// when a PATH entry could not be read at all.
pub fn which_state(name: &str) -> ProbeOutcome {
    if name.contains('/') || name.contains('\\') {
        return scan_one(Path::new(name));
    }
    scan_dirs(name, &path_entries())
}

/// One explicit path (contains a separator): exists or not, no PATH involved.
fn scan_one(p: &Path) -> ProbeOutcome {
    match std::fs::metadata(p) {
        Ok(md) if md.is_file() => ProbeOutcome::found(p.to_string_lossy().into_owned()),
        Ok(_) => ProbeOutcome::absent(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => ProbeOutcome::absent(),
        Err(_) => ProbeOutcome::unreadable(),
    }
}

/// Scan PATH entries for `name`. Pure over `dirs`, so the unreadable branch is
/// testable (a directory that cannot be read) on any machine that is not root.
fn scan_dirs(name: &str, dirs: &[PathBuf]) -> ProbeOutcome {
    let mut unreadable = false;
    for dir in dirs {
        // A directory we cannot even open cannot be searched: distinguish that
        // from "the file is not there".
        match std::fs::read_dir(dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => {
                unreadable = true;
                continue;
            }
        }
        for cand_name in executable_candidates(name) {
            let cand = dir.join(&cand_name);
            match std::fs::metadata(&cand) {
                Ok(md) if md.is_file() => {
                    if is_wsl_bash_shim(&cand) {
                        // Windows: %SystemRoot%\System32\bash.exe is the WSL
                        // launcher, not a usable bash — with no distro installed
                        // it prints a UTF-16LE "no distributions" message and
                        // exits 1. Treat it as absent so probe/recipes/tests
                        // never route through it (Git Bash lives elsewhere).
                        continue;
                    }
                    return ProbeOutcome::found(cand.to_string_lossy().into_owned());
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => unreadable = true,
            }
        }
    }
    if unreadable {
        ProbeOutcome::unreadable()
    } else {
        ProbeOutcome::absent()
    }
}

/// `System32\bash.exe` on Windows is always the WSL launcher, never a real
/// bash. Pure (the caller supplies `SystemRoot`) so the decision is testable on
/// every platform, not only where the shim exists.
fn is_wsl_bash_shim_for(path: &Path, system_root: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name != "bash.exe" {
        return false;
    }
    path.starts_with(system_root.join("System32"))
}

/// Platform-bound wrapper for [`is_wsl_bash_shim_for`].
fn is_wsl_bash_shim(path: &Path) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let sys = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    is_wsl_bash_shim_for(path, &sys)
}

/// Candidate filenames for a bare command name.
///
/// Pure-form for tests: Windows resolves `python` through `python.exe` and
/// friends because a PATH file-scan has to, unlike `CreateProcess`.
fn executable_candidates_for(name: &str, windows: bool) -> Vec<String> {
    let mut v = vec![name.to_string()];
    if windows && !name.contains('.') {
        v.push(format!("{}.exe", name));
        v.push(format!("{}.cmd", name));
        v.push(format!("{}.bat", name));
        v.push(format!("{}.ps1", name));
    }
    v
}

fn executable_candidates(name: &str) -> Vec<String> {
    executable_candidates_for(name, cfg!(windows))
}

fn path_entries() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Full capability snapshot for the current host.
pub fn probe() -> Capabilities {
    let mut unreadable: Vec<String> = Vec::new();
    let shell_names: &[&str] = &["bash", "sh", "zsh", "pwsh", "powershell", "cmd"];
    let shells: Vec<ShellInfo> = shell_names
        .iter()
        .map(|n| {
            let o = which_state(n);
            if o.state == ProbeState::Unreadable {
                unreadable.push((*n).to_string());
            }
            ShellInfo {
                name: (*n).to_string(),
                path: o.path,
                state: o.state,
            }
        })
        .collect();

    let timeout_outcome = which_state("timeout");
    let gtimeout_outcome = which_state("gtimeout");
    if timeout_outcome.state == ProbeState::Unreadable {
        unreadable.push("timeout".to_string());
    }
    if gtimeout_outcome.state == ProbeState::Unreadable {
        unreadable.push("gtimeout".to_string());
    }
    let timeout = timeout_outcome.path.clone();
    let gtimeout = gtimeout_outcome.path.clone();
    let gnu_timeout_available =
        timeout.is_some() && is_gnu_coreutils("timeout") || gtimeout.is_some();

    let tool_names: &[&str] = &[
        "python3", "node", "git", "curl", "uname", "sed", "awk", "find", "rsync", "tar",
    ];
    let tools: Vec<ToolInfo> = tool_names
        .iter()
        .map(|n| {
            let o = which_state(n);
            if o.state == ProbeState::Unreadable {
                unreadable.push((*n).to_string());
            }
            ToolInfo {
                name: (*n).to_string(),
                path: o.path,
                state: o.state,
            }
        })
        .collect();

    Capabilities {
        platform: platform_name(),
        arch: std::env::consts::ARCH.to_string(),
        shells,
        coreutils: CoreutilsInfo {
            timeout,
            gtimeout,
            gnu_timeout_available,
        },
        tools,
        unreadable,
    }
}

fn platform_name() -> String {
    match std::env::consts::OS {
        "macos" => "macos".into(),
        "linux" => "linux".into(),
        "windows" => "windows".into(),
        other => other.into(),
    }
}

/// Cheap check that a `timeout` binary is GNU coreutils (macOS/BSD have none
/// by default; Homebrew coreutils installs `gtimeout` instead).
fn is_gnu_coreutils(bin: &str) -> bool {
    match Command::new(bin).arg("--version").output() {
        Ok(out) => {
            let head = String::from_utf8_lossy(&out.stdout);
            head.contains("coreutils") || head.contains("GNU")
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Platform differences, part 1 and 18: the shim decision and the extension
    /// resolution are pure, so both are pinned on every OS (the Windows CI job
    /// runs them too, against the same values).
    #[test]
    fn wsl_shim_and_extension_rules_are_platform_facts() {
        let sys = Path::new(r"C:\Windows");
        assert!(is_wsl_bash_shim_for(
            &sys.join("System32").join("bash.exe"),
            sys
        ));
        // Real Git Bash, a differently-named shim, and a decoy outside System32.
        assert!(!is_wsl_bash_shim_for(
            Path::new(r"C:\Program Files\Git\bin\bash.exe"),
            sys
        ));
        assert!(!is_wsl_bash_shim_for(
            &sys.join("System32").join("sh.exe"),
            sys
        ));
        assert!(!is_wsl_bash_shim_for(Path::new(r"D:\tools\bash.exe"), sys));
        // Case-insensitive, as Windows paths are.
        assert!(is_wsl_bash_shim_for(
            &sys.join("System32").join("BASH.EXE"),
            sys
        ));

        // Windows resolves bare names through executable extensions; POSIX does
        // not (and must not, or `python` would look for `python.exe`).
        let win = executable_candidates_for("python", true);
        assert_eq!(
            win,
            vec![
                "python",
                "python.exe",
                "python.cmd",
                "python.bat",
                "python.ps1"
            ]
        );
        assert_eq!(executable_candidates_for("python", false), vec!["python"]);
        // A name that already carries an extension is left alone.
        assert_eq!(executable_candidates_for("run.cmd", true), vec!["run.cmd"]);
    }

    /// A PATH entry that cannot be read is `unreadable`, not `absent`: reporting
    /// "this host has no python" because a directory was blocked is the same
    /// mistake as calling an unreadable process "gone".
    #[cfg(unix)]
    #[test]
    fn unreadable_path_entries_are_not_reported_as_absent() {
        use std::os::unix::fs::PermissionsExt;
        let base = std::env::temp_dir().join(format!("unirun-probe-{}", std::process::id()));
        let locked = base.join("locked");
        let open = base.join("open");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::create_dir_all(&open).unwrap();
        std::fs::write(locked.join("bash"), b"#!/bin/sh\n").unwrap();
        std::fs::write(open.join("bash"), b"#!/bin/sh\n").unwrap();

        // A readable directory resolves, an empty one is simply absent.
        assert_eq!(
            scan_dirs("bash", std::slice::from_ref(&open)).state,
            ProbeState::Found
        );
        assert_eq!(
            scan_dirs("bash", &[base.join("nowhere")]).state,
            ProbeState::Absent
        );

        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        let root_can_read_anything = std::fs::read_dir(&locked).is_ok();
        let outcome = scan_dirs("bash", std::slice::from_ref(&locked));
        // Restore before asserting, so a failure still cleans up.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        if root_can_read_anything {
            eprintln!("skipping the unreadable branch: this user can read anything");
        } else {
            assert_eq!(
                outcome.state,
                ProbeState::Unreadable,
                "a blocked directory must not read as absent"
            );
            assert!(outcome.path.is_none());
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// An explicit path (one containing a separator) never touches PATH.
    #[test]
    fn explicit_paths_report_found_or_absent() {
        let me = std::env::current_exe().expect("test binary path");
        assert_eq!(
            scan_one(&me).state,
            ProbeState::Found,
            "the test binary exists"
        );
        assert_eq!(
            scan_one(Path::new("/nonexistent/unirun-probe-decoy")).state,
            ProbeState::Absent
        );
    }

    /// The snapshot's own consistency: a `Found` entry has a path, a non-Found
    /// entry does not, and the coverage list is exactly the unreadable ones.
    #[test]
    fn probe_snapshot_is_internally_consistent() {
        let caps = probe();
        for (name, path, state) in caps
            .shells
            .iter()
            .map(|s| (&s.name, &s.path, s.state))
            .chain(caps.tools.iter().map(|t| (&t.name, &t.path, t.state)))
        {
            match state {
                ProbeState::Found => {
                    let p = path
                        .as_deref()
                        .unwrap_or_else(|| panic!("{name} is found but carries no path"));
                    assert!(Path::new(p).is_file(), "{name} -> {p} is not a file");
                }
                ProbeState::Absent | ProbeState::Unreadable => assert!(
                    path.is_none(),
                    "{name} is {} but carries a path",
                    state.as_str()
                ),
            }
        }
        let unreadable: Vec<String> = caps
            .shells
            .iter()
            .filter(|s| s.state == ProbeState::Unreadable)
            .map(|s| s.name.clone())
            .chain(
                caps.tools
                    .iter()
                    .filter(|t| t.state == ProbeState::Unreadable)
                    .map(|t| t.name.clone()),
            )
            .collect();
        assert_eq!(caps.unreadable, unreadable, "coverage list must match");
    }

    #[test]
    fn probe_shape() {
        let caps = probe();
        assert!(caps.platform == "macos" || caps.platform == "linux" || caps.platform == "windows");
        // Platform differences, part 18: the platform comes from the compiler's
        // target, never from running `uname` — Windows OpenSSH has no `uname`,
        // and its absence used to be reported as a GBK error string instead of
        // "unsupported".
        assert_eq!(
            caps.platform,
            match std::env::consts::OS {
                "macos" => "macos",
                "linux" => "linux",
                "windows" => "windows",
                other => other,
            }
        );
        assert_eq!(caps.arch, std::env::consts::ARCH);
        assert!(!caps.shells.is_empty());
        // Every supported host must expose at least one native shell.
        let has_native = if cfg!(windows) {
            caps.shells.iter().any(|s| {
                (s.name == "cmd" || s.name == "powershell" || s.name == "pwsh") && s.path.is_some()
            })
        } else {
            caps.shells
                .iter()
                .any(|s| (s.name == "bash" || s.name == "sh") && s.path.is_some())
        };
        assert!(has_native, "no native shell found: {:?}", caps.shells);
    }

    #[test]
    fn which_finds_self_in_path() {
        // sh should exist on all POSIX targets where unirun tests run.
        if cfg!(unix) {
            assert!(which("sh").is_some());
        }
    }

    #[cfg(windows)]
    #[test]
    fn wsl_bash_shim_is_excluded() {
        // System32\bash.exe (WSL launcher) must never resolve as bash; Git
        // Bash lives under Program Files.
        let sys = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let shim = PathBuf::from(&sys).join("System32").join("bash.exe");
        assert!(is_wsl_bash_shim(&shim), "{:?}", shim);
        let git_bash = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        assert!(!is_wsl_bash_shim(&git_bash));
    }
}
