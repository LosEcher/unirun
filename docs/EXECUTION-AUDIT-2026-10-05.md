# unirun 最近 30 天执行记录审计

窗口：2026-09-05 → 2026-10-05（审计日 2026-10-05）
仓库：`~/syncfolder/project/dsfolder/unirun`（git，origin=GitHub LosEcher/unirun）
方法：git/CI/crates.io 台账 + `.fmtguard/runs.jsonl` 执行台账 + 本地实测 + 下游 los 消费面核查（全部命令见附录 A，可复跑）

---

## 修复进展（审计后执行）

本文是 2026-10-05 的审计快照；逐条修复记录在这里（编号对应第 0 节，提交号可在 `git log` 复核）。

**里程碑**：`main` 于 2026-10-05 三平台 + msrv 全绿（run `37253792695`，head `bb4b08c`）——自 2026-09-06 以来首次；此前 29 天 CI 全红。同日发布 **0.4.0**（tag run `37255097682` 全绿 + GitHub Release 5 个平台资产 + crates.io 0.4.0）。

| 事项 | 状态 | 提交 / 证据 |
|---|---|---|
| P0-1 clippy 1.98 新 lint 挂 main | 已修 | `c269350`：`units.chunks_exact(2)` → `as_chunks::<2>().0.iter()`（语义等价，仍丢弃尾部奇数字节） |
| P0-2 门禁无工具链锚定 | 已修 | `623b998`：新增 `rust-toolchain.toml`(1.97.0)，CI 由 `@stable` 改钉 `@1.97.0`，新增 `msrv` job 用 `cargo +1.88.0`（`rust-toolchain.toml` 会覆盖默认，MSRV job 必须显式 `+1.88.0`） |
| P1-3 MSRV / README 漂移 | 已修 | `623b998`：`rust-version = "1.88"`（`as_chunks` 下限）；README `Rust 1.70+` → `1.88+` |
| P2-1 仓库卫生 | 已修 | `b03222f`（AGENTS.md 的 fmt 终裁改进单独提交）、`c45b85e`（`.DS_Store` 进 ignore） |
| P2-2 CI 覆盖缺口（winrm 只 check） | 已修 | `623b998`：新增 `cargo test --features winrm` step |
| P0-4 分支保护 | 已开启 | `main` 要求 `test (ubuntu/macos/windows)` + `msrv` 必绿、strict、禁 force-push 与删除 |
| macOS `mcp_session_start_wait_output` 红 | 已修 | `c2463f0`：无锁 read-check-write 把 runner 刚落盘的终态覆盖成 `interrupted`（宽 liveness 窗口 + macOS 走 `ps`）；改为宽限期回读，`list()` 不再 settle |
| Windows clippy `unused import` | 已修 | `918c980`：`thread` / `Duration` / `Instant` 仅在 `#[cfg(unix)]` 测试模块使用 |
| Windows 身份/存活探针失效 | 已修 | `b3ca75e`：`is_alive` 补真实存活探测；`read_windows_identity` 改发 epoch 毫秒并容忍 `/Date(<ms>)/` 渲染 |
| Windows 默认 shell 每次运行误报 `COMMAND_NOT_FOUND` | 已修 | `182516d`：PowerShell 的 `=` 右值按语句解析，裸词会被当命令执行 ⇒ token 必须加引号；同时 `command_carries_token` 接受带引号的 token（实测 `bare_needle=false/quoted_needle=true`，不同步改会让 Windows 树杀全部退化成 TokenMismatch 拒绝） |
| P1-1 身份校验静默降级（`ps` 不可用即跳过 start-epoch） | 已修 | `bb4b08c`：新增 `IdentityVerdict::Unverifiable`——token 不可观测且没抓到 start-epoch 时不再返回 `Matches`，两个调用方都拒杀，exec 侧报 `IDENTITY_UNVERIFIABLE`（不再借用 `PID_REUSED` 这个不成立的类）；同时把探针缺失时的身份测试改为跳过（沙箱内 `cargo test --lib` 由 4 红转绿）。同一提交还修掉 CI 暴露的 fork→execve 窗口误判（详见下一行） |
| fork→execve 窗口内误判 PID_REUSED（CI 发现） | 已修 | `bb4b08c`：`fork` 后 `execve` 前子进程仍带**父进程**的 argv/env，token 不可见而 start-epoch 匹配 ⇒ 立即 abort/SIGINT 会拒杀自己的子进程（ubuntu 上 `run_with_custom_abort_flag_reports_aborted` 实报 `PID_REUSED`）。`verify_before_kill` 限时 250ms 重读后再拒（回收的 pid 不会获得随机 token，保证不变） |
| P0-3 发布（tag + 平台资产 + `cargo publish`） | 已执行 | `2b1d1f5` 先在 `publish` job 加断言（tag 必须等于 Cargo 版本，否则建 release 前就失败），`2961b94` bump 0.4.0（两个新特性按 semver 走 minor）。tag run `37255097682` 全绿：test×3 + msrv + release×5 + publish；GitHub Release `v0.4.0` 挂 5 个 `unirun-<os>-<arch>` 资产；`cargo publish` 完成，index 显示 0.4.0（非 yanked）。**三处版本一致：tag v0.4.0 = Cargo.toml 0.4.0 = crates.io 0.4.0** |
| P1-2 los 集成漂移 | **未修** | 改动在 los 仓（本会话工作区之外），需另开范围 |
| P2-3 fmtguard 台账保留 | **未修** | 改的是 fmtguard 仓（工作区之外）；本仓侧 `.fmtguard/runs.jsonl` 已 gitignore，无仓库内动作 |
| P3 文档收尾 | 已修 | `bb4b08c` 给 `docs/SESSION-AGENT-ADOPTION-PLAN.md` 补状态行；README Roadmap 本就有 P4（该条为误报）；README 新增 Releasing 清单 + `2b1d1f5` 的 tag↔crate 机械断言（历史 `v0.2.1` 无 crate 作为反例写进断言注释，tag 未删） |
| **「读不到进程」被当成「进程已退出」** | 已修 | 观测拆成三态 `Observed::{Alive,Gone,Unreadable}`（unix 用 `kill(pid,0)` 精确判存在/不存在，Windows 让 CIM 脚本显式回 `GONE`，探针本身失败才算 Unreadable），`verify` 不再把「读不到」报成 `NotRunning`。**策略按「这个 pid 是不是我们的」分档**：①exec 运行持有**未 reap 的子进程**——POSIX 在 `wait` 前不回收该 pid、Windows 在句柄关闭前不回收，所以归属是结构性的，探针只能做旁证而非否决 ⇒ 一律照杀，未确认时在 `hint` 写明 `tree kill not identity-confirmed: …`（旧行为是静默跳过并把结果报成 TIMEOUT，或对从未被回收的 pid 谎报 PID_REUSED）；②后台会话的 runner 不是本进程的子进程（可能跨重启）⇒ 无法结构性证明归属，读不到就 fail-closed 拒杀并报 `IDENTITY_UNVERIFIABLE`。验证：沙箱内 `cargo test --all-targets` 由 `tests/matrix.rs` 超时 2 红转为 **84 lib + 7/18/9/8/5 全绿**（超时真杀了），非沙箱同样全绿；新增 `kill_gate_never_refuses_an_owned_child`（策略钉死）、`cleared_environment_child_is_still_killed_and_reported`（`exec env -i` 抹掉 token 后仍被杀且如实标注）、`gone_and_unreadable_are_distinct_verdicts`、`kill_refuses_unverifiable_identity`（会话侧 fail-closed） |

