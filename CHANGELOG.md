# Changelog

All notable changes to JYC will be documented in this file.

## [0.1.2] - 2026-04-07

### Added

**Chat Log Storage System** — New unified storage architecture
- Replaced timestamped directory storage (`messages/YYYY-MM-DD_HH-MM-SS/`) with log-based storage (`chat_history_YYYY-MM-DD.md`)
- HTML comment metadata format: `<!-- timestamp | type:received/reply | matched:true/false | sender:... | channel:... | external_id:... -->`
- **Dual-write integration**: Smooth transition with backward compatibility (writes to both formats during migration)
- **AI chat history access**: System prompt instructions for accessing chat logs via tools (`glob`, `read`, `grep`)

**Feishu Footer Support** — Consistent model/mode display across channels
- Feishu replies now include model and mode information footer (same format as email)
- Format: `---\n\nModel: <model> | Mode: <mode>` (or variations when only one is available)
- Automatically reads from `reply-context.json` (existing infrastructure)
- No footer added when model/mode information is unavailable (backward compatible)

### Changed

**Message Storage Architecture** — Simplified and unified
- Removed timestamped directory creation logic from `MessageStorage::store_with_match()`
- All messages and replies now append to daily chat log files
- `store_reply()` no longer creates separate `reply.md` files
- **Backward compatibility**: `email_parser::build_thread_trail()` reads from logs first, falls back to directory storage if needed

**Email Parser Enhancements** — Log-aware history building
- New `parse_chat_log_entry()` function for parsing log entries
- `build_thread_trail_from_logs()` reads conversation history from chat logs
- Maintains compatibility with existing directory-based storage during transition

### Fixed

**Storage Consistency Issues**
- Prevented duplicate storage between MCP tool and outbound adapters
- Fixed current message appearing twice in quoted history
- Removed stale references to legacy `reply.md` and `received.md` files

**MCP Tool Integration**
- Fixed reply delivery failures caused by tool/adapter storage conflicts
- Ensured reply text is properly extracted and delivered via outbound adapters

### Technical Details

**Dependencies Updated**
- Added `glob` crate dependency for file pattern matching in chat log operations

**API Changes**
- `MessageStorage::store_with_match()` now only creates log entries, not directories
- `email_parser` module extended with log parsing capabilities
- `TrailCurrentMessage` now implements `Clone` trait for history building

**Testing**
- All 158 tests pass with new storage architecture
- New unit tests added for chat log parsing functionality

## [0.1.1] - 2026-04-06

### Changed

**Feishu Message Format Enhancement**
- Changed Feishu message sending from plain text (`msg_type: "text"`) to interactive cards with native markdown support (`msg_type: "interactive"`)
- Messages now render with full markdown formatting: bold, italic, code blocks, lists, links, and blockquotes
- Matches email channel behavior where markdown is converted to HTML for rich rendering
- Improves readability and formatting consistency across channels

## [0.1.0] - 2026-04-06

First multi-channel release: JYC is now a truly channel-agnostic AI agent framework with full Feishu (飞书/Lark) support alongside email.

### Added

**Feishu Channel — Full Implementation**
- Real-time WebSocket connection via openlark SDK (`LarkWsClient`)
- Message receiving: text, image, file, and interactive (card) message types
- Message sending via Feishu IM API (`CreateMessageRequest`)
- Chat/user name lookup with in-memory caching (readable thread directories)
- @mention placeholder stripping (replaces `@_user_1` with `@displayname`)
- WebSocket reconnection with configurable backoff
- `FEISHU.md` onboarding guide with required scopes, setup steps, troubleshooting

**Channel-Agnostic Architecture**
- `ChannelMatcher` trait: split from `InboundAdapter` for pure-logic pattern matching and thread name derivation
- `EmailMatcher` and `FeishuMatcher` stateless implementations
- `MessageRouter.route()`: channel-agnostic, delegates to `&dyn ChannelMatcher`
- `OutboundAdapter` trait: `clean_body()` for channel-specific body cleaning, `send_reply()` with full lifecycle (format + send + store)
- `ThreadManager`, `AlertService`, `process_message`: all use `Arc<dyn OutboundAdapter>` instead of `Arc<EmailOutboundAdapter>`

