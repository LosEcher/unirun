# unirun 迭代候选：跨项目在册问题盘点

日期：2026-10-07
对象：`~/syncfolder/project/dsfolder/unirun`（v0.4.0，origin=GitHub LosEcher/unirun）
问题：**各项目遇到的本项目（unirun）范畴的问题，哪些可以加入迭代计划实现？**
方法：读本仓 README/设计文档/审计文档 + 全库源码核验（`src/`；每条结论都落到行号）+
跨仓证据采集（dsfolder 187 份 notes、7 个 Rust 工具仓、los 下游、DSH 与插件、Windows/远程相关产品仓）。
证据口径：只采「能指到文件:行号且读到的原文」，无证据的推断一律标注 **推断**。

> 本文件是**候选池 + 排期建议**，不是承诺；进入实现前按第 4 节的分批门禁逐项立项。

---

## 0. 结论速览

跨项目命中约 44 条，判为 unirun 范畴的 31 条：**A 类 17 条**（unirun 能根治 + 证据充分，建议进迭代计划）、
**B 类 14 条**（候选/推迟，需设计确认或证据单薄）、**C 类 13 条**（不属于 unirun，需路由到对应项目）。
其中 4 条已发布缺陷（A0/A14/A15/A16）与 2 条取消语义缺陷（A2/A3）由本仓源码 + 实测同时确认。

三簇高价值缺口（按「是否 unirun 能根治 × 证据重复度」排序）：

1. **已发布缺陷（最优先）**——远程路径 `truncated` 恒为 false（截断标志被丢弃）+ 远程输出上限固定
   256 KiB 不可配（A0）；**GBK/OEM 中文不可解**，只标 `utf-8-lossy`（A15，实测「你好」→ `���`）；
   **`B64_THRESHOLD=60_000` 越过 `CreateProcess` ~32 767 上限**，注释却自称「conservative」（A14）；
   `ssh`/`winrm` 完全不响应 abort（SIGINT 被全局吞掉但只有本地 exec 消费），
   MCP `notifications/cancelled` 是空实现（A2/A3）；「未派发失败」与「已派发但结果丢失」不可区分，
   使下游的 native 回退可能**重复执行**非幂等命令（A13）。
2. **kill 之后的不确定态**——kill 后收集输出**没有排空上限**（孙进程持有管道即永久挂起，A4）；
   「杀不掉」（D 态）没有分类、`exit code` 没有「是否观测到」的置信字段（A5）。
3. **接口契约与生存期**——未知 flag 静默拼进被执行的命令/脚本（A1）；ssh 退出码 255 无法区分
   「连接失败」与「远端真返回 255」（A16）；远端没有后台会话，长任务被迫「前台 + 远端自杀」（A6）；
   `bg output` 只有 `--tail` 没有增量游标（A7）；无机器可读的 capability 面（A10）。

---

## 0.1 实施进度

| 项 | 状态 | 证据 |
|---|---|---|
| A0 远程截断标志 + 可配上限 | ✅ 已完成 | `src/transport.rs` 的 `assemble_ssh_result`/`output_cap`、`src/winrm.rs` 的 `tail_keep`、CLI `--max-output`；单测 5 条 + 本地 E2E（`seq 1 200000 --max-output 1024` → `truncated:true` + 保尾 1024B） |
| A14 `B64_THRESHOLD` | ✅ 已完成 | 60 000 → 30 000，新增 `inline_encoded_command_fits` 与 `CREATEPROCESS_COMMAND_LINE_LIMIT` 不变量测试 |
| A15 代码页解码 | ✅ 已完成 | GBK 自动回退 + `--output-encoding`/recipe `[conventions] encoding`（此前该字段解析了但从未生效）+ CP437/CP850/CP1252/Big5 显式提示；15 单测 + 4 CLI 集成测试；dist 二进制 +165 KB（已记录在 Cargo.toml） |
| A1 未知 flag fail-closed | ✅ 已完成 | 未知 `--flag` → exit 2（不再拼进命令/远端脚本）；`--flag=value` 与 `--` 逃生口；`tests/cli_args.rs` 8 例 |
| A16 `transport_error` | ✅ 已完成 | 新增 `transport_error`/`transport_stderr` 字段 + taxonomy 类 `TRANSPORT`；255 需有 ssh 自身诊断证据；真实「连接被拒」集成测试 |
| A13 `dispatched` | ✅ 已完成 | 新增 `dispatched`（只有前派发证据才为 false：spawn 失败/连接/认证/上传）；远端 255 与中途断连保守为 true；README 增「never ran vs ran and failed」节 |
| A10 `capabilities --json` | ✅ 已完成 | 新增 `capabilities` 子命令 + MCP `exec.capabilities`（18 个稳定能力键，schema=1；`winrm` 随 feature 出现）；5 单测 + CLI/MCP 集成测试；README 增「Consumers: ask what the build can do」 |
| A11 README 重试/unknown 语义 | ⏳ 待办 | |
| A2–A5（取消与 kill 语义） | ⏳ 待办 | |
| A6–A8、A12（会话/探测/平台差异） | ⏳ 待办 | |
| A9 + 0.5.0 发布 | ⏳ 待办 | |

## 1. 口径：什么算「本项目范畴」

**属于**（in-scope）：把命令/脚本在异构主机上执行并归一化结果所必需的机制——
shell 选择与 payload 传递、exit code 契约、进程内 deadline、整树 kill 与 kill 前身份校验、
编码解码（UTF-8/UTF-16LE/BOM、Windows 上的 GBK/CLIXML 处置）、有界输出与截断语义、
取消/abort 语义、后台会话生命周期、能力探测、错误分类学与 hint、本地/SSH/WinRM 传输、
以及这些能力的对外契约（CLI `--json`、MCP/ACP、库 API）。

**不属于**（out-of-scope，见 §5）：任务编排与调度（job 级超时、重试策略、审批）、
LLM/provider 层错误归一化、GUI/浏览器能力、其它项目的业务逻辑、
「帮某个项目改它的业务代码」（属于集成收口，记在对方仓）。

---

## 2. 现状基线（2026-10-07）

### 2.1 已发布

- v0.4.0（2026-10-05 发布，tag v0.4.0 = Cargo.toml 0.4.0 = crates.io 0.4.0，5 平台资产齐全）——见 `docs/EXECUTION-AUDIT-2026-10-05.md:13`。
- 能力面：本地执行（macOS/Linux/Windows）、SSH（Unix + Windows）、WinRM（POC，feature `winrm`）、
  进程内 deadline + 整树 kill + generation-token 身份校验、编码管线、错误分类学、
  保尾截断、本地后台会话、probe、recipe/registry、MCP `exec.*`+`session.*`、ACP v1。

### 2.2 设计承诺但未落地（源码核验）

