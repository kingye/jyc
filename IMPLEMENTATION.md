# JYC Implementation Plan

## Overview

This document outlines the phased implementation plan for building JYC, the Rust rewrite of jiny-m. Each phase produces a testable, functional increment. Phases are ordered by dependency — later phases build on earlier ones.

**Estimated total: ~35 implementation tasks across 6 phases.**

---

## Phase 1: Foundation (Skeleton + Config + CLI + Types)

**Goal:** A compilable binary that parses CLI args, loads TOML config, and validates it. All core types defined.

### Tasks

| # | Task | Files | Description | Test Strategy |
|---|------|-------|-------------|---------------|
| 1.1 | Initialize Cargo project | `Cargo.toml` | All dependencies declared, features configured | `cargo build` compiles |
| 1.2 | CLI skeleton with clap | `src/main.rs`, `src/cli/*.rs` | All subcommands as stubs: `monitor`, `config init`, `config validate`, `patterns list`, `state`, `mcp-reply-tool` (hidden). Global `-w/--workdir` flag | `cargo run -- --help` shows all commands |
| 1.3 | Core types | `src/channels/types.rs` | `InboundMessage`, `MessageContent`, `MessageAttachment`, `PatternMatch`, `ChannelPattern`, `InboundAdapter` trait, `OutboundAdapter` trait, `SendResult` | Compiles, types are `Send + Sync` |
| 1.4 | Config types + TOML parsing | `src/config/types.rs`, `src/config/mod.rs` | `AppConfig`, `ChannelConfig`, `ImapConfig`, `SmtpConfig`, `MonitorConfig`, `AgentConfig`, `AlertingConfig` structs with serde derives. `load_config()`: read file, parse TOML, expand env vars | Unit test: parse sample TOML, env var substitution |
| 1.5 | Config validation | `src/config/validation.rs` | Validate required fields, port ranges, TLS modes, pattern regex compilation, file size string parsing. Return structured validation errors | Unit test: valid config passes, invalid configs produce correct errors |
| 1.6 | `config init` command | `src/cli/config.rs` | Write default `config.toml` template to working directory | Manual: `jyc config init` creates file |
| 1.7 | `config validate` command | `src/cli/config.rs` | Load and validate config, print results | Manual: `jyc config validate` reports issues |
| 1.8 | Tracing setup | `src/main.rs` | Initialize `tracing-subscriber` with env filter (`RUST_LOG`), structured format. `-d/--debug` and `-v/--verbose` flags set log levels | Logs appear with correct levels |
| 1.9 | Error types | `src/utils/mod.rs` | Define `thiserror` error enums for config, IMAP, SMTP, OpenCode, MCP errors | Compiles |
| 1.10 | Utility functions | `src/utils/helpers.rs`, `src/utils/constants.rs` | `parse_file_size("25mb")`, regex validation helpers, default constants (timeouts, limits) | Unit tests for parse_file_size, regex validation |

**Deliverable:** `jyc config init && jyc config validate` works end-to-end.

---

## Phase 2: Email I/O Layer

**Goal:** Can connect to IMAP, fetch emails, parse them into `InboundMessage`, and send replies via SMTP.

### Tasks