**Pattern Matching**
- `mentions`: match Feishu messages by @-mentioned bot/user names or IDs (OR logic)
- `keywords`: match by message body content (OR, case-insensitive)
- `chat_name`: match by Feishu group chat name (OR, case-insensitive) — enables per-group behavior (e.g., reply to all messages in private groups, require @mention in public groups)
- All rules use AND logic within a pattern, first-match-wins across patterns

**Heartbeat Configuration**
- Configurable `[heartbeat]` section: `enabled`, `interval_secs` (default 600 = 10 minutes), `min_elapsed_secs` (default 60)
- Per-channel `heartbeat_template` with `{elapsed}` placeholder for multilingual messages (e.g., `"正在处理中，请稍候... (已用时 {elapsed})"`)

**SMTP Error Handling**
- Structured error handling using lettre's `SmtpError` API (replaces string-based matching)
- Permanent errors (5xx): fail immediately with SMTP code logged
- Transient errors (4xx): retry with exponential backoff (3 attempts, 5–60s)
- Connection/timeout errors: reconnect + retry (2 attempts)

**Security**
- `"external_directory": "deny"` in OpenCode permissions — blocks AI from accessing files outside the thread directory

**Build**
- `protobuf-compiler` added as build prerequisite (required by `lark-websocket-protobuf`)

### Changed

- **MCP Reply Tool**: no longer sends messages directly. Writes `reply.md` + signal file; monitor process delivers via pre-warmed outbound adapter. Eliminates cold-start timeouts for Feishu API calls.
- **BUILD MODE Prompt**: categorizes messages — information questions (→ use `curl`), coding tasks (→ use tools), general conversation (→ reply directly). Prevents AI from exploring the filesystem for simple questions.
- **Email Quoted History**: truncated to 1024 characters per entry (`MAX_QUOTED_BODY_CHARS`) with `...[truncated]` suffix
- **ThreadManager**: uses `cancel.child_token()` — one channel shutting down no longer kills other channels
- **Heartbeat Interval**: default changed from 2 minutes to 10 minutes (avoids SMTP rate limits)
- **MCP Tool Timeout**: increased from 60s to 180s
- **System Prompt**: updated default to instruct AI to use tools for real-time information lookup

### Fixed

- Model name missing in `ai` log span (`m=?:build` → `m=ark/deepseek-v3.2:build`) — restored `tracing::field::Empty` + `.record()` pattern
- UTF-8 panic in Feishu outbound adapter (byte slicing on multi-byte Chinese/emoji characters)
- Feishu channel causing cascade shutdown of all email channels via shared cancel token
- Feishu reply tool timeout (>180s) due to cold-start HTTP calls in MCP subprocess
- Chat name lookup double-unwrap (`extract_response_data` already unwraps outer envelope)

### Removed

- Dead `[agent.progress]` / `ProgressConfig` config and `DEFAULT_PROGRESS_*` constants
- Dead `include_thread_history` config field
- Dead `workspace` field on `ChannelConfig`
- `feishu_` prefix from thread directory names (now consistent with email: just the chat/subject name)

## [0.0.13] - 2026-04-05

### Added

**Feishu (飞书/Lark) Channel Implementation - Phase 7**
- Complete Feishu channel support with real-time messaging capabilities
- **FeishuInboundAdapter**: WebSocket-based real-time message reception
- **FeishuOutboundAdapter**: API-based message sending using openlark SDK
- **FeishuClient**: Authentication, token management, and API integration
- **FeishuFormatter**: Multi-format message support (markdown, text, HTML)
- **FeishuWebSocket**: Real-time event handling with automatic reconnection
- Comprehensive error handling with `FeishuError` enum
- Full unit test coverage for all components
- Configuration support for Feishu app credentials and WebSocket settings

