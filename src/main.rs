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
  unirun acp                            serve the Agent Client Protocol over stdio
  unirun ssh <host> '<script>' [opts]  run a script on a remote host (Unix or Windows)
  unirun bg <start|status|output|kill|list|wait> ...   background sessions
  unirun recipe <list|show|add|rm|path|effective|check>   recipe registry
  unirun winrm <host> '<script>' [opts]  run a script via WinRM (feature: winrm)
  unirun --version | --help

OPTIONS:
  --timeout <sec>    deadline in seconds (default 120)
  --shell <name>     bash | sh | zsh | cmd | powershell | pwsh
  --workdir <dir>    working directory
  --env K=V          environment override (repeatable)
  --toolchain <name> run via a recipe toolchain runner (e.g. python -> uv run)
  --user <name>      SSH user for `unirun ssh` (user@host)
  --port <n>         SSH port for `unirun ssh` (default 22 / ssh config)
  --identity <file>  SSH identity file for `unirun ssh` (-i)
  --json             emit the normalized result as JSON (agent mode)
  --pretty           pretty-print JSON (implies --json)

Projects may ship a `.unirun/recipe.toml`; run/script auto-apply its
timeout and output conventions, and `--toolchain` resolves its runners.
Recipe layers: built-in defaults <- registry (extends) <- project recipe.
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
        "winrm" => cmd_winrm(&args[1..]),
        "recipe" => cmd_recipe(&args[1..]),
        "bg" => cmd_bg(&args[1..]),
        "__bg-runner" => {
            let Some(dir) = args.get(1).map(PathBuf::from) else {
                eprintln!("unirun __bg-runner: missing session dir");
                return ExitCode::from(2);
            };
            ExitCode::from(unirun::session::run_runner(&dir).min(255) as u8)
        }
        "mcp" => {
            if let Err(e) = unirun::mcp::serve() {
                eprintln!("unirun mcp: {}", e);
                return ExitCode::from(1);
            }
            ExitCode::SUCCESS
        }
        "acp" => {
            if let Err(e) = unirun::acp::serve() {
                eprintln!("unirun acp: {}", e);
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
    label: Option<String>,
    tail_bytes: Option<usize>,
    json: bool,
    pretty: bool,
    /// SSH-only: `unirun ssh` identity options.
    user: Option<String>,
    port: Option<u16>,
    identity: Option<PathBuf>,
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
            "--user" => {
                i += 1;
                let v = args.get(i).ok_or("--user needs a value")?;
                opts.user = Some(v.clone());
            }
            "--port" => {
                i += 1;
                let v = args.get(i).ok_or("--port needs a value")?;
                opts.port = Some(v.parse().map_err(|_| "invalid --port (integer)")?);
            }
            "--identity" | "--identity-file" => {
                i += 1;
                let v = args.get(i).ok_or("--identity needs a path")?;
                opts.identity = Some(PathBuf::from(v));
            }
            "--label" => {
                i += 1;
                let v = args.get(i).ok_or("--label needs a value")?;
                opts.label = Some(v.clone());
            }
            "--tail" => {
                i += 1;
                let v = args.get(i).ok_or("--tail needs a value")?;
                opts.tail_bytes = Some(v.parse().map_err(|_| "invalid --tail (byte count)")?);
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
        if spec.error_maps.is_empty() {
            spec.error_maps = recipe.error_maps.clone();
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
        eprintln!("unirun ssh: usage: unirun ssh <host> '<script>' [--shell bash|sh|zsh|powershell|pwsh|cmd] [--user U] [--port N] [--identity FILE] [--timeout N]");
        return ExitCode::from(2);
    }
    // Remote cwd/env are not yet supported — refuse loudly instead of
    // silently ignoring them.
    if opts.workdir.is_some() || !opts.env.is_empty() {
        eprintln!("unirun ssh: --workdir/--env are not supported for remote execution (yet)");
        return ExitCode::from(2);
    }
    let mut target = unirun::SshTarget {
        host: positional[0].clone(),
        user: opts.user.clone(),
        port: opts.port,
        identity_file: opts.identity.clone(),
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

const RECIPE_HELP: &str = "\
unirun recipe — user-level recipe registry

USAGE:
  unirun recipe list                     list registry recipes
  unirun recipe show <name> [--raw]      show a registry recipe (effective unless --raw)
  unirun recipe add <name> <file.toml>   add/overwrite a registry recipe
  unirun recipe rm <name>                remove a registry recipe
  unirun recipe path                     print the registry directory
  unirun recipe effective [--workdir d] [--json]   effective project recipe (extends resolved)
  unirun recipe check                    validate registry recipes (parse + extends cycles)

The registry lives in $UNIRUN_HOME/recipes (default ~/.unirun/recipes).
Project recipes opt in via `extends = [\"name\", ...]` (earlier = lower layer).
";

fn cmd_recipe(args: &[String]) -> ExitCode {
    use unirun::recipe::{effective_recipe, registry_dir, Recipe, RecipeRegistry};
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        eprintln!("{}", RECIPE_HELP);
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    match sub {
        "list" => {
            let entries = RecipeRegistry::list();
            if entries.is_empty() {
                println!("(no registry recipes in {})", registry_dir().display());
            }
            for (name, path) in entries {
                println!("{:<20} {}", name, path.display());
            }
            ExitCode::SUCCESS
        }
        "show" => {
            let raw = rest.iter().any(|a| a == "--raw");
            let Some(name) = rest.iter().find(|a| !a.starts_with('-')) else {
                eprintln!("unirun recipe show: missing recipe name");
                return ExitCode::from(2);
            };
            if raw {
                let path = registry_dir().join(format!("{}.toml", name));
                match std::fs::read_to_string(&path) {
                    Ok(text) => {
                        print!("{}", text);
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("unirun recipe show: {}", e);
                        ExitCode::from(2)
                    }
                }
            } else {
                match RecipeRegistry::load(name) {
                    Some(r) => {
                        let (eff, warnings) =
                            effective_recipe(&r, &mut |n| RecipeRegistry::load(n));
                        for w in warnings {
                            eprintln!("unirun: {}", w);
                        }
                        print!("{}", toml::to_string(&eff).unwrap_or_default());
                        ExitCode::SUCCESS
                    }
                    None => {
                        eprintln!("unirun recipe show: no registry recipe `{}`", name);
                        ExitCode::from(2)
                    }
                }
            }
        }
        "add" => {
            if rest.len() < 2 {
                eprintln!("unirun recipe add: usage: unirun recipe add <name> <file.toml>");
                return ExitCode::from(2);
            }
            match RecipeRegistry::add(&rest[0], std::path::Path::new(&rest[1])) {
                Ok(dest) => {
                    println!("added {}", dest.display());
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("unirun recipe add: {}", e);
                    ExitCode::from(2)
                }
            }
        }
        "rm" | "remove" => {
            let Some(name) = rest.first() else {
                eprintln!("unirun recipe rm: missing recipe name");
                return ExitCode::from(2);
            };
            match RecipeRegistry::remove(name) {
                Ok(()) => {
                    println!("removed `{}`", name);
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("unirun recipe rm: {}", e);
                    ExitCode::from(2)
                }
            }
        }
        "path" => {
            println!("{}", registry_dir().display());
            ExitCode::SUCCESS
        }
        "effective" => {
            let json = rest.iter().any(|a| a == "--json");
            let workdir = rest
                .iter()
                .position(|a| a == "--workdir")
                .and_then(|i| rest.get(i + 1))
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            match Recipe::load_from_dir(&workdir) {
                Some(eff) => {
                    let out = if json {
                        serde_json::to_string_pretty(&eff).unwrap_or_default()
                    } else {
                        toml::to_string(&eff).unwrap_or_default()
                    };
                    print!("{}", out);
                    ExitCode::SUCCESS
                }
                None => {
                    eprintln!(
                        "unirun recipe effective: no recipe found from {}",
                        workdir.display()
                    );
                    ExitCode::from(2)
                }
            }
        }
        "check" => match RecipeRegistry::check() {
            Ok(names) => {
                println!("recipe registry OK ({} recipe(s))", names.len());
                for n in names {
                    println!("  ok: {}", n);
                }
                ExitCode::SUCCESS
            }
            Err(errors) => {
                for e in errors {
                    eprintln!("unirun recipe check: {}", e);
                }
                ExitCode::from(1)
            }
        },
        other => {
            eprintln!("unirun recipe: unknown subcommand `{}`", other);
            eprintln!("{}", RECIPE_HELP);
            ExitCode::from(2)
        }
    }
}

const BG_HELP: &str = "\
unirun bg — background sessions (detached execution agents can poll)

USAGE:
  unirun bg start '<command>' [--shell s] [--workdir d] [--env K=V] [--timeout N] [--label L] [--json]
  unirun bg status <id> [--json]
  unirun bg output <id> [--tail N] [--json]
  unirun bg kill <id> [--json]
  unirun bg wait <id> [--timeout N] [--json]
  unirun bg list [--json]

Sessions live in $UNIRUN_HOME/sessions (default ~/.unirun/sessions).
Exit-code contract (non-JSON): completed rc mirrors the child; timed out 124;
aborted/killed 130; other terminal states 1.
";

fn cmd_bg(args: &[String]) -> ExitCode {
    use unirun::session as sess;
    let Some(sub) = args.first().map(|s| s.as_str()) else {
        eprintln!("{}", BG_HELP);
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    match sub {
        "start" => {
            let mut opts = CliOpts::default();
            let positional = match parse_flags(rest, &mut opts) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("unirun bg start: {}", e);
                    return ExitCode::from(2);
                }
            };
            if positional.is_empty() {
                eprintln!("unirun bg start: missing command");
                return ExitCode::from(2);
            }
            let command = positional.join(" ");
            let label = opts
                .label
                .clone()
                .unwrap_or_else(|| truncate_label(&command));
            let spec = unirun::spec::ExecSpec {
                command,
                shell: opts.shell,
                workdir: opts.workdir.clone(),
                env: opts.env.clone(),
                timeout_ms: opts.timeout_sec.map(|s| s * 1000).unwrap_or(0),
                ..Default::default()
            };
            match sess::start(&spec, &label) {
                Ok(st) => {
                    if opts.json {
                        println!("{}", serde_json::to_string(&st).unwrap_or_default());
                    } else {
                        println!(
                            "session {} started (pid {}) — {}",
                            st.id,
                            st.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into()),
                            label
                        );
                    }
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("unirun bg start: {}", e);
                    ExitCode::from(1)
                }
            }
        }
        "status" | "output" | "kill" | "wait" | "list" => {
            let mut opts = CliOpts::default();
            let positional = match parse_flags(rest, &mut opts) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("unirun bg {}: {}", sub, e);
                    return ExitCode::from(2);
                }
            };
            let id = positional.first().map(|s| s.as_str());
            match sub {
                "status" => match id {
                    Some(id) => match sess::status(id) {
                        Ok(st) => emit_state(&st, opts.json),
                        Err(e) => {
                            eprintln!("unirun bg status: {}", e);
                            ExitCode::from(2)
                        }
                    },
                    None => {
                        eprintln!("unirun bg status: missing session id");
                        ExitCode::from(2)
                    }
                },
                "output" => match id {
                    Some(id) => {
                        let tail = opts.tail_bytes.unwrap_or(65_536);
                        match sess::output(id, tail) {
                            Ok((so, se, truncated_log)) => {
                                if opts.json {
                                    println!(
                                        "{}",
                                        serde_json::json!({
                                            "id": id,
                                            "stdout": so,
                                            "stderr": se,
                                            "truncated_log": truncated_log,
                                        })
                                    );
                                } else {
                                    print!("{}", so);
                                    if !se.is_empty() {
                                        println!("--- stderr ---");
                                        print!("{}", se);
                                    }
                                    if truncated_log {
                                        eprintln!("(log truncated at {} bytes)", tail);
                                    }
                                }
                                ExitCode::SUCCESS
                            }
                            Err(e) => {
                                eprintln!("unirun bg output: {}", e);
                                ExitCode::from(2)
                            }
                        }
                    }
                    None => {
                        eprintln!("unirun bg output: missing session id");
                        ExitCode::from(2)
                    }
                },
                "kill" => match id {
                    Some(id) => match sess::kill(id) {
                        Ok(st) => emit_state(&st, opts.json),
                        Err(e) => {
                            eprintln!("unirun bg kill: {}", e);
                            ExitCode::from(2)
                        }
                    },
                    None => {
                        eprintln!("unirun bg kill: missing session id");
                        ExitCode::from(2)
                    }
                },
                "wait" => match id {
                    Some(id) => {
                        let timeout_ms = opts.timeout_sec.map(|s| s * 1000).unwrap_or(120_000);
                        match sess::wait(id, timeout_ms) {
                            Ok(st) => emit_state(&st, opts.json),
                            Err(e) => {
                                eprintln!("unirun bg wait: {}", e);
                                ExitCode::from(2)
                            }
                        }
                    }
                    None => {
                        eprintln!("unirun bg wait: missing session id");
                        ExitCode::from(2)
                    }
                },
                "list" => {
                    let all = sess::list();
                    if opts.json {
                        println!(
                            "{}",
                            serde_json::to_string(&all).unwrap_or_else(|_| "[]".into())
                        );
                    } else if all.is_empty() {
                        println!("(no sessions in {})", sess::sessions_dir().display());
                    } else {
                        for st in &all {
                            println!(
                                "{:<24} {:<11} exit={:<5} {}",
                                st.id,
                                st.status,
                                st.exit_code
                                    .map(|c| c.to_string())
                                    .unwrap_or_else(|| "-".into()),
                                st.label
                            );
                        }
                    }
                    ExitCode::SUCCESS
                }
                _ => unreachable!(),
            }
        }
        other => {
            eprintln!("unirun bg: unknown subcommand `{}`", other);
            eprintln!("{}", BG_HELP);
            ExitCode::from(2)
        }
    }
}

