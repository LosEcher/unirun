//! Heredoc risk scanning: keep payload out of the shell's parser.
//!
//! A heredoc is the one place a shell accepts *payload* inline, and the failure
//! mode is silent: an unquoted delimiter expands `$VAR`/`` `cmd` ``/`\` before
//! the payload is ever written, a delimiter that already appears in the body
//! ends the heredoc early (the remainder runs as commands), a heredoc nested in
//! `$( … )` makes the outer quote scan ambiguous, and a heredoc attached to a
//! process that also reads its script from stdin (`printf … | python3 - <<'PY'`)
//! silently discards the piped data. Every one of those exits 0.
//!
//! This module is a pure string→findings function: no clock, no filesystem, no
//! environment, which is what lets Miri prove it. The CLI surfaces it as
//! `unirun lint`, and `unirun run|ssh|winrm` + the MCP `exec.*` tools consult it
//! before a command reaches `Command::spawn`.
//!
//! The scope is deliberately narrow: only *provably risky* shapes are reported.
//! A quoted heredoc with a unique delimiter is the correct spelling and is never
//! flagged, so the guard does not push callers toward rewriting working scripts.

use serde::Serialize;

/// Which risky shape was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// `<<EOF` (unquoted) with expandable text in the body.
    UnquotedDelimiter,
    /// The delimiter also shows up as a line after the heredoc closed: the
    /// body ended at the wrong place and the rest ran as commands.
    DelimiterReused,
    /// A heredoc inside `$( … )`, `"…"` or backticks.
    NestedInSubstitution,
    /// No closing delimiter line before the end of the input.
    Unterminated,
    /// A pipeline feeds stdin to a process that already reads its script from
    /// the heredoc (`python3 - <<'PY'`, `sh -s <<'SH'`).
    StdinConflict,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::UnquotedDelimiter => "unquoted-delimiter",
            Kind::DelimiterReused => "delimiter-reused",
            Kind::NestedInSubstitution => "nested-in-substitution",
            Kind::Unterminated => "unterminated",
            Kind::StdinConflict => "stdin-conflict",
        }
    }
}

/// One risky heredoc shape.
#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub kind: Kind,
    /// 1-based line of the `<<` operator.
    pub line: usize,
    pub delimiter: String,
    /// What goes wrong (the mechanism, not just the label).
    pub detail: String,
    /// The substitution that removes the risk.
    pub fix: String,
}

impl Finding {
    /// One human-readable block, used by the CLI and the MCP error text.
    pub fn render(&self) -> String {
        format!(
            "line {}: {} — {}\n  fix: {}",
            self.line,
            self.kind.as_str(),
            self.detail,
            self.fix
        )
    }
}

/// What `--heredoc` (and `UNIRUN_HEREDOC` for MCP) asks the guard to do.
///
/// There is deliberately no `rewrite` mode: quoting an unquoted delimiter stops
/// expansion, which changes semantics whenever the body really does want
/// `$VAR`. A guard that silently rewrites that would be the same class of bug it
/// is meant to prevent, so the caller chooses between "refuse" and "run anyway".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardMode {
    /// Refuse to run (default).
    Deny,
    /// Print the findings to stderr, then run.
    Warn,
    /// Do not scan.
    Allow,
}

impl GuardMode {
    pub fn parse(s: &str) -> Option<GuardMode> {
        match s {
            "deny" => Some(GuardMode::Deny),
            "warn" => Some(GuardMode::Warn),
            "allow" => Some(GuardMode::Allow),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GuardMode::Deny => "deny",
            GuardMode::Warn => "warn",
            GuardMode::Allow => "allow",
        }
    }

    /// Resolve from the environment, falling back to the given default.
    pub fn from_env(default: GuardMode) -> GuardMode {
        std::env::var("UNIRUN_HEREDOC")
            .ok()
            .and_then(|v| GuardMode::parse(v.trim()))
            .unwrap_or(default)
    }
}

impl Default for GuardMode {
    fn default() -> Self {
        GuardMode::Deny
    }
}

#[derive(Debug, Clone)]
struct Op {
    line: usize,
    body_start: usize,
    delimiter: String,
    quoted: bool,
    strip_tabs: bool,
    stdin_conflict: bool,
}

fn is_tok_end(b: u8) -> bool {
    matches!(
        b,
        b' ' | b'\t' | b'\r' | b'\n' | b';' | b'|' | b'&' | b'<' | b'>' | b'(' | b')' | b'`'
    )
}