| 设计条目 | 现状 | 证据 |
|---|---|---|
| `exitCodeUnknown`（无显式 exit 时标注 rc 不可依赖） | **未实现**（全库 0 命中） | 设计 `UNIRUN-CROSS-PLATFORM-EXEC-PRODUCT-DESIGN-2026-08-19.md:81`；`grep -rn exitCodeUnknown src/` = 0；`src/spec.rs:150-175` 只有 `exit_code: Option<i32>` |
| 后台会话「增量 read 游标」 | **未实现**，只有 `--tail` | 设计 `:96`；`src/session.rs:358` `output(id, tail_bytes)`；`src/mcp.rs:345-351` 只有 `tail` |
| `recipe.infer`（项目适配自省） | **未实现** | 设计 `:97`；`src/main.rs:431` recipe 子命令 = `list\|show\|add\|rm\|path\|effective\|check` |
| 执行时按可用性降级 shell（bash→sh→cmd→powershell→pwsh） | **部分**：`Shell::from_path` 只按扩展名推断，无「首选不可用则降级」链 | 设计 `:78`；`src/spec.rs:47-56` |
| 凭据 scrub | **未实现**：`--password P` 直接走 argv（`ps` 可见） | 设计 `:103`「凭据 scrub」；`src/main.rs:854-855`、`src/winrm.rs:63` |
| 平台差异矩阵文档（每 OS×shell×编码一张表） | **未落盘**（`docs/` 至今 2 个文件） | 设计 `:213`；`ls docs/` |
| README「Why」段承诺的 GBK 乱码已解决 | **不成立**：解码器无 CP936/CP437/CP1252 分支，只标 `utf-8-lossy` | `README.md:24`；`src/encoding.rs:74-75`；实测见 A15 |
| Windows stderr 的 OEM 编码 | **未覆盖**：黄金配方只设 stdout 相关编码（`OutputEncoding`/`$OutputEncoding`） | `src/exec.rs:342`；`win-exec/README.md:81` 自述「stderr … OEM codepage … may be garbled」 |

### 2.3 在飞未提交改动（工作树）

- `Cargo.toml` 加 `exclude = ["docs/", …AGENTS.md, .github/]`、`.github/workflows/ci.yml` 加
  「Packaging content gate」（`cargo package --list` 断言内部文件不进 `.crate`）——来自
  `RUST-CRATE-PAYLOAD-HYGIENE-2026-10-07.md`，尚未提交。
- 本地领先 `origin/main` 1 个提交（`d9de886` build profile）。

### 2.4 backlog 需要重新分诊的两条

- README Roadmap P4 的「per-stream caps」**做了一半**：两个流各自有独立缓冲与上限
  （`src/exec.rs:193-201` 对 stdout/stderr 各传一次 `max = spec.effective_max_output()`），
  但**只能是同一个值**，且 `ExecResult` 只有一个 `truncated` 位（`src/spec.rs:171-172`）。
  真实消费者要的是「stdout/stderr 分别限额、分别标截断」
  （`cantool/src-tauri/src/extensions.rs:186-187` 的 `stdout_limit_bytes`/`stderr_limit_bytes`）
  ⇒ 该项应改写为「分流量上限 + 分流截断位」，而不是关闭。
- P4 的「Windows 本地执行 polish」与「transport plugins」需要重新定义（当前没有可验收的具体项，
  见 §3.1 A12 与 §3.2 B3 的替代表述），否则 backlog 会长期停在「看起来还有事」的状态。

### 2.5 采用面（比能力缺口更值钱）

| 事实 | 证据 |
|---|---|
| 全舰队**唯一**代码级消费点 = los gateway | `los/packages/gateway/src/ssh-command-runner.ts`、`unirun-capabilities.ts`、`los/packages/agent/src/tools/core/registry.ts:359` |
| 本机安装版本落后仓内**一整代**（0.3.0 vs 0.4.0） | 实测 `unirun --version` → `unirun 0.3.0`，`~/.cargo/bin/unirun` 时间戳 08-20；`Cargo.toml:3` = 0.4.0 |
| los 已改为「按 `--version` 派生能力」而非假设 | `unirun-capabilities.ts:83-94`（`sshWorkdirEnv: ≥0.4.0`）；`ssh-command-runner.ts:119-120,241-243` |
| → 但装的是 0.3.0，能力门控恒判「不支持 cwd/env ⇒ 回落 native」 | 推断（由上面两条直接推出；`unirunSshUsable` 逻辑见 `unirun-capabilities.ts:114-120`） |
| DSH 侧 0 接线 | `deepseek-harness` 全仓（排除 node_modules）`grep -rl unirun` = 0 命中；`LMSTUDIO-WIN-EXEC-CHANNEL-2026-08-22.md:95` 同结论 |
| sandbox-run / verify-gate 计划「委托 unirun 做整树 kill」但各自有简化实现 | `SANDBOX-RUN-DESIGN-2026-08-20.md:150`；`VERIFY-GATE-DESIGN-2026-08-21.md:182` |

---

## 3. 候选清单

分级：**A** = unirun 能根治 + 跨项目证据充分（建议进计划）；**B** = unirun 能根治 + 证据单薄或需设计确认；
**C** = 不属于 unirun（路由到别处）。

A 类按四组排列：**已发布缺陷**（A0/A14/A15/A16/A13）→ **取消与 kill 语义**（A1–A5）→
**会话与探测**（A6–A8）→ **采用与知识**（A9–A12）。编号不重排，便于与排期表对照。

### 3.1 A 类（建议进迭代计划）

#### A0【已发布缺陷】远程路径的 `truncated` 恒为 false，且远程输出上限固定 256 KiB 不可配

- 证据（本项目源码，v0.4.0 现状）：
  - `src/transport.rs:296-297` 把每流的截断标志丢弃：`let (out_raw, _) = to.join()…`、`let (err_raw, _) = te.join()…`；
  - `src/transport.rs:315` 硬写 `truncated: false`；`src/winrm.rs:151` 同样硬写；
  - 上限是常量而非 spec：`src/transport.rs:29` `const MAX_OUTPUT: usize = 256 * 1024;`，
    `src/transport.rs:261-262` 直接用它；`SshTarget`（`src/transport.rs:33-50`）没有 `max_output_bytes` 字段。
- 与承诺的差距：README:44-45「bounded, drained (no pipe deadlock), tail kept and marked `truncated`」，
  本地路径（`src/exec.rs:193-201` 走 `spec.effective_max_output()`）成立，**远程路径不成立**。
- 跨项目影响：los 消费面把 unirun 的字段透传给 agent（`packages/gateway/src/ssh-command-runner.ts:290-301`），
  截断不告警 ⇒ agent 拿到「不完整但声称完整」的 stdout。
- 处置：接受并透传 `read_capped` 的截断标志（`truncated = out || err`）；`SshTarget` 增 `max_output_bytes`，
  由 CLI `--max-output` / recipe `conventions.max_output_bytes` 驱动。
- 验收：真实 remote 产生 > 上限的输出 → 断言 `truncated: true` 且内容为尾部；`--max-output` 生效测试。

#### A14【已发布缺陷】`B64_THRESHOLD = 60_000` 越过 `CreateProcess` 的 ~32 767 上限