**Documentation Updates**
- Added "Feishu Channel Implementation" chapter to DESIGN.md
- Added Phase 7 to IMPLEMENTATION.md detailing Feishu implementation
- Updated README.md with "Supported Channels" section
- Configuration examples for Feishu channel setup

### Changed

- **OutboundAdapter trait**: Added `send_heartbeat()` method for progress updates
- **Channel registry**: Extended to support Feishu channel type
- **Thread naming**: Enhanced to support Feishu chat metadata
- **Test suite**: Expanded to 115 tests with Feishu component tests

### Fixed

- **OutboundAdapter implementation**: Fixed missing `send_heartbeat()` method in FeishuOutboundAdapter
- **Test failures**: Fixed config tests expecting 2.0 hours timeout (actual default is 1.0 hours)
- **Unused code warnings**: Cleaned up unused imports and variables in Feishu modules

### Technical Details

- **API Integration**: Uses official openlark Rust SDK for Feishu API
- **WebSocket Protocol**: Implements Feishu's custom WebSocket protocol
- **Authentication**: App token management with automatic refresh
- **Message Formatting**: Support for Feishu's rich message formats
- **Thread Compatibility**: Seamless integration with existing thread management

## [0.0.12] - 2026-04-02

### Added

**Skill-based bootstrapping (replaces per-prompt system.md)**
- Migrate bootstrapping instructions from `system.md` (sent every prompt) to OpenCode's native discovery mechanisms
- `AGENTS.md` (project-level): project context, tech stack, coding conventions, git rules, dev workflow
- `agents.example.md`: template for thread-level AGENTS.md with self-bootstrapping context and environment hint
- `.opencode/skills/jyc-deploy-bare/SKILL.md`: on-demand skill for bare metal deployment (deploy.sh + nohup)
- `.opencode/skills/jyc-deploy-docker/SKILL.md`: on-demand skill for Docker deployment (s6 process supervisor)
- Skills loaded by AI only when needed, reducing prompt size and improving performance

**Model listing with wildcard filtering**
- Add `/model ls [pattern]` command to list available models with wildcard support
- Support `*` (multiple characters) and `?` (single character) wildcards
- Handle email escaping (`ark\*` → `ark*`) for better UX
- Case-insensitive pattern matching
- Remove bare `/model` command (now requires arguments)
- Comprehensive tests for wildcard functionality

### Fixed

**Multiple reply support**
- Reply context file (`.jyc/reply-context.json`) now persists between replies instead of being deleted after each send
- Allows AI models to send multiple replies in the same thread without file-not-found errors
- Context file is overwritten on each new incoming message; cleanup only for tests and manual operations
- Updated documentation in `DESIGN.md` to reflect new lifecycle

**IMAP monitor resilience and timeout handling**
- Add 60s timeout to all IMAP operations (connect, select, fetch_range, fetch_uid) to detect dead TCP connections
- Add 2-min hard timeout guard around IMAP IDLE to detect half-open TCP connections
- Add 5s timeout to IMAP logout to prevent 15-min hang on dead connections (TCP retransmission timeout)
- Remove fatal retry limit — monitor retries indefinitely at max backoff instead of giving up after 5 failures
- Force disconnect after `check_for_new()` failure to avoid entering IDLE on a dead connection
- Clean up closed senders from thread_queues to prevent unbounded HashMap growth
- Drain completed worker JoinHandles when spawning new workers
- Add UID compaction to StateManager (auto-prune when exceeding 5000 entries)
- Share `reqwest::Client` across OpenCode requests (connection pool reuse)
- Move 10 regex compilations to `LazyLock` statics (email_parser and smtp/client)