/// 1-based line number and byte offset of the start of that line.
fn line_of(cmd: &str, pos: usize) -> (usize, usize) {
    let mut line = 1usize;
    let mut start = 0usize;
    for (i, b) in cmd.as_bytes().iter().enumerate().take(pos.min(cmd.len())) {
        if *b == b'\n' {
            line += 1;
            start = i + 1;
        }
    }
    (line, start)
}

/// True when the text immediately before `<<` is a pipeline feeding a process
/// that reads its program from stdin (`python3 -`, `sh -s`, …).
fn pipeline_into_stdin_reader(prefix: &str) -> bool {
    let cut = prefix
        .char_indices()
        .filter(|(_, c)| matches!(c, ';' | '|' | '&'))
        .last()
        .map(|(i, _)| i);
    let Some(cut) = cut else {
        return false;
    };
    let boundary = prefix.as_bytes()[cut];
    let before = prefix.as_bytes().get(cut.wrapping_sub(1)).copied();
    let after = prefix.as_bytes().get(cut + 1).copied();
    let single_pipe = boundary == b'|' && before != Some(b'|') && after != Some(b'|');
    if !single_pipe {
        return false;
    }
    let command = &prefix[cut + 1..];
    let mut tokens = command.split_whitespace();
    let Some(first) = tokens.next() else {
        return false;
    };
    let base = first.rsplit('/').next().unwrap_or(first);
    let interpreter = matches!(
        base,
        "sh" | "bash"
            | "zsh"
            | "ksh"
            | "dash"
            | "fish"
            | "python"
            | "python3"
            | "node"
            | "deno"
            | "bun"
            | "perl"
            | "ruby"
    );
    interpreter
        && tokens.any(|t| {
            t == "-" || t == "-s" || t == "--stdin" || t == "/dev/stdin" || t.ends_with("-s")
        })
}

fn parse_op(cmd: &str, at: usize, line: usize, line_start: usize) -> Option<Op> {
    let b = cmd.as_bytes();
    let mut j = at + 2;
    let mut strip_tabs = false;
    if j < b.len() && b[j] == b'-' {
        strip_tabs = true;
        j += 1;
    }
    while j < b.len() && (b[j] == b' ' || b[j] == b'\t') {
        j += 1;
    }
    if j >= b.len() {
        return None;
    }
    let (delimiter, quoted, after_tok) = if b[j] == b'\'' || b[j] == b'"' {
        let quote = b[j];
        let start = j + 1;
        let mut k = start;
        while k < b.len() && b[k] != quote {
            k += 1;
        }
        if k >= b.len() {
            return None;
        }
        (cmd[start..k].to_string(), true, k + 1)
    } else {
        let start = j;
        let mut k = start;
        while k < b.len() && !is_tok_end(b[k]) {
            k += 1;
        }
        if k == start {
            return None;
        }
        (cmd[start..k].to_string(), false, k)
    };
    // The body begins on the next line. No newline at all is not an early
    // return: an empty body makes the terminator search fail, which is the
    // honest `unterminated` finding.
    let mut nl = after_tok;
    while nl < b.len() && b[nl] != b'\n' {
        nl += 1;
    }
    let body_start = (nl + 1).min(b.len());
    Some(Op {
        line,
        body_start,
        delimiter,
        quoted,
        strip_tabs,
        stdin_conflict: pipeline_into_stdin_reader(&cmd[line_start..at]),
    })
}

/// First line equal to the delimiter; returns (terminator_line_start, next).
fn find_terminator(
    cmd: &str,
    body_start: usize,
    delim: &str,
    strip_tabs: bool,
) -> Option<(usize, usize)> {
    let b = cmd.as_bytes();
    let mut pos = body_start;
    while pos < b.len() {
        let mut end = pos;
        while end < b.len() && b[end] != b'\n' {
            end += 1;
        }
        let raw = &cmd[pos..end];
        let cmp = if strip_tabs {
            raw.trim_start_matches('\t')
        } else {
            raw
        };
        if cmp == delim {
            return Some((pos, (end + 1).min(b.len())));
        }
        pos = end + 1;
    }
    None
}