- 证据（本项目源码）：`src/transport.rs:26-28` 注释自称
  「CreateProcess command-line limit ≈ 32 KB; this threshold is conservative」，值却是 `60_000`；
  判定点 `src/transport.rs:177` `if b64.len() <= B64_THRESHOLD`。
- 后果：PowerShell 脚本的 base64（UTF-16LE，膨胀 ≈2.67×）落在 `(32767, 60000]` 时仍走
  `-EncodedCommand`，远端 `CreateProcess` 命令行超限 → 失败；只有 > 60 000 才降级到 scp+`-File`。
  即约 12 KB–22 KB 的 PowerShell 正文会命中这个窗口。
- 同源证据：`win-exec/win-exec.py:84` 阈值同样是 60000，注释同样写「CreateProcess 命令行上限 32767，
  留足余量」——两个副本的注释都想对了，值都写反了。
- 处置：阈值降到 ~30 000（或按 `exe + 选项 + b64` 的总长度算），并把边界用例钉进测试。
- 验收：构造 b64 长度 ≈35 000 的脚本，断言走 scp/`-File`（或 stdin）路径且在远端执行成功。

#### A15【已发布缺陷】GBK/OEM 中文不可解，只标 `utf-8-lossy`（README 的「编码管线」名不副实）

- 证据（本项目源码 + 实测）：
  - `README.md:24` 把「PowerShell 5.1 emits CLIXML / GBK mojibake」列为**已解决**的痛点；
  - 但解码器只有 UTF-8 / UTF-16LE/BE / lossy 四态：`src/encoding.rs:58-76`，无 CP936/CP437/CP1252 分支；
  - **实测（2026-10-07，本机二进制）**：`unirun run 'printf "\xc4\xe3\xba\xc3\n"' --json`
    → `"stdout":"���\n","encoding":"utf-8-lossy"`（GBK 的「你好」不可恢复）。
- 相关但独立的一面：Windows stderr 的 OEM 编码不在黄金配方里（配方只设 `[Console]::OutputEncoding`
  与 `$OutputEncoding`，`src/exec.rs:342`）；win-exec 自己承认
  「PS 5.1 writes the error stream using the OEM codepage; the recipe only guarantees UTF-8 for stdout」
  （`win-exec/README.md:81`）。
- 跨项目证据：`SSH-WIN-REMOTE-EXEC-RESEARCH-2026-08-18.md:21`
  「远端系统错误信息以 OEM/GBK 码页输出 → 本地看到乱码（`reg query` 的"系统找不到指定的注册表项或值"乱码现场复现）」；
  `LOS-NODE-ONBOARDING-AND-SSH-CONFIG-ANALYSIS-2026-08-19.md:13`（Windows probe 报 GBK 乱码）。
- 处置：加显式代码页回退（CP936/CP950/CP437/CP1252，按 BOM/chcp/OEM 探测）；
  标签区分「非法 UTF-8 序列」（`utf-8-lossy`）与「已知代码页」（如 `gbk`）；
  配方补 stderr 侧编码设置（try/catch 包裹，见 A12 事实 3）。
- 验收：GBK 字节样本 → 断言输出为「你好」且 `encoding == "gbk"`；Windows stderr 中文用例断言无乱码。

#### A16 ssh 退出码 255 无法区分「连接失败」与「远端真返回 255」

- 证据（本项目源码）：`src/transport.rs:270-275` 直接取 ssh 进程退出码；
  `src/transport.rs:296-302` 只做 `filter_banner`，ssh 自身的连接诊断与远端 stderr 合流进同一个
  `stderr` 字段，没有判别位。
- 跨项目证据：`DSH-EXECUTION-AUDIT-2026-10-05.md:122` 的退出码分布里
  「255×10（ssh）」与「127×5（command not found）」同类高频——消费方无从区分「认证/网络失败」与
  「远端脚本自己 exit 255」，错误分类学可能给出误导性 hint。
- 处置：`ExecResult` 增 `transport_error: bool`（或 `transport: "ssh-connect-failed"`）+
  独立 `transport_stderr`；对 255 做证据式分类（`ssh: connect to host … port 22`、
  `Permission denied (publickey)`、`Host key verification failed` 等）。
- 验收：连接不可达 / 认证失败 / 远端 `exit 255` 三类用例分别得到不同的 `error_class` 与 `transport_error`。

#### A13 派发语义不可判定：「未派发失败」与「已派发但结果丢失」不可区分

- 证据：los 回退逻辑无条件重跑——`packages/gateway/src/ssh-command-runner.ts:169-183`
  （`log.warn('unirun ssh failed, falling back to native ssh'…)` 后直接调 native）；
  而 unirun `--json` 模式下自身失败才会非 0，下游无法判断命令是否已经执行过。
- 影响：非幂等命令（部署、写库、清理）可能被**执行两次**；这会让「回落 native」这个安全网本身变成风险源。
- 处置：`ExecResult` 增 `dispatched: bool`（Local：spawn 成功即 true；SSH/WinRM：ssh 进程成功建立到
  远端执行前视为未派发、之后视为已派发），或定义为不可判定时的显式 `dispatched: null` + `hint`。
- 验收：spawn 失败 / 认证失败 / 远端中途断开三类用例的 `dispatched` 取值与文档一致；
  下游据该字段决定是否允许回退。

#### A1 未知 flag 静默拼进被执行的命令/脚本（fail-open）

- 证据（本项目源码）：`src/main.rs:194` `_ => positional.push(a.clone())`；`src/main.rs:302` run 分支
  `positional.join(" ")`；`src/main.rs:383` ssh 分支 `script = positional[1..].join(" ")`。
- 跨项目证据：下游为此写了整层版本门控——`los/packages/gateway/src/unirun-capabilities.ts:1-6`
  「Pre-0.3.0 builds do not reject unknown flags — they append them to the remote script」。
- 影响：`unirun ssh host 'script' --cwd /srv`（拼错的 flag）会把 `--cwd /srv` **追加进远端脚本**；
  `unirun run 'rm -rf x' --dryrun` 会把 flag 当命令参数执行。
- 处置：未知 `--flag` → usage error `exit 2`（保留「`--` 之后全部 positional」逃生口）。
- 验收：`unirun run 'echo hi' --unknown-flag` exit 2 且不执行；新增回归测试钉住（`tests/`）。

#### A2 ssh/winrm 不响应取消；SIGINT 被全局吞掉

- 证据：`src/main.rs:56` 对所有子命令 `install_sigint_handler()`；`src/exec.rs:39-42` 处理器只置
  `ABORT` 标志；`ABORT` 仅被 `src/exec.rs:71,81`（本地执行）消费；`src/transport.rs:272-295` 的
  ssh 等待循环不查 ABORT，`src/transport.rs:310` 硬写 `aborted: false`。
- 影响：远程执行期间 Ctrl-C 表现为「无反应」，要等命令自己结束或撞上 deadline；
  与 README 给 SSH 的「同样的归一化保证」承诺不一致。
- 处置：`--abort` 语义打通到 SSH/WinRM（SIGINT → kill ssh 进程树 → `aborted:true` / exit 130）。
- 验收：集成测试注入中断，断言 `aborted:true`、无残留 ssh 进程、rc=130（门禁同本地 abort 用例）。

