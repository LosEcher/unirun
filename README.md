# unirun

**Cross-platform command execution normalization for AI agents.**

One command spec in, one normalized result out — on any platform
(macOS / Linux / Windows, local or SSH-remote), regardless of shell,
encoding, or toolchain quirks. Built for agents that need to *act* on
command output, not decode it.

```
$ unirun run 'echo "中文OK"; exit 42' --json
{"exit_code":42,"stdout":"中文OK\n","stderr":"","timed_out":false,"aborted":false,
 "duration_ms":12,"error_class":null,"hint":null,"encoding":"utf-8","truncated":false,
 "shell_used":"bash"}
```

## Why

Every agent harness (Claude Code, Codex, Cursor, DSH, opencode, …) silently
re-solves the same cross-platform execution matrix:

- `timeout` doesn't exist on stock macOS (GNU coreutils only);
- `uname` doesn't exist on Windows OpenSSH;
- PowerShell 5.1 emits CLIXML / GBK mojibake instead of UTF-8;
- cmd.exe eats `>` before PowerShell ever sees it;
- PowerShell doesn't propagate native exit codes;
- Windows' `System32\bash.exe` is the WSL launcher — with no distro installed
  it prints a UTF-16LE "no distributions" message and exits 1 (unirun's probe
  excludes it; real Git Bash still resolves);
- output formats drift between OS versions (`iostat`, `netstat`, …).

unirun makes this matrix a solved, tested, shared problem:

- **argv, never hand-quoted strings** — script content reaches the shell as a
  single `-c` argument; no outer quoting layer can corrupt it.
- **In-process deadline** — no dependency on a GNU `timeout` binary; timeout
  is enforced by unirun itself, with whole-tree termination
  (POSIX: negative-pgid SIGTERM→SIGKILL; Windows: `taskkill /T /F`).
- **Stable error taxonomy** — `TIMEOUT`, `COMMAND_NOT_FOUND`, `PERMISSION`,
  `DEPENDENCY_MISSING`, `SYNTAX`, … plus actionable `hint`s, so an agent can
  take its next step instead of guessing.
- **Encoding pipeline** — BOM sniffing (UTF-8/UTF-16LE/UTF-16BE), clean
  UTF-8 fast path, lossy fallback labeled `utf-8-lossy`.
- **Capped tail-keeping output** — bounded, drained (no pipe deadlock), tail
  kept and marked `truncated` — errors cluster at the end.
- **SIGINT = abort** — an in-flight process tree is terminated and the result
  reports `aborted: true` (agent-safe retry).
- **`unirun probe`** — the agent's first question answered: what shells,
  coreutils and tools actually exist here.

## Install

