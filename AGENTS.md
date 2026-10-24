# AGENTS.md — unirun

面向编码 agent（DSH / Codex / Claude Code / Cursor）与自动化脚本的项目约定。

## 项目概览

- `unirun`：跨平台命令执行归一化库（one command spec in, one normalized result out）。
- 纯 Rust（edition 2021），无 workspace；`src/` 为库 + `src/main.rs` 二进制。
- 测试：`cargo test`（`tests/` 集成 + 单元）。

## 提交前：Rust 格式化门禁（必须）

修改 Rust 代码后，**不要用 `cargo fmt`**（会重排整个 workspace）。用 `fmtguard`
（scoped, gated rustfmt，本机 `~/.cargo/bin/fmtguard`，PATH 已就绪）：

```sh
# 1. 检查 fmtguard 打算改什么（dry-run，永不写盘）
fmtguard --scope-from-git --emit patch

# 2. 确认 patch 只覆盖你实际编辑过的文件/区域（±3 行上下文内），再应用
fmtguard --scope-from-git --apply

# 3. 机械检查
fmtguard --scope-from-git --emit json   # verdict 必须是 "ok"
git diff --check                        # 无空白错误
git diff --stat                         # 规模符合预期（未被 formatter 放大）

# 4. 最终裁决（必须）：fmtguard 只保证改动范围不新增格式债，不保证整文件
#    rustfmt-clean；CI 门禁是 cargo fmt --check（检查整个文件），以它为准。
cargo fmt --check                       # 红 = scope 外有债：整文件 cargo fmt 或手动补齐，不允许带红提交
```

> 职责分工：fmtguard = 范围裁剪器（限制 formatter 改动范围，防全文件重排）；
> `cargo fmt --check` = 整文件干净性裁决（CI 门禁语义）。fmtguard 报 ok ≠ 文件干净。

退出码契约：`0` = ok / 无事可做；`1` = 某道门禁拒绝（**任何文件都不写盘**）——
先检查自己的改动，不要盲目放宽预算；`2` = fmtguard 自身出错（含 rustfmt 解析失败），
修好源文件再重试。

### 门禁与预算（默认值，一般不用动）

| gate | 默认 | 触发时 |
|------|------|--------|
| `scope.containment` | — | formatter 改了 scope 外文件 → bug 信号，先查再放宽 |
| `budget.per_file_added` | 200 行/文件 | 改动本身会让 rustfmt 重排大片时提高 |
| `budget.diff_ratio` | 3.0 | formatter 新增 > 你新增 ×3 时检查范围或提高 |
| `budget.max_files` | 5 | 一次改了 >5 个 .rs 文件时提高 |
| `whitespace.clean` | — | formatter 产生尾随空白 → 异常，先查 |

### 事件日志（审计）

每次运行追加写入 `.fmtguard/runs.jsonl`（已加入 `.gitignore`，不入库）。
审计/回放：`fmtguard replay <run_id> --emit json|patch`（run_id 来自 `--emit json` 输出的 `run_id`）。

## 改「纯核心」后：Miri 门禁（必须）

`encoding` / `coalesce` / `taxonomy` 是无系统调用、无时钟、无文件系统的纯数据变换，
因此是**唯一能被机器证明**的部分（Miri 过不了 FFI 与真实 syscall）。改动它们后跑：

```sh
bash scripts/miri-pure-core.sh              # 0=pass  1=fail（含 UB / 低于测试下限）  2=判不了
bash scripts/miri-pure-core.sh --self-test  # 门禁自检：注入 UB 必须报红，干净 crate 必须绿
```

三条不可让步的语义：

- **`2` 不是通过**。找不到 Miri / 没有 sysroot / 输出无法解析一律 `2`，调用方不得当绿。
- **测试数下限（`MIRI_MIN_PASSED`，当前 34）是判据的一部分**。filter 匹配不到测试时的
  "0 passed" 与"真的都过了"必须可区分；新增纯核心测试后同步抬高下限。
- **禁止 `#[cfg(miri)]` 分支**（门禁会 grep 并在命中时直接失败）：它会让门禁证明的
  是另一个程序——Miri 下才存在的代码永不进生产，Miri 下消失的代码藏起待证路径。

Miri 比原生慢一个数量级以上，所以只喂纯核心，不喂 `exec`/`process_identity`/`transport`/`session`
（它们是薄壳，由集成测试与真机验收负责）。

## 其他约定

- 分支：`main`；提交信息遵循 conventional commits（见 `git log` 历史风格）。
- 修改涉及 `src/` 与 `tests/` 时，先补测试再提交。