/// A later line equal to the delimiter means the body almost certainly closed
/// early (its content contained the delimiter) — but a *second* heredoc reusing
/// the same delimiter is legitimate, so stop at the next `<<`.
fn reused_delimiter_line(cmd: &str, after: usize, delim: &str) -> Option<usize> {
    let b = cmd.as_bytes();
    let mut pos = after;
    while pos < b.len() {
        let mut end = pos;
        while end < b.len() && b[end] != b'\n' {
            end += 1;
        }
        let raw = &cmd[pos..end];
        if raw == delim {
            return Some(line_of(cmd, pos).0);
        }
        if raw.contains("<<") {
            return None;
        }
        pos = end + 1;
    }
    None
}

/// Scan a command or script body for risky heredoc shapes.
///
/// Empty result = nothing provably risky (which is not the same as "no heredoc":
/// a quoted heredoc with a unique delimiter is the correct spelling).
pub fn scan(cmd: &str) -> Vec<Finding> {
    let b = cmd.as_bytes();
    let mut out: Vec<Finding> = Vec::new();
    let mut i = 0usize;
    let mut line = 1usize;
    let mut line_start = 0usize;
    let mut subst = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;

    while i < b.len() {
        let c = b[i];
        if in_single {
            match c {
                b'\'' => in_single = false,
                b'\n' => {
                    line += 1;
                    line_start = i + 1;
                }
                _ => {}
            }
            i += 1;
            continue;
        }
        if in_double {
            match c {
                b'\\' => {
                    i += 2;
                    continue;
                }
                b'"' => in_double = false,
                b'$' if i + 1 < b.len() && b[i + 1] == b'(' => {
                    subst += 1;
                    i += 2;
                    continue;
                }
                b')' if subst > 0 => subst -= 1,
                b'\n' => {
                    line += 1;
                    line_start = i + 1;
                }
                _ => {}
            }
            i += 1;
            continue;
        }
        if in_backtick {
            match c {
                b'\\' => {
                    i += 2;
                    continue;
                }
                b'`' => in_backtick = false,
                b'\n' => {
                    line += 1;
                    line_start = i + 1;
                }
                _ => {}
            }
            i += 1;
            continue;
        }
        match c {
            b'\\' => {
                i += 2;
                continue;
            }
            b'\'' => {
                in_single = true;
                i += 1;
            }
            b'"' => {
                in_double = true;
                i += 1;
            }
            b'`' => {
                in_backtick = true;
                i += 1;
            }
            b'$' if i + 1 < b.len() && b[i + 1] == b'(' => {
                subst += 1;
                i += 2;
            }
            b')' if subst > 0 => {
                subst -= 1;
                i += 1;
            }
            b'\n' => {
                line += 1;
                line_start = i + 1;
                i += 1;
            }
            b'<' if i + 1 < b.len() && b[i + 1] == b'<' => {
                // `<<<` is a here-string, not a heredoc.
                if i + 2 < b.len() && b[i + 2] == b'<' {
                    i += 3;
                    continue;
                }
                let nested = subst > 0 || in_double || in_backtick;
                let Some(op) = parse_op(cmd, i, line, line_start) else {
                    i += 2;
                    continue;
                };
                match find_terminator(cmd, op.body_start, &op.delimiter, op.strip_tabs) {
                    Some((term_start, after)) => {
                        let body = &cmd[op.body_start..term_start];
                        let expandable =
                            body.contains('$') || body.contains('`') || body.contains('\\');
                        if !op.quoted && expandable {
                            out.push(Finding {
                                kind: Kind::UnquotedDelimiter,
                                line: op.line,
                                delimiter: op.delimiter.clone(),
                                detail: format!(
                                    "delimiter `{}` is unquoted, so $VAR / `cmd` / \\ are expanded before the body is written",
                                    op.delimiter
                                ),
                                fix: "quote it as <<'DELIM' when the body is literal, or move the payload into a file and run `bash file.sh` / `python3 file.py`".into(),
                            });
                        }
                        if nested {
                            out.push(Finding {
                                kind: Kind::NestedInSubstitution,
                                line: op.line,
                                delimiter: op.delimiter.clone(),
                                detail: "a heredoc inside `$( … )`, \"…\" or backticks makes the outer quote scan ambiguous".into(),
                                fix: "put the inner script in a file and call it by path; do not nest heredocs in a substitution".into(),
                            });
                        }
                        if op.stdin_conflict {
                            out.push(Finding {
                                kind: Kind::StdinConflict,
                                line: op.line,
                                delimiter: op.delimiter.clone(),
                                detail: "a pipeline feeds stdin to a process that already reads its script from this heredoc — the piped data is discarded".into(),
                                fix: "pass the data as a file or argv instead (`python3 script.py < data.json`), or keep the heredoc and drop the pipe".into(),
                            });
                        }
                        if let Some(reuse_line) = reused_delimiter_line(cmd, after, &op.delimiter) {
                            out.push(Finding {
                                kind: Kind::DelimiterReused,
                                line: reuse_line,
                                delimiter: op.delimiter.clone(),
                                detail: format!(
                                    "delimiter `{}` also appears as a line after the heredoc closed at line {} — the body most likely ended early and the rest ran as commands",
                                    op.delimiter, op.line
                                ),
                                fix: "use a unique delimiter such as <<'UNIRUN_INPUT' so body content cannot close it".into(),
                            });
                        }
                        i = after;
                        let (l, ls) = line_of(cmd, after);
                        line = l;
                        line_start = ls;
                    }
                    None => {
                        out.push(Finding {
                            kind: Kind::Unterminated,
                            line: op.line,
                            delimiter: op.delimiter.clone(),
                            detail: format!(
                                "no line matching `{}` closes this heredoc — the body swallows the rest of the input",
                                op.delimiter
                            ),
                            fix: "add the closing delimiter line, or move the payload into a file".into(),
                        });
                        break;
                    }
                }
            }
            _ => i += 1,
        }
    }
    out.sort_by_key(|f| f.line);
    out
}