审计后新增的回归测试：`injected_token_is_recognizable_by_the_verifier_for_every_shell`（六种 shell 的 inject→verify 往返，钉住注入器与校验器必须同步演进）、`default_shell_run_reports_no_spurious_error`（Windows 默认 shell 端到端，其它身份测试都固定在 `Shell::Bash` 且 bash 缺失时提前返回，这正是默认-shell 缺陷能长期存活的原因）与 `verify_before_kill_waits_out_the_fork_exec_window`（宽限必须被遵守且宽限后仍拒杀）。修复后测试基线（非沙箱）：lib 80 + 集成 7/18/9/8/5 全绿，9 ignored（SSH/WinRM 冒烟）。

---

## 0. 摘要：需要修复 / 更新 / 优化

| 级别 | 事项 | 关键证据 | 机械验收 |
|---|---|---|---|
| **P0-1 修复** | `main` 自 2026-09-06 起三平台 CI 全红，29 天无人碰 | clippy 1.98 `chunks_exact_to_as_chunks` @ `src/encoding.rs:96`；ubuntu/macos/windows 三 job 同一错误 | `cargo clippy --all-targets -- -D warnings` 在**与 CI 同版本**的 toolchain 上 exit 0 |
| **P0-2 修复** | 门禁无工具链锚定：CI `@stable` 漂移（09-06=1.98，今日=1.99），本机=1.97 ⇒ 本地永远复现不了 CI 的红 | `ci.yml:22 dtolnay/rust-toolchain@stable`；本机 `rustc 1.97.0`；当前 stable `1.99.0 (2026-09-28)` | 存在 `rust-toolchain.toml` 或 CI 钉版本；新增 MSRV job；`cargo clippy` 本地与 CI 同版本 |
| **P0-3 修复** | 5 个提交（含 2 个新特性）29 天未发布；`release`/`publish` job 因 `needs: test` 全 skipped；crates.io 停在 0.3.0 | `v0.3.0..origin/main` = 5 commits；v0.3.0 之后无 tag；crates.io max_version=0.3.0；tag `v0.2.1` 从未发布到 crate | 新 tag 的 CI 三平台绿 + GitHub Release 5 个 `unirun-<os>-<arch>` 资产 + `cargo publish` 版本 = tag |
| **P0-4 更新** | 无分支保护 + 红 CI 下连续合并两次 PR ⇒ 红 main 变成默认状态 | `GET /branches/main/protection` → 404；PR#1(04:03) / PR#2(04:10) 合并后主分支 run 均 failure | main 保护开启并要求 `test` 必绿；PR 未绿不可 merge |
| **P1-1 修复** | 身份校验在受限环境**静默降级**：`ps` 不可用 ⇒ `read_start_epoch_ms=None` ⇒ `verify()` 跳过 start-epoch 判定（树杀可能命中被回收的 pid） | 沙箱内 `cargo test` 4 红（`process_identity` ×3 + `session::kill_refuses_recycled_identity`）；同机非沙箱 123 绿 | 能力缺失时 fail-closed 或显式返回 `Unverifiable`；测试在 `ps` 不可用时 skip 而非 fail |
| **P1-2 更新** | 下游 los 集成漂移：仍在「unirun ssh 不支持 cwd/env」退回 native（该能力 09-06 已实现）；且 los 侧无安装/版本 pin | `los/packages/gateway/src/ssh-command-runner.ts:142-143`；los 内 unirun 引用 63 处但 Dockerfile/脚本/版本 pin 0 命中；本机 `~/.cargo/bin/unirun` 仍是 08-20（v0.3.0） | 删除该 skip 分支 + 契约测试 `cwd/env → unirun`；los 增加安装步骤与版本锁 |
| **P1-3 更新** | README 宣称 `Rust 1.70+`，实际 ≥1.87（`is_multiple_of`）；`Cargo.toml` 无 `rust-version` | `README.md:96`；`src/encoding.rs:83` | `rust-version` 落地 + README 同步 + MSRV job 通过 |
| **P2-1 更新** | 仓库卫生：`.DS_Store` 未 ignore；`AGENTS.md` 有一处有效改进未提交（`cargo fmt --check` 终裁） | `git status` → `M AGENTS.md` + `?? .DS_Store`；`.gitignore` 仅 `/target` `.fmtguard/` | `.gitignore` 含 `.DS_Store`；AGENTS.md 改进单独提交 |
| **P2-2 优化** | CI 覆盖缺口：`winrm` 只 `check` 不 `test`；9 条 SSH/WinRM 冒烟测试 `#[ignore]` 且无 CI 路径 | `ci.yml:29` 只 `cargo check --features winrm`；`cargo test --features winrm` 本地 79+ 通过但 CI 从不跑 | 增加 `cargo test --features winrm` step（或 matrix 维度） |
| **P2-3 优化** | fmtguard 台账 39 次运行 0 门禁拒绝（不是瓶颈），但保留全部历史、cwd 记录含旧路径 | `.fmtguard/runs.jsonl` 598 事件 / 39 runs / `gate_check` 失败 0 / verdict 全 `ok` | 台账加保留策略；新记录只写当前路径 |
| **P3 优化** | 文档/发布面收尾：P4 backlog 未落到 README Roadmap；`docs/SESSION-AGENT-ADOPTION-PLAN.md` 无状态行；tag↔crate 存在历史漂移（v0.2.1） | `README.md:316-333`；`docs/SESSION-AGENT-ADOPTION-PLAN.md` 全文无 Status | Roadmap 补 P4/未发布项；plan 补状态；发布清单加「tag=crate 版本」断言 |