| # | Task | Files | Description | Test Strategy |
|---|------|-------|-------------|---------------|
| 2.1 | IMAP client wrapper | `src/services/imap/client.rs` | Wrap `async-imap`: connect with TLS, login, select mailbox, fetch by UID range, fetch by sequence range, IDLE support, disconnect. Handle `async-imap`'s `runtime-tokio` feature | Integration test: connect to real IMAP (behind feature flag) |
| 2.2 | Email parser | `src/core/email_parser.rs` | `strip_reply_prefix(subject)` — strip Re:/Fwd:/回复:/转发:. `clean_email_body(text)` — fix bracket nesting, normalize. `strip_quoted_history(text)` — line-by-line scan for reply headers, dividers, `>>` quotes. `truncate_text(text, max)`. `derive_thread_name(subject, prefixes)` — strip reply prefixes, strip pattern prefixes (sorted longest-first), sanitize for filesystem | Unit tests: port all cases from jiny-m's email-parser.test.ts |
| 2.3 | Email inbound adapter | `src/channels/email/inbound.rs` | Implement `InboundAdapter` trait. `mail-parser` → `InboundMessage` conversion. `match_message()`: sender exact/domain/regex + subject prefix/regex (AND logic). `derive_thread_name()`: channel-specific implementation | Unit tests: message conversion, pattern matching |
| 2.4 | SMTP client | `src/services/smtp/client.rs` | Wrap `lettre`: connect with TLS/STARTTLS, send email with HTML body (markdown→HTML via `comrak`), set `In-Reply-To`, `References`, `Message-ID` headers, `Re:` subject prefix. Auto-reconnect on connection errors (one retry) | Integration test: send test email (behind feature flag) |
| 2.5 | Email outbound adapter | `src/channels/email/outbound.rs` | Implement `OutboundAdapter` trait. `send_reply()`: builds full email from `InboundMessage` + reply text. `send_alert()`: fresh email without threading headers. `send_progress_update()`: progress email in same thread | Unit test: email construction (headers, body) |
| 2.6 | State manager | `src/core/state_manager.rs` | Per-channel state persistence: `state.json` (last_sequence_number, last_processed_uid, uid_validity) + `processed-uids.txt` (append-only). `StateManager::for_channel(name)` constructor. Load/save/track_uid/reset | Unit test: state persistence round-trip |
| 2.7 | HTML to Markdown | `src/services/smtp/client.rs` | `comrak` for MD→HTML (GFM mode: tables, autolinks, strikethrough). `htmd` for HTML→MD (used when storing HTML-only emails) | Unit test: round-trip sample content |

**Deliverable:** Can fetch an email from IMAP, parse it into `InboundMessage`, and send an SMTP reply.

---

## Phase 3: Core Processing Pipeline

**Goal:** Messages flow from IMAP through pattern matching, thread queuing, and storage. No AI yet — replies are static text.

### Tasks

| # | Task | Files | Description | Test Strategy |
|---|------|-------|-------------|---------------|
| 3.1 | Message storage | `src/core/message_storage.rs` | `store(msg, thread_name)` → create `messages/<timestamp>/`, write `received.md` with YAML frontmatter, save allowlisted attachments. `store_reply(thread_path, reply_text, message_dir)` → write `reply.md`. Collision handling (counter suffix) | Unit test: write/read round-trip, collision handling |
| 3.2 | Thread trail builder | `src/core/email_parser.rs` | `build_thread_trail(thread_path, options)` — read message dirs sorted newest-first, parse received.md and reply.md, strip history, return `Vec<TrailEntry>`. `prepare_body_for_quoting(thread_path, current_msg)` — format trail as quoted markdown blocks | Unit test: trail ordering, formatting |
| 3.3 | Channel registry | `src/channels/registry.rs` | `HashMap<String, Arc<dyn InboundAdapter>>` + `HashMap<String, Arc<dyn OutboundAdapter>>`. Register/lookup by channel name | Unit test: register and retrieve adapters |
| 3.4 | Message router | `src/core/message_router.rs` | Receives `InboundMessage` via mpsc. Looks up adapter from registry. Calls `adapter.match_message()`. Calls `adapter.derive_thread_name()`. Sends to `ThreadManager::enqueue()` | Unit test with mock adapters |
| 3.5 | Thread manager | `src/core/thread_manager.rs` | `Semaphore` + per-thread `mpsc` channels. `enqueue()`, `spawn_worker()`, process loop. Graceful shutdown via `CancellationToken`. Stats reporting (`get_stats()`) | Unit test: concurrency limiting, queue overflow, ordering |
| 3.6 | IMAP monitor | `src/services/imap/monitor.rs` | Main monitoring loop: connect → check_for_new → IDLE/poll → loop. Recovery mode on deletion/suspicious jump. Uses `StateManager` for tracking. `CancellationToken` for shutdown. Exponential backoff on errors | Integration test with real IMAP (feature flag) |
| 3.7 | Monitor command wiring | `src/cli/monitor.rs` | Wire everything: load config → create registry → register adapters → create storage + router + thread_manager → start all inbound adapters as tokio tasks → await shutdown signal | Manual: `jyc monitor` connects and processes |
| 3.8 | Security: PathValidator | `src/security/path_validator.rs` | File path validation: no traversal, no hidden files, no null bytes, max length, Unicode NFC. Extension allowlist. Size limit check | Unit tests: various attack patterns |