Prebuilt binaries are attached to each [GitHub Release](https://github.com/LosEcher/unirun/releases).
Assets follow a fixed `unirun-<os>-<arch>` naming contract so installers
(Dockerfiles, deploy scripts, agent toolchains) can resolve the right file
deterministically — never a bare `unirun`:

| Asset | Platform |
|---|---|
| `unirun-linux-x86_64` | Linux x86_64 (glibc) |
| `unirun-linux-x86_64-musl` | Linux x86_64, static musl — Alpine / distroless Docker |
| `unirun-linux-aarch64-musl` | Linux arm64, static musl — ARM NAS, Raspberry Pi |
| `unirun-macos-aarch64` | macOS Apple Silicon |
| `unirun-windows-x86_64.exe` | Windows x86_64 |

```bash
# Linux/macOS — pick the asset for your platform
curl -fL -o unirun \
  https://github.com/LosEcher/unirun/releases/latest/download/unirun-linux-x86_64
chmod +x unirun && ./unirun probe
```

Alpine/Docker install step (static musl build — no glibc needed):

```dockerfile
FROM alpine:3.20
RUN apk add --no-cache ca-certificates \
 && wget -qO /usr/local/bin/unirun \
      https://github.com/LosEcher/unirun/releases/latest/download/unirun-linux-x86_64-musl \
 && chmod +x /usr/local/bin/unirun
```

Or from source (any platform, including macOS Intel):

```bash
# From source (Rust 1.70+)
cargo install unirun

# Or build locally
git clone https://github.com/LosEcher/unirun.git
cd unirun && cargo build --release
# binary at target/release/unirun
```

## Usage

```bash
unirun run '<command>' [--timeout 30] [--shell bash] [--workdir dir] [--env K=V]
unirun script path/to/script [options]     # shell inferred from extension
unirun probe [--json]                      # host capability snapshot
unirun mcp                                 # serve MCP over stdio (agents)
unirun acp                                 # serve Agent Client Protocol v1 over stdio
unirun ssh <host> '<script>' [--shell powershell|pwsh|cmd]   # remote Windows (SSH)
unirun winrm <host> '<script>' [opts]      # remote Windows (WinRM; feature: winrm)
unirun bg <start|status|output|kill|wait|list> ...   # background sessions
unirun recipe <list|show|add|rm|path|effective|check> # recipe registry
```

### MCP — plug into any agent

`unirun mcp` is a stdio MCP server exposing `exec.run`, `exec.script`,
`exec.probe` and the background-session tools `session.start`, `session.status`,
`session.output`, `session.kill`, `session.wait`, `session.list`
(JSON-RPC 2.0, newline-delimited). Point any MCP-capable agent at it:

```json
// Claude Desktop / Cursor / DSH mcp config
{ "mcpServers": { "unirun": { "command": "unirun", "args": ["mcp"] } } }
```

Tools return the normalized `ExecResult` JSON (exit_code / error_class /
hint / encoding / truncated), with `isError` reflecting the taxonomy class.

### ACP — drive it from Zed / Cursor / any ACP v1 client

`unirun acp` is an [Agent Client Protocol](https://agentclientprotocol.com)
v1 server over stdio (baseline surface: `initialize`, `session/new`,
`session/prompt` with streamed `session/update` output, `session/cancel`).
A client sends the command as the prompt text — or as a JSON spec
(`{"command": "…", "shell": "…", "timeout": 30, "workdir": "…", "env": {…}}`) —
and receives the normalized result as the final streamed chunk. Any ACP v1
host (Zed, Cursor, …) can be pointed at `unirun acp` as a "run this command"
agent.

### Background sessions

Long-running work without blocking the agent loop — start, poll, kill,
collect output later:

```bash
$ unirun bg start 'pnpm build' --label web-build
session 862718cd5514c7ea4d380 started (pid 12345) — web-build

$ unirun bg status 862718cd5514c7ea4d380          # running | completed | timed_out | aborted | …
$ unirun bg wait 862718cd5514c7ea4d380 --timeout 300 --json
$ unirun bg output 862718cd5514c7ea4d380 --tail 65536
$ unirun bg kill 862718cd5514c7ea4d380
$ unirun bg list
```

Sessions live in `$UNIRUN_HOME/sessions` (default `~/.unirun/sessions`),
survive the launching CLI, and stream decoded output to `stdout.log` /
`stderr.log` (1 MiB cap, flagged `truncated_log`). The same API is exposed
as MCP `session.*` tools. Exit-code contract for `bg wait` mirrors the CLI:
completed rc mirrors the child, timed out 124, aborted/killed 130.

### Remote Windows execution

**SSH** (`unirun ssh`) is the win-exec knowledge ported to Rust: UTF-16LE
`-EncodedCommand` payloads, auto-injected UTF-8 "golden recipe" (no
CLIXML/GBK mojibake), exact exit-code propagation via `exit $LASTEXITCODE`,
and automatic scp + `-File` fallback for large scripts (UTF-8 BOM so Chinese
content works). cmd.exe targets run via temp `.bat` files.

**WinRM** (`unirun winrm`, build with `--features winrm`) is a POC over
[psrp-rs](https://crates.io/crates/psrp-rs) (PowerShell Remoting Protocol on
WS-Management) for hosts that expose WinRM instead of OpenSSH — HTTP 5985 /
HTTPS 5986, Basic / NTLM / Kerberos auth:

```bash
unirun winrm win-srv 'Get-Process | Select-Object -First 5 Name, Id' \
  --user administrator --password '…' --tls --insecure
```

Output arrives CLIXML-decoded (clean UTF-8) and the exit code is propagated
via a `$LASTEXITCODE` sentinel; the PSRP error stream becomes stderr, so the
normalized `ExecResult` and taxonomy apply. Known PSRP limit: a top-level
`exit N` terminates the runspace before the sentinel runs — prefer native
commands (e.g. `cmd /c exit N`) to set `$LASTEXITCODE`.

### Per-project adaptation (recipes)

Ship a `.unirun/recipe.toml` in a project and unirun auto-applies it
(toolchain runners, timeouts, output caps, **error maps**):

```toml
schema = 1
extends = ["python"]               # layer a registry recipe underneath (optional)

[toolchains.python]
runner = "uv"
fallbacks = ["python3", "py"]
args = ["run"]

[toolchains.node]
runner = "pnpm"
fallbacks = ["npm", "yarn"]

[conventions]
max_output_bytes = 262144

[timeouts]
default_ms = 30000

[error_maps]                       # project patterns win over the built-in library
"ModuleNotFoundError: *" = { class = "DEPENDENCY_MISSING", hint = "run `uv sync`" }
```

```bash
unirun script main.py --toolchain python --json   # → uv run main.py
```

**Recipe registry** (`unirun recipe`) keeps reusable named recipes in
`$UNIRUN_HOME/recipes` and layers them via `extends` (deep merge: built-in
defaults ← registry layers ← project recipe; `toolchains`/`error_maps` merge
per key, `conventions`/`timeouts` overlay per present field):

```bash
unirun recipe add python ~/dotfiles/recipes/python.toml   # register once
unirun recipe list / show python / rm python / path / check
unirun recipe effective --workdir .                        # merged project recipe
```

Capability results are cached at `.unirun/capabilities.json` with a drift
check (platform + shell paths), so agents stop hitting "it worked yesterday".


### Exit-code contract (non-JSON mode)

| Situation            | Exit code |
|----------------------|-----------|
| child exit code      | same rc   |
| timed out            | 124       |
| aborted (SIGINT)     | 130       |
| usage error          | 2         |
| internal error       | 1         |

In `--json` mode unirun exits 0 whenever it itself ran; the full normalized
result (including `exit_code` and `error_class`) is in the JSON.

### Error taxonomy (`error_class`)

| Class                | When                                            |
|----------------------|-------------------------------------------------|
| `TIMEOUT`            | deadline elapsed, tree terminated               |
| `ABORTED`            | cancelled by caller / SIGINT                    |
| `COMMAND_NOT_FOUND`  | exit 127, or "command not found"                |
| `PERMISSION`         | exit 126 / "Permission denied"                  |
| `NOT_FOUND`          | "No such file or directory"                     |
| `DEPENDENCY_MISSING` | ModuleNotFoundError and friends                 |
| `SYNTAX`             | shell syntax error                              |
| `NETWORK`            | unreachable host / repo / registry (P2)         |
| `COMPILE_ERROR`      | compiler/toolchain diagnostics (P2)             |
| `UNKNOWN_FAILURE`    | non-zero exit with unrecognized stderr          |
| *(none)*             | success, or explicit non-zero exit w/o evidence |

Classification is **evidence-based**: unirun never invents a class without a
confirmed pattern, so an explicit `exit 42` stays class-less (rc is the signal).
Built-in patterns live in the public **error-map library** (`unirun::error_maps`,
curated for Python / Node / Rust / Go / TypeScript / Git / network), matched as
case-insensitive substrings on whitespace-flattened stderr; project recipe
`[error_maps]` patterns are consulted first (project knowledge beats generic
heuristics).

### Performance benchmarks

Library-level (criterion, `cargo bench`) on this M1 Mac — the full pipeline:
spawn + process-group setup, streaming capture, encoding, classification:

| Benchmark                     | Time            |
|-------------------------------|-----------------|
| `true` through bash           | ~23 ms          |
| `echo hello`                  | ~23 ms          |
| Unicode output (中文)         | ~23 ms          |
| 10 MiB output (capped drain)  | ~35 ms          |
| Timeout + tree-kill latency   | ~153 ms (100 ms deadline + kill) |

End-to-end CLI (`scripts/bench.sh`, release binary, hyperfine if present else
a python timing loop): a bare `unirun run 'echo hello'` lands at ~16 ms on
this host — the normalization pipeline is effectively free next to the shell
spawn itself.

## Roadmap

- **P0 (released)** — local execution normalization: probe, in-process
  timeout, tree kill, encoding pipeline, taxonomy, CLI `--json`.
- **P1 (released)** — MCP server, SSH-remote transport (win-exec port),
  per-project recipe system, Windows CI matrix.
- **P2 (released)** — error-map library, recipe registry (`extends` +
  `unirun recipe`), background sessions (`unirun bg` + MCP `session.*`),
  ACP v1 server (`unirun acp`), WinRM provider (psrp-rs POC, `winrm`
  feature), performance benchmarks.
- **P3 (backlog)** — Windows local execution polish, session resume/replay,
  per-stream caps, recipe schema registry (semver'd), transport plugins.

Independent by design: MCP + CLI only, no harness dependency, no telemetry,
MIT licensed.

## License

MIT © 2026 LosEcher
