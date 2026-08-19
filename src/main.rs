//! unirun CLI: `run`, `script`, `probe`, `ssh`, `mcp`.
//!
//! Exit-code contract (non-JSON mode):
//!   child rc        → same rc
//!   timed out       → 124 (GNU timeout convention)
//!   aborted         → 130 (SIGINT convention)
//!   usage error     → 2
//!   internal error  → 1
//! In `--json` mode the process exits 0 whenever unirun itself ran; the full
//! normalized result (including `exit_code`/`error_class`) is in the JSON.

use std::path::PathBuf;
use std::process::ExitCode;
use unirun::exec::install_sigint_handler;
use unirun::recipe::Recipe;
use unirun::spec::{ExecKind, ExecResult, ExecSpec, Shell};

const HELP: &str = "\
unirun — cross-platform command execution normalization for AI agents

USAGE:
  unirun run '<command>' [options]      run a command through a shell
  unirun script <file> [options]        run a script file (shell by extension)
  unirun probe [--json]                 show host capabilities
  unirun mcp                            serve the MCP protocol over stdio
  unirun ssh <host> '<script>' [opts]  run a script on a remote Windows host
  unirun --version | --help

OPTIONS:
  --timeout <sec>    deadline in seconds (default 120)
  --shell <name>     bash | sh | zsh | cmd | powershell | pwsh
  --workdir <dir>    working directory
  --env K=V          environment override (repeatable)
  --toolchain <name> run via a recipe toolchain runner (e.g. python -> uv run)
  --json             emit the normalized result as JSON (agent mode)
  --pretty           pretty-print JSON (implies --json)

Projects may ship a `.unirun/recipe.toml`; run/script auto-apply its
timeout and output conventions, and `--toolchain` resolves its runners.
";

fn main() -> ExitCode {
    install_sigint_handler();
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", HELP);
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("unirun {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    let Some(sub) = args.first().map(|s| s.as_str()) else {
        eprintln!("{}", HELP);
        return ExitCode::from(2);
    };

    match sub {
        "run" => cmd_run(&args[1..]),
        "script" => cmd_script(&args[1..]),
        "probe" => cmd_probe(&args[1..]),
        "ssh" => cmd_ssh(&args[1..]),
        "mcp" => {
            if let Err(e) = unirun::mcp::serve() {
                eprintln!("unirun mcp: {}", e);
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("unirun: unknown subcommand `{}`", other);
            eprintln!("{}", HELP);
            ExitCode::from(2)
        }
    }
}

#[derive(Default)]
struct CliOpts {
    timeout_sec: Option<u64>,
    shell: Option<Shell>,
    workdir: Option<PathBuf>,
    env: Vec<(String, String)>,
    toolchain: Option<String>,
    json: bool,
    pretty: bool,
}

/// Parse `--flag value` pairs; positional args are returned in order.
fn parse_flags(args: &[String], opts: &mut CliOpts) -> Result<Vec<String>, String> {
    let mut positional = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "--json" => opts.json = true,
            "--pretty" => {
                opts.json = true;
                opts.pretty = true;
            }
            "--timeout" => {
                i += 1;
                let v = args.get(i).ok_or("--timeout needs a value")?;
                opts.timeout_sec = Some(
                    v.parse()
                        .map_err(|_| "invalid --timeout (integer seconds)")?,
                );
            }
            "--shell" => {
                i += 1;
                let v = args.get(i).ok_or("--shell needs a value")?;
                opts.shell =
                    Some(Shell::from_name(v).ok_or_else(|| format!("unknown shell `{}`", v))?);
            }
            "--workdir" => {
                i += 1;
                let v = args.get(i).ok_or("--workdir needs a value")?;
                opts.workdir = Some(PathBuf::from(v));
            }
            "--env" => {
                i += 1;
                let v = args.get(i).ok_or("--env needs K=V")?;
                let (k, val) = v.split_once('=').ok_or("--env must be K=V")?;
                opts.env.push((k.to_string(), val.to_string()));
            }
            "--toolchain" => {
                i += 1;
                let v = args.get(i).ok_or("--toolchain needs a value")?;
                opts.toolchain = Some(v.clone());
            }
            _ => positional.push(a.clone()),
        }
        i += 1;
    }
    Ok(positional)
}

fn build_spec(command: String, kind: ExecKind, opts: &CliOpts) -> ExecSpec {
    let mut spec = ExecSpec {
        command,
        kind,
        shell: opts.shell,
        workdir: opts.workdir.clone(),
        env: opts.env.clone(),
        timeout_ms: opts.timeout_sec.map(|s| s * 1000).unwrap_or(0),
        ..Default::default()
    };
    // Per-project adaptation: auto-apply recipe defaults when the caller did
    // not override them.
    let base = opts
        .workdir
        .clone()
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
    if let Some(recipe) = Recipe::load_from_dir(&base) {
        if spec.timeout_ms == 0 {
            if let Some(t) = recipe.default_timeout_ms() {
                spec.timeout_ms = t;
            }
        }
        if spec.max_output_bytes == 0 {
            if let Some(m) = recipe.max_output_bytes() {
                spec.max_output_bytes = m as usize;
            }
        }
    }
    spec
}

