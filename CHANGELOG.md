# Changelog

All notable changes to JYC will be documented in this file.

## [0.0.1] - 2026-03-27

### Added

**Phase 1: Foundation**
- CLI skeleton with `clap` — subcommands: `monitor`, `config init`, `config validate`, `patterns list`, `state`, and hidden `mcp-reply-tool`
- TOML configuration with `${ENV_VAR}` substitution for secrets
- Configuration validation with structured error reporting
- Core types: `InboundMessage`, `InboundAdapter`/`OutboundAdapter` traits, channel pattern matching types
- `ChannelRegistry` for adapter lookup by channel name
- Unified `CommandRegistry::process_commands()` — single-pass parse, execute, and strip commands from message body (improved over jiny-m's split design)
- `CommandHandler` trait for extensible email commands (`/model`, `/plan`, `/build`)
- `tracing` + `tracing-subscriber` for structured async-aware logging with `--debug` and `--verbose` CLI flags
- Error types via `thiserror`, application errors via `anyhow`
- Utility functions: `parse_file_size`, `validate_regex`, `extract_domain`, `sanitize_for_filesystem`
- Default constants for timeouts, context limits, and configuration defaults

**Phase 2: Email I/O Layer**
- IMAP client wrapper (`async-imap` + `async-native-tls`) with TLS, login, SELECT, FETCH by UID/range, IDLE support, and disconnect
- IMAP ID command (RFC 2971) sent after login — required by 163.com (NetEase) to avoid "Unsafe Login" rejection
- Email parser: `strip_reply_prefix` (Re:/Fwd:/回复:/转发:), `derive_thread_name`, `strip_quoted_history`, `clean_email_body`, `truncate_text`, `parse_stored_message`, `parse_stored_reply`, `format_quoted_reply`
- Email inbound adapter: `mail-parser` raw bytes → `InboundMessage` with boundary cleaning; pattern matching (sender exact/domain/regex + subject prefix/regex, AND logic, first match wins)
- SMTP client (`lettre`) with TLS, threading headers (`In-Reply-To`, `References`), markdown→HTML via `comrak` (GFM), auto-reconnect on connection errors
- HTML→Markdown conversion via `htmd`
- Email outbound adapter: `send_reply`, `send_alert`, `send_progress_update` — thread-safe via `Arc<Mutex<SmtpClient>>`
- Per-channel state manager: `.imap/.state.json` + `.processed-uids.txt` for IMAP sequence tracking and UID deduplication

**Phase 3: Core Processing Pipeline**
- Message storage: `received.md` with YAML frontmatter, `reply.md`, attachment saving with extension allowlist, size limits, collision resolution
- Thread manager: per-thread `tokio::sync::mpsc` channels with `Semaphore`-bounded concurrency (configurable `max_concurrent_threads`)
- Message router: delegates pattern matching to channel adapter, derives thread name, dispatches to thread manager
- IMAP monitor: connect → SELECT → check_for_new → IDLE/poll → loop; exponential backoff on errors; recovery on message deletion; first-run only processes latest message
- Full `jyc monitor` wiring: load config → validate → Ctrl+C handler → per-channel SMTP connect → ThreadManager → Router → StateManager → spawn ImapMonitor tasks → await shutdown
- Placeholder reply in OpenCode mode (sends confirmation email with message metadata until Phase 4 AI integration)

### Directory Layout

```
<root>/
├── config.toml
├── <channel>/
│   ├── .imap/
│   │   ├── .state.json
│   │   └── .processed-uids.txt
│   └── workspace/
│       └── <thread>/
│           ├── messages/<timestamp>/
│           │   ├── received.md
│           │   └── reply.md
│           ├── .jyc/
│           ├── .opencode/
│           ├── opencode.json
│           └── system.md
```