---

## 1. 30 天执行量：三个数字说明现状

| 面 | 30 天内（2026-09-05 → 10-05） | 结论 |
|---|---|---|
| 代码 | `main` 上新增 5 个提交（2 特性 + 1 修复 + 2 merge），全部集中在 **09-06 一天**（`ab11293` 的 author date 是 08-24：工作副本积压 13 天后才提交） | 30 天内只有 1 天有产出，之后 29 天静止 |
| CI | 35 次历史 run 中 23 failure / 12 success；窗口内 4 次 run **全部 failure**；09-06 04:10 之后 **0 次 run** | 最后一次 CI 运行就是红的，此后无人触发，红状态被"冻结"至今 |
| 发布 | 09-06 之后 **0 个 tag**、0 次 crate 发布；crates.io `max_version=0.3.0`（2026-08-19） | 新特性从未到达任何用户/下游 |

> 口径说明：DSH 会话日志中 `unirun` 工作区的会话只有 2026-08-20~08-22 的 5 个（窗口外）+ 本次会话；09-06 的实际执行发生在一个 cwd 为 `dsfolder` 的会话里，会话日志里 `unirun` 命中主要来自记忆注入而非工具调用。因此本审计以 **git / CI / crates.io / fmtguard 台账 / 下游消费面** 为一手执行记录，而不是会话日志推断。