#### A3 MCP `notifications/cancelled` 是空实现；server 吞 SIGINT

- 证据：`src/mcp.rs:38-40` `Some("notifications/cancelled") => { /* 无回复 */ }`（**不取消在跑的 exec**）；
  `src/mcp.rs:15-19` serve 循环阻塞在 stdin 上；SIGINT 只置标志（A2 同源）⇒ `unirun mcp` 无法被 Ctrl-C 打断。
- 影响：agent 侧「取消长命令」只能在客户端断开/超时；服务端继续跑完，浪费且留残留进程。
- **实测（2026-10-07，本机 v0.3.0 二进制）**：`unirun mcp` 挂起后 `kill -INT` → **进程仍存活**
  （对照：`unirun run 'sleep 30'` 同一手法 → 1.0s 内返回
  `{"aborted":true,"error_class":"ABORTED","exit_code":null,"timed_out":false}`，本地路径的 abort 语义正确）。
- 处置：把 cancellation token 接到 `run_with_abort_streaming`（`src/exec.rs:86` 已支持外部 abort 标志）；
  server 收到 SIGINT 时干净退出（或显式只对 exec 生效、保留 serve 语义并文档化）。
- 验收：MCP 级 E2E——`exec.run` 长任务 + 发 `notifications/cancelled` → 断言返回 ABORTED 且进程树消失。

#### A4 kill 后输出排空无上限（孙进程持有管道即永久挂起）

- 证据（本项目源码）：`src/exec.rs:508-519` `join_capture()` 直接 `h.join()`，**无超时**；
  `kill_tree`（`src/exec.rs:523-539`）只保证直接子进程组被 SIGTERM→SIGKILL。
- 跨项目证据（互相独立的两处）：`CODEX-DSH-HARNESS-DESIGN-ANALYSIS-2026-08-21.md:248`
  （Codex exec 内核有「IO 排空 2s」，unirun 缺）；`INFOMARCHY-ANALYSIS-2026-09-19.md:64`
  「被杀的工具有孤儿孙进程持有管道，挂起的读会让 Bun 永远活着」。
- 处置：kill 后收集改成有界 drain（默认 2s），超限截断并在结果标 `drain_timeout`。
- 验收：造「孙进程 `setsid` 后持有 stdout」的用例，断言 unirun 在 deadline+grace+drain 内返回且不挂。

#### A5 「杀不掉」与「没观测到 rc」缺显式语义

- 证据（本项目源码）：`ExecResult` 只有 `exit_code/signal/timed_out/aborted`（`src/spec.rs:149-175`），
  无 kill 结果与 rc 置信字段；`exitCodeUnknown` 设计承诺未实现（§2.2）。
- 跨项目证据：`DSH-DASHBOARDS-PLUGIN-DESIGN-2026-08-16.md:143`（NAS 不可达时 `df` 进 D 态，
  `spawnSync` 的 SIGTERM 杀不掉）；`SSH-WIN-REMOTE-EXEC-RESEARCH-2026-08-18.md:54`
  「PS 原生命令失败不自动置 rc ⇒ 无显式 exit 时不可依赖」。
- 处置：增 `kill_status`（`clean` / `sigkill_escalated` / `unconfirmed` / `survived`）与
  `exit_code_confidence`（`observed` / `unknown`）；taxonomy 补 `PROCESS_UNKILLABLE`。
- 验收：D 态/不可中断用例断言 `kill_status=survived`（而非静默 TIMEOUT）；PowerShell 无显式 exit
  的远端用例断言 `exit_code_confidence=unknown`。

#### A6 远端没有后台会话：`bg` 只做本地，长任务被迫「前台 + 远端自杀」

- 证据：`src/session.rs:82-87` 会话目录是本地 `$UNIRUN_HOME/sessions`；`unirun ssh` 无 detach 入口
  （`src/main.rs:355-387` ssh 参数表）。
- 跨项目证据（6 份独立文档，全部以「绕行」收尾）：
  `NETWORK-FLEET-AND-TRANSFER-DESIGN-ANALYSIS-2026-09-18.md:445`「后台 `du` 会在 ssh 会话结束时被带走
  （实测 4 个大共享静默消失）…远程长任务一律用 `timeout N du …`」；
  `WIN-COLLECTION-SELF-HEALING-PLAN-2026-08-18.md:78`「会被调用方（SSH/计划任务）进程回收，
  **必须经 `schtasks /run` 中转**」（落地见 `lot2extension/scripts/win-extension-watchdog.ps1:121`）；
  `MULTIPLATFORM-CONTENT-AGGREGATION-RESEARCH-2026-08-16.md:150`（SSH-attached 进程随会话结束而死）；
  `DSH-0.2.1-UPGRADE-LOOP-INCIDENT-2026-10-06.md:46`（detached via launchctl 才能活过 teardown）；
  `WIN-INPUT-EVENTS-KIMI-TAB-RESEARCH-2026-08-16.md:49,51`（SSH 里 `Start-Process` 的 GUI 进程
  在用户桌面上不可见、键鼠注不进去 ⇒ 静默假成功；交互式会话必须 `schtasks … /it /run`）。
- 处置：`unirun ssh --detach`（POSIX `setsid`+`nohup`/`systemd-run --scope`；Windows
  `schtasks /create /run` 或 `schtasks … /it` 交互式会话）+ 本地 session 记录 + 递增游标读回，
  复用现有 generation-token 校验做远端 kill 的安全门；新增错误类 `NO_INTERACTIVE_DESKTOP` 杜绝静默假成功。
- 验收：真实远端起 10 分钟任务 → 本地 CLI 退出 → 会话可 `status/output/kill`，且远端进程数可证。

#### A7 `bg output` 无增量游标（反复拉全量）

- 证据：`src/session.rs:358-363`（`read_tail`）、`src/mcp.rs:345-351`（只接受 `tail`）；设计承诺 `:96`。
- 处置：`--since <cursor>` / MCP `cursor`，返回 `next_cursor`（字节偏移，截断时明确重置语义）。
- 验收：连续两次增量读的内容拼接 = 一次性全量（字节级相等），含轮转/截断场景。

#### A8 能力探测与 probe 的「读不到 ≠ 不存在」语义

- 证据（本项目源码）：`src/probe.rs:10-40` 全部是 `Option<String>`（None 只有「没找到」一种含义）。
- 跨项目证据：`MAC-PERFORMANCE-MONITOR-ANALYSIS-2026-08-27.md:118`「探针加覆盖率诚实字段…
  哪些指标读不到（Windows 无 uname、无 SMART）标记为 gap 而非伪造 0」；
  `TERMINAL-BROWSER-ANALYSIS-2026-08-25.md:123`「探测失败不算失败（unknown → 兜底）」；
  `LOS-NODE-ONBOARDING-AND-SSH-CONFIG-ANALYSIS-2026-08-19.md:13`（Windows probe 因 `uname` 缺失直接失败）。
