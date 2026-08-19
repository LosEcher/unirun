//! WinRM provider — POC over psrp-rs 1.0 (PowerShell Remoting Protocol on
//! WS-Management, the `blocking` wrapper API).
//!
//! Executes PowerShell on remote Windows hosts that expose WinRM
//! (HTTP 5985 / HTTPS 5986) with Basic / NTLM / Kerberos auth — no OpenSSH
//! required. The POC scope mirrors the SSH transport contract as closely as
//! PSRP allows: clean UTF-8 output (CLIXML is decoded by psrp-rs), an exact
//! exit code via a `$LASTEXITCODE` sentinel, and the normalized `ExecResult`
//! shape (stderr from the PSRP error stream).
//!
//! Compile with `--features winrm` (pulls tokio + reqwest via psrp-rs).

use crate::spec::ExecResult;
use crate::taxonomy::classify;
use psrp_rs::blocking::run_pipeline;
use psrp_rs::clixml::PsValue;
use psrp_rs::pipeline::Pipeline;
use psrp_rs::records::{ErrorRecord, FromPsObject};
use psrp_rs::{WinrmClient, WinrmConfig, WinrmCredentials};
use std::time::Instant;
use winrm_rs::AuthMethod;

/// The trailing `Write-Output '__UNIRUN_EXIT__'$LASTEXITCODE` sentinel —
/// PSRP does not propagate process exit codes, so the script reports it.
pub const EXIT_SENTINEL: &str = "__UNIRUN_EXIT__";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WinrmAuth {
    Basic,
    Ntlm,
    Kerberos,
}

impl WinrmAuth {
    pub fn from_name(name: &str) -> Option<WinrmAuth> {
        match name.to_ascii_lowercase().as_str() {
            "basic" => Some(WinrmAuth::Basic),
            "ntlm" | "negotiate" => Some(WinrmAuth::Ntlm),
            "kerberos" => Some(WinrmAuth::Kerberos),
            _ => None,
        }
    }

    fn to_winrm(self) -> AuthMethod {
        match self {
            WinrmAuth::Basic => AuthMethod::Basic,
            WinrmAuth::Ntlm => AuthMethod::Ntlm,
            WinrmAuth::Kerberos => AuthMethod::Kerberos,
        }
    }
}

/// Remote WinRM target.
#[derive(Debug, Clone)]
pub struct WinrmTarget {
    pub host: String,
    /// WinRM listener port (default 5985 HTTP / 5986 HTTPS).
    pub port: u16,
    pub use_tls: bool,
    /// Skip TLS chain verification (test environments only).
    pub accept_invalid_certs: bool,
    pub username: String,
    pub password: String,
    /// NetBIOS domain; empty = auto-detect from the NTLM challenge.
    pub domain: String,
    pub auth: WinrmAuth,
    pub timeout_ms: u64,
}

impl Default for WinrmTarget {
    fn default() -> Self {
        WinrmTarget {
            host: String::new(),
            port: 5985,
            use_tls: false,
            accept_invalid_certs: false,
            username: String::new(),
            password: String::new(),
            domain: String::new(),
            auth: WinrmAuth::Ntlm,
            timeout_ms: 120_000,
        }
    }
}