**Deployment reliability**
- Use `systemd-run` to escape jyc cgroup during self-deploy (prevents deployment from being killed)
- Ensure `deploy.sh` survives parent process death
- Add `jyc/` path prefix to deploy skills for proper resolution

### Changed
- Send model as `{providerID, modelID}` object in prompt API (breaking API change in OpenCode)
- Show model in log span immediately at prompt time instead of waiting for SSE discovery
- Fix duplicate `m=` field in log span (was recorded twice: upfront + SSE)
- Remove deprecated `system.md.example` files with migration notice

## [0.0.11] - 2026-04-01

### Added

**Live message injection**
- Follow-up messages sent during AI processing are injected into the ongoing session via `prompt_async`
- Queue receiver (`rx`) flows through: ThreadManager → AgentService → OpenCodeService → SSE Client
- New `tokio::select!` arm in SSE loop monitors `pending_rx.recv()` for incoming messages
- Injected messages: stored as `received.md`, reply-context.json updated, body sent as raw prompt (same as OpenCode TUI)
- OpenCode API `POST /session/:id/prompt_async` supports sending to busy sessions
- AgentService trait: added `pending_rx: &mut mpsc::Receiver<QueueItem>` parameter
- QueueItem made public for cross-module access

**Logging improvements**
- `<system-reminder>` filtered from `is_prompt_echo()` — prevents OpenCode plan mode reminders from appearing in fallback replies
- `<system-reminder>` filtered from AI response text DEBUG log
- Session retry logs include `message` field for better debugging
- `logged_tools` HashSet cleared on retry — retried tool calls are now visible in logs

### Changed
- Injection prompt: raw body only (no framing instructions) — matches OpenCode TUI behavior
- Dev build profile: reduced debug info (debug=1, no debug for deps) for faster builds

### Fixed
- Removed stale `mode` field from `GenerateReplyResult` struct

## [0.0.10] - 2026-03-30

### Added

**/reset command to clear opencode session**
- New `/reset` command that deletes `.jyc/opencode-session.json`
- Allows users to manually reset the AI conversation session
- Next AI prompt after reset starts with a fresh session
- Session state tracked per-thread in `.jyc/opencode-session.json`

### Changed

- **SYSTEMD.md**: Added deployment warnings to `systemctl stop` commands
- **system.md.example**: Updated systemd stop command warning text

## [0.0.9] - 2026-03-30

### Added

**systemd service support for process supervision and self-bootstrapping**
- systemd user service at `~/.config/systemd/user/jyc.service` for process supervision
- `run-jyc.sh` wrapper script that sources `~/.zshrc.local` for environment variables
- `jyc-ctl.sh` control script for service management (status, logs, restart, stop, start)
- `SYSTEMD.md` documentation with setup, usage, and troubleshooting guide
- `system.md.example` updated with systemd bootstrap instructions
- Automatic restarts on crash (`Restart=always` with 5-second delay)
- Service configuration tracked in repository (no s6-overlay)
- Environment variables from `.zshrc.local` available to jyc (API keys, etc.)

**Combined provider/model name in reply context and log spans**
- Model field in reply-context.json now uses `<provider-id>/<model-id>` format (instead of just model_id)
- Log span `m` field also uses combined format (e.g., `ark/deepseek-v3.2:build`)
- Applied to both email reply footers and structured logging
- Example: `ark/deepseek-v3.2` instead of `deepseek-v3.2`

### Removed

**s6-overlay approach** (replaced by systemd)
- `s6-rc.d/` directory and service configuration files
- `start-jyc.sh` (s6 initialization script)
- `NATIVE_S6.md` (s6-specific documentation)

### Changed

- **DESIGN.md**: Added reference to `SYSTEMD.md` in References section
- **Cargo.toml**: Bumped version from 0.0.8 to 0.0.9

## [0.0.8] - 2026-03-28

### Changed

