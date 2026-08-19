//! Capability probing — what shells, coreutils and tools actually exist on
//! this host. `unirun probe` answers the agent's first question: "what can
//! I rely on here?" before it picks a strategy. This is the probe half of
//! the probe-then-degrade pattern.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// A tool found (or not) on PATH.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolInfo {
    pub name: String,
    pub path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellInfo {
    pub name: String,
    pub path: Option<String>,
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
}

/// Resolve a command to its absolute path by scanning PATH, or `None`.
/// On Windows, executable extensions (`.exe/.cmd/.bat/.ps1`) are probed
/// because `cmd` exists as `cmd.exe` (CreateProcess resolves extensions,
/// but a PATH file-scan must do so explicitly).
pub fn which(name: &str) -> Option<String> {
    if name.contains('/') || name.contains('\\') {
        let p = Path::new(name);
        return if p.is_file() {
            Some(p.to_string_lossy().into_owned())
        } else {
            None
        };
    }
    for dir in path_entries() {
        for cand_name in executable_candidates(name) {
            let cand = dir.join(&cand_name);
            if cand.is_file() {
                if is_wsl_bash_shim(&cand) {
                    // Windows: %SystemRoot%\System32\bash.exe is the WSL
                    // launcher, not a usable bash — with no distro installed
                    // it prints a UTF-16LE "no distributions" message and
                    // exits 1. Treat it as absent so probe/recipes/tests
                    // never route through it (Git Bash lives elsewhere).
                    continue;
                }
                return Some(cand.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// `System32\bash.exe` on Windows is always the WSL shim.
fn is_wsl_bash_shim(path: &Path) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name != "bash.exe" {
        return false;
    }
    let sys = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    path.starts_with(sys.join("System32"))
}

/// Candidate filenames for a bare command name, platform-aware.
fn executable_candidates(name: &str) -> Vec<String> {
    let mut v = vec![name.to_string()];
    if cfg!(windows) && !name.contains('.') {
        v.push(format!("{}.exe", name));
        v.push(format!("{}.cmd", name));
        v.push(format!("{}.bat", name));
        v.push(format!("{}.ps1", name));
    }
    v
}

fn path_entries() -> Vec<PathBuf> {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default()
}

/// Full capability snapshot for the current host.
pub fn probe() -> Capabilities {
    let shell_names: &[&str] = &["bash", "sh", "zsh", "pwsh", "powershell", "cmd"];
    let shells = shell_names
        .iter()
        .map(|n| ShellInfo {
            name: (*n).to_string(),
            path: which(n),
        })
        .collect();

    let timeout = which("timeout");
    let gtimeout = which("gtimeout");
    let gnu_timeout_available =
        timeout.is_some() && is_gnu_coreutils("timeout") || gtimeout.is_some();

    let tool_names: &[&str] = &[
        "python3", "node", "git", "curl", "uname", "sed", "awk", "find", "rsync", "tar",
    ];
    let tools = tool_names
        .iter()
        .map(|n| ToolInfo {
            name: (*n).to_string(),
            path: which(n),
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

    #[test]
    fn probe_shape() {
        let caps = probe();
        assert!(caps.platform == "macos" || caps.platform == "linux" || caps.platform == "windows");
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