**Deliverable:** `jyc monitor` connects to IMAP, matches emails, queues them per-thread, stores as markdown, and sends a static reply.

---

## Phase 4: AI Integration

**Goal:** OpenCode AI generates replies via SSE streaming with full session management.

### Tasks

| # | Task | Files | Description | Test Strategy |
|---|------|-------|-------------|---------------|
| 4.1 | OpenCode server manager | `src/services/opencode/mod.rs` | Start/stop OpenCode server process (`tokio::process::Command`). Auto-find free port (49152+). Health check (HTTP ping). Auto-restart on death | Manual: server starts, health check passes |
| 4.2 | OpenCode HTTP client | `src/services/opencode/client.rs` | `reqwest`-based client: `create_session()`, `prompt_async()`, `prompt_blocking()`, `get_session()`. JSON request/response types matching OpenCode API | Unit test with mock HTTP server |
| 4.3 | SSE streaming | `src/services/opencode/client.rs` | `subscribe_events(directory)` → `reqwest-eventsource` stream. Event parsing: `server.connected`, `message.updated`, `message.part.updated`, `session.status`, `session.idle`, `session.error`. Part accumulation with dedup by ID | Unit test: parse sample SSE events |
| 4.4 | Activity-based timeout | `src/services/opencode/client.rs` | `tokio::select!` with `tokio::time::interval(5s)`. Check `last_activity` vs now. 30min default, 60min when tool running. Progress logging every 10s | Unit test: timeout triggers correctly |
| 4.5 | Session management | `src/services/opencode/session.rs` | Per-thread session persistence (`opencode-session.json`). `get_or_create_session()`, `delete_session()`. Thread OpenCode config (`opencode.json`) generation with staleness detection | Unit test: session create/read/delete, config staleness |
| 4.6 | Prompt builder | `src/services/opencode/prompt_builder.rs` | `build_system_prompt(thread_path)` — base prompt + optional `system.md`. `build_prompt(msg, thread_path, message_dir)` — context from thread files + stripped body + base64 reply_context. Respects token budget constants | Unit test: prompt construction with various inputs |
| 4.7 | Stale session detection | `src/services/opencode/client.rs` | If SSE reports tool success but signal file missing → delete session → retry once with fresh session. Signal file cleanup before each prompt | Unit test: detection logic |
| 4.8 | ContextOverflow recovery | `src/services/opencode/client.rs` | On `session.error` with ContextOverflow → create new session → retry with blocking prompt | Unit test: recovery flow |
| 4.9 | Wire AI into workers | `src/core/thread_manager.rs` | Replace static reply with `OpenCodeService::generate_reply()`. Handle reply-sent-by-tool vs fallback | Manual: end-to-end AI reply |

**Deliverable:** `jyc monitor` processes emails with AI-generated replies via OpenCode SSE streaming.

---

## Phase 5: MCP + Commands

**Goal:** MCP reply tool works as a subprocess. Email commands (/model, /plan, /build) are functional.

### Tasks

