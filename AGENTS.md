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
- `cargo test --workspace` 默认并行模式必须稳定通过
- 测试间不得共享可变状态；如必须串行，使用 `#[serial]` 标记并在 CI 中串行执行
- 资源泄漏（如端口占用）的测试必须实现 `Drop` 或使用 `TempDir` 自动清理

## 工作流约定

### 分支命名
- 功能分支：`feat/issue-{N}-<简短描述>`（如 `feat/issue-220-add-imap-idle`）
- 修复分支：`fix/issue-{N}-<简短描述>`（如 `fix/issue-42-fix-timeout-panic`）
- 使用连字符（`-`）分隔单词，禁止大写字母

### PR 前检查清单
提交 PR 前必须在本地通过以下检查：

> 注意：`.github/workflows/ci.yml` 会自动执行格式化、Clippy、测试（覆盖率检查 `cargo llvm-cov` 会运行全部测试）等检查；Agent 无需在本地重复运行 CI 已覆盖的慢速检查（如 `cargo test`、`cargo llvm-cov`）。

1. **格式化检查**
   ```bash
   cargo fmt --check
   ```
2. **Clippy 静态检查**
   ```bash
   cargo clippy --workspace -- -D warnings
   ```
3. **文档确认** — 根据变更类型检查是否需要更新相关文档（参见「文档约定」章节）
4. **禁止本地运行 CI 专属检查** — `cargo llvm-cov`、`cargo-tarpaulin` 等覆盖率工具，以及 GitHub Actions 工作流中已自动执行的其他检查（包括测试），禁止在本地运行。
   这些检查速度太慢，CI（`.github/workflows/ci.yml`）会在 PR 提交后自动运行并检查阈值。

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
