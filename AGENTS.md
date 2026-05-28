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

## Development Workflow
- Always create a feature branch: `git checkout -b feat/<name>`
- After changes, run tests: `cargo test`
- Commit with clear messages describing what changed and why
- Push immediately after committing

## References
- See DESIGN.md for architecture
- See CHANGELOG.md for version history
- See IMPLEMENTATION.md for implementation phases
- OpenCode Server API: https://opencode.ai/docs/server/
