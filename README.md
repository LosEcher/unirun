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
unirun ssh <host> '<script>' [--shell powershell|pwsh|cmd]   # remote Windows
```

### MCP — plug into any agent

`unirun mcp` is a stdio MCP server exposing `exec.run`, `exec.script` and
`exec.probe` (JSON-RPC 2.0, newline-delimited). Point any MCP-capable agent
at it:

```json
// Claude Desktop / Cursor / DSH mcp config
{ "mcpServers": { "unirun": { "command": "unirun", "args": ["mcp"] } } }
```

Tools return the normalized `ExecResult` JSON (exit_code / error_class /
hint / encoding / truncated), with `isError` reflecting the taxonomy class.

### Remote Windows execution (SSH)

`unirun ssh` is the win-exec knowledge ported to Rust: UTF-16LE
`-EncodedCommand` payloads, auto-injected UTF-8 "golden recipe" (no
CLIXML/GBK mojibake), exact exit-code propagation via `exit $LASTEXITCODE`,
and automatic scp + `-File` fallback for large scripts (UTF-8 BOM so Chinese
content works). cmd.exe targets run via temp `.bat` files.

### Per-project adaptation (recipes)

Ship a `.unirun/recipe.toml` in a project and unirun auto-applies it
(toolchain runners, timeouts, output caps):

```toml
schema = 1
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

[error_maps]
"ModuleNotFoundError: *" = { class = "DEPENDENCY_MISSING", hint = "run `uv sync`" }
```

```bash
unirun script main.py --toolchain python --json   # → uv run main.py
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
| `UNKNOWN_FAILURE`    | non-zero exit with unrecognized stderr          |
| *(none)*             | success, or explicit non-zero exit w/o evidence |

Classification is **evidence-based**: unirun never invents a class without a
confirmed pattern, so an explicit `exit 42` stays class-less (rc is the signal).

## Roadmap

- **P0 (released)** — local execution normalization: probe, in-process
  timeout, tree kill, encoding pipeline, taxonomy, CLI `--json`.
- **P1 (released)** — MCP server (`exec.run` / `exec.script` / `exec.probe`),
  SSH-remote transport (win-exec port: EncodedCommand / scp fallback / BOM /
  exit contract — verified against a real Windows node), per-project recipe
  system (`.unirun/recipe.toml`, toolchain runners, capability cache),
  Windows CI matrix.
- **P2** — Windows local execution polish, error-map libraries, recipe
  registry, background sessions, ACP, WinRM provider (psrp-rs POC),
  performance benchmarks.

Independent by design: MCP + CLI only, no harness dependency, no telemetry,
MIT licensed.

## License

MIT © 2026 LosEcher