/// `true` when the input carries a shape the guard is meant to stop.
pub fn is_risky(cmd: &str) -> bool {
    !scan(cmd).is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(cmd: &str) -> Vec<&'static str> {
        scan(cmd).iter().map(|f| f.kind.as_str()).collect()
    }

    #[test]
    fn quoted_unique_delimiter_is_clean() {
        let cmd =
            "cat > note.txt <<'UNIRUN_INPUT'\nprice is $100 and `not a command`\nUNIRUN_INPUT\n";
        assert!(scan(cmd).is_empty(), "{:?}", kinds(cmd));
    }

    #[test]
    fn unquoted_delimiter_with_expansion_is_risky() {
        let cmd = "cat > note.txt <<EOF\nprice is $PRICE\nEOF\n";
        assert_eq!(kinds(cmd), vec!["unquoted-delimiter"]);
    }

    #[test]
    fn unquoted_delimiter_without_expansion_is_not_flagged() {
        let cmd = "cat > note.txt <<EOF\nplain words only\nEOF\n";
        assert!(scan(cmd).is_empty(), "{:?}", kinds(cmd));
    }

    #[test]
    fn unterminated_heredoc_is_risky() {
        let cmd = "cat > note.txt <<'EOF'\nno closing line\n";
        assert_eq!(kinds(cmd), vec!["unterminated"]);
    }

    #[test]
    fn delimiter_reused_after_close_is_risky() {
        let cmd = "cat > note.txt <<'EOF'\nfirst\nEOF\nsecond\nEOF\n";
        assert_eq!(kinds(cmd), vec!["delimiter-reused"]);
    }

    #[test]
    fn second_heredoc_reusing_the_delimiter_is_not_flagged() {
        let cmd = "cat > a <<'EOF'\none\nEOF\ncat > b <<'EOF'\ntwo\nEOF\n";
        assert!(scan(cmd).is_empty(), "{:?}", kinds(cmd));
    }

    #[test]
    fn heredoc_inside_command_substitution_is_risky() {
        let cmd = "x=$(cat <<'EOF'\nbody\nEOF\n)";
        assert!(
            kinds(cmd).contains(&"nested-in-substitution"),
            "{:?}",
            kinds(cmd)
        );
    }

    #[test]
    fn pipe_into_stdin_reading_interpreter_is_risky() {
        let cmd = "printf 'a' | python3 - <<'PY'\nprint(1)\nPY\n";
        assert_eq!(kinds(cmd), vec!["stdin-conflict"]);
    }

    #[test]
    fn heredoc_piped_into_another_command_is_clean() {
        let cmd = "cat <<'EOF' | grep x\nx\nEOF\n";
        assert!(scan(cmd).is_empty(), "{:?}", kinds(cmd));
    }

    #[test]
    fn here_string_is_not_a_heredoc() {
        assert!(scan("grep foo <<< \"$text\"").is_empty());
    }

    #[test]
    fn guard_mode_parses_and_rejects_unknown() {
        assert_eq!(GuardMode::parse("deny"), Some(GuardMode::Deny));
        assert_eq!(GuardMode::parse("allow"), Some(GuardMode::Allow));
        assert_eq!(GuardMode::parse("rewrite"), None);
    }
}