- 处置：probe/ExecResult 增 `unknown`/`unreadable` 语义（三态：found / absent / unreadable）。
- 验收：在 `ps`/CIM 被拒的环境下 probe 仍返回可用结果并标 unreadable（与 `process_identity` 的
  `Observed::{Alive,Gone,Unreadable}` 三态对齐）。

#### A9 下游只能「整二进制委托」，执行原语不可作为库复用

- 证据：`SANDBOX-RUN-DESIGN-2026-08-20.md:150`（检测到二进制就委托，否则自建 spawn+pgid kill）；
  `VERIFY-GATE-DESIGN-2026-08-21.md:182,192`（P0 不做整树 kill，P1 委托 unirun）；
  `GROK-BOT-0.18-BORROW-DESIGN-2026-08-24.md:221-222`（下游重造输出节流与 {pid,startEpoch,token} 身份校验）。
- 采用面实测：**除 unirun 自身，6 个 Rust 兄弟仓的 `Cargo.toml` 里 0 处依赖 unirun**
  （`grep -l unirun */Cargo.toml` 只命中 `unirun/Cargo.toml`）；`sandbox-run/src/exec.rs:4`
  只有注释级模仿「Mirrors unirun's lifecycle semantics」；设计里承诺的 `--runner unirun` 源码零命中。
  重复实现清单（同一套 exec plumbing）：deadline+kill 5 处、保尾截断 reader 3 处、
  手写 shell 分派 3 处、无编码管线直接 `from_utf8_lossy` 48 处/5 仓。
- 已知障碍：MSRV 摩擦（rustopt `rust-version = "1.87"` < unirun 1.88，见 C13）；
  无示例/薄 API 文档，消费方要自己拼 CLI 调用。
- 处置：把 `ExecSpec → ExecResult`（含 `exec::run`、`run_streaming`）正式作为 crate 公共 API 稳定下来
  （lib 已存在：`src/lib.rs`；缺的是「承诺稳定 + 版本化 + 示例」），并提供 CLI 版 `--json` 与 lib 语义一致性测试。
- 验收：sandbox-run/verify-gate 至少一处改用 lib 或明确委托路径，且比较测试证明结果等价。

#### A10 安装/版本漂移没有机械门禁（采用面第一障碍）

- 证据：本机 `unirun 0.3.0` vs 仓 0.4.0（§2.4）；`RUST-REPO-LOS-GOVERNANCE-ANALYSIS-2026-10-07.md:232`
  把这条列为漂移事实；los 只能靠 `--version` 解析自保（`unirun-capabilities.ts:83-94`）。
- 处置（unirun 侧能做的部分）：提供**机器可读能力面**（`unirun capabilities --json`：
  版本 + 支持的 flag/子命令 + 平台），让下游不必解析人类可读文本；README 写「升级 checklist」。
  （下游安装与 pin 是 los 仓的事，见 C4。）
- 验收：`unirun capabilities --json` 有 schema 与契约测试；los 的 `--version` 正则可退化为兜底。

#### A11 设计承诺的 `exitCodeUnknown` / 明确的 unknown 语义文档

- 证据：设计 `:81`；`JEV-ULTRAFAST-DESIGN-AND-ADOPTION-2026-09-18.md:189`
  「**给 unirun 补「不重试变更 + unknown 语义」说明**」——唯一一条下游点名要 unirun 补文档的条目。
- 处置：随 A5 落地 `exit_code_confidence`，并在 README 增「重试与 unknown 语义」小节
  （哪些结果可重试：仅 transport/transient；语法/权限/not-found/abort 不重试；TIMEOUT 重试需幂等声明）。
- 验收：README 有该节且与 `docs/SESSION-AGENT-ADOPTION-PLAN.md:34-36` 的既有策略一致。

#### A12 平台差异知识库缺失（Windows 10 个真坑没有文档与回归表）

- 证据：`UNIRUN-PITFALLS-ANALYSIS-2026-08-20.md:142`「把 2.1 的 10 个 Windows 坑固化为回归测试 +
  `docs/PLATFORM-DIFFS.md` 平台差异知识库（README 已承诺，尚未落盘）」；`ls docs/` 至今只有 2 个文件。
- 处置：`docs/PLATFORM-DIFFS.md`（OS × shell × 编码 × 进程树 × 已知 shim/陷阱），每条挂一个测试名。
  输入已备齐，可直接搬：`UNIRUN-PITFALLS-ANALYSIS-2026-08-20.md:36-45`（Windows 10 坑）、
  `docs/EXECUTION-AUDIT-2026-10-05.md:26-35`（身份/存活探针与 fork→execve 窗口）、
  `SSH-WIN-REMOTE-EXEC-RESEARCH-2026-08-18.md:13,20,30,54,56,76-83`（PS 5.1 编码/元字符/exit 契约/32KB 上限）、
  `ZSPACE-Z4PRO-SSH-ACCESS-AND-MANAGEMENT-2026-09-16.md:588`（`ControlMaster=auto` 造成认证假阳性）。
  其中**尚无测试覆盖**的至少有：控制台窄宽度折行破坏 stderr 分类、MCP 协议流被 tree-kill 输出污染、
  `=` 右值裸词解析（负向控制）、CIM `/Date(<ms>)/` 渲染、沙箱内 ControlPath 被拒、
  无 BOM+中文注释的 .ps1 语法失败、Windows 无 `uname`。
- 验收：文档中每条差异对应 `tests/matrix.rs` 里的可执行用例（文档↔测试双列）。

### 3.2 B 类（候选，需设计确认或证据单薄）