---

## 2. P0：红 main 与「本地永远绿、CI 永远红」

### 2.1 实测证据

CI（三平台同一错误，`gh run view 34010848966 --log-failed`）：

```
error: using `chunks_exact` with a constant chunk size
  --> src/encoding.rs:96:27
   = note: `-D clippy::chunks-exact-to-as-chunks` implied by `-D warnings`
   = help: for further information visit .../rust-clippy/rust-1.98.0/index.html#chunks_exact_to_as_chunks
error: could not compile `unirun` (lib) due to 1 previous error
```

- `test (ubuntu-latest)` / `test (macos-latest)` / `test (windows-latest)` 三个 job 同一处失败；
- 失败点在 **clippy 步骤**，因此后续 `cargo build` / `cargo test` **从未执行**——即 09-06 引入的两个特性在 CI 上**连编译测试都没跑过**；
- `release` / `publish` job `needs: test` ⇒ 全 skipped（这就是没有新 Release 资产的直接原因）。

### 2.2 根因不是代码，是「门禁在赌环境」

| 主体 | 版本 |
|---|---|
| 本机 | `rustc 1.97.0` / `clippy 0.1.97`（2026-07-07）——该 lint **不存在**，本地 `cargo clippy --all-targets -- -D warnings` **exit 0** |
| 09-06 的 CI | stable ⇒ clippy **1.98.0**（引入该 lint 的版本） |
| 今日 stable | `1.99.0 (2026-09-28)`（channel date 2026-10-01）⇒ 若今天再跑，可能还有**更多**新 lint |

`Cargo.toml` 无 `rust-version`，仓库无 `rust-toolchain.toml`，CI 用 `dtolnay/rust-toolchain@stable`。三者叠加的后果：**任何一次上游 stable 发版都可能把 main 变红，而开发者在本机无法复现**。09-06 那次就是首发命中——commit 03868cf 明明叫 "fix clippy"，但它修的是 `transport.rs` 的 string-push lint（当时本机能看到的那个），`encoding.rs` 的 1.98 新 lint 在本机不存在，于是"修完"仍红。

### 2.3 精确修复