| # | Task | Files | Description | Test Strategy |
|---|------|-------|-------------|---------------|
| 5.1 | Reply context serialization | `src/mcp/context.rs` | `ReplyContext` struct. `serialize_context()` → JSON → base64. `deserialize_context()` → base64 → JSON → validate + integrity checks (nonce, formatting detection) | Unit test: round-trip, tamper detection |
| 5.2 | MCP reply tool (rmcp) | `src/mcp/reply_tool.rs` | `rmcp` stdio server with `reply_message` tool. Decode context → load config → create outbound adapter → read received.md → build full reply → send → store → write signal file. Log to `reply-tool.log` | Manual: invoke via MCP client |
| 5.3 | Hidden subcommand | `src/cli/mcp_reply.rs` | `jyc mcp-reply-tool` starts the rmcp stdio server. Reads `JYC_ROOT` env var | Manual: `echo '...' \| jyc mcp-reply-tool` |
| 5.4 | Reply tool command resolution | `src/services/opencode/session.rs` | `get_reply_tool_command()`: find `jyc` binary path → `["/path/to/jyc", "mcp-reply-tool"]`. Write into `opencode.json` MCP config | Unit test: path resolution |
| 5.5 | Unified command processing | `src/core/command/registry.rs`, `src/core/command/handler.rs` | `CommandRegistry::process_commands(body, ctx)`: single-pass parse, execute, and strip commands from body. Returns `CommandOutput { results, cleaned_body, body_empty }`. Defines `CommandHandler` trait, `CommandContext`, `CommandResult`, `CommandOutput` types. Unlike jiny-m's split design (parseCommands + separate stripping in thread-manager), all command logic lives here | Unit test: parse various formats, body stripping, empty body detection |
| 5.6 | /model command | `src/core/command/model_handler.rs` | Write `.jyc/model-override`, delete `opencode-session.json`, return result. `/model reset` removes override | Unit test: file operations |
| 5.7 | /plan and /build commands | `src/core/command/mode_handler.rs` | Write/remove `.jyc/mode-override`. Pass `agent: "plan"` to OpenCode prompt when active | Unit test: mode switching |
| 5.8 | Wire commands into workers | `src/core/thread_manager.rs` | After store, before prompt: call `command_registry.process_commands()`. Check `body_empty` + results → direct reply or continue with `cleaned_body`. ThreadManager has no knowledge of command syntax | Manual: send email with /model command |

**Deliverable:** Full MCP reply tool pipeline. Email commands change AI model and mode per-thread.

**Additional changes implemented during Phase 5:**
- `AgentService` trait (`src/services/agent.rs`) — ThreadManager dispatches via `Arc<dyn AgentService>` instead of `match` on mode
- `StaticAgentService` (`src/services/static_agent.rs`) — implements `AgentService` for static reply mode
- `OpenCodeService` implements `AgentService` — owns full reply lifecycle (building, sending, storing)
- File attachment support in SMTP client (`MultiPart::mixed` + `Attachment` parts)
- `message.channel` set to config channel **name** (e.g., "jiny283"), not type ("email")
- HTML→Markdown body extraction (prefers HTML over raw plain text for proper line breaks)
- HTML cleaning: strips `<style>`, `<script>`, `<head>`, CSS rules before htmd conversion
- MCP server name fixed to `"jiny_reply"` + `#[tool_handler]` for tool discovery

---

## Phase 6: Resilience + Polish

**Goal:** Production-ready with alerting, health checks, progress tracking, and all CLI commands.

### Tasks