| # | 事项 | 证据 | 说明 |
|---|---|---|---|
| B1 | `unirun script -`（stdin 收脚本），替代 heredoc 拼正文 | `SESSION-RECORDS-OPTIMIZATION-SCOPE-2026-08-21.md:13`（内层 heredoc 提前终止外层 → SyntaxError）；`src/main.rs:317` 现要求文件路径 | 小改动、直接消一类事故；但属于「用法便利」，排在 A 类之后 |
| B2 | SSH 连接复用 opt-in（`--multiplex`） | `LOS-NODE-ONBOARDING-AND-SSH-CONFIG-ANALYSIS-2026-08-19.md:148,158`（每节点 1–2s 握手开销）；unirun 现强制 `ControlMaster=no`（README:202-204） | 与「整树 deadline」保证有取舍，需默认关闭 + 文档说明 |
| B3 | 远端大输出的代理/流式呈现面（SOCKS/增量帧） | `TERMINAL-BROWSER-ANALYSIS-2026-08-25.md:120`、`GROK-BOT-0.18-RECONSTRUCTED-ANALYSIS-2026-08-24.md:177` | 设计面较大，先定「增量帧契约」（与 A7 同源）再谈传输 |
| B4 | `session resume/replay` | `SESSION-RECORDS-OPTIMIZATION-SCOPE-2026-08-21.md:53`；README Roadmap P4 自承未做 | 与 A6（远端 bg）合流更划算 |
| B5 | `rust-toolchain.toml` 是否该进 `.crate`（钉 1.97.0 与 MSRV 1.88 并存） | `RUST-CRATE-PAYLOAD-HYGIENE-2026-10-07.md:52`；`M1-HOMEBREW-TOOLCHAIN-AUDIT-2026-10-07.md:455` | 行为决策：影响从 registry 构建的人；两种口径都能论证，建议排除并写 README |
| B6 | recipe.infer / shell 可用性降级链 | 设计 `:78,:97` | 价值取决于是否真有「同项目多 shell」场景，建议先补 probe 三态（A8）再看 |
| B7 | 凭据不进 argv（`--password-stdin`/env，且从 hint/日志 scrub） | 设计 `:103`；`src/main.rs:854-855` | WinRM 目前是 POC；建议随「winrm 转正」一起做 |
| B8 | 批量/常驻调用面（NDJSON 一次 spawn 跑多命令；或 Rust/Node 绑定） | DSH 侧 `packages/shell/bash-local/src/index.ts:98,188`（进程内 seam，每命令不额外 spawn）；`unirun/docs/SESSION-AGENT-ADOPTION-PLAN.md:63-65`（DSH 直连被明确排除在范围外） | 若要覆盖「已有成熟 in-process executor 的 harness」，CLI+MCP 三面之外的这条路是前提；成本高，先定需求真伪 |
| B9 | `unirun script <file> --ssh <host>`（本地脚本文件直接作为远端 payload） | win-exec 的主用法是 `win-exec [options] <script_file>`（`win-exec/README.md:27`），unirun `cmd_script` 只做本地（`src/main.rs:308-353`） | 复用已有 scp+UTF-8 BOM+`-File` 路径即可；消除「把文件内容塞进引号」这一层 |
| B10 | `unirun probe --ssh <host>`（远端能力矩阵：PS 版本/pwsh/WinRM/DefaultShell） | win-exec 有 `--check`（`win-exec/win-exec.py:145-150`）；unirun `probe()` 无 host 参数（`src/probe.rs:112`） | 把 `capabilities.json` 从单机语义扩成 `host+platform` 维度（含漂移检查） |
| B11 | `--login` / `--path-prepend` / `--resolve-path`（非交互登录 shell 的 PATH 归一化） | `M1-HOMEBREW-TOOLCHAIN-AUDIT-2026-10-07.md:543`（`path_helper` 把 `/opt/homebrew/bin` 压到第 16 位 ⇒ `python3` 落到系统 3.9.6）；`ZSPACE-Z4PRO-SSH-ACCESS-AND-MANAGEMENT-2026-09-16.md:92`（非交互 `$HOME=/home/`）；`cankey/docs/plan/remaining-work-2026-10-01.md:95-96`（沙箱内 ControlPath socket 被拒） | 与 probe 联动（报告 PATH 来源）；是「同一 spec 到处同结果」承诺的一部分 |
| B12 | `unirun fanout --hosts <file> '<script>' --concurrency N --json` | `CROSS-NODE-DATA-DISTRIBUTION-DESIGN-2026-09-16.md:269-270`（手写 for 循环 + `ControlPath=none`，N 台 N 次握手） | 每主机独立 deadline/树杀；与 A6 的远端会话合流后价值更高 |
| B13 | 对外开放的「身份校验过的杀进程树」面（`unirun kill --pid … --expect …`） | 脚本层手搓 `pgrep`+`kill -9`（`scripts/dsh-web-unstick-restart.sh:44,58`、`scripts/z4pro/nas-tunnels.sh:34`）；cantool 用 `taskkill /F /IM powershell.exe` 按镜像名全局杀（`cantool/src-tauri/src/tts/runtime.rs:330-331`）；wechatdp 只信状态文件里的裸 PID（`wechatdp/docs/code-review-and-perf-analysis.md:86`） | `src/process_identity.rs` 已有 token+start-epoch 与 fail-closed 判定，缺的是 CLI/MCP 出口 |
| B14 | 显式 non-goal + `--stream` 逃生口（需要全量/二进制流的消费者） | `wechatdp/reports/2026-09-11-p2-media-pack-cold-archive.md:66`（3 GiB 分卷必须流式读 + 增量 sha256，与保尾截断语义冲突） | 文档优先；`--stream` 直通 fd、不做 cap 与解码，明确「不归一化」的代价 |

### 3.3 C 类（不属于 unirun，需路由）

| # | 事项 | 为什么不属 | 该去哪 |
|---|---|---|---|
| C1 | los `verification-runner.ts` 超时只发 SIGTERM、无整树 kill、证据只有 rc + 8000 字符摘要 | unirun 已有该能力，是**消费缺口** | los：requiredChecks 走 `unirun run --json` |
| C2 | 非零退出丢弃 stdout（`|| true` 包裹） | 同上 | 消费方（zspace/脚本）改契约 |
| C3 | 探针用 `uname` 探 Windows 平台导致失败 | 同上（unirun probe 已解决） | los node-probe 改调 `unirun probe --json` |
| C4 | los 无 unirun 安装步骤 / 版本 pin；本机 0.3.0 | 部署面在 los | los：Dockerfile/deploy 加安装 + pin（unirun 侧只提供 A10 的机器可读能力面） |
| C5 | DSH 完全没接线 unirun | 采用决策，不是能力缺失 | DSH：若要接，走 MCP（`unirun mcp`）或 host-only 工具包 |
| C6 | LLM provider 层错误归一化（token 超限等） | unirun 边界明写不做语言运行时/服务层 | los/DSH |
| C7 | job 级编排超时（单次 2,703,558 ms 等） | 调度层语义 | 调度器（如 los governance / DSH scheduler） |
| C8 | los native 回退用 `; ` 拼 `cd`/`export`，`cd` 失败仍在错误目录继续执行 | 这是 **native 路径**的缺陷（unirun 的 unix 前缀是 fail-fast：`src/transport.rs` 里 `\|\| exit $?`） | los：要么修 native 拼接，要么让带 cwd/env 的调用**必然**走 unirun（与 C4 同批做） |
| C9 | gateway 丢弃 unirun 的结构化字段（`timed_out/aborted/error_class/duration_ms` 在工具层恒为默认值） | 映射层丢字段；unirun 已返回全部字段（`src/spec.rs:150-175`） | los：扩 `SshRunResult` 透传（adoption plan `:53` 的验收门） |
| C10 | dsh-verify-gate 用 `SIGKILL` 代替输出上限，输出多即误判失败 | unirun 的 drained+tail-kept+`truncated` 正是该语义 | dsplugins：改用 unirun（注意先修 A0） |
| C11 | fmtguard 的 rustfmt 调用管道自锁死（退出后才排空 → 大文件误报「rustfmt 超时」） | unirun 的并发排空读正是反面参照（`src/exec.rs:196-201` 起两条读线程）；实测见 `fmtguard/src/engine.rs:160,182,191` | fmtguard：改成边写边排空；unirun 侧可加一条「大输出不阻塞」的**门禁用例**（现仅有 bench） |
| C12 | verify-gate 静默截掉 stdout **头部**（`OUTPUT_CAP=64 KiB` + `drain(0..drop)`，无 `truncated` 字段） | 与 unirun 的「保尾 + 标记」语义相反；截断导致 `stdout_contains` 确定性假 FAIL | verify-gate：接 unirun 或至少加 truncated 并把「截断导致 miss」降级为 na/工具错误 |
| C13 | MSRV 摩擦：rustopt `rust-version = "1.87"` < unirun `1.88` | 直接阻碍「Rust 兄弟仓依赖 unirun」（`grep -l unirun */Cargo.toml` 除自身外 0 命中） | unirun：评估把 MSRV 降到 1.87（`is_multiple_of` 是 1.87，位于 `src/encoding.rs:83`）或提供「只依赖 ExecResult 契约」的轻量说明 |