**(a) 代码（1 行，语义等价，本机 1.97 可编译——已用 `as_chunks` 探针验证）**

```diff
-    let mut chars = units.chunks_exact(2).map(|c| {
+    let mut chars = units.as_chunks::<2>().0.iter().map(|c| {
         let u = u16::from_le_bytes([c[0], c[1]]);
```

`chunks_exact(2)` 丢弃尾字节、`as_chunks::<2>().0` 同样丢弃尾部不足 2 字节的部分，闭包体（`c[0]`/`c[1]`）无需改动。

**(b) 门禁（防复发，二选一或并用）**

1. 钉版本：`dtolnay/rust-toolchain@1.97.0`（或写 `rust-toolchain.toml`），使「本地 = CI」；
2. 补 MSRV：`Cargo.toml` 加 `rust-version = "1.87"`（`is_multiple_of` 的下限），CI 增一个 MSRV job ⇒ 新版 lint 不再无声打穿 main。

> 修复必须在**与 CI 同版本**的 toolchain 上验收（本机需 `rustup toolchain install 1.98.0`/或直接钉 CI 到本机版本）。本机 rustup 现有：stable(1.97)、nightly、1.81、1.95、1.96.1、1.97.0。

---

## 3. P1：功能面与下游面

### 3.1 身份校验在受限环境静默降级（安全语义问题，不是测试问题）

同一份代码、同一台机器，**两种结果**：

| 运行环境 | `cargo test` 结果 |
|---|---|
| 沙箱内（本 DSH 会话默认 shell：`ps` 被拒 → `bash: /bin/ps: Operation not permitted`） | **4 红**：`process_identity::{self_start_epoch_is_stable_and_recent, spawned_child_identity_carries_token, zombie_process_is_not_alive}` + `session::tests::kill_refuses_recycled_identity`（`unwrap_err()` 拿到 `Ok`） |
| 非沙箱（`danger-full-access` 复跑） | **123 绿 / 0 红 / 9 ignored**，`--features winrm` 亦绿 |

机制：`read_start_epoch_unix` 只有 Linux 有 `/proc` 快路，非 Linux 走 `ps -p <pid> -o lstart= -o state=`；macOS 在被 Seatbelt/沙箱限制进程枚举时 `ps` 直接失败 ⇒ `read_start_epoch_ms` 返回 `None` ⇒ `verify_identity()` 里 `if let Some(expected_epoch)` 不成立 ⇒ **跳过 start-epoch 比较**，只要 token 能观测（或 `token_observable=false`）就判 `Matches`。也就是说：**「树杀前先验证 pid 没被回收」这条安全保证，在受限环境里是静默失效的**，而调用方拿到的仍是"校验通过"。

处置建议（择一，需与调用方契约对齐）：`ps` 不可用时返回 `IdentityVerdict::Unverifiable` 并让 kill 走保守路径（拒绝/降级为 kill 前二次确认）而不是 `Matches`；同时测试按能力探测 skip（`ps` 不可用 → `#[ignore]`/early-return），避免"沙箱里 4 红"制造假故障。这一条与既有跨项目纪律同源：**门禁的红必须指向真实原因**。

### 3.2 下游 los 集成已落后于 unirun 能力

- los `packages/gateway/src/ssh-command-runner.ts` 已是 unirun 的正式消费面（`LOS_SSH_RUNNER=auto|unirun|native`，默认 auto，失败回落 native；los 内 unirun 引用 63 处）；
- 但 `:142-143` 仍写着：

  ```ts
  // unirun ssh does not support remote cwd/env yet — route those to native.
  const needsNativeOnly = Boolean(opts.cwd) || Object.keys(opts.env ?? {}).length > 0;
  ```

  而 unirun **PR#1（21da6ad，2026-09-06）** 正是 `feat(ssh): support governed remote cwd and env`。⇒ 凡是带 `cwd`/`env` 的任务（los 节点探测、远程构建的常态）**仍全部走手搓 native SSH**——即 09-06 那次集成升级的实际收益被这行 skip 抵消，Windows 节点 GBK/CLIXML 盲区也只在无 cwd/env 时被消除。los 侧测试 `ssh-command-runner.test.ts:122` 还在断言"cwd/env ⇒ native"，是把这个漂移**钉进了测试**。
