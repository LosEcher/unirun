# unirun Session/Agent Adoption Plan

Date: 2026-09-06

## Decision

`unirun` is a governed execution adapter for heterogeneous local and remote
hosts. It is not an agent loop and must not replace DSH or in-process Pi.
LOS owns authorization, scheduling, durable state, and verification; the
selected runtime owns the agent loop; unirun owns command execution
normalization.

## Selection Policy

Use `unirun` when a task needs cross-platform shell/encoding behavior, SSH
transport, bounded output, process-tree cancellation, or a normalized JSON
result. Prefer native execution for simple local Unix commands, interactive
TTY/GUI work, strict raw streaming, or sub-millisecond command startup.

The initial policy is:

| Condition | Runner |
|---|---|
| LOS SSH node without executor; non-interactive; no unsupported fields | unirun |
| Remote `cwd`/`env` with SSH support | unirun; arguments are forwarded and escaped per remote shell |
| TTY/GUI or caller requires raw shell semantics | native/sandbox |
| Explicit `execution_policy=unirun` | unirun or a hard failure |
| Explicit `execution_policy=native` | native |

Retries are limited to transport and explicitly transient failures. Do not
retry syntax, permission, command-not-found, or user abort results. Timeout
retry requires an idempotency declaration.

## Invocation Surfaces

### LOS session

The scheduler or ToolBroker creates an execution request containing `run_id`,
`session_id`, `project`, `node_id`, command, shell, timeout, cwd/env, and an
`execution_policy`. The gateway performs capability detection, selects
unirun/native, and maps the result to canonical `session_events` and
verification evidence. Agent code consumes the normalized result and never
parses platform-specific stderr.

### MCP agent

Configure `unirun mcp` and expose `exec.run`, `exec.script`, `exec.probe`, and
the background `session.*` tools. Require JSON results and preserve
`error_class`, `timed_out`, `aborted`, `truncated`, and `duration_ms`.

### ACP/IDE session

Use `unirun acp` for external IDE command execution. When the task belongs to
an LOS WorkItem, an adapter must write the resulting canonical events; ACP
must not bypass RunContract or verification gates.

### DSH/Pi

DSH/Pi may call unirun only through LOS ToolBroker or a dedicated MCP/runtime
adapter. Direct spawning inside the agent loop is out of scope because it
would bypass policy and evidence ownership.

The agent package now exposes an optional `run_remote_command` capability. It
is registered only when the host injects `remoteCommandRunner`; the callback
owns node authorization and transport, while ToolBroker owns phase/risk/
approval gates. The tool carries `nodeId`, command, remote cwd/env, timeout,
and the current `sessionId`/`runSpecId`, and returns normalized JSON evidence.
The LOS chat service now injects this adapter only when the executor feature is
enabled. It loads the requested node, rejects missing or non-`ssh_target`
nodes, and then calls the gateway SSH runner with the correlation fields. No
adapter is injected by default, so existing local sessions retain their
current sandboxed `run_shell` behavior.

## Iteration Plan

### P0 — evidence and session correctness

- Emit structured adoption, latency, result, and fallback metrics.
- Include `runner`, `fallback_reason`, `node_id`, `run_id`, and `session_id`.
- Reconcile stale background sessions whose PID no longer exists.
- Persist `finished_at`, `duration_ms`, terminal status, and bounded logs.
- Add a stale-session reaper and atomic state-file writes.

### P1 — capability completeness and E2E proof

- Add remote `cwd` and `env` support. Implemented in unirun SSH and LOS
  gateway forwarding; invalid environment keys are skipped and values are
  shell-escaped.
- Test Linux SSH, Windows PowerShell, encoding, timeout, abort, large output,
  transport failure, and remote command failure against real nodes.
- Add crash/restart recovery tests for background sessions.
- Compare unirun and native results for the same task corpus.

### P2 — controlled expansion

- Evaluate recipe registry, WinRM, ACP extensions, and performance work only
  after P0/P1 metrics are stable.
- Canary by task type and node capability before changing the default policy.
- Keep native as a per-run fallback and retain rollback evidence.

## Acceptance Gates

Promotion requires result-semantic parity between unirun and native execution,
explainable fallback decisions, durable `run/session/node` evidence, no orphan
processes after timeout/cancel, and measured canary results. Adoption rate is
not a success metric by itself; task success, recovery rate, latency, and
operator intervention are required.

## Current Evidence and Gaps

The gateway has auto detection, structured selection/completion/fallback logs,
and fallback in `packages/gateway/src/ssh-command-runner.ts`; this repository
documents the MCP/ACP/SSH surfaces in `README.md`. Stale local background
sessions are reconciled to `interrupted` with `finished_at` and
`duration_ms`. Remaining evidence gaps are real Linux/Windows SSH E2E runs,
crash/restart recovery, and durable LOS database/session-event persistence;
without those, production parity and recovery rates remain unmeasured.
