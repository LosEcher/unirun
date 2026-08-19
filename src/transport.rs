//! SSH remote transport — the win-exec knowledge ported to Rust.
//!
//! Kills the `bash → ssh → cmd.exe → PowerShell` escaping chain the same way
//! win-exec does: script content travels as a **payload** (UTF-16LE base64
//! `-EncodedCommand`, or a scp-uploaded temp file for large scripts), never
//! as a hand-quoted command string. The "golden recipe" is auto-injected so
//! PowerShell 5.1 emits clean UTF-8 (no CLIXML/OEM/GBK mojibake), and the
//! `exit $LASTEXITCODE` contract propagates exact remote exit codes.

use crate::spec::{ExecResult, Shell};
use crate::taxonomy::classify;
use base64::Engine;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const GOLDEN_PREFIX: &str = "$ProgressPreference='SilentlyContinue'\n[Console]::OutputEncoding=[Text.Encoding]::UTF8\n$OutputEncoding=[Text.Encoding]::UTF8\n";
const EXIT_CONTRACT: &str = "\nexit $LASTEXITCODE\n";
/// EncodedCommand base64 length beyond which we fall back to scp + `-File`
/// (CreateProcess command-line limit ≈ 32 KB; this threshold is conservative).
const B64_THRESHOLD: usize = 60_000;
const MAX_OUTPUT: usize = 256 * 1024;

/// Remote host + shell selection for SSH execution.
#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    /// Remote shell: `Powershell` (PS 5.1) | `Pwsh` (7) | `Cmd`.
    pub shell: Shell,
    pub timeout_ms: u64,
    pub connect_timeout: u64,
}

impl Default for SshTarget {
    fn default() -> Self {
        SshTarget {
            host: "win-los".into(),
            shell: Shell::Powershell,
            timeout_ms: 120_000,
            connect_timeout: 15,
        }
    }
}

/// Run a script on a remote Windows host over SSH. Returns a normalized
/// `ExecResult` with exact remote exit code and clean UTF-8 output.
pub fn ssh_run(target: &SshTarget, script: &str) -> ExecResult {
    match target.shell {
        Shell::Powershell | Shell::Pwsh => ssh_powershell(target, script),
        Shell::Cmd => ssh_cmd_file(target, script),
        other => {
            let mut r = ExecResult::success(String::new(), String::new(), other.as_str());
            r.error_class = Some("COMMAND_NOT_FOUND".into());
            r.hint = Some(format!(
                "shell `{}` is not a Windows remote shell; use powershell / pwsh / cmd",
                other.as_str()
            ));
            r
        }
    }
}

fn ssh_powershell(target: &SshTarget, script: &str) -> ExecResult {
    let exe = if target.shell == Shell::Pwsh {
        "pwsh.exe"
    } else {
        "powershell.exe"
    };
    let payload = format!("{}{}{}", GOLDEN_PREFIX, script.trim_end(), EXIT_CONTRACT);
    let b64 = base64_utf16le(&payload);
    if b64.len() <= B64_THRESHOLD {
        let remote_cmd = format!("{} -NoProfile -NonInteractive -EncodedCommand {}", exe, b64);
        run_ssh(target, &remote_cmd)
    } else {
        // Large payload: scp a UTF-8-BOM temp .ps1, run with -File, clean up.
        let remote_path = format!(r"C:\Windows\Temp\unirun-{}.ps1", nonce());
        let mut bytes = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM: PS 5.1 parses UTF-8 correctly
        bytes.extend_from_slice(payload.as_bytes());
        let uploaded = upload_scp(target, &remote_path, &bytes);
        if let Err(e) = uploaded {
            let mut r = ExecResult::success(String::new(), format!("upload failed: {}", e), exe);
            r.error_class = Some("COMMAND_NOT_FOUND".into());
            r.hint = Some("scp to the remote host failed; check host/credentials".into());
            return r;
        }
        let remote_cmd = format!(
            "{} -NoProfile -NonInteractive -ExecutionPolicy Bypass -File {}",
            exe, remote_path
        );
        let r = run_ssh(target, &remote_cmd);
        let _ = run_ssh(target, &format!("del /q {}", remote_path));
        r
    }
}