**Disk-based reply context (replaces REPLY_TOKEN)**
- Reply context saved to `.jyc/reply-context.json` per-thread before AI prompt
- MCP reply tool reads context from disk (cwd) instead of decoding a base64 token
- AI never sees or touches the context — zero corruption risk
- `token` parameter removed from `reply_message` tool schema — only `message` and `attachments`
- REPLY_TOKEN line removed from AI prompt entirely
- Token-related system prompt instructions removed (no more "pass as-is" warnings)
- Context includes `model` and `mode` fields for future footer use
- Context file deleted by reply tool after successful send (cleanup)

### Removed
- `serialize_context()` and `deserialize_context()` functions (base64 token approach)
- `REPLY_TOKEN=` from prompt text
- Token integrity checks (backtick detection, nonce validation) — no longer needed
- `build_footer()` function and model/mode from `build_full_reply_text()`
- `model` and `mode` fields from `AgentResult` (agent is channel-agnostic)
- `model` and `mode` parameters from `EmailOutboundAdapter::send_reply()`

## [0.0.7] - 2026-03-27

### Changed

**Session preservation — keep session whenever possible**
- Model passed per-prompt (`PromptRequest.model`) — `/model` switch no longer deletes session
- Mode passed per-prompt (`PromptRequest.agent`) — `/plan` and `/build` switches no longer delete session
- `opencode.json` config changes no longer delete session — server picks up changes per-directory
- Session survives: model switches, mode switches, config changes, container restarts
- Session only deleted for error recovery: ContextOverflow and stale session detection

**Prompt echo stripping fix**
- Changed from join-then-strip to per-part filtering
- Each text part individually checked for prompt echo markers (`## Incoming Message`, `REPLY_TOKEN=`)
- Fixes: AI fallback text was lost when prompt echo and actual response were in separate SSE parts

**Logging improvements (from pre-release fixes)**
- Duplicate `m` field in `ai` span fixed — recorded once when model discovered
- Duplicate tool logs deduplicated with HashSet per step
- Tool input shown in logs (`Tool running tool=bash input="cargo build"`)
- Duplicate "Reply sent by MCP tool" log removed from thread_manager
- Session reuse: `get_session` now sends `x-opencode-directory` header
- Debug logging for `config_changed` and `get_session` response status

### Fixed
- Session reuse across container restarts: `get_session()` was missing `x-opencode-directory` header → server couldn't find session → always created new
- Fallback reply empty when AI produces prompt echo + actual response in separate text parts
- `/model` and mode commands unnecessarily deleted session (model/mode are per-prompt, not per-session)
- Cleaned up agent task artifacts: removed model/mode from ReplyContext, AgentResult, build_full_reply_text, EmailOutboundAdapter (these are per-prompt concerns, not per-token/per-adapter)

### Added

**Docker: two image variants**
- `jyc:dev` (target `dev`, ~2GB) — Rust pre-installed for self-bootstrapping, no timeout during cargo install
- `jyc:latest` (target `production`, ~740MB) — no Rust, production use
- Both share the same `base` stage (cached) — building one caches the base for the other
- `docker-compose.yml` defaults to `dev` target, configurable via `JYC_BUILD_TARGET` env var

## [0.0.6] - 2026-03-27

### Changed

**Token format: `REPLY_TOKEN=`**
- `<reply_context>TOKEN</reply_context>` → `REPLY_TOKEN=TOKEN` — no XML tags, avoids triggering AI's "parse structured data" instinct
- Tool parameter description updated to reference `REPLY_TOKEN=` line
- Prompt echo stripping marker updated

**Conversation history removed from AI prompt**
- OpenCode session memory handles multi-turn conversation context
- `build_conversation_history()` function removed (dead code)
- `include_history` parameter removed from `build_prompt()`
- System prompt simplified — no "Conversation history" section reference
- `include_thread_history` config field deprecated (kept for backward compat but ignored)