### 3.4 下游对 unirun 的硬需求（缺则采用永远是 partial）

1. **能力自描述**：`capabilities --json`（版本 + 特性集），取代所有下游的 semver 解析与硬编码门槛
   （los `unirun-capabilities.ts` 整个模块可删）→ A10。
2. **证据字段全链路可靠**：`truncated/timed_out/aborted/error_class/duration_ms` 在**远程路径同样成立**
   → A0 + A13 + C9。
3. **远程可取消**：ssh/winrm 接 SIGINT/caller cancel 并如实上报 → A2。
4. **注入面**：`error_class`+`hint` 覆盖超时/输出超限/被杀 → A5。
5. **输出上限语义统一**：全路径 drained + tail-kept + `truncated`，绝不因输出多而杀进程 → A0 + A5。
6. **本地执行同等可用**：跨平台 shell 解析 + 树杀 + 编码 → A9（sandbox-run/verify-gate/los executor 的采用前提）。
7. **安装与版本契约**：Release 资产命名已就绪；`capabilities --json` 供运行时校验 → C4 + A10。

---

## 4. 建议迭代计划（P5）

依赖关系：P5.0 是契约与安全（改动小、解锁下游）；P5.1 依赖 P5.0 的 abort 语义；
P5.2 与 P5.1 并行；P5.3 是采用与分发（可与 P5.1 并行，但发布应等 P5.0/P5.1 落地）。

起手建议：**第一个 PR 做 A0**（远程 `truncated` + 可配上限，约 15 行 + 1 个远端用例）——
它是已发布版本里的正确性缺陷（「不完整却声称完整」），且直接决定下游能否信任结果；
**第二个 PR 做 A1**（未知 flag fail-closed，约 10 行 + 1 个测试）——纯风险削减、无兼容性争议；
**第三个 PR 做 A4**（kill 后有界 drain，约 20 行 + 1 个用例），消掉「unirun 自己挂死」这类最难查的故障。

### P5.0 契约与安全（建议 1–2 天，可直接进）

| 项 | 内容 | 机械验收 |
|---|---|---|
| A0 | 远程截断标志透传 + `SshTarget.max_output_bytes` / `--max-output` | 远程超限用例 `truncated: true` 且内容为尾部；上限可配 |
| A14 | `B64_THRESHOLD` 降到 ~30 000（或按总命令行长度算） | b64≈35 000 的脚本走 scp/stdin 并在远端成功执行 |
| A15 | 代码页回退解码（CP936/CP950/CP437/CP1252）+ 标签区分「非法 UTF-8」与「已知代码页」；配方补 stderr 编码 | GBK 样本 → 「你好」且 `encoding == "gbk"`；Windows stderr 中文无乱码 |
| A1 | 未知 flag 报 usage error（exit 2） | `unirun run 'echo hi' --nope` → exit 2 且不产出 stdout；新增测试 |
| A16 | `transport_error` + 独立 `transport_stderr`；255 的证据式分类 | 连接失败/认证失败/远端 exit 255 三例分类互不相同 |
| A5 前置 | `exit_code_confidence` + `kill_status` 字段落 schema（先只做 observed/unknown 两态） | `--json` 输出含新字段；schema 文档更新；既有测试断言不破 |
| A13 | `dispatched` 语义（区分未派发失败 / 已派发但结果丢失） | 三类失败用例取值与文档一致；下游回退策略据此可判定 |
| A10 | `unirun capabilities --json`（版本 + 子命令/flag 能力矩阵） | 契约测试 + los 侧可删除正则解析 |
| A11 | README 增「重试与 unknown 语义」节 | 文档评审 + 与 `SESSION-AGENT-ADOPTION-PLAN.md` 的策略一致 |

门禁：`cargo fmt --check` → `cargo clippy --all-targets -- -D warnings` → `cargo test --all-targets` →
`cargo test --features winrm`；提交走 fmtguard 流程（AGENTS.md）。

### P5.1 取消与生存期（建议 3–5 天）

| 项 | 内容 | 机械验收 |
|---|---|---|
| A2 | SSH/WinRM 打通 abort（SIGINT → 杀 ssh 树 → aborted:true / 130） | 中断注入测试；无残留 ssh 进程 |
| A3 | MCP `notifications/cancelled` 真正取消在跑的 `exec.run`；server 不再吞 SIGINT | MCP E2E 取消用例 |
| A4 | kill 后有界 drain（默认 2s）+ `drain_timeout` 标记 | 「孙进程持有管道」用例在限定时间内返回 |
| A5 | `kill_status` 四态 + taxonomy `PROCESS_UNKILLABLE` | D 态/不可中断用例断言 survived |

### P5.2 输出与可观测（建议 3–5 天）

| 项 | 内容 | 机械验收 |
|---|---|---|
| A7 | `bg output --since <cursor>` / MCP cursor + `next_cursor` | 增量拼接 = 全量（含截断/轮转） |
| A6 | `unirun ssh --detach` + 远端会话 status/output/kill（POSIX `setsid`/`nohup`；Windows `schtasks`/`/it`） | 真实远端 10 分钟任务活过 CLI 退出；交互式会话假成功返回 `NO_INTERACTIVE_DESKTOP` |
| A8 | probe/unreadable 三态语义 | `ps`/CIM 被拒环境 probe 仍出结果且标 unreadable |
| A12 | `docs/PLATFORM-DIFFS.md` | 每条差异↔一个测试名 |
| B12 前置 | per-stream 上限与分流 `truncated` 位（先把 `ExecResult` 的单一 `truncated` 拆成 stdout/stderr 两态） | cantool 类消费者可用；既有断言兼容 |
| A9 前置 | 「大输出不阻塞」进 CI 门禁（现仅 bench） | 生成 > 1 MiB 输出并断言不挂、`truncated` 正确 |

### P5.3 采用与分发（建议与 P5.1 并行，发布等 P5.0/1）

| 项 | 内容 | 机械验收 |
|---|---|---|
| A9 | 稳定 lib API（`ExecSpec → ExecResult`、`run_streaming`）+ 示例与一致性测试 | CLI 与 lib 结果等价测试；下游至少一处引用 |
| B9 / B10 | `script <file> --ssh <host>`、`probe --ssh <host>` | 本地 .ps1 → 远端执行成功；远端能力矩阵入缓存且带漂移检查 |
| 发布 | 0.5.0（新字段 = minor）+ GitHub 5 资产 + crates.io；本机 `cargo install` 升到新版 | tag = Cargo.toml = crates.io；本机 `unirun --version` = 0.5.0 |
| 分发 | npm wrapper / brew tap / MCP 目录注册（设计 `:212`） | 任一路径安装后 `unirun probe` 可用 |

