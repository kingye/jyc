# Changelog

All notable changes to JYC will be documented in this file.

## [0.2.0] - 2026-04-23

### Added

- **Multi-agent workflow refactor** — Developer agent is now a persistent reactive agent (#59), label-based reviewer trigger (#74), removed trigger_mode (#76), unconditional hand-off (#78, #85)
- **Pattern mode triggers issue/PR directly** — No longer relies solely on comments (#69)
- **AND/OR label logic** — LabelRule supports CNF nested array boolean combinations (#83)
- **Dashboard TUI shows thread last active time** (#81)
- **deploy-templates supports --as flag** (#87), integrated into jyc CLI (#94)
- **Comment filtering for closed issues/PRs** (#89)
- **Main branch protection** (#65)
- **coding-principles skill integration** (#61)

### Fixed

- **Planner empty commit handling** (#93)
- **SSE loop exit fix** (#89)
- **Activity panel shows AI thinking and tool errors** (#78)
- **Pattern mode self-loop protection** (#69)

### Changed

- **README documentation updates** (#96, #98)
- **Developer agent template simplification** (#59)
- **Removed model-override file** (#67)
- **Removed @j:role mentions, use label-based handover instead** (#76)
- **Dockerfile optimization for dummy main caching** (#57)

### Removed

- **TriggerMode enum and trigger_mode field** (#76)

## [0.1.11] - 2026-04-20

### Added

**Inspect Server + TUI Dashboard** — Live monitoring of running jyc processes
- `[inspect]` config section: `enabled`, `bind` (default `127.0.0.1:9876`)
- TCP-based JSON line protocol for querying runtime state
- `jyc dashboard` CLI command with ratatui TUI
- Panels: channels bar, threads table (selectable), detail panel, status bar
- Shows: thread name, channel, pattern, status, model, mode, token usage, uptime, version
- Key bindings: q/Esc quit, Up/Down/j/k select, r refresh
- Auto-polls every 500ms, handles disconnected state gracefully
- Works across Docker (via `network_mode: host`) and bare metal

**MetricsCollector** — Lightweight replacement for AlertService
- Accumulates health stats (messages received/processed, errors, per-thread) in `Arc<Mutex<>>`
- Queryable by the inspect server — no email dependency
- `MetricsHandle` for components to report events (same API as old `AppLogger`)

### Removed

**AlertService** — Removed email-based alerting
- Startup notification email removed
- Error digest email removed
- Health report email removed
- `[alerting]` and `[alerting.health_check]` config sections deprecated (ignored if present)
- `AlertingConfig`, `HealthCheckConfig` structs kept for backward compatibility but unused

### Changed

**ThreadManager** — Added introspection for dashboard
- `channel_name` and `workspace_dir` fields for identifying channel ownership
- `list_threads()` method: returns thread info by reading `.jyc/` state files
- `channel_name()`, `max_concurrent()` accessor methods

## [0.1.10] - 2026-04-20

### Added

**GitHub Label-Based Routing** — Auto-label matching for GitHub channel patterns
- Patterns with a `role` field get an implicit routing label: `Developer` → `jyc:develop`, `Reviewer` → `jyc:review`, `Planner` → `jyc:plan`
- Auto-label is combined (OR) with any explicit `labels` in pattern config
- PRs/issues must have the matching label to be routed (labels added by agents during hand-off)

**GitHub Label Change Detection** — Adding labels to existing issues/PRs triggers routing
- When labels are added to an existing issue/PR, the change is detected by comparing against cached labels
- This allows users to add a label (e.g., `jyc:plan`) to an existing issue and have it routed to the planner

**@j:<role> Mention-Driven Routing** — Refactored hand-off mechanism
- Patterns match on `@j:<role>` mentions in comments (e.g., `@j:developer`, `@j:reviewer`, `@j:planner`)
- Replaces earlier `[Role]` prefix filter approach
- Agent templates updated to use `@j:<role>` for hand-offs

**Persistent Comment Tracking** — Re-process edited comments
- Track comment ID + `updated_at` to detect edits
- Edited comments are re-processed through the routing pipeline
- Backward-compatible with old `processed-comments.txt` format

**SQLite Storage for Invoice Processing** — Persistent invoice database
- SQLite database for invoice records
- Enables duplicate checking and query capabilities
- Schema: invoice_number, receipt_date, amount, seller, buyer, status, etc.

**GitHub Enterprise Support** — Configurable API endpoint
- `api_url` config option for GitHub Enterprise instances
- Default: `https://api.github.com` for public GitHub

**Assignee Matching** — Route issues/PRs by assignee
- `assignees` field on `ChannelPattern` for GitHub channel
- Match issues/PRs where any of the specified users are assigned

**Pattern Rule Filtering** — Enforce `github_type`, `labels`, `assignees` rules in routing
- `GithubMatcher::match_message()` now validates all present rules before accepting a pattern match
- All rules use AND logic (all must pass); within each rule, OR logic (any value suffices)
- Case-insensitive matching for labels and assignees
- Patterns that fail rule checks are skipped, allowing fallback to the next matching pattern

**CLI Patterns List Enhancement** — Display all rule fields
- `jyc patterns list` now shows GitHub rules (`github_type`, `labels`, `assignees`), Feishu rules (`mentions`, `keywords`, `chat_name`), `role`, and `template`

**Planner Template: Copy Issue Metadata to PR** — Preserve routing context
- Planner template reads assignees and labels from the source issue
- Copies them to the created PR via `--assignee` and `--label` flags
- Ensures PRs inherit routing context for developer/reviewer pattern matching

**Docker Container Environment Injection** — Propagate `.env` to container
- Added `env_file: .env` directive to `docker-compose.yml`
- Environment variables from `.env` are now injected into the container runtime (previously only used for compose-file interpolation)

**Close Event Detection** — Improved event handling
- Fetch all open issues instead of `list_closed_since` for detecting close events
- Compare cached state to detect actual closes

**Bare Metal Deployment** — Deploy jyc on Ubuntu/Debian servers without Docker
- `deploy-bare-metal.sh` script for automated deployment
- `dotfiles/zsh/` - zsh configuration and environment template
- `dotfiles/opencode/opencode.jsonc` - OpenCode configuration
- `docs/bare-metal-deploy.md` - Deployment guide

**nohup Fallback** — Run on servers without systemd
- Detect systemd user session availability
- Fall back to `nohup` + redirect for process supervision

### Fixed

**Worker Semaphore Permit Release** — Fix resource leak on thread close
- Workers now properly release semaphore permits when closing threads
- Prevents thread pool exhaustion

**GitHub Self-Loop Prevention** — Replaces global `[Role]` prefix filter
- Previously: ALL comments prefixed with `[Planner]`, `[Developer]`, or `[Reviewer]` were globally filtered (invisible to all patterns)
- Now: each pattern only skips comments from its **own** role. `[Developer]` comments are visible to the reviewer pattern, and vice versa
- Enables cross-agent feedback visibility (reviewer feedback triggers developer)

**Developer-Reviewer Handoff** — Improved workflow for requesting changes
- Reviewer template now explicitly triggers `@jyc:developer` when submitting review with request-changes
- Ensures developer is notified when feedback needs to be addressed

**Invoice Processing Fixes**
- Add duplicate invoice check before adding to Excel
- Fix EXCEL.md with template copy step, clarify MONTH variable usage
- Template lookup logic fixes, zero amount handling, template cleanup

**Template Generalization** — Multi-language support
- Templates generalized for multi-language workflows
- Repository name included in trigger messages

**Model Override in Templates** — Per-template model configuration
- Support `model-override` in templates while ignoring `.jyc` elsewhere
- Updated GitHub developer template with model override support

### Changed

**GitHub Agent Templates** — Updated hand-off workflow with routing labels
- Planner: adds `--label "jyc:develop"` when creating PRs
- Developer: adds `jyc:review` label when handing off to reviewer
- Reviewer: adds `jyc:develop` label when requesting changes from developer

**Invoice Summary Export** — Generate summary xlsx
- Summary xlsx with correct naming (`summary_YYYY-MM.xlsx`)
- Template-based generation with proper file handling

**Docker Simplification** — Removed s6-overlay
- Removed s6-overlay from Docker setup
- Simplified entrypoint/CMD in Dockerfile
- Removed jyc-deploy-docker skill (no longer self-bootstrapping)

## [0.1.9] - 2026-04-15

### Added

**Live Message Injection Toggle** — Per-pattern control over sequential processing
- `live_injection` field on `ChannelPattern` (default: true)
- When false, messages queue and process sequentially instead of being injected into active AI session

**Invoice Processing Enhancements**
- Add duplicate invoice check before adding to Excel
- Two-level HTML download with Playwright fallback for invoice processing

### Fixed

**Email Processing**
- Check attachments/ directory first before searching email body for URLs
- Fix byte boundary panic when truncating filenames with Chinese characters

**Invoice Install Script**
- Fix install script: remove set -e, use if/elif for pip fallback
- Fix install script: show pip output, handle --break-system-packages

### Changed

**PDF Extraction**
- Reorder PDF extraction: try text extraction first, fall back to vision MCP
- Handle two-level download: extract real PDF/image URL from intermediate HTML

**Invoice Processing**
- Clarify invoice month folder is based on receipt date, not invoice date

## [0.1.8] - 2026-04-13

### Added

**Invoice Processing Skill** — Automated invoice extraction and bookkeeping
- Invoice processing skill with 7-step workflow (download, extract, Excel, summarize, export)
- Chinese invoice (发票) Excel template with 15 columns (发票号码, 开票日期, 购买方/销售方, etc.)
- Monthly folder organization (`invoice_YYYY-MM/`)
- Summary template (IIT deduction claim form) with category mapping
- Vision tool integration for PDF/image invoice extraction
- pypdf fallback for text-based PDF extraction
- Zip export of monthly invoice folders
- QR code image detection and filtering (skip small images, prefer download URLs)
- `agents.invoice.example.md` template for invoice processing threads

**Thread Name Override** — Fixed thread routing from config
- `thread_name` field on `ChannelPattern` for routing all matching messages to a fixed thread
- Channel-agnostic: works for email, Feishu, and any future channel
- Example: all invoice emails → `invoice-processing` thread regardless of subject

**MCP Question Tool** — Ask users questions and wait for answers
- `ask_user` tool with self-delivery (writes reply.md + signal file)
- Background delivery watcher (`pending_delivery.rs`) delivers messages during SSE stream
- 5-minute polling timeout for user response
- Thread manager routes next message as answer via `question-sent.flag`

**Skills**
- `invoice-processing` — complete invoice workflow with templates
- `plan-solution` — structured implementation planning for plan mode
- `incremental-dev` — small-step iteration with validation
- `pr-review` — read-only PR analysis via gh CLI
- `github-dev` — GitHub issue/PR development workflow (removed with GitHub channel)

**Thread Close** — `/close` command and Feishu disband event
- `/close` command to delete thread directory and clean up state
- Feishu `im.chat.disbanded_v1` event detection for automatic thread cleanup

**Central Path Resolution** — `thread_path.rs` module
- `resolve_workspace()` function for consistent path construction
- 10 end-to-end tests covering email, Feishu, config override, and attachment paths

### Fixed

**Email Parser Simplification**
- Removed quoted history from email replies (reply = text + footer only)
- Fixed forwarded email body extraction (was stripped as "quoted text")
- Removed 600 lines of dead quoted history code (`email_parser.rs`: 1296 → 693 lines)

**Attachment Handling**
- Fixed double-nested attachment directory path (`workspace/channel/workspace/` → `workspace/`)
- Moved attachment saving to after thread routing (correct directory with `thread_name` override)
- Reply delivery moved from `messages/<dir>/reply.md` to `.jyc/reply.md`

**Question Tool Delivery**
- Question tool now self-delivers via reply signal (AI doesn't need two-step flow)
- Background delivery watcher delivers during SSE stream (no 5-min wait)
- SSE handler detects `ask_user` tool completion alongside `reply_message`

**Permissions**
- `external_directory: allow` for threads with symlinks (prevents plan mode sub-agent deadlock)
- Auto-detect symlinks up to 3 levels deep for permission configuration

**Activity Timeouts**
- Both `ACTIVITY_TIMEOUT` and `TOOL_ACTIVITY_TIMEOUT` set to 30 min for thinking models

### Changed

- GitHub channel removed (reverted all GitHub-specific implementation)
- `dev-workflow` skill enhanced with gh CLI instructions and token scope documentation
- `deploy.sh` auto-detects paths from `JYC_BINARY` env var and script directory
- `run-jyc.sh` requires `JYC_BINARY` and `JYC_WORKDIR` environment variables
- `SYSTEMD.md` updated with deployment flow diagram and env var documentation
- SSE stream no longer exits early after reply tool (allows post-reply actions)
- `config_template.toml` consolidated into `config.example.toml`

### Removed

- GitHub channel (`src/channels/github/`, `GITHUB_CHANNEL.md`, `agents.github-dev.example.md`)
- `labels` field from `PatternRules`
- Dead quoted history functions and tests (600 lines)
- Dead `thread_path` functions (unused resolve helpers)
- `messages/` directory references (replaced by `.jyc/` and chat log)

## [0.1.7] - 2026-04-11

### Added

- **Thread Template** — Initialize threads with predefined files and directories
  - Pattern-level template configuration (`template = "name"` in config.toml)
  - Template files copied to thread directory on first message
  - `/template` command to re-apply template to existing thread
  - `copy_template_files` shared utility function

- **chat_name prefix matching** — Feishu chat_name pattern now uses prefix match instead of exact match

### Fixed

- Template initialization order bug (template now initialized before .jyc directory check)
- PR review comments addressed (.unwrap() → .expect(), logged file operation warnings)

## [0.1.6] - 2026-04-10

### Added

- PR Review skill for code review workflow
- Bump version skill for automated version bumping workflow

### Changed

- MCP tool names unified to `jyc_` prefix (`jyc_reply`, `jyc_question`, `jyc_vision`)

## [0.1.5] - 2026-04-09

### Added

**Vision MCP Tool** — Image and visual content analysis
- New `vision_analyze_image` MCP tool for analyzing images, PDFs, screenshots, and videos
- Provider-agnostic: works with Kimi, Volcengine/Ark, OpenAI, or any OpenAI-compatible vision API
- Configuration via `[vision]` section in `config.toml` (api_key, api_url, model)
- Supports local file paths (absolute) and HTTP(S) URLs
- Base64 data URI encoding for API transport
- 300s MCP timeout for large files
- File-based logging to `.jyc/vision-tool.log`
- Hidden CLI subcommand `mcp-vision-tool` (spawned by OpenCode)

**Unified Attachment Configuration** — Channel-agnostic attachment handling
- New `[attachments.inbound]` and `[attachments.outbound]` config sections
- Per-pattern attachment overrides in channel pattern config
- Shared `core/attachment_storage.rs` module (replaces duplicated code in email/Feishu)
- Consistent filename generation with extension preservation and 50-char truncation
- Path traversal protection at ingestion time (strips directory components, control chars)

**System Prompt Enhancements**
- Tool usage instructions: use `webfetch` for web searches (not curl/wget)
- Resilience instructions: try multiple approaches, don't give up after single failure
- Try alternative sites when a URL fails
- Enhanced plan mode with `<system-reminder>` tag for emphasis
- Updated PLAN mode system prompt with clearer allowed/prohibited actions

### Fixed

**Feishu Image Downloads** — Complete fix for image attachment handling
- Use message resource endpoint (`/im/v1/messages/:id/resources/:key`) instead of standalone image endpoint (was returning 400)
- Direct HTTP for tenant access token retrieval (openlark SDK returned empty responses)
- Validate download responses are actual file data, not JSON API errors
- Skip phantom attachments on download failure (no more zero-byte entries)

**Feishu Command Parsing** — Strip mentions for `/command` recognition
- Remove `@mention` placeholders entirely instead of replacing with display names
- `@jyc /model ls ark` now correctly parsed as `/model ls ark`

**Model Display in Logs** — Restored ai{m=...} span
- Use `Empty` + `.record()` pattern for model discovery via SSE
- SSE handler records model on parent span when discovered from `message.updated`
- Upfront recording when model is known from config or `/model` override

**Duplicate Footer Separators** — Prevent duplicate '---' in Feishu and email replies
- Added `strip_trailing_separators()` function to email_parser module
- Clean reply text before adding footer in both Feishu and email outbound adapters

**Attachment Security**
- Unified file size parser (removed duplicate `parse_human_size`)
- Consistent dot-prefix extension validation across all channels
- Size check before creating attachment (not after full download)

### Changed

- Consolidated `config_template.toml` and `config.example.toml` into single `config.example.toml`
- `config init` CLI command now uses `config.example.toml`
- Vision tool timeout: 300s (was 120s)
- `Updated processing state` log reduced from debug to trace
- Refactored `get_mcp_tool_command()` shared between reply and vision tools
- Remove `.opencode/package-lock.json` from git tracking

### Removed

- Dead code: `save_attachment_to_disk` (websocket.rs), `parse_attachment_size` (attachment_validator.rs), `parse_human_size` (websocket.rs)
- Duplicate `config_template.toml` file

## [0.1.3] - 2026-04-08

### Added

**Token-based Session Management** — Replace time-based with token-based approach
- Real-time token tracking from SSE `step.finish` events
- Automatic model context detection (95% as safety threshold)
- Session reset when accumulated tokens exceed configured maximum
- Immediate persistence of token count after each processing step

**Token Usage Display** — Real-time token monitoring in user interface
- Reply footer displays current token usage: `Tokens: 20.7K/122K`
- Standardized K unit display (1024 basis) with 0.1K precision
- Shows actual reset threshold (model context 95% or configured value)

**Advanced Configuration** — Flexible token limit management
- `max_input_tokens` config option in `config.toml`
- Default threshold: 122,880 tokens (120K × 1024)
- Supports automatic detection of model context limits
- Override capability for specific use cases

### Changed

**SessionState Data Structure** — Updated for token tracking
- Removed `total_active_time` and `last_active_start` fields
- Added `total_input_tokens` and `max_input_tokens` fields
- Session lifecycle now based on token limits instead of time

**DESIGN.md Documentation** — Complete update for token-based system
- Revised session management architecture
- Updated flowcharts and process descriptions
- Added configuration and user interface documentation
- Removed obsolete time-based session management content

**Reply Footer Format** — Enhanced with token information
- Format: `---\n\nModel: <model> | Mode: <mode> | Tokens: <current>K/<max>K`
- Clean formatting with standardized units
- Clear display of remaining token capacity

### Fixed

**Token Counting Accuracy** — Standardized units for consistency
- Use 1024 instead of 1000 for K unit calculations
- Default max input tokens: 120 × 1024 = 122,880 (not 120,000)
- Precise display formatting to 0.1K

**Debug Logging** — Enhanced model context detection visibility
- Added detailed logging for model limit lookup process
- Log available models in provider and found model details
- Improved diagnostics for detection success/failure scenarios

### Technical Details

**Session Persistence** — State saved in `.jyc/opencode-session.json`
- Includes current token count and maximum threshold
- Automatic reset detection at session creation
- AI prompt includes notification when session resets due to token limit

**Configuration Example**:
```toml
[opencode]
# Optional: Maximum input tokens per session before resetting
# Default: 120*1024 = 122,880 tokens (95% of typical 128K model context)
max_input_tokens = 122880
```

**System Integration** — Seamless adoption in existing architecture
- Maintains all existing API contracts
- Compatible with both Email and Feishu channels
- Full backward compatibility with chat history system

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