| # | Task | Files | Description | Test Strategy |
|---|------|-------|-------------|---------------|
| 6.1 | Alert service | `src/core/alert_service.rs` | Custom `tracing::Layer` captures ERROR events → mpsc channel → alert task. Error buffering with rolling context window. Periodic flush → digest email via outbound adapter. Self-protection (skip `_alert_internal` events) | Unit test: buffering, digest formatting |
| 6.2 | Health check | `src/core/alert_service.rs` | Stats tracking from tracing events (pattern-match on known messages). Periodic health report email. Per-thread breakdown. Queue stats from `ThreadManager::get_stats()`. Stats reset after report | Unit test: stats tracking, report formatting |
| 6.3 | Progress tracker | `src/core/progress_tracker.rs` | Background `tokio::time::interval`. Sends progress update emails at configured intervals during long AI operations. Uses outbound adapter's `send_progress_update()` | Unit test: timing logic |
| 6.4 | Startup health check | `src/cli/monitor.rs` | Send one-time startup notification email with version and timestamp | Manual: startup email received |
| 6.5 | `patterns list` command | `src/cli/patterns.rs` | Load config, display all patterns with their rules in formatted output | Manual: `jyc patterns list` shows patterns |
| 6.6 | `state` command | `src/cli/state.rs` | Load per-channel state files, display last sequence number, last processed UID, timestamp | Manual: `jyc state` shows state |
| 6.7 | Graceful shutdown | `src/main.rs`, all components | `tokio::signal::ctrl_c()` → `CancellationToken::cancel()`. Await all worker join handles. Alert service final flush. OpenCode session cleanup. SMTP disconnect | Manual: Ctrl+C exits cleanly, no orphan processes |
| 6.8 | Comprehensive error handling | All files | Audit all `unwrap()` calls → replace with `?` or proper error handling. Ensure all errors have context via `anyhow::Context` | Code review pass |
| 6.9 | Logging audit | All files | Ensure structured fields (`thread`, `channel`, `message_id`) on all log lines. Consistent log levels. Sensitive data redacted | Code review pass |

**Deliverable:** Production-ready binary with alerting, health checks, progress tracking, graceful shutdown, and all CLI commands functional.

---

## Testing Strategy

### Unit Tests

- **Email parser**: Port all test cases from jiny-m's `email-parser.test.ts` (669 lines — the most comprehensive test file)
- **Config parsing**: Valid/invalid TOML, env var substitution, validation errors
- **Pattern matching**: Sender exact/domain/regex, subject prefix/regex, AND logic
- **Thread name derivation**: Reply prefix stripping, pattern prefix stripping, filesystem sanitization
- **Message storage**: Write/read round-trip, collision handling, frontmatter parsing
- **Thread trail**: Ordering, stripping, formatting
- **Reply context**: Serialization round-trip, tamper detection, nonce validation
- **Command parsing**: Various formats, edge cases
- **Utility functions**: `parse_file_size`, regex validation, date formatting

### Integration Tests (behind `#[cfg(feature = "integration")]`)

- **IMAP**: Connect, fetch, IDLE (requires real IMAP server)
- **SMTP**: Send test email (requires real SMTP server)
- **End-to-end**: IMAP → process → SMTP (requires both servers)

### Manual Testing

- Run `jyc monitor` against a real email account
- Send test emails with various patterns, commands, attachments
- Verify AI replies, thread continuity, model switching

---

## Build & Distribution

```bash
# Development build
cargo build

# Release build (optimized, single static binary)
cargo build --release

# Cross-compile (example: Linux x86_64 musl for Docker)
cargo build --release --target x86_64-unknown-linux-musl

# Run
./target/release/jyc monitor --workdir /path/to/data
```

**Binary name:** `jyc` (single binary, ~10-20 MB estimated release size)

**Docker:** Same approach as jiny-m but simpler — single binary `COPY` instead of Bun runtime + two binaries.

---

## Migration from jiny-m

JYC uses a fresh TOML config format. Users migrating from jiny-m need to:

1. Convert `.jyc/config.json` → `config.toml` (manual, one-time)
2. Existing workspace data (`messages/`, `.jyc/` state) is compatible — same directory structure
3. Per-channel state files (`.state.json`, `.processed-uids.txt`) are compatible
4. `opencode.json` per-thread is regenerated automatically

A future `jyc migrate` command could automate config conversion.

---

## Phase Dependencies

```
Phase 1: Foundation
    │
    ▼
Phase 2: Email I/O ──────────────────┐
    │                                 │
    ▼                                 │
Phase 3: Core Pipeline               │
    │                                 │
    ▼                                 │
Phase 4: AI Integration              │
    │                                 │
    ├─────────────────┐               │
    ▼                 ▼               │
Phase 5: MCP     Phase 6: Resilience │
  + Commands       + Polish ◄────────┘
    │                                 │
    └─────────────────────────────────┘
    │
    ▼
Phase 7: Feishu Channel
    (Can be developed in parallel with Phases 5-6
     once Core Pipeline is established)
```

 Phases 5 and 6 can be worked on partially in parallel once Phase 4 is complete. Phase 6 tasks like alerting and progress tracking depend on Phase 2's outbound adapter but not on Phase 4's AI integration.

