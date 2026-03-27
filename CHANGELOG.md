# Changelog

All notable changes to JYC will be documented in this file.

## [0.0.3] - 2026-03-27

### Added

**Phase 5: MCP Reply Tool + Commands**
- MCP reply tool (`src/mcp/reply_tool.rs`): `rmcp` stdio server with `reply_message` tool. Decodes context token → loads config → reads received.md → builds full reply with quoted history → sends via SMTP with file attachments → stores reply.md → writes signal file
- `jyc mcp-reply-tool` hidden subcommand wired to rmcp server
- Reply context deserialization (`src/mcp/context.rs`): base64 → JSON → validation with tamper detection
- `/model <id>`, `/model reset` command handler — writes `.jyc/model-override`, forces new session
- `/plan`, `/build` command handlers — writes/removes `.jyc/mode-override`
- Commands wired into thread_manager: parse → execute → reply results → strip → check body → dispatch to agent

**Architecture: AgentService trait**
- `AgentService` trait (`src/services/agent.rs`): `process(message, thread_name, thread_path, message_dir) → AgentResult`
- `StaticAgentService` (`src/services/static_agent.rs`): fixed text reply with quoted history
- `OpenCodeService` implements `AgentService`: owns full reply lifecycle (AI interaction + fallback send + storage)
- ThreadManager dispatches via `Arc<dyn AgentService>` — zero mode-specific code
- Adding new agent modes requires only: implement trait + match arm in `cli/monitor.rs`

**File attachment support**
- SMTP client: `MultiPart::mixed` with `Attachment` parts, MIME type detection by extension
- Email outbound adapter: reads files from disk, builds `EmailAttachment` structs
- MCP reply tool: validates attachment paths, builds `OutboundAttachment`, passes to outbound

**Email body extraction fix**
- Prefers HTML→Markdown conversion (via `htmd`) over raw plain text — mobile email clients generate poor plain text with no line breaks
- HTML cleaning before conversion: strips `<style>`, `<script>`, `<head>`, `<meta>`, `<link>`, CSS `@import`/`@media` rules, HTML comments

### Changed
- `message.channel` now contains config channel **name** (e.g., "jiny283"), not type ("email") — fixes MCP reply tool config lookup
- Session reuse restored: `get_or_create_session()` reuses existing session if valid on server, only creates new on config change or server restart — AI maintains conversation memory across messages
- Session state file renamed: `session.json` → `opencode-session.json` — avoids future naming conflicts with other service sessions
- Removed unused `emailCount` field from `SessionState`
- MCP server name: `"rmcp"` → `"jiny_reply"` with `#[tool_handler]` macro — fixes tool discovery (was `toolCount=0`)
- Noisy IMAP polling logs moved from DEBUG to TRACE level
- Empty AI text parts no longer logged at DEBUG level
- Session error logging: fallback to raw property extraction when struct deserialization fails
- SSE model_id/provider_id: no longer overwritten with None by subsequent events

### Fixed
- MCP tool not discovered by OpenCode: missing `#[tool_handler]` attribute on `ServerHandler` impl
- Channel lookup in reply tool: `config.channels.get("email")` → `config.channels.get("jiny283")`
- `strip_quoted_history`: added `发件时间` to Chinese reply header detection

## [0.0.2] - 2026-03-27

### Added

**Phase 4: AI Integration**
- OpenCode server manager: auto-start `opencode serve`, free port discovery, stdout-based readiness detection, health check, graceful shutdown with `kill_on_drop`
- OpenCode HTTP client: `create_session`, `get_session`, `prompt_async`, `prompt_blocking` with `x-opencode-directory` header and `?directory=` query param
- SSE streaming: subscribe to `/event?directory=`, parse events from JSON `{"type": "...", "properties": {...}}` format, activity-based timeout (30min default, 60min when tool running), progress logging with model info
- SSE event handling: `server.connected`, `server.heartbeat`, `message.updated` (model/provider capture), `message.part.updated` (tool state tracking), `session.status`, `session.idle`, `session.error`
- Session management: per-thread `.jyc/session.json`, fresh session per prompt (avoids stale sessions across server restarts), `opencode.json` generation with staleness check
- Prompt builder: system prompt (config + directory boundaries + reply instructions + system.md), user prompt (conversation history + incoming body + base64 reply_context token)
- OpenCodeService (`src/services/opencode/service.rs`): encapsulates all AI logic — server lifecycle, sessions, prompts, SSE, error recovery. Returns `GenerateReplyResult` to ThreadManager.
- ContextOverflow recovery: delete session, create new, retry with blocking prompt
- Stale session detection: tool reported success in SSE but signal file missing → delete + retry
- Fallback reply with quoted history: `build_full_reply_text()` shared function for both fallback and future MCP reply tool
- Prompt echo stripping: removes `## Incoming Message`, `<reply_context>`, `## Conversation history` markers from AI output when tool fails

**Architecture: ThreadManager ↔ OpenCodeService separation**
- ThreadManager: queue management, concurrency control, agent mode dispatch, fallback send
- OpenCodeService: AI-specific logic isolated from infrastructure. Does NOT send emails.

### Changed
- IMAP ID command: now logs `server_name`, `server_vendor`, `trans_id` as structured fields (no raw map dump)
- IMAP monitor: backoff on SELECT failure (was tight retry loop)
- DESIGN.md: added OpenCode Server HTTP API reference (https://opencode.ai/docs/server/), responsibility separation docs, updated Worker Processing Flow diagram, OpenCode server shutdown lifecycle table

### Fixed
- IMAP `SELECT INBOX` rejected by 163.com with "Unsafe Login" — added RFC 2971 ID command after login
- OpenCode server command: `opencode server` → `opencode serve` with `--hostname=` / `--port=` syntax
- OpenCode server readiness: detect by parsing stdout for `"opencode server listening on http://..."` instead of HTTP polling
- SSE event parsing: event type is in JSON `data.type` field, not SSE `event:` field
- SSE subscription: added `?directory=` query param to scope events to thread project context
- Explicit `opencode_server.stop()` on graceful shutdown

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