- 部署面同样缺位：los 仓库的 Dockerfile / 部署脚本 / 版本引用里 **0 处 unirun 安装或版本 pin**；本机 `~/.cargo/bin/unirun` 停留在 **08-20 07:41**（v0.3.0 之后无更新，且不在 agent shell 的 PATH 里——los 代码已为此写了"探已知路径"的兜底）。
- 历史规划（记忆 2026-08-20「unirun 接入 los 分层方案」）中的"版本锁 v0.2.0 / deploy-to-remote.sh 加安装步骤"均未见落地。

建议：修好 main 并发布一个 patch（见 §2/§4），然后单独立项做「los 消费面收口」——删 skip 分支 + 改契约测试 + 加安装步骤与版本锁，验收用**同一条真实链路**（los 发一个带 cwd+env 的远程任务，断言走 unirun 且结果为 `SshRunResult`）。

### 3.3 MSRV 与文档不一致

`README.md:96` 写 "From source (Rust 1.70+)"，但 `src/encoding.rs:83` 用了 `usize::is_multiple_of`（stable **1.87**），`is_some_and`（1.70）。⇒ README 承诺的 1.70~1.86 用户 `cargo install unirun` 会编译失败；同时因为 `Cargo.toml` 没有 `rust-version`，cargo 不会给出可读的 MSRV 报错，只会抛 API 缺失。

---

## 4. 发布面：tag / crate / 资产三者的现状与漂移

| 对象 | 现状 |
|---|---|
| git tag | `v0.1.0` `v0.2.0` `v0.2.1` `v0.3.0`（最新 2026-08-19） |
| GitHub Release | 4 个；`v0.3.0` 为 latest |
| crates.io | `0.1.0` `0.2.0` `0.3.0`；**`v0.2.1` 从未发布到 crate**（tag 只用于修 CI 资产命名） |
| 未发布内容 | `v0.3.0..main` 共 5 commit：`ab11293` output coalescing limiter + generation-token tree-kill identity；`21da6ad` ssh governed cwd/env；`03868cf` clippy 修复；2 个 merge |
| 发布自动化 | `ci.yml` 的 `publish` job 只创建 GitHub Release（`softprops/action-gh-release`）；**crates.io 发布无 workflow**（手工 `cargo publish`）⇒ tag↔crate 漂移没有任何机械门禁 |

建议发布纪律：新 tag 前断言「`Cargo.toml` 版本 = tag 名 = crates.io 将发布的版本」，并把 `cargo publish --dry-run` 放进 tag 流程；`v0.2.1` 这类"只改 CI 的 tag"要么不打 tag（走 workflow_dispatch），要么在 README/Release note 里注明"不含 crate 版本"。

---

## 5. 台账与仓库卫生

- **fmtguard 台账**（`.fmtguard/runs.jsonl`，598 事件 / 39 runs）：`gate_check` 失败 **0**、`apply.refused` **0**、36 次 `report_emit` 全 `ok`。⇒ 30 天里 fmtguard 不是瓶颈，也从未误报；但记录里 `cwd` 有 28 条旧路径 `~/syncthing/project/...`（仓库已迁 `~/syncfolder/...`），且台账**无保留策略**（`grep -c` 598 行会持续增长）。这是台账口径问题，不是门禁问题。
- `git status`：`M AGENTS.md`（未提交的**有效**改进：补充「fmtguard 只保证范围不新增格式债，最终裁决是 `cargo fmt --check`」+ 职责分工段）+ `?? .DS_Store`（未进 `.gitignore`）。AGENTS.md 的改动应当提交（它修的是"fmtguard 报 ok ≠ 文件干净"这个真实误解，正是本地 CI 语义的来源）。
- 本地分支 `fix/2026-09-06-unirun-clippy` = `origin/main` 的父提交（落后 1 个 merge commit，可 fast-forward），无本地未推送工作，无丢失风险。

---

## 6. 建议执行顺序（每项带机械验收）

