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
- 功能分支：`feat/issue-{N}`（如 `feat/issue-220`）
- 修复分支：`fix/issue-{N}`（如 `fix/issue-42`）
- 使用下划线分隔多词，禁止大写字母

### PR 前检查清单
提交 PR 前必须在本地通过以下四步检查：

1. **格式化检查**
   ```bash
   cargo fmt && cargo fmt --check
   ```
2. **Clippy 静态检查**
   ```bash
   cargo clippy --workspace -- -D warnings
   ```
3. **测试**
   ```bash
   cargo test --workspace
   ```
   默认并行执行，确保所有测试稳定通过。
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

## References
- See DESIGN.md for architecture
- See CHANGELOG.md for version history
- See IMPLEMENTATION.md for implementation phases
- OpenCode Server API: https://opencode.ai/docs/server/