/// Resolve a recipe toolchain to a direct argv (`runner args… <file>`).
fn toolchain_argv(recipe: &Recipe, name: &str, file: &str) -> Result<Vec<String>, String> {
    recipe
        .resolve_toolchain(name)
        .map(|(runner, args)| {
            let mut argv = vec![runner];
            argv.extend(args);
            argv.push(file.to_string());
            argv
        })
        .ok_or_else(|| format!("toolchain `{}` not resolvable on this host", name))
}

fn emit(result: &ExecResult, opts: &CliOpts, pretty: bool) -> ExitCode {
    if opts.json || pretty {
        let out = if pretty {
            serde_json::to_string_pretty(result).unwrap_or_default()
        } else {
            serde_json::to_string(result).unwrap_or_default()
        };
        println!("{}", out);
        ExitCode::SUCCESS
    } else {
        if !result.stdout.is_empty() {
            print!("{}", result.stdout);
            if !result.stdout.ends_with('\n') {
                println!();
            }
        }
        if !result.stderr.is_empty() {
            eprint!("{}", result.stderr);
            if !result.stderr.ends_with('\n') {
                eprintln!();
            }
        }
        if result.timed_out {
            return ExitCode::from(124);
        }
        if result.aborted {
            return ExitCode::from(130);
        }
        match result.exit_code {
            Some(0) => ExitCode::SUCCESS,
            Some(rc) => ExitCode::from(rc.min(255) as u8),
            None => ExitCode::from(1),
        }
    }
}

fn cmd_run(args: &[String]) -> ExitCode {
    let mut opts = CliOpts::default();
    let positional = match parse_flags(args, &mut opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("unirun: {}", e);
            return ExitCode::from(2);
        }
    };
    if positional.is_empty() {
        eprintln!("unirun run: missing command");
        return ExitCode::from(2);
    }
    let command = positional.join(" ");
    let spec = build_spec(command, ExecKind::Run, &opts);
    let result = unirun::run(&spec);
    emit(&result, &opts, opts.pretty)
}

fn cmd_script(args: &[String]) -> ExitCode {
    let mut opts = CliOpts::default();
    let positional = match parse_flags(args, &mut opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("unirun: {}", e);
            return ExitCode::from(2);
        }
    };
    let Some(path) = positional.first() else {
        eprintln!("unirun script: missing file");
        return ExitCode::from(2);
    };
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("unirun script: cannot read `{}`: {}", path, e);
            return ExitCode::from(2);
        }
    };
    // Shell inference by extension when not explicitly overridden.
    if opts.shell.is_none() {
        opts.shell = Shell::from_path(PathBuf::from(path).as_path());
    }
    let mut spec = build_spec(content, ExecKind::Script, &opts);
    // --toolchain: run through a recipe runner instead of a shell.
    if let Some(tc) = &opts.toolchain {
        let base = opts
            .workdir
            .clone()
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let recipe = Recipe::load_from_dir(&base).unwrap_or_default();
        match toolchain_argv(&recipe, tc, path) {
            Ok(argv) => {
                spec.direct = Some(argv);
                spec.command.clear();
            }
            Err(e) => {
                eprintln!("unirun: {}", e);
                return ExitCode::from(2);
            }
        }
    }
    let result = unirun::run(&spec);
    emit(&result, &opts, opts.pretty)
}

fn cmd_ssh(args: &[String]) -> ExitCode {
    let mut opts = CliOpts::default();
    let positional = match parse_flags(args, &mut opts) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("unirun: {}", e);
            return ExitCode::from(2);
        }
    };
    if positional.len() < 2 {
        eprintln!("unirun ssh: usage: unirun ssh <host> '<script>' [--shell powershell|pwsh|cmd] [--timeout N]");
        return ExitCode::from(2);
    }
    let mut target = unirun::SshTarget {
        host: positional[0].clone(),
        ..Default::default()
    };
    if let Some(s) = opts.shell {
        target.shell = s;
    }
    if let Some(t) = opts.timeout_sec {
        target.timeout_ms = t * 1000;
    }
    let script = positional[1..].join(" ");
    let result = unirun::ssh_run(&target, &script);
    emit(&result, &opts, opts.pretty)
}

fn cmd_probe(args: &[String]) -> ExitCode {
    let json = args.iter().any(|a| a == "--json" || a == "--pretty");
    let caps = unirun::probe();
    if json {
        let out = if args.iter().any(|a| a == "--pretty") {
            serde_json::to_string_pretty(&caps).unwrap_or_default()
        } else {
            serde_json::to_string(&caps).unwrap_or_default()
        };
        println!("{}", out);
    } else {
        println!("platform: {} ({})", caps.platform, caps.arch);
        println!("shells:");
        for s in &caps.shells {
            let found = s.path.as_deref().unwrap_or("-");
            println!("  {:<12} {}", s.name, found);
        }
        println!(
            "timeout: {} (gnu available: {})",
            caps.coreutils.timeout.as_deref().unwrap_or("-"),
            caps.coreutils.gnu_timeout_available
        );
        println!("tools:");
        for t in &caps.tools {
            let found = t.path.as_deref().unwrap_or("-");
            println!("  {:<10} {}", t.name, found);
        }
    }
    ExitCode::SUCCESS
}
