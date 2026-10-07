# Changelog

Release notes for `unirun`. The crate version, the git tag and the crates.io
version are asserted to agree before a release is created (see CI's publish job),
so a version listed here is a version that shipped.

## 0.5.0

The release that closed the cross-project audit's P5 plan
(`docs/ITERATION-CANDIDATES-2026-10-07.md`): most of it is **trust** — making the
result say what is actually known — rather than new surface.

### Fixed (results that used to lie)

- **Remote `truncated` was always `false`.** The per-stream truncation flags were
  discarded on the way into `ExecResult`, so an SSH or WinRM run past the 256 KiB
  cap returned a silently incomplete stdout while claiming it was complete. The
  cap is now a target field and `--max-output N` applies to local, SSH and WinRM
  alike.
- **GBK/OEM output was thrown away.** Text that was not valid UTF-8 came back as
  `���` labelled `utf-8-lossy`, although the README listed GBK as solved
  (PowerShell 5.1 writes *stderr* through the OEM page even with the golden
  recipe). CP936/GBK is now detected automatically; `--output-encoding` (or
  recipe `[conventions] encoding`, which was parsed but never applied) selects
  Big5/CP437/CP850/CP1252 explicitly, and `utf-8` switches the guess off.
- **`B64_THRESHOLD` was 60 000 while its own comment cited `CreateProcess`'s
  32 767-unit limit**, so a ~12–22 KB PowerShell script built a remote command
  that could never start. Now 30 000, with a compile-time invariant.
- **A run could hang after its own kill.** EOF never arrives when a grandchild
  inherited the pipe (`sleep 5 &`, a daemon), so the reader blocked forever. The
  post-exit drain is now bounded (2 s, one shared budget) and reports
  `drain_timeout`.
- **`exit_code: 0` from a local PowerShell run was taken as proof of success.**
  PowerShell does not propagate native exit codes and that path appends no
  `exit $LASTEXITCODE`; `exit_code_confidence: "unknown"` now says so. The cmd
  path gained the missing `exit /b %ERRORLEVEL%` contract.
- **"The run ended" was reported as if the tree were gone.** `kill_status` now
  distinguishes `clean` / `sigkill-escalated` / `survived` / `unconfirmed`, and a
  tree that survives an escalated kill classifies as `PROCESS_UNKILLABLE` instead
  of a tidy `TIMEOUT`.
- **`ssh` exit 255 could not be told apart from a remote `exit 255`**, and the
  client's diagnostics were mixed into the remote's stderr. `transport_error` +
  `transport_stderr` + the structural `TRANSPORT` class fix that; a remote script
  that itself exits 255 stays a remote failure.

### Added

- **`dispatched`** — whether the command may have run, so a caller knows if
  resubmitting a non-idempotent command is safe.
- **`unirun capabilities --json`** and MCP `exec.capabilities`: stable capability
  keys, so consumers stop keeping a version→feature table (los's
  `unirun-capabilities.ts` can delete one).
- **`unirun ssh --detach`** for POSIX remotes: a real detached run (setsid, with
  a nohup fallback) recorded as a session, driven by `bg status` / `bg output` /
  `bg kill`. Windows targets are refused with `UNSUPPORTED` and a `schtasks` hint
  rather than half-supported.
- **`bg output --since <cursor>`** (and MCP `session.output`'s `cursor`):
  incremental reads with `next_cursor`, so following a long build no longer means
  re-parsing the same tail.
- **Three-state probe**: `found` / `absent` / `unreadable`, plus a `unreadable`
  coverage list — "could not check" no longer reads as "not installed".
- **`docs/PLATFORM-DIFFS.md`**: every platform difference with the witness it was
  learned from and the test that keeps it true.
- **Library surface**: `run_with_abort` / `run_with_abort_streaming` for
  embedders that own their cancellation, plus a documented stability contract and
  a CLI↔library parity test.

### Changed (breaking)

- **An unknown `--flag` is a usage error (exit 2).** It used to be appended to the
  command (or to the remote script), which made a typo indistinguishable from
  script content: `unirun ssh host 'script' --cwd /srv` sent `--cwd /srv` to the
  remote. `--flag=value` is now accepted, `--` ends flag parsing, and single-dash
  tokens stay positional.
- **`ExecResult` gained fields** (`transport_error`, `transport_stderr`,
  `dispatched`, `kill_status`, `exit_code_confidence`, `drain_timeout`). Additive
  for JSON consumers, but a struct-literal consumer of the Rust type must add
  them; `ExecResult::success` covers the common case.
- **`SshTarget` / `WinrmTarget` / `ExecSpec` gained fields**
  (`max_output_bytes`, `output_encoding`, `drain_ms` for the targets). Use
  `Default::default()` (or `..Default::default()`) when constructing them.

### Fixed (behaviour)

- **MCP `notifications/cancelled` actually cancels.** It used to be a no-op that
  could not even be read until the call finished; calls now run on their own
  threads with a per-request abort flag, and Ctrl-C stops the server (exit 130)
  instead of being swallowed.
- **Ctrl-C cancels remote runs**, not just local ones: the ssh client's tree is
  signalled and `aborted` is reported truthfully.
- **Windows/`cmd` payloads** end with `exit /b %ERRORLEVEL%`; the PowerShell
  fallback file keeps its UTF-8 BOM; the WSL `bash.exe` shim and Windows
  executable extensions are covered by tests that run on every platform.

## 0.4.0

Output coalescing for streamed consumers, SSH `--workdir`/`--env` for governed
remote runs, and the identity/tree-kill fixes from the October execution audit
(pid-reuse refusal, `Unverifiable` fail-closed, fork→execve grace).

## 0.3.0

Unix SSH remotes, SSH identity options (`--user`/`--port`/`--identity`), and the
whole-tree deadline with `ControlMaster` disabled.

## 0.2.0

MCP server, Windows payload machinery (UTF-16LE `-EncodedCommand`, golden recipe,
scp fallback), recipe system.

## 0.1.0

Local execution normalization: probe, in-process timeout, tree kill, encoding
pipeline (UTF-8/UTF-16), error taxonomy, CLI `--json`.
