# PLATFORM-DIFFS — the platform differences unirun exists to absorb

Every row is a **fact with a witness**: the evidence it was learned from, and the
test that keeps it true. If a row loses its test, the fix is to restore the test,
not to soften the row. This file is the target of the debt recorded in
`UNIRUN-PITFALLS-ANALYSIS-2026-08-20.md:142` ("把 2.1 的 10 个 Windows 坑固化为
回归测试 + `docs/PLATFORM-DIFFS.md`"), and the "platform diff matrix" the
product design promised (`UNIRUN-CROSS-PLATFORM-EXEC-PRODUCT-DESIGN-2026-08-19.md:213`).

Test names are `<module>::tests::<name>` for unit tests and
`tests/<file>.rs::<name>` for integration tests. **`[win]`** marks a test that is
compiled only on Windows (`#[cfg(windows)]`), so it will not appear in
`cargo test -- --list` on macOS/Linux — that is why every `[win]` row also has a
platform-neutral witness test where one is possible.

## Shells and payloads

| # | Fact | Why it bites | Witness | Test |
|---|---|---|---|---|
| 1 | `C:\Windows\System32\bash.exe` is the **WSL launcher**, not bash | Resolving `bash` to it silently replaces the local shell with WSL (or prints a UTF-16LE "no distributions" message and exits 1) | `UNIRUN-PITFALLS-ANALYSIS-2026-08-20.md:36` | `probe::tests::wsl_shim_and_extension_rules_are_platform_facts`, `probe::tests::wsl_bash_shim_is_excluded` `[win]` |
| 4 | A PATH file-scan on Windows must try `.exe/.cmd/.bat/.ps1` | `which python` finds nothing even though `python.exe` exists; `CreateProcess` would have resolved it | `UNIRUN-PITFALLS-ANALYSIS-2026-08-20.md:40` | `probe::tests::wsl_shim_and_extension_rules_are_platform_facts` |
| 5 | Windows' default shell is PowerShell, not `sh` | The POSIX default makes every unquoted Windows command fail with `COMMAND_NOT_FOUND` | `UNIRUN-PITFALLS-ANALYSIS-2026-08-20.md:41` | `exec::tests::default_shell_run_reports_no_spurious_error` |
| 16 | `cmd.exe` consumes `>` **before** PowerShell sees it | `Get-Date > $null` creates a side-effect file and PowerShell never receives the redirect, while rc stays 0 | `SSH-WIN-REMOTE-EXEC-RESEARCH-2026-08-18.md:20,76,82` | `transport::tests::powershell_payload_is_encoded_and_bom_prefixed` (the payload is base64: no metacharacter survives), `tests/matrix.rs::t_f_quoted_metachars_safe` |
| 17 | PowerShell 5.1 reads a `.ps1` as ANSI unless it has a **UTF-8 BOM** | A remote script with Chinese comments fails to parse; `-File -` is unsupported in 5.1 (it starts an interactive session) | `SSH-WIN-REMOTE-EXEC-RESEARCH-2026-08-18.md:13,30`; `win-exec/README.md:77` | `transport::tests::powershell_payload_is_encoded_and_bom_prefixed` |
| 19 | `CreateProcess` caps the command line at **32 767** units | A large `-EncodedCommand` payload builds a remote command that can never start; unirun shipped a 60 000 threshold that ignored its own comment | `SSH-WIN-REMOTE-EXEC-RESEARCH-2026-08-18.md:56`; iteration item A14 | `transport::tests::inline_payloads_fit_the_createprocess_limit` |
| 21 | `cmd /C file.bat` can reset the exit status at the tail | The remote status stops being evidence; win-exec appended a contract, unirun did not | `win-exec/win-exec.py:234` | `transport::tests::cmd_payload_appends_the_exit_contract` |

## Encoding

| # | Fact | Why it bites | Witness | Test |
|---|---|---|---|---|
| 2 | PowerShell can emit **BOM-less UTF-16LE** | The bytes are *valid UTF-8 full of NULs*, so a naive fast path hands the agent NUL-garbage labelled `utf-8` | `UNIRUN-PITFALLS-ANALYSIS-2026-08-20.md:37` | `encoding::tests::bomless_utf16le_detected_by_nul_pattern`, `encoding::tests::plain_utf8_with_single_nul_stays_utf8` |
| — | CP936/GBK and OEM output | PowerShell 5.1 writes **stderr** through the OEM code page even with the golden recipe; Windows OpenSSH answers `uname` with GBK text. Labelling it `utf-8-lossy` threw the text away | `win-exec/README.md:81`; `SSH-WIN-REMOTE-EXEC-RESEARCH-2026-08-18.md:21`; iteration item A15 | `encoding::tests::gbk_chinese_decodes_automatically`, `encoding::tests::big5_requires_a_hint_and_says_so`, `encoding::tests::oem_pages_decode_through_the_hint`, `encoding::tests::explicit_utf8_disables_the_gbk_guess` |
| — | CRLF line endings | Every downstream diff/parse sees different bytes per platform | — | `tests/matrix.rs::t_stdout_stderr_split`, `encoding` decode tests |

## Process identity and lifetime

| # | Fact | Why it bites | Witness | Test |
|---|---|---|---|---|
| 9 | PowerShell parses the **right-hand side of `=` as a statement** | A bare injected generation token is executed as a command: the default shell reports `COMMAND_NOT_FOUND` on every run, and Windows tree-kill degrades to `TokenMismatch` | `docs/EXECUTION-AUDIT-2026-10-05.md:26` (commit `182516d`) | `process_identity::tests::inject_generation_token_forms`, `process_identity::tests::command_carries_token_requires_exact_argument`, `process_identity::tests::injected_token_is_recognizable_by_the_verifier_for_every_shell` |
| 10 | CIM renders dates as `/Date(<epoch-ms>)/` | Identity reads silently fail on Windows, and the kill gate degrades | `docs/EXECUTION-AUDIT-2026-10-05.md:25` (commit `b3ca75e`) | `process_identity::tests::creation_date_parses_both_renderings`, `process_identity::tests::wmi_datetime_parses_to_epoch` |
| 11 | Between `fork` and `execve` the child still carries the **parent's** argv/env | An immediate SIGINT refused to kill unirun's own child (`PID_REUSED`); a recycled pid never gains the random token, so a bounded re-read is the fix | `docs/EXECUTION-AUDIT-2026-10-05.md:28` (commit `bb4b08c`) | `exec::tests::verify_before_kill_waits_out_the_fork_exec_window` |
| 12 | "Cannot read the process" ≠ "the process is gone" | Treating an unreadable probe as `NotRunning` made tree-kill skip silently; a zombie is gone, a blocked probe is unknown | `docs/EXECUTION-AUDIT-2026-10-05.md:33` | `process_identity::tests::gone_and_unreadable_are_distinct_verdicts`, `process_identity::tests::zombie_process_is_not_alive`, `exec::tests::kill_gate_never_refuses_an_owned_child`, `exec::tests::cleared_environment_child_is_still_killed_and_reported` |
| 13 | macOS session liveness goes through `ps` with a wider race window | A runner's just-written terminal state was clobbered back to `interrupted` by an unlocked read-check-write | `docs/EXECUTION-AUDIT-2026-10-05.md:23` (commit `c2463f0`) | `session::tests::status_keeps_a_terminal_state_that_lands_during_stale_check` |
| — | A detached session's pid cannot be structurally proven ours | Fail-closed beats signalling a possibly recycled pid | README "anti pid-reuse kill protection" | `session::tests::kill_refuses_recycled_identity`, `session::tests::kill_refuses_unverifiable_identity` |
| 3 | `[Console]::OutputEncoding` throws "handle is invalid" with no console | An unguarded setter pollutes the agent's stderr | `UNIRUN-PITFALLS-ANALYSIS-2026-08-20.md:39` | `exec::tests::powershell_recipe_guards_every_setter` `[win]` |
| 7 | A narrow console **wraps error text**, breaking substring classification | `The term 'X' is not\nrecognized …` never matches its pattern | `UNIRUN-PITFALLS-ANALYSIS-2026-08-20.md:43` | `error_maps::tests::flatten_collapses_wrapped_console_lines` |
| — | A grandchild can hold the pipe after the child exits | EOF never arrives: the run hangs after its own kill (unbounded, or for the grandchild's lifetime) | `CODEX-DSH-HARNESS-DESIGN-ANALYSIS-2026-08-21.md:248`; iteration item A4 | `exec::tests::a_grandchild_holding_the_pipe_does_not_hang_the_run`, `exec::tests::a_normal_run_reports_no_drain_timeout`, `exec::tests::the_drain_budget_is_shared_between_streams` |
| — | A process in uninterruptible sleep survives even `SIGKILL` | "The run ended" was reported as if the tree were gone | `DSH-DASHBOARDS-PLUGIN-DESIGN-2026-08-16.md:143`; iteration item A5 | `exec::tests::kill_status_maps_the_attempt`, `taxonomy::tests::surviving_tree_beats_timeout`, `exec::tests::a_term_ignoring_child_reports_an_escalated_kill` |
| — | A deadline must kill the whole process **group**, not just the direct child | A PID-based killer leaves the backgrounded work running (reparented), so the caller retries into a busy tree and the result describes a run that is still happening. Same exit code (124, GNU convention), different *what gets killed* | differential arm of `tests/process_tree.rs::t_tree_kill_diverges_from_killing_the_direct_child` (a PID-based killer leaves a survivor; asserted, not assumed) | `tests/process_tree.rs::t_tree_kill_diverges_from_killing_the_direct_child`, `tests/matrix.rs::t_timeout_kills_whole_tree` |
| — | PowerShell does not propagate native exit codes | A `0` from a local `-Command` run is not evidence that the script's commands succeeded | `SSH-WIN-REMOTE-EXEC-RESEARCH-2026-08-18.md:54`; iteration item A5 | `exec::tests::exit_code_confidence_is_unknown_only_for_powershell_zero` |

## Transport

| # | Fact | Why it bites | Witness | Test |
|---|---|---|---|---|
| 14 | `ControlMaster=auto` reuses a detached master, and reused sessions **skip authentication** | It breaks the whole-tree deadline (the remote process is not in our process group) and produces "root password login worked" false positives in auth checks | README:202-204; `ZSPACE-Z4PRO-SSH-ACCESS-AND-MANAGEMENT-2026-09-16.md:588` | `transport::tests::argv_defaults_include_batch_and_accept_new` |
| 15 | A sandbox cannot reach the `~/.ssh` ControlMaster socket | `ssh` fails with what looks like a permission problem; `ControlPath=none` makes it work | `cankey/docs/plan/remaining-work-2026-10-01.md:95-96` | same assertion (the flag is unconditional) |
| — | A process started from a non-interactive `ssh` session is reaped when the session ends | Long remote jobs died with the connection (measured: four large shares vanished); Windows needs a scheduled task instead, which is why `--detach` refuses that platform rather than pretending | `NETWORK-FLEET-AND-TRANSFER-DESIGN-ANALYSIS-2026-09-18.md:445`; `WIN-COLLECTION-SELF-HEALING-PLAN-2026-08-18.md:78`; iteration item A6 | `transport::tests::detach_wrapper_detaches_and_reports_a_pid`, `transport::tests::detach_refuses_non_posix_targets`, `session::tests::remote_probe_classification`, `session::tests::remote_record_round_trips_through_the_state_file`, `tests/ssh.rs::ssh_detach_survives_and_is_pollable` |
| — | A process started from a non-interactive `ssh` session is reaped when the session ends | Long remote jobs died with the connection (measured: four large shares vanished); Windows needs a scheduled task instead, which is why `--detach` refuses that platform rather than pretending | `NETWORK-FLEET-AND-TRANSFER-DESIGN-ANALYSIS-2026-09-18.md:445`; `WIN-COLLECTION-SELF-HEALING-PLAN-2026-08-18.md:78`; iteration item A6 | `transport::tests::detach_wrapper_detaches_and_reports_a_pid`, `transport::tests::detach_refuses_non_posix_targets`, `session::tests::remote_probe_classification`, `session::tests::remote_record_round_trips_through_the_state_file`, `tests/ssh.rs::ssh_detach_survives_and_is_pollable` |
| — | A blocked PATH entry says nothing about the host | "I could not check" must not read as "absent" — the process probe already distinguished the two, the host probe did not | `MAC-PERFORMANCE-MONITOR-ANALYSIS-2026-08-27.md:118`; iteration item A8 | `probe::tests::unreadable_path_entries_are_not_reported_as_absent`, `probe::tests::probe_snapshot_is_internally_consistent` |
| — | A non-interactive `ssh` runs a **non-login** shell | `path_helper` reorders PATH and `python3` resolves to the system 3.9; remote `$HOME` can differ | `M1-HOMEBREW-TOOLCHAIN-AUDIT-2026-10-07.md:543`; `ZSPACE-Z4PRO-SSH-ACCESS-AND-MANAGEMENT-2026-09-16.md:92` | *not covered* — tracked as iteration item B11 |
| — | `ssh` reserves exit 255 for **its own** failures | 255 could not be told apart from a remote `exit 255`, and the client's diagnostics were mixed into the remote's stderr | `DSH-EXECUTION-AUDIT-2026-10-05.md:122`; iteration item A16 | `transport::tests::ssh_client_diagnostics_are_split_and_flagged`, `transport::tests::remote_exit_255_is_not_a_transport_error`, `tests/ssh.rs::ssh_transport_failure_is_classified` |
| 8 | Tree-kill output must not reach a protocol stream | `taskkill` prints `SUCCESS: …`; on stdio protocols that corrupts the JSON-RPC frame | `UNIRUN-PITFALLS-ANALYSIS-2026-08-20.md:44` | `tests/mcp.rs::mcp_exec_run_timeout` (the frame after a killed run still parses) |

## Not a platform difference, but a known boundary

| Fact | Status |
|---|---|
| PTY vs pipe changes a program's output (`isatty` branches); unirun always runs piped | **Not covered by a test** — no measured evidence yet, and the product boundary says TTY/GUI belongs to native execution (`SESSION-AGENT-ADOPTION-PLAN.md:30`). Listed here so the gap is deliberate rather than forgotten. |
| `path_helper` / minimal-PATH differences (row 3 of the transport table) | Gap with an owner: iteration item B11 (`--login` / `--path-prepend`). |

## Maintaining this file

1. A new platform difference is a bug report until it has a witness and a test.
2. Add the row with the test name in the same commit that adds the test.
3. Never delete a row because a test was renamed: rename the row too. A row whose
   test no longer exists is worse than no row, because it claims coverage.