### P5.4 明确推迟（有证据但优先级低）

B1（`script -`）、B2（`--multiplex`）、B3（代理/流式帧）、B4（resume/replay）、
B5（toolchain 打包口径）、B6（recipe.infer / 降级链）、B7（凭据不进 argv，随 winrm 转正）、
B8（批量/常驻调用面，先确认真需求）、B11（`--login`/PATH 归一化）、B13（对外的身份校验杀树面）、
B14（`--stream` 逃生口与 non-goal 文档）、C13（MSRV 是否降到 1.87）。

---

## 5. 不纳入（负向清单）

- **不做 agent loop / 调度 / 重试编排**：unirun 只归一化「一次执行」，重试与幂等属于调用方
  （`docs/SESSION-AGENT-ADOPTION-PLAN.md:12-15,34`）。
- **不做 TTY/交互式与 GUI**：设计明确「交互式/GUI 用 native」（`:17-22`）。
  注意边界：A6 的「交互式**会话上下文**」（Windows `schtasks /it` 让进程出现在用户桌面）
  属于「会话归一化」，与「PTY/GUI 自动化」不同——前者做，后者不做。
- **不做 LLM/provider 层错误归一化、job 级超时**（C6/C7）；HTTP 空闲超时（如 20s/300s 静默中止）
  属宿主网络层，不属本工具。
- **不承诺「保尾截断」之外的全量字节流**：需要 3 GiB 级全量流/增量哈希的消费者必须走原生
  （`wechatdp/reports/2026-09-11-p2-media-pack-cold-archive.md:66`）；
  这条应写进 README 的 non-goal，并给 `--stream` 逃生口（B14）。
- **不替下游改业务代码**：C1–C13 都是「让下游来用」或「对方自己的缺陷」，unirun 侧只提供能力面与文档。

> 优先级纪律：**已发布缺陷（A0/A14/A15/A16 + A2/A3 的取消语义）先于新特性**——
> 它们让消费方拿到「看起来正常但不可信」的结果，对 agent 的伤害大于「少一个功能」。

---

## 6. 附录

### 6.1 本仓源码核验命令（可复跑）

```sh
cd ~/syncfolder/project/dsfolder/unirun
grep -n "_ => positional.push" src/main.rs            # A1 fail-open
grep -n "truncated: false" src/transport.rs src/winrm.rs   # A0 远程截断标志被丢弃
grep -n "MAX_OUTPUT" src/transport.rs                 # A0 远程上限是常量、不可配
grep -n "B64_THRESHOLD" -B 2 src/transport.rs         # A14 注释说 32KB、值是 60000
unirun run 'printf "\xc4\xe3\xba\xc3\n"' --json        # A15 GBK 实测 → ��� / utf-8-lossy
grep -n "lossy" src/encoding.rs                        # A15 无代码页分支
grep -n "ABORT" src/exec.rs src/transport.rs          # A2/A3 abort 只在本地被消费
grep -n "notifications/cancelled" src/mcp.rs          # A3 空实现
grep -n "join_capture" -A 10 src/exec.rs              # A4 无超时 join
grep -n "pub struct ExecResult" -A 26 src/spec.rs     # A5 缺字段
grep -n "pub fn output" -A 5 src/session.rs           # A7 只有 tail
grep -n "pub struct ToolInfo" -A 6 src/probe.rs       # A8 Option 语义
grep -rn "exitCodeUnknown" src/                       # A11 0 命中（设计未落地）
grep -n "exit /b" src/transport.rs                    # A14 附注：cmd 分支无 win-exec 的退出码契约
unirun --version; grep -n '^version' Cargo.toml       # A10 安装版 vs 仓版
grep -l unirun ../*/Cargo.toml                        # A9 除自身外 0 个 Rust 仓依赖
ls docs/                                              # A12 PLATFORM-DIFFS 仍缺
```

### 6.2 跨仓证据索引（外仓）

- los：`packages/gateway/src/unirun-capabilities.ts`、`packages/gateway/src/ssh-command-runner.ts`、
  `packages/agent/src/tools/core/registry.ts:359`
- dsfolder notes（原文见 §3 各条行号）：`UNIRUN-CROSS-PLATFORM-EXEC-PRODUCT-DESIGN-2026-08-19.md`、
  `UNIRUN-PITFALLS-ANALYSIS-2026-08-20.md`、`EXECUTION-AUDIT-2026-10-05.md`、
  `CODEX-DSH-HARNESS-DESIGN-ANALYSIS-2026-08-21.md`、`NETWORK-FLEET-AND-TRANSFER-DESIGN-ANALYSIS-2026-09-18.md`、
  `LOS-NODE-ONBOARDING-AND-SSH-CONFIG-ANALYSIS-2026-08-19.md`、`SANDBOX-RUN-DESIGN-2026-08-20.md`、
  `VERIFY-GATE-DESIGN-2026-08-21.md`、`MAC-PERFORMANCE-MONITOR-ANALYSIS-2026-08-27.md`、
  `SESSION-RECORDS-OPTIMIZATION-SCOPE-2026-08-21.md`、`JEV-ULTRAFAST-DESIGN-AND-ADOPTION-2026-09-18.md`
- 工具仓：`sandbox-run/`、`verify-gate/`、`run-diff/`、`fmtguard/`、`rustopt/`、`session-index/`、`win-exec/`
- 本轮另采的外仓：`cantool/src-tauri/src/{extensions.rs,tts/runtime.rs}`、`cantool/docs/architecture/adr/ADR-005-command-runner.md`、
  `wechatdp/{scripts,reports,docs}/`、`lot2extension/{TODO.md,scripts/win-extension-watchdog.ps1}`、
  `deepseek-harness/packages/{shell,subprocess,ssh}/*`、`dsplugins/dsh-verify-gate/lib/tools.mjs`、
  `dsfolder/scripts/*.mjs`、`cankey/docs/plan/remaining-work-2026-10-01.md`

### 6.3 采信说明

- 第 3 节每条 A 类均含「本项目源码行号」或「外仓文件:行号」；外仓引用已抽样复读原始行，非二手转述。
- 本项目源码结论逐条复跑过（第 6.1 节的命令）；A2/A3/A15 另有**实测**（SIGINT 行为、GBK 解码）。
- 证据采集由 4 路并行审计完成（dsfolder notes / Rust 兄弟仓 / 下游消费者 / Windows 与跨平台痛点），
  合并时对冲突项以本仓源码实读为准（例：P4「per-stream caps」被重新分诊，见 §2.4）。
- 标 **推断** 的只有两处：§2.5 的「0.3.0 安装版导致 los 门控回落 native」、平台事实清单第 20 条（PTY 差异）。