fn emit_state(st: &unirun::session::SessionState, json: bool) -> ExitCode {
    if json {
        println!("{}", serde_json::to_string(st).unwrap_or_default());
        return ExitCode::SUCCESS;
    }
    println!(
        "session {} — {} (exit {:?}, class {:?}) — {}",
        st.id, st.status, st.exit_code, st.error_class, st.label
    );
    match st.status.as_str() {
        "completed" => match st.exit_code {
            Some(0) => ExitCode::SUCCESS,
            Some(rc) => ExitCode::from(rc.min(255) as u8),
            None => ExitCode::from(1),
        },
        "timed_out" => ExitCode::from(124),
        "aborted" | "killed" => ExitCode::from(130),
        _ => ExitCode::from(1),
    }
}

fn truncate_label(command: &str) -> String {
    let flat: String = command.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s: String = flat.chars().take(60).collect();
    if flat.chars().count() > 60 {
        s.push('…');
    }
    s
}

#[cfg(feature = "winrm")]
const WINRM_HELP: &str = "\
unirun winrm — run PowerShell on a remote Windows host over WinRM (psrp-rs POC)

USAGE:
  unirun winrm <host> '<script>' [--user U] [--password P] [--domain D]
               [--port N] [--tls] [--insecure] [--auth basic|ntlm|kerberos]
               [--timeout N] [--json] [--pretty]

