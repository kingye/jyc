# JYC Project

JYC is a channel-agnostic AI agent framework written in Rust.
It monitors inbound channels (email via IMAP), routes messages to topics,
and uses an in-process AI agent to generate replies.

## Tech Stack
- Rust, tokio async runtime
- IMAP/SMTP for email channels
- In-process AI agent as the AI backend
- Docker for containerized deployment

## Code Conventions
- Use `tracing` for all logging (never `println!`)
- Error handling: propagate with `?`, use `.context()` for meaningful errors
- All public functions must have doc comments

## Git Rules
- NEVER run `git config user.name` or `git config user.email` (local or global)
- NEVER run `git config --global` for any setting

## 测试要求

### 测试隔离
- 使用 `tempfile::TempDir` 创建临时目录，测试后自动清理
- 禁止使用 `unsafe { std::env::set_var() }` 修改环境变量，避免污染全局状态
- 测试用例不得依赖外部服务（网络、文件系统固定路径），使用 mock 或 test fixture

### 并行安全
- `cargo test --workspace` 默认并行模式必须稳定通过
- 测试间不得共享可变状态；如必须串行，使用 `#[serial]` 标记并在 CI 中串行执行
- 资源泄漏（如端口占用）的测试必须实现 `Drop` 或使用 `TempDir` 自动清理

## 工作流约定

### 分支命名
- 功能分支：`feat/issue-{N}-<简短描述>`（如 `feat/issue-220-add-imap-idle`）
- 修复分支：`fix/issue-{N}-<简短描述>`（如 `fix/issue-42-fix-timeout-panic`）
- 使用连字符（`-`）分隔单词，禁止大写字母

### PR 前检查清单
- 本地只跑 `cargo check -p <改动的 crate>`（改了该 crate 的测试才加 `--tests`）和 `cargo fmt -- --check`；**禁止** `--workspace` 全量 check，以及本地运行 `cargo build` / `cargo test` / `cargo clippy` / `cargo llvm-cov`（开发机资源受限：全量 check 会 OOM 并把 `target/` 撑满，完整验证以 CI 为准）
- `cargo check --tests` **只编译不执行**：绿灯只证明测试能编译，禁止据此声称测试通过；测试结论只能来自 CI 的测试运行
- 本地 gate 每个变更集只跑一次：批量改完再跑，测试文件放最后改（`#[cfg(test)]` / `tests/*.rs` 一旦改动，test cfg 单元必然整包重编）
- 本地 gate 是编译级检查，热缓存下只需数秒；若突然变慢，通常是 `check` 缓存被打脏（`cargo build` / `cargo test` 的产物与 `check` 不同一类，本地跑一次就会让下一次 gate 重付整个依赖图，`CARGO_BUILD_JOBS=1` 时逐 crate 串行）。**禁止**以「太慢所以换更重的命令」为理由跑 `cargo test` / `cargo build`——那条命令正是把缓存打脏的元凶
- 跨 crate 的类型/接口变更：先用 grep 找全调用点，只 check 受影响的 crate，其余交给 CI；不为「更保险」重跑已通过的检查
- CI（`.github/workflows/ci.yml`）自动执行：fmt、clippy -D warnings、llvm-cov（60% 阈值）
- 改依赖时提交 `cargo check -p <crate>` 顺带刷新的 `Cargo.lock`
- 按「文档约定」检查是否需更新相关文档

### CI 等待规则
- **禁止**阻塞等待 CI：不得轮询 `gh pr checks` / `gh run watch`，不得用 `sleep` 重试，不得因为「CI 还没跑完」而推迟提交或推迟回复
- push / 开 PR 之后**立即结束本轮**并把链接交给用户；CI 由远端异步执行，失败时由用户或下一次消息再驱动修复
- 如确需状态：只查一次（`gh pr checks <branch>`），拿到输出立刻返回，不重复查询

### 提交信息格式
遵循 [Conventional Commits](https://www.conventionalcommits.org/)：`feat:` / `fix:` / `refactor:` / `docs:` / `test:` / `chore:`（示例：`feat: add IMAP idle support for real-time email monitoring`）

## 文档约定

### 文件用途映射
| 文件 | 定位 |
|------|------|
| `DESIGN.md` | 系统架构设计文档，记录设计决策和 trade-off |
| `CHANGELOG.md` | 面向用户的版本变更记录 |
| `docs/` | 专题文档目录（API 文档、配置指南等） |
| `AGENTS.md` | AI agent 行为约束规则，使用精简、断言式语言编写 |

> **AGENTS.md 编写规则**：使用断言式语言（"必须……" / "禁止……"），避免冗长描述，每条规则可直接作为判断依据。

### 文档更新触发规则
| 变更类型 | 需更新文档 |
|----------|------------|
| 架构变更、新 crate、模块拆分/合并 | `DESIGN.md` |
| 新增配置项或环境变量 | `config.example.toml` 及 `README.md` |
| 新增 channel 类型 | `docs/channels/` 对应文档 |
| 功能变更（新增/修改/移除） | `CHANGELOG.md` |
| Agent 行为规则变更 | `AGENTS.md` |

### CHANGELOG 格式约束
遵循 [Keep a Changelog](https://keepachangelog.com/) 规范，按 **Added / Changed / Fixed / Removed** 顺序组织，每项格式：`- {简短描述} (#{issue/PR 编号})`

## Agent Behavior Rules

### Reply vs. SendMessage
- Agent must use `reply_message` for in-topic responses; `jyc_send_message` only for out-of-topic proactive messages.
- Agent must not use `jyc_send_message` to spam users; limit to alerts and notifications.

### Task List
- Multi-step work (a plan, an implementation, a cross-file fix) must start with `task_create`; the list is the topic's single plan of record.
- Mark an item `in_progress` when picking it up and `completed` with `task_update` as soon as it is genuinely done — never ahead of the work, never with validation still failing.
- Re-plan with a new `task_create` (it replaces the list and renumbers ids) rather than patching a stale one item by item.
- After a context reset, call `task_list` for the ids and current progress; never guess ids from memory.

## References
- See DESIGN.md for architecture
- See CHANGELOG.md for version history

- jin AGENTS.md (约束来源参考): https://github.com/kingye/jin/blob/main/AGENTS.md