**DESIGN.md comprehensive update**
- Removed all jiny-m references (moved to IMPLEMENTATION.md)
- Removed "Differences from jiny-m" comparison table
- PromptBuilder: updated for no history, REPLY_TOKEN format
- ReplyContext → Reply Token: minimal 5-field description
- Context Management Strategy: rewritten for session-based (not prompt-based)
- Data Flow Summary, sequence diagram, block diagrams: all updated
- MCP Tool section: reads from disk, not token
- Stripping Strategy table: removed AI Prompt Context row
- Config example: removed `include_thread_history`

**Cargo.toml description**
- Removed "Rust rewrite of jiny-m" — JYC is its own project

## [0.0.5] - 2026-03-27

### Changed

**Minimal reply context token (corruption-proof)**
- Token slimmed from 12 fields to 5: `channel`, `threadName`, `incomingMessageDir`, `uid`, `_nonce`
- All message metadata (sender, recipient, topic, threading headers) now read from stored `received.md` frontmatter — NOT from the AI-passed token
- Prevents AI model corruption (e.g., `petalmail.com` → `petailmail.com` causing bounced emails)
- Token is now ~120 bytes base64 instead of ~400 bytes — shorter = less corruption risk
- Switched to standard base64 (with padding) matching jiny-m's format

**Token serialization moved to `mcp/context.rs`**
- `serialize_context()` and `deserialize_context()` now live together in `src/mcp/context.rs`
- Removed from `prompt_builder.rs` — the prompt builder imports from `mcp::context`
- All token logic (struct, serialize, deserialize, validate) in one place

**Enriched received.md frontmatter**
- Added `sender`, `sender_address`, `external_id`, `reply_to_id`, `thread_refs`, `matched_pattern` to YAML frontmatter
- Reply tool reads all metadata from disk (authoritative source) instead of trusting token
- `parse_stored_message()` extracts all new frontmatter fields

**Docker: 3-stage build + image size optimization**
- Restructured to base (tools, cached) → builder (Rust compile) → final (base + binary)
- Removed Rust toolchain from runtime image (~1.23GB saved, image ~740MB)
- AI installs Rust on-demand for self-bootstrapping (~30s)
- `CARGO_TARGET_DIR=/tmp/jyc-target` avoids cross-platform conflict with host macOS builds
- Cargo registry + git cached in named Docker volumes
- OpenCode data volume for session persistence across container restarts
- Builder uses `rust:bookworm` matching runtime's glibc version

**Logging**
- `system.md loaded` / `No system.md found` log when building system prompt

## [0.0.4] - 2026-03-27

### Added

**Phase 6: Resilience + Polish**
- Alert service (`src/core/alert_service.rs`): background task buffers ERROR events, flushes as digest emails at configured intervals. Health check reports with per-thread stats at configured intervals. Self-protection via `eprintln` for send failures (no feedback loop).
- `AppLogger` — unified logging + alerting handle. Components call `app_logger.info()`, `.error()`, `.message_received()`, `.reply_by_tool()` etc. Each call delegates to `tracing` for console output AND sends structured events to the alert service for stats tracking + error buffering. Replaces separate `tracing` + `AlertHandle` dependencies.
- Progress tracker (`src/core/progress_tracker.rs`): sends periodic "still working" emails during long AI operations. Configurable initial delay (default 3 min), interval (default 3 min), max messages (default 5). Polling every 5s with `tokio::time::interval`.
- Startup notification email: sent on monitor start with version, timestamp, channel count, agent mode
- Graceful shutdown: alert service final flush before exit, OpenCode server stopped, all worker tasks awaited

### Changed
- `/model` with no args now shows current model (from override or config default) instead of "not yet implemented"
- `AlertHandle` renamed to `AppLogger` to reflect its dual role as logger + alerter
- Structured logging: `channel=` and `thread=` fields added consistently to all key log lines across IMAP monitor, message router, thread manager, and OpenCode service. Enables easy filtering by channel or thread in production logs.

### Fixed
- Error handling audit: all production `unwrap()` calls verified safe (static regex, guarded strip_prefix)

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