/// cmd.exe: always file mode (a `.bat` avoids cmd eating metacharacters;
/// content stays ASCII-safe per win-exec guidance).
fn ssh_cmd_file(target: &SshTarget, script: &str) -> ExecResult {
    let remote_path = format!(r"C:\Windows\Temp\unirun-{}.bat", nonce());
    if let Err(e) = upload_scp(target, &remote_path, script.as_bytes()) {
        let mut r = ExecResult::success(String::new(), format!("upload failed: {}", e), "cmd");
        r.error_class = Some("COMMAND_NOT_FOUND".into());
        r.hint = Some("scp to the remote host failed; check host/credentials".into());
        return r;
    }
    let remote_cmd = format!("cmd.exe /C \"{}\"", remote_path);
    let r = run_ssh(target, &remote_cmd);
    // Clean the temp file best-effort.
    let _ = run_ssh(target, &format!("del /q {}", remote_path));
    r
}

/// Spawn ssh, capture output with a deadline, filter the OpenSSH banner.
fn run_ssh(target: &SshTarget, remote_cmd: &str) -> ExecResult {
    let start = Instant::now();
    let mut cmd = Command::new("ssh");
    cmd.arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={}", target.connect_timeout))
        .arg("-o")
        .arg("ServerAliveInterval=30")
        .arg(&target.host)
        .arg(remote_cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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

    let so = child.stdout.take().unwrap();
    let se = child.stderr.take().unwrap();
    let to = thread::spawn(move || read_capped(so, MAX_OUTPUT));
    let te = thread::spawn(move || read_capped(se, MAX_OUTPUT));

    let timeout = Duration::from_millis(if target.timeout_ms == 0 {
        120_000
    } else {
        target.timeout_ms
    });
    let grace = Duration::from_millis(2_000);
    let mut exit_code = None;
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                exit_code = status.code();
                break;
            }
            Ok(None) => {
                if start.elapsed() >= timeout {
                    timed_out = true;
                    #[cfg(unix)]
                    kill_ssh_tree(&mut child, grace);
                    #[cfg(windows)]
                    kill_ssh_tree(&child, grace);
                    let _ = child.wait();
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => {
                let _ = child.wait();
                break;
            }
        }
    }
    let (out_raw, _) = to.join().unwrap_or((Vec::new(), false));
    let (err_raw, _) = te.join().unwrap_or((Vec::new(), false));
    let stdout = crate::encoding::decode(&out_raw);
    let stderr_raw = filter_banner(&err_raw);
    let stderr = crate::encoding::decode(&stderr_raw);

    let mut result = ExecResult {
        exit_code,
        signal: None,
        stdout: stdout.text,
        stderr: stderr.text,
        timed_out,
        aborted: false,
        duration_ms: start.elapsed().as_millis() as u64,
        error_class: None,
        hint: None,
        encoding: stdout.encoding.to_string(),
        truncated: false,
        shell_used: target.shell.as_str().to_string(),
    };
    let (class, hint) = classify(&result);
    result.error_class = class;
    result.hint = hint;
    result
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
    let status = Command::new("scp")
        .arg("-o")
        .arg("BatchMode=yes")
        .arg("-o")
        .arg(format!("ConnectTimeout={}", target.connect_timeout))
        .arg(&local)
        .arg(format!("{}:{}", target.host, remote_path))
        .status()?;
    let _ = std::fs::remove_file(&local);
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other("scp exited non-zero"))
    }
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

fn read_capped<R: Read>(mut reader: R, max: usize) -> (Vec<u8>, bool) {
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
            }
            Err(_) => break,
        }
    }
    (tail, total > max)
}

#[cfg(unix)]
fn kill_ssh_tree(child: &mut Child, grace: Duration) {
    let pid = child.id() as i32;
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

#[cfg(windows)]
fn kill_ssh_tree(child: &Child, _grace: Duration) {
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .status();
}