/// Run a PowerShell script on a remote host over WinRM. Returns a normalized
/// `ExecResult` (exit code from the `$LASTEXITCODE` sentinel, stderr from the
/// PSRP error stream, taxonomy classification applied).
pub fn winrm_run(target: &WinrmTarget, script: &str) -> ExecResult {
    let start = Instant::now();
    let config = WinrmConfig {
        port: target.port,
        use_tls: target.use_tls,
        accept_invalid_certs: target.accept_invalid_certs,
        auth_method: target.auth.to_winrm(),
        connect_timeout_secs: 15,
        operation_timeout_secs: (target.timeout_ms / 1000).max(1),
        ..Default::default()
    };
    let credentials = WinrmCredentials::new(
        target.username.clone(),
        target.password.clone(),
        target.domain.clone(),
    );
    let client = match WinrmClient::new(config, credentials) {
        Ok(c) => c,
        Err(e) => return err_result(format!("winrm client init failed: {}", e), start, target),
    };

    let full = format!(
        "{}\nWrite-Output '{}'$LASTEXITCODE",
        script.trim_end(),
        EXIT_SENTINEL
    );
    let pipeline = Pipeline::new(&full);
    let result = run_pipeline(&client, &target.host, pipeline);

    let mut r = match result {
        Ok(pr) => {
            let mut stdout_lines: Vec<String> = pr.output.iter().map(ps_value_text).collect();
            // Extract the exit-code sentinel (the last output line).
            let mut exit_code: Option<i32> = None;
            if let Some(last) = stdout_lines.last() {
                if let Some(rest) = last.strip_prefix(EXIT_SENTINEL) {
                    exit_code = rest.trim().parse().ok();
                    stdout_lines.pop();
                }
            }
            let stdout = join_lines(stdout_lines);
            let stderr = join_lines(pr.errors.iter().map(ps_error_text).collect());
            let shell_used = if cfg!(windows) {
                "powershell"
            } else {
                "winrm-powershell"
            };
            ExecResult {
                exit_code: exit_code.or(if pr.errors.is_empty() {
                    Some(0)
                } else {
                    Some(1)
                }),
                signal: None,
                stdout,
                stderr,
                timed_out: false,
                aborted: false,
                duration_ms: start.elapsed().as_millis() as u64,
                error_class: None,
                hint: None,
                encoding: "utf-8".to_string(),
                truncated: false,
                shell_used: shell_used.to_string(),
            }
        }
        Err(e) => return err_result(format!("winrm/psrp error: {}", e), start, target),
    };
    let (class, hint) = classify(&r);
    r.error_class = class;
    r.hint = hint;
    r
}

fn err_result(message: String, start: Instant, target: &WinrmTarget) -> ExecResult {
    let mut r = ExecResult::success(String::new(), message, "winrm");
    r.exit_code = None;
    r.error_class = Some("COMMAND_NOT_FOUND".into());
    r.hint = Some(format!(
        "WinRM connection to {}:{} failed; check host, port, auth, and that WinRM is enabled",
        target.host, target.port
    ));
    r.duration_ms = start.elapsed().as_millis() as u64;
    r
}

/// Render one output value as text (strings as-is, common scalars via their
/// natural formatting, objects via Debug).
pub fn ps_value_text(v: &PsValue) -> String {
    match v {
        PsValue::Null => String::new(),
        PsValue::String(s) => s.clone(),
        PsValue::Bool(b) => b.to_string(),
        PsValue::I8(x) => x.to_string(),
        PsValue::U8(x) => x.to_string(),
        PsValue::I16(x) => x.to_string(),
        PsValue::U16(x) => x.to_string(),
        PsValue::I32(x) => x.to_string(),
        PsValue::U32(x) => x.to_string(),
        PsValue::I64(x) => x.to_string(),
        PsValue::U64(x) => x.to_string(),
        PsValue::F32(x) => x.to_string(),
        PsValue::Double(x) => x.to_string(),
        other => match other.as_str() {
            Some(s) => s.to_string(),
            None => format!("{:?}", other),
        },
    }
}

/// Render one PSRP error record as text (exception message when available).
pub fn ps_error_text(v: &PsValue) -> String {
    if let Some(rec) = ErrorRecord::from_ps_object(v) {
        if let Some(msg) = rec.exception.as_ref().and_then(|e| e.message.clone()) {
            return msg;
        }
        if let Some(id) = &rec.fully_qualified_error_id {
            return id.clone();
        }
        return format!("{:?}", rec);
    }
    ps_value_text(v)
}

fn join_lines(lines: Vec<String>) -> String {
    let mut out = lines.join("\n");
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_parsing() {
        assert_eq!(WinrmAuth::from_name("basic"), Some(WinrmAuth::Basic));
        assert_eq!(WinrmAuth::from_name("NTLM"), Some(WinrmAuth::Ntlm));
        assert_eq!(WinrmAuth::from_name("kerberos"), Some(WinrmAuth::Kerberos));
        assert_eq!(WinrmAuth::from_name("bogus"), None);
    }

    #[test]
    fn value_text_renders_strings_and_scalars() {
        assert_eq!(ps_value_text(&PsValue::String("中文".into())), "中文");
        assert_eq!(ps_value_text(&PsValue::I32(42)), "42");
        assert_eq!(ps_value_text(&PsValue::Bool(true)), "true");
    }

    #[test]
    fn join_lines_appends_newline() {
        assert_eq!(join_lines(vec![]), "");
        assert_eq!(join_lines(vec!["a".into(), "b".into()]), "a\nb\n");
    }
}