Defaults: HTTP port 5985, NTLM auth. Requires a `winrm`-feature build
(cargo install unirun --features winrm).
";

#[cfg(not(feature = "winrm"))]
fn cmd_winrm(_args: &[String]) -> ExitCode {
    eprintln!("unirun: built without WinRM support; rebuild with `cargo build --features winrm`");
    ExitCode::from(1)
}

#[cfg(feature = "winrm")]
fn cmd_winrm(args: &[String]) -> ExitCode {
    use unirun::winrm::{winrm_run, WinrmAuth, WinrmTarget};
    let mut target = WinrmTarget::default();
    let mut timeout_sec: Option<u64> = None;
    let mut json = false;
    let mut pretty = false;
    let mut positional: Vec<String> = Vec::new();
    let mut i = 0;
    let fail = |msg: &str| -> ExitCode {
        eprintln!("unirun winrm: {}", msg);
        ExitCode::from(2)
    };
    while i < args.len() {
        let a = &args[i];
        let next = |i: &mut usize| -> Option<&String> {
            *i += 1;
            args.get(*i)
        };
        match a.as_str() {
            "--json" => json = true,
            "--pretty" => {
                json = true;
                pretty = true;
            }
            "--user" => match next(&mut i) {
                Some(v) => target.username = v.clone(),
                None => return fail("--user needs a value"),
            },
            "--password" => match next(&mut i) {
                Some(v) => target.password = v.clone(),
                None => return fail("--password needs a value"),
            },
            "--domain" => match next(&mut i) {
                Some(v) => target.domain = v.clone(),
                None => return fail("--domain needs a value"),
            },
            "--port" => match next(&mut i).and_then(|v| v.parse().ok()) {
                Some(p) => target.port = p,
                None => return fail("--port needs an integer"),
            },
            "--tls" => target.use_tls = true,
            "--insecure" => target.accept_invalid_certs = true,
            "--auth" => match next(&mut i).and_then(|v| WinrmAuth::from_name(v)) {
                Some(auth) => target.auth = auth,
                None => return fail("--auth must be basic | ntlm | kerberos"),
            },
            "--timeout" => match next(&mut i).and_then(|v| v.parse().ok()) {
                Some(t) => timeout_sec = Some(t),
                None => return fail("--timeout needs integer seconds"),
            },
            other => positional.push(other.to_string()),
        }
        i += 1;
    }
    if positional.len() < 2 {
        eprintln!("{}", WINRM_HELP);
        return ExitCode::from(2);
    }
    target.host = positional[0].clone();
    let script = positional[1..].join(" ");
    if let Some(t) = timeout_sec {
        target.timeout_ms = t * 1000;
    }
    let result = winrm_run(&target, &script);
    let opts = CliOpts {
        json,
        pretty,
        ..Default::default()
    };
    emit(&result, &opts, pretty)
}