1. **P0-1/P0-2**（一次性，~10 分钟）：改 `src/encoding.rs:96` → 加 `rust-toolchain.toml`（钉 CI 用的版本）→ 本地 `cargo clippy --all-targets -- -D warnings && cargo fmt --check && cargo test` 全绿 → push main → 确认三平台 CI 绿。
2. **P0-4**（并行，2 分钟）：开 main 分支保护，required check = `test (ubuntu-latest)` 等三条。
3. **P2-1**（顺手）：`.gitignore` 加 `.DS_Store`；`AGENTS.md` 改进单独提交（符合"一个逻辑变更一条提交"）。
4. **P0-3**（CI 绿之后）：打 `v0.3.1`，确认 5 平台资产齐全 + `cargo publish` 版本一致 + `cargo install unirun` 在 1.87+ 可编译。
5. **P1-2**（unirun 发布完成后，在 los 仓库单独立项）：删 skip 分支、改契约测试、加安装步骤与版本锁；验收 = los 一条带 cwd+env 的真实远程任务走 unirun。
6. **P1-1**（需要设计确认）：`Unverifiable` 语义 + 受限环境测试 skip；验收 = 沙箱内 `cargo test` 不再出现 4 红、`verify` 对不可核实身份不返回 `Matches`。
7. **P1-3 / P2-2 / P2-3 / P3**：MSRV 与 README 对齐 + CI 加 `--features winrm` 测试 + fmtguard 台账保留策略 + 文档状态行。

---

## 附录 A：可复跑命令

```sh
export PATH="$HOME/.cargo/bin:/opt/homebrew/bin:$PATH"
cd ~/syncfolder/project/dsfolder/unirun

# 现状
git log --oneline v0.3.0..origin/main          # 未发布的 5 个提交
git status --short                              # AGENTS.md 未提交 / .DS_Store 未 ignore
gh run list --limit 100 --json conclusion       # 23 failure / 12 success
gh api repos/LosEcher/unirun/branches/main/protection   # 404 = 无保护

# CI 失败根因（需可写 cache：export XDG_CACHE_HOME=/tmp/gh-cache）
gh run view 34010848966 --log-failed | grep -E "chunks_exact|error|exit code"

# 本地门禁（本机 1.97 复现不了 CI 的 1.98 lint：exit 0）
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --all-targets                        # 需非沙箱：ps 被沙箱拒时 4 红
cargo test --features winrm

# 版本与 MSRV
rustc -V; cargo clippy -V
curl -s https://static.rust-lang.org/dist/channel-rust-stable.toml | grep -m1 '^date'
grep -rn "is_multiple_of" src/                   # 1.87+ 的下限证据

# 台账
python3 - <<'PY'
import json,collections
evs=[json.loads(l) for l in open('.fmtguard/runs.jsonl') if l.strip()]
print(len(evs), collections.Counter(e.get('t') for e in evs))
print('gate failures:', len([e for e in evs if e.get('t')=='gate_check' and e.get('pass') is False]))
PY

# 下游消费面
cd ~/syncfolder/project/los-workspace/projects/los
sed -n '138,150p' packages/gateway/src/ssh-command-runner.ts   # 仍按"不支持 cwd/env"退回 native
grep -rn "unirun" --include="*.ts" --include="*.sh" . | grep -v node_modules | wc -l   # 63
```

## 附录 B：本次审计的原始数据快照

- CI 汇总：`{"failure":23,"success":12}`（35 runs，窗口内 4/4 failure）
- 窗口内 commits：`03868cf` `548e8c9` `21da6ad`（`--since=2026-09-05`）；`v0.3.0..main` = 5
- 本地测试：lib 76 绿（默认）/ 79 绿（`--features winrm`）；集成 7+18+9+8+5 绿；9 ignored（SSH/WinRM 冒烟，需 `UNIRUN_TEST_SSH_HOST`）
- crates.io：`max_version 0.3.0`，`downloads 47`，`0.1.0 / 0.2.0 / 0.3.0`（无 0.2.1）
- fmtguard：39 runs / 598 事件 / `gate_check` 失败 0 / verdict 全 `ok` / toolchain `rustfmt 1.9.0-stable (2d8144b788 2026-07-07)`
- 工具链：本机 1.97.0；09-06 CI 1.98.0；今日 stable 1.99.0（2026-09-28）
