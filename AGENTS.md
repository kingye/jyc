# JYC Project

JYC is a channel-agnostic AI agent framework written in Rust.
It monitors inbound channels (email via IMAP), routes messages to threads,
and uses OpenCode to generate AI replies.

## Tech Stack
- Rust, tokio async runtime
- IMAP/SMTP for email channels
- OpenCode as the AI backend
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
- 测试必须保证 `cargo test --workspace` 默认并行模式下稳定通过；测试间不得共享可变状态；如必须串行，使用 `#[serial]` 标记并在 CI 中串行执行
- 资源泄漏（如端口占用）的测试必须实现 `Drop` 或使用 `TempDir` 自动清理

## Agent 本地验证规则（强制约束）

CI 流水线（`.github/workflows/ci.yml`）会在 PR 提交后自动执行以下耗时检查。Agent **必须**遵守以下约束，避免在本地重复运行：

### 禁止运行的命令

- `cargo test`（任何形式：`cargo test --workspace`、`cargo test -p <crate>`、单个测试等）
- `cargo test -p <crate> --all-targets` / `cargo test --all-targets`
- `cargo llvm-cov`、`cargo-tarpaulin` 或其他覆盖率工具
- 无 `-p` 参数的 `cargo clippy`（即 `cargo clippy --workspace`、`cargo clippy` 默认行为）
- 无 `-p` 参数的 `cargo build`（即 `cargo build --workspace`、`cargo build` 默认行为）

这些命令在 monorepo 上单次运行耗时数分钟，多步骤开发中累积成本极高。CI 是唯一的验证路径。

### 允许运行的命令（即时反馈，CI 不替代）

- `cargo fmt --check` — 格式化检查，秒级
- `cargo check` — 全 workspace 编译期类型检查，快速（比 `cargo build` 快数倍）
- `cargo check -p <crate>` — 针对单 crate 的类型检查
- `cargo clippy -p <crate>` — 针对单 crate 的 lint 检查
- `cargo build -p <crate>` — 仅在必要时（如集成测试需要外部 binary），不替代 CI 全 workspace 构建

`cargo clippy -p <crate>` 允许用于针对性的 lint 验证，因为 Agent 需要在编写代码时立即发现 clippy 警告。但是 `cargo clippy --workspace`（无 `-p`）禁止，因为本质上是 CI 已覆盖的检查。`cargo build -p <crate>` 同理——仅在真的需要构建单个 crate 的 binary 时运行（例如 `cargo run -p jyc-cli -- --help` 或集成测试需要 `cargo build -p jyc-channels`）。

同样的：`cargo check` / `cargo check -p <crate>` 允许（编译期类型检查，比完整构建快几个数量级）；`cargo build` / `cargo build --workspace` 禁止（完整构建，CI 覆盖）。

## 工作流约定

### 分支命名
- 功能分支：`feat/issue-{N}-<简短描述>`（如 `feat/issue-220-add-imap-idle`）
- 修复分支：`fix/issue-{N}-<简短描述>`（如 `fix/issue-42-fix-timeout-panic`）
- 使用连字符（`-`）分隔单词，禁止大写字母

### PR 前检查清单（Agent 必须遵守本地验证规则）

提交 PR 前在本地执行以下**允许的**检查即可。CI 流水线（`.github/workflows/ci.yml`）会自动运行其余检查：

> 完整的 CI 覆盖范围：`.github/workflows/ci.yml` 在 PR 提交后自动执行 `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo llvm-cov --workspace --all-targets --summary-only` 等检查。Agent 严禁在本地重复运行 `cargo test`、`cargo clippy --workspace`、`cargo llvm-cov` 等耗时检查——参见「Agent 本地验证规则」一节。

1. **格式化检查**（CI 之外的本地强制项）
   ```bash
   cargo fmt --check
   ```
2. **本地类型检查**（针对受影响 crate）
   ```bash
   cargo check -p <affected-crate>
   # 或全 workspace 的快速类型检查（不构建）：
   cargo check
   ```
3. **本地 lint 检查**（仅当需要即时发现警告时）
   ```bash
   cargo clippy -p <affected-crate> -- -D warnings
   ```
4. **文档确认** — 根据变更类型检查是否需要更新相关文档（参见「文档约定」章节）

### 提交信息格式
遵循 [Conventional Commits](https://www.conventionalcommits.org/) 格式：

| 类型 | 用途 |
|------|------|
| `feat:` | 新功能 |
| `fix:` | 错误修复 |
| `refactor:` | 重构（无功能变更） |
| `docs:` | 文档变更 |
| `test:` | 测试相关 |
| `chore:` | 构建、CI、依赖等杂务 |

示例：`feat: add IMAP idle support for real-time email monitoring`

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
遵循 [Keep a Changelog](https://keepachangelog.com/) 规范，按以下顺序组织：

1. **Added** — 新增功能
2. **Changed** — 已变更的功能
3. **Fixed** — 已修复的 bug
4. **Removed** — 已移除的功能

每项使用 `-` 列表，格式：`- {简短描述} (#{issue/PR 编号})`

## Agent Behavior Rules

### Reply vs. SendMessage
- Agent must use `reply_message` for in-thread responses; `jyc_send_message` only for out-of-thread proactive messages.
- Agent must not use `jyc_send_message` to spam users; limit to alerts and notifications.

## References
- See DESIGN.md for architecture
- See CHANGELOG.md for version history
- See IMPLEMENTATION.md for implementation phases
- OpenCode Server API: https://opencode.ai/docs/server/
- jin AGENTS.md (约束来源参考): https://github.com/kingye/jin/blob/main/AGENTS.md