---

## Phase 7: Feishu Channel Implementation

**Goal:** Add support for Feishu (飞书/Lark) as a fully functional channel with real-time messaging capabilities.

### Background

Feishu channel implementation was added as an extension to the core JYC architecture, demonstrating the channel-agnostic design. Unlike email which uses IMAP/SMTP protocols, Feishu uses a modern REST API with WebSocket support for real-time updates.

### Implementation Summary

The Feishu channel was implemented in a series of focused iterations:

**Phase 7.1: Foundation**
* **FeishuConfig structure** - Configuration for Feishu app credentials and WebSocket settings
* **Channel type registration** - Integration with channel registry system
* **Basic adapter skeletons** - Inbound and outbound adapter stubs

**Phase 7.2: Client Implementation**
* **FeishuClient** - Integration with openlark SDK for API calls
* **Authentication** - App token management with automatic refresh
* **Message sending** - Basic text message sending via Feishu API
* **Error handling** - Comprehensive FeishuError enum with detailed error types

**Phase 7.3: WebSocket Integration**
* **FeishuWebSocket** - WebSocket connection management
* **Real-time reception** - Event parsing and message conversion
* **Reconnection logic** - Automatic reconnection with exponential backoff
* **Configuration** - WebSocket enable/disable and timing controls

**Phase 7.4: Complete Adapter Implementation**
* **FeishuInboundAdapter** - Full InboundAdapter trait implementation
  * Message matching and thread derivation
  * WebSocket integration for real-time updates
  * Conversion of Feishu events to InboundMessage
* **FeishuOutboundAdapter** - Full OutboundAdapter trait implementation
  * Message sending with proper formatting
  * Heartbeat/progress update support
  * Alert notification capabilities

**Phase 7.5: Formatter and Utilities**
* **FeishuFormatter** - Multi-format message support
  * Markdown, text, and HTML formatting
  * Content escaping and sanitization
  * Rich message construction
* **Configuration validator** - Validation of Feishu configuration
* **Unit tests** - Comprehensive test coverage for all components

**Phase 7.6: Production Readiness**
* **Error recovery** - Graceful handling of API failures
* **Performance optimization** - Efficient WebSocket and API usage
* **Documentation** - Configuration examples and usage guidelines
* **Integration testing** - End-to-end testing with mock Feishu server

### Key Technical Details

1. **API Integration** - Uses the official openlark Rust SDK for all Feishu API interactions
2. **WebSocket Protocol** - Implements Feishu's custom WebSocket protocol for real-time events
3. **Token Management** - Automatic app token refresh with caching and error handling
4. **Message Formatting** - Support for Feishu's rich message formats including markdown cards
5. **Thread Compatibility** - Seamless integration with existing thread management system

### Configuration Example

```toml
[channels.feishu]
type = "feishu"

[channels.feishu.config]
app_id = "cli_xxxxxx"
app_secret = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
websocket.enabled = true
websocket.reconnect_delay_ms = 5000
```

### Testing Strategy

* **Unit tests** - Test individual components in isolation
* **Integration tests** - Test WebSocket and API interactions with mocks
* **End-to-end tests** - Full channel functionality testing
* **Compatibility tests** - Ensure compatibility with existing email channel

### Status

✅ **Completed** - All Feishu channel features are implemented and tested
✅ **Integrated** - Fully integrated with core JYC architecture
✅ **Production Ready** - Passes all 115 tests in the test suite
✅ **Documented** - Comprehensive documentation in DESIGN.md

The Feishu channel implementation demonstrates the extensibility of JYC's channel-agnostic architecture and provides a blueprint for adding additional messaging platforms in the future.
