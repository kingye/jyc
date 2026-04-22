# JYC: Channel-Agnostic AI Agent (Rust)

## Overview

JYC is a channel-agnostic AI agent that operates through messaging channels. Users interact with the agent by sending messages (email, FeiShu, Slack, etc.), and the agent responds autonomously using OpenCode AI. The agent maintains conversation context per thread, enabling coherent multi-turn interactions.

**Core Concept:** Messaging channels are the interface; AI is the brain. The architecture is channel-agnostic — adding a new channel requires only implementing an inbound and outbound adapter trait.

**Why Rust:** Single static binary, zero runtime dependencies, memory safety without GC, and predictable low-latency performance for long-running server processes.

## Use Cases

- **Support Agent** — Automatically respond to support inquiries with context-aware replies
- **Task Automation** — Execute tasks requested via messages and respond with results
- **Notification Processor** — Process notifications and take action based on content
- **Personal Assistant** — Manage schedules, reminders, and information requests via messaging
- **Cross-Channel Agent** — Same AI agent accessible through multiple channels (email, FeiShu, etc.)

## Architecture

### High-Level Flow

```
User sends message (any channel) → Pattern Match → Thread Queue → Worker (AI) → Reply via originating channel
                                                         ↓
                                               Thread-based context
                                               (remembers conversation)
```

### Architecture Block Diagram

```
┌──────────────────────────────────────────────────────────────────────────────────────────┐
│                       Inbound Channels (tokio tasks, run concurrently)                    │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐                 │
│  │ Email Inbound│  │FeiShu Inbound│  │GitHub Inbound│  │ Slack Inbound│ (future)        │
│  │  (IMAP/TLS)  │  │ (WebSocket)  │  │ (REST poll)  │  │  (WebHook)   │                 │
│  │              │  │              │  │              │  │              │                 │
│  │ match_message│  │ match_message│  │ match_message│  │ match_message│                 │
│  │ derive_thread│  │ derive_thread│  │ derive_thread│  │ derive_thread│                 │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘                 │
└─────────┼──────────────────┼──────────────────┼──────────────────┼───────────────────────┘
          │                  │                  │                  │
          ▼                  ▼                  ▼                  ▼
    InboundMessage     InboundMessage     InboundMessage     InboundMessage
    (channel:"email")  (channel:"feishu") (channel:"github") (channel:"slack")
          │                  │                  │                  │
          └────────┬─────────┘──────────────────┘──────────────────┘
                   ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                       MessageRouter                                      │
│  - Receives ALL messages from all channels via mpsc::Sender              │
│  - Delegates matching to adapter.match_message()                         │
│  - Delegates thread naming to adapter.derive_thread_name()               │
│  - Sends to ThreadManager via mpsc channel (fire-and-forget)             │
└────────────────────────┬────────────────────────────────────────────────┘
                         │ send (non-blocking)
                         ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                       ThreadManager                                      │
│  max_concurrent_threads: 3 (Semaphore-bounded)                           │
│  max_queue_size_per_thread: 10                                           │
│                                                                          │
│  ┌─────────────────────────────────────────────────────┐                │
│  │        Thread Queues (HashMap<String, ThreadQueue>)  │                │
│  │                                                      │                │
│  │  "thread-A" → mpsc::Receiver ← [msg2, msg3]         │                │
│  │  "thread-B" → mpsc::Receiver ← [msg4]               │                │
│  │  "thread-C" → mpsc::Receiver ← []                   │                │
│  └─────────────────────────────────────────────────────┘                │
│                                                                          │
│  Tokio Semaphore (3 permits):                                            │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                     │
│  │ Worker A    │  │ Worker B    │  │ Worker C    │                     │
│  │ (permit 1)  │  │ (permit 2)  │  │ (permit 3)  │                     │
│  │ processing  │  │ processing  │  │ idle        │                     │
│  │ thread-A/m1 │  │ thread-B/m4 │  │             │                     │
│  │   ┌─────┐   │  │   ┌─────┐   │  │             │                     │
│  │   │Event│   │  │   │Event│   │  │             │                     │
│  │   │Bus A│   │  │   │Bus B│   │  │             │                     │
│  │   └─────┘   │  │   └─────┘   │  │             │                     │
│  └─────────────┘  └─────────────┘  └─────────────┘                     │
│                                                                          │
│  Thread Event System (per thread):                                       │
│  ┌─────────────────────────────────────────────────────┐                │
│  │  • Thread-isolated event bus                        │                │
│  │  • SSE → ThreadEvent conversion (OpenCode Client)   │                │
│  │  Heartbeat timer (default 10min interval, configurable)│                │
│  │  • Processing state tracking                        │                │
│  └─────────────────────────────────────────────────────┘                │
│                                                                          │
│  New thread arrives → tokio::spawn → acquire semaphore permit            │
│  Worker loop: recv from mpsc → process (rx passed to agent for           │
│    live injection) → recv next                                           │
│  Thread queue empty + no pending → release permit, task exits            │
└────────────────────────┬────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                    Worker (per message) — ThreadManager                   │
│                                                                          │
│  0. If event support enabled: create thread event bus and start listener │
 │  1. MessageStorage::store(msg) → append to chat_history_YYYY-MM-DD.md   │
│  2. Save inbound attachments (allowlisted)                               │
│  3. CommandRegistry::process_commands(body, ctx)                         │
│     → parse, execute, strip in single pass → cleaned body + results      │
│  4. If body empty after commands → direct reply with results, return     │
│  5. Dispatch to agent mode:                                              │
│     - "static" → send configured text via OutboundAdapter                │
│     - "opencode" → OpenCodeService::generate_reply(msg)                  │
│  6. If agent returns fallback text → send via OutboundAdapter            │
│  7. Event listener monitors progress and sends heartbeats (default 10min interval, configurable)│
│  8. Worker picks next message from thread queue                          │
└────────────────────────┬────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────────────────┐
│             OpenCodeService::generate_reply() (agent-specific)           │
│                                                                          │
│  1. Ensure OpenCode server is running (auto-start)                       │
│  2. Setup per-thread opencode.json (model, MCP tools, permissions)       │
│  3. Get or create session (verify via API, persist .jyc/opencode-session.json)    │
│     - Check if session has exceeded max_input_tokens threshold           │
│     - If exceeded → delete old session, create new one                   │
│     - Record if session was reset due to token limit                     │
│  4. Clean up stale signal file                                           │
│  5. Build system prompt (config + directory rules + system.md)           │
│     - Include session reset notification if token limit was hit          │
│  6. Build user prompt (stripped body )                       │
│  7. Check mode override (plan/build)                                     │
│  8. Send prompt via SSE streaming (activity timeout, tool detection)     │
│     - Track input tokens from step-finish events                         │
│     - Persist token count immediately after each step                    │
│  9. Handle result → return GenerateReplyResult                           │
│     - reply_sent_by_tool: true → done                                    │
│     - ContextOverflow → new session + retry                              │
│     - Stale session → delete + retry                                     │
│     - No tool used → return reply_text for fallback                      │
│                                                                          │
│  ┌─────────────────────────────────────┐                            │
│  │  MCP Tool: reply_message (subprocess)   │                            │
│  │  Binary: jyc mcp-reply-tool             │                            │
│  │  Transport: stdio (rmcp)                │                            │
│  │                                         │                            │
│  │  1. Decode reply-context.json (routing only)   │                            │
 │  │  2. Append reply to chat log (chat_history_YYYY-MM-DD.md) │                │
 │  │  3. Write reply-sent.flag signal file   │                            │
 │  │  (Monitor reads from chat log + sends via│                            │
 │  │   pre-warmed outbound adapter)          │                            │
│  └─────────────────────────────────────────┘                            │
│                                                                          │
│  ┌─────────────────────────────────────┐                            │
│  │  MCP Tool: analyze_image (subprocess)   │                            │
│  │  Binary: jyc mcp-vision-tool            │                            │
│  │  Transport: stdio (rmcp)                │                            │
│  │                                         │                            │
│  │  1. Read image from absolute file path  │                            │
│  │     or download from HTTP(S) URL        │                            │
│  │  2. Convert to base64 data URI          │                            │
│  │  3. Call OpenAI-compatible vision API   │                            │
│  │  4. Return analysis text                │                            │
│  │  Config: [vision] in config.toml        │                            │
│  │  (api_key, api_url, model passed via    │                            │
│  │   env vars in opencode.json)            │                            │
│  └─────────────────────────────────────────┘                            │
└─────────────────────────────────────────────────────────────────────────┘
                      │
                      ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                     Outbound Channels (Reply)                            │
│  context.channel → ChannelRegistry → OutboundAdapter                     │
│  ┌───────────────┐  ┌───────────────┐  ┌───────────────┐  ┌───────────┐│
│  │ Email Outbound│  │FeiShu Outbound│  │GitHub Outbound│  │Slack(Fut) ││
│  │  (SMTP/TLS)   │  │  (API)        │  │  (API)        │  │  (API)    ││
│  │ markdown→HTML │  │ format for    │  │ Issue/PR comms│  │ format    ││
│  │ threading hdrs│  │ feishu msg    │  │ [Role] prefix │  │ for slack ││
│  └───────────────┘  └───────────────┘  └───────────────┘  └───────────┘│
└─────────────────────────────────────────────────────────────────────────┘
```

### Components

1. **Inbound Adapters** — Channel-specific message receivers (Email/IMAP, FeiShu/WebSocket, GitHub/REST polling)
2. **Outbound Adapters** — Channel-specific reply senders (Email/SMTP, FeiShu/API, GitHub/REST)
3. **Message Router** — Receives messages from all channels, delegates matching to adapters, routes to ThreadManager
4. **Thread Manager** — Per-thread queues with semaphore concurrency control, worker spawn/manage
5. **Thread Event Bus** — Thread-isolated event bus for publishing and subscribing to processing events (SSE → ThreadEvent conversion)
6. **Thread Event System** — Heartbeat rhythm control: monitors processing progress and sends periodic updates (default every 10 minutes, configurable via `[heartbeat]` config section)
7. **Prompt Builder** — Builds channel-agnostic prompts from InboundMessage
8. **MCP Reply Tool** — `reply_message` tool via `rmcp`, appends reply to chat log and writes signal file. Monitor reads from chat log and sends via pre-warmed outbound adapter
9. **MCP Vision Tool** — `analyze_image` tool via `rmcp`, analyzes images using OpenAI-compatible vision API. Configure via `[vision]` section
10. **MCP Question Tool** — `ask_user` tool via `rmcp`, sends question to user and waits for reply (up to 5 minutes)
11. **Pending Delivery Watcher** — Background task that runs alongside SSE stream, watches for signal files and delivers messages immediately
12. **Message Storage** — Unified chat log storage: daily log files (`chat_history_YYYY-MM-DD.md`) with HTML comment metadata
13. **State Manager** — Track processed UIDs per channel, handle migrations
14. **Security Module** — Path validation, file size/extension checks for attachments
15. **Attachment Storage** — Channel-agnostic attachment saving with path traversal protection
16. **Inspect Server + Dashboard** — TCP JSON line protocol for runtime state queries, TUI dashboard for live monitoring
17. **MetricsCollector** — Lightweight stats accumulation for monitoring thread/channel activity
18. **Command System** — `/command` parsing and execution (`/model`, `/plan`, `/build`, `/reset`, `/close`, `/template`)
19. **Thread Lifecycle** — Channel-agnostic thread close mechanism via `on_thread_close` callback
20. **Template System** — Initialize new threads with predefined files from `templates/` directory
21. **AgentService** — Unified agent dispatch trait for static and OpenCode modes

### Design Principles: Component Responsibilities

Each component has a single, clear responsibility. Data flows through the system with transformations happening at well-defined boundaries.

**InboundAdapter** (e.g., `EmailInboundAdapter`)
- Boundary between the external world and the internal system
- Parses raw data from the channel (e.g., raw email bytes via `mail-parser`)
- Cleans and normalizes data at the boundary: strips redundant `Re:/回复:` from subject, cleans bracket-nested duplicates
- Produces a clean `InboundMessage` — all downstream consumers receive clean data

**MessageStorage**
- Unified chat log storage: appends messages and replies to daily log files
- HTML comment metadata: stores timestamp, type, sender, channel, and external ID
- Dual-write support: during migration, maintains compatibility with legacy format
- No transformation or business logic - stores messages exactly as given
- Chat logs are stored in `chat_history_YYYY-MM-DD.md` files
- Format: HTML comments for metadata + Markdown content

**PromptBuilder**
- Builds the user prompt from the incoming message
- Strips quoted history from body and truncates to fit AI token budget
- Note: Reply context is saved to disk (.jyc/reply-context.json), NOT embedded in the prompt
- Does NOT include conversation history in the prompt (OpenCode session memory handles multi-turn context)

**Reply Tool** (MCP `reply_message`)
- Orchestrator for the reply flow
- Reads `.jyc/reply-context.json` from disk to get routing info (channel name, message timestamp)
- Reads ALL message metadata (sender, recipient, topic, threading headers) from reply-context.json — NOT from the AI prompt
- Builds the full reply in markdown: AI reply text + quoted history (`prepare_body_for_quoting`)
- Delegates sending to OutboundAdapter (passes the full markdown reply)
- Delegates storage to MessageStorage (appends to daily chat log file)
- Chat log entries reflect exactly what was sent to the recipient

**SmtpClient** (and other transport services)
- Dumb transport: receives markdown, converts to HTML (via `comrak`), adds email headers, sends via `lettre`
- Adds `Re:` to subject, sets `In-Reply-To` and `References` headers for threading
- Does NOT build quoted history, does NOT clean or transform content
- **Structured error handling**: Uses lettre's structured SmtpError API for error classification: permanent errors (5xx) fail immediately, transient errors (4xx) retry with exponential backoff (3 attempts, 5-60s), connection/timeout errors reconnect with backoff (2 attempts).
 - **Shared instance**: A single `SmtpClient` (via `EmailOutboundAdapter`) is created at monitor startup and shared across ThreadManager fallback, monitor reply send path (when MCP tool appends to chat log), and AlertService

**Thread Event System**
- **Thread Event Bus** - Thread-isolated event bus for SSE → ThreadEvent conversion
- **Heartbeat Control** - Sends periodic progress updates (default every 10 minutes, configurable via `[heartbeat]` section) during long AI operations
- **Thread Isolation** - Each thread has independent event bus and heartbeat state
- **Event Types**:
  - `ProcessingStarted`, `ProcessingProgress`, `ProcessingCompleted`
  - `ToolStarted`, `ToolCompleted`
  - `Heartbeat` - Generated by Thread Manager based on processing progress
- **Heartbeat Conditions**:
  1. ✅ Current message being processed
  2. ✅ Processing state available (from `ProcessingProgress` events)
  3. ✅ Processing elapsed ≥ `min_elapsed_secs` (default 1 minute)
   4. ✅ Time since last heartbeat ≥ `interval_secs` (default 10 minutes)
 - **Event Flow**: SSE events → OpenCode Client conversion → Thread Event Bus → Thread Manager monitoring → Heartbeat via `send_heartbeat()` (pre-formatted from per-channel template)

**Reply context** saved to `.jyc/reply-context.json` — the AI never sees it
- Only 5 fields: `channel`, `threadName`, `incomingMessageDir`, `uid`, `_nonce`
- Channel-agnostic — no email-specific fields (no sender, recipient, topic, threading headers)
- The AI passes it through unchanged as `reply-context.json=<base64>` (not XML tags)
 - The Reply Tool decodes it for routing only — reads all message metadata from chat log frontmatter
 - Short token (~120 bytes) reduces AI corruption risk compared to the old 12-field token (~400 bytes)

### Chat Log Storage Architecture

JYC 0.1.2 introduces a new unified chat log storage system that replaces the previous timestamped directory approach.

#### Design Goals
1. **Simplicity**: Single log file per day instead of nested directories
2. **Searchability**: All messages in chronological order for easy searching
3. **AI Accessibility**: Chat history accessible to AI via tool calls
4. **Backward Compatibility**: Smooth transition with dual-write support

#### Log File Format
```
<!-- 2026-04-07T01:18:31.002+00:00 | type:received | matched:true | sender:ou_c36ae8bf58a1d727fffd2289467fefce | channel:feishu_bot | external_id:om_x100b5271f8a044a0b4ca586517f9e5d -->
**FROM:** ou_c36ae8bf58a1d727fffd2289467fefce
**SUBJECT:** self-hosting-jyc

部署完成了吗？

---
```

#### Migration Strategy
1. **Dual-write Phase**: Messages written to both legacy directories and new logs
2. **Email Parser Enhancement**: `email_parser` reads from logs first, falls back to directories
3. **Directory Removal**: Legacy directories gradually phased out after verification

#### AI Access Pattern
- AI uses `glob "chat_history_*.md"` to find log files
- `read "chat_history_2026-04-07.md"` to access specific logs
- `grep "keyword" --include "chat_history_*.md"` to search history

### Data Flow Summary

```
Email arrives
  → InboundAdapter: parse, clean subject + body → clean InboundMessage
     → MessageStorage: store as-is → append to chat log (with full frontmatter metadata)
       → PromptBuilder: strip + truncate body → prompt =<routing token>
         → AI: receives stripped body + minimal routing token
           → Reply Tool: decode reply-context.json → read chat log for all metadata
            → build_full_reply_text(): AI reply + quoted history
            → SmtpClient: markdown→HTML, add headers + attachments, send via SMTP
             → MessageStorage: append full reply to chat log (= what was sent)
```

### End-to-End Sequence Diagram

```
┌──────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐
│ IMAP │  │ Inbound  │  │ Message  │  │  Thread  │  │ Prompt   │  │ OpenCode │  │  Reply   │  │  SMTP    │
│Server│  │ Adapter  │  │ Storage  │  │ Manager  │  │ Builder  │  │  (AI)    │  │  Tool    │  │ Client   │
└──┬───┘  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘  └────┬─────┘
   │           │             │             │             │             │             │             │
   │ new email │             │             │             │             │             │             │
   │ (IDLE     │             │             │             │             │             │             │
   │  notify)  │             │             │             │             │             │             │
   ├──────────>│             │             │             │             │             │             │
   │           │             │             │             │             │             │             │
   │      mail_parser::      │             │             │             │             │             │
   │      Message::parse()   │             │             │             │             │             │
   │      clean_at_boundary: │             │             │             │             │             │
   │       strip_reply_prefix│             │             │             │             │             │
   │       clean_email_body  │             │             │             │             │             │
   │           │             │             │             │             │             │             │
   │      InboundMessage     │             │             │             │             │             │
   │      (via mpsc::send)   │             │             │             │             │             │
   │           ├─────────────┼────────────>│             │             │             │             │
   │           │             │             │             │             │             │             │
   │           │             │  store()    │             │             │             │             │
   │           │             │<────────────┤             │             │             │             │
   │           │             │             │             │             │             │             │
   │           │        tokio::fs::write   │             │             │             │             │
    │           │        append to chat log │             │             │             │             │
    │           │        (full frontmatter)  │             │             │             │             │
   │           │             │             │             │             │             │             │
   │           │             │             │ build_prompt()            │             │             │
   │           │             │             ├────────────>│             │             │             │
   │           │             │             │             │             │             │             │
   │           │             │        strip_quoted_history│             │             │             │
   │           │             │        + truncate body     │             │             │             │
   │           │             │        + append reply-context.json=│             │             │             │
   │           │             │          (minimal 5-field  │             │             │             │
   │           │             │           routing token)   │             │             │             │
   │           │             │             │             │             │             │             │
   │           │             │             │ generate_reply()          │             │             │
   │           │             │             ├─────────────┼────────────>│             │             │
   │           │             │             │             │             │             │             │
   │           │             │             │             │    SSE stream              │             │
   │           │             │             │             │    (reqwest-eventsource)   │             │
   │           │             │             │             │             │             │             │
   │           │             │             │             │    AI calls reply_message  │             │
   │           │             │             │             │      message = AI reply    │             │
   │           │             │             │             │      context = base64      │             │
   │           │             │             │             │             │             │             │
   │           │             │             │             │             ├────────────>│             │
   │           │             │             │             │             │             │             │
   │           │             │             │             │        decode context      │             │
   │           │             │             │             │        read chat log      │             │
   │           │             │             │             │             │             │             │
   │           │             │             │             │   prepare_body_for_quoting │             │
   │           │             │             │             │             │             │             │
   │           │             │             │             │   send_reply(full_text)    │             │
   │           │             │             │             │             ├────────────>│             │
   │           │             │             │             │             │             │             │
   │           │             │             │             │             │   comrak:   │             │
   │           │             │             │             │             │    md→html  │             │
   │           │             │             │             │             │   lettre:   │             │
   │           │             │             │             │             │    headers  │             │
   │           │             │             │             │             │    send     │────> recipient
   │           │             │             │             │             │             │             │
   │           │             │             │             │  store_reply()            │             │
   │           │             │             │             │  write signal file        │             │
   │           │             │             │             │             │             │             │
   │           │             │             │  detect signal file /     │             │             │
   │           │             │             │  SSE tool completion      │             │             │
   │           │             │             │             │             │             │             │
   │           │             │             │  worker done,             │             │             │
   │           │             │             │  pick next from queue     │             │             │
```

**Key invariants:**
- **InboundAdapter** is the only place where data is cleaned (subject + body)
- **MessageStorage** stores data as-is (with full frontmatter metadata) — the authoritative source of message data
- **PromptBuilder** strips quoted history from body for the AI prompt; does NOT include conversation history (OpenCode session memory handles that)
- **`build_full_reply_text()`** is the single shared function for assembling the full reply (AI text + quoted history) — called by `EmailOutboundAdapter` and the monitor's reply send path, NOT by agents or ThreadManager
- **SmtpClient** is a dumb transport: markdown→HTML + headers + attachments + send
 - **reply-context.json** is a minimal routing token (5 fields) — all message metadata comes from chat log frontmatter
 - **Chat log entries** = exactly what the recipient receives (minus HTML formatting)

## Feishu Channel Implementation

The Feishu (飞书) channel implementation provides real-time messaging capabilities through the Lark/Feishu platform. Unlike email which uses IMAP/SMTP, Feishu uses a modern API-based approach with WebSocket for real-time message reception.

### Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    Feishu Platform (Cloud)                   │
│                                                             │
│  ┌──────────────┐    API Calls    ┌────────────────────┐  │
│  │  Feishu API  │◄───────────────►│ Feishu WebSocket  │  │
│  │   (REST)     │                 │    (Real-time)    │  │
│  └──────────────┘                 └─────────┬──────────┘  │
└──────────────────────────────────────────────┼─────────────┘
                                               │
                                        WebSocket Events
                                               │
                                               ▼
┌─────────────────────────────────────────────────────────────┐
│                     JYC Feishu Channel                      │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐  │
│  │             FeishuInboundAdapter                    │  │
│  │  • LarkWsClient WebSocket connection management    │  │
│  │  • Real-time message reception via WebSocket       │  │
│  │  • Event parsing (im.message.receive_v1)           │  │
│  │  • FeishuMatcher for matching and thread derivation│  │
│  │  • Converts Feishu events to InboundMessage         │  │
│  └─────────────────────────────────────────────────────┘  │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐  │
│  │             FeishuOutboundAdapter                   │  │
│  │  • Feishu API client for message sending            │  │
│  │  • Message formatting (markdown, text)              │  │
│  │  • Heartbeat/progress updates                       │  │
│  │  • Alert notifications                              │  │
│  └─────────────────────────────────────────────────────┘  │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐  │
│  │                FeishuClient                         │  │
│  │  • Authentication and token management              │  │
│  │  • API request handling                             │  │
│  │  • Name lookup: get_chat_name, get_user_name        │  │
│  │    (with in-memory caching)                         │  │
│  │  • Error handling and retry logic                   │  │
│  └─────────────────────────────────────────────────────┘  │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐  │
│  │              LarkWsClient                           │  │
│  │  • WebSocket connection to Feishu platform          │  │
│  │  • Automatic reconnection with exponential backoff  │  │
│  │  • Event frame parsing and dispatch                 │  │
│  └─────────────────────────────────────────────────────┘  │
│                                                             │
│  ┌─────────────────────────────────────────────────────┐  │
│  │              FeishuFormatter                        │  │
│  │  • Multi-format message support                     │  │
│  │  • Markdown, text, and HTML formatting              │  │
│  │  • Content escaping and sanitization                │  │
│  └─────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
```

### Key Features Implemented

1. **Real-time Message Reception** via LarkWsClient WebSocket
   - `LarkWsClient` manages WebSocket connection to Feishu platform
   - Event frame parsing for `im.message.receive_v1` events
   - Automatic reconnection with exponential backoff
   - Configurable WebSocket enable/disable
   - JSON event body extraction and validation

2. **API-based Message Sending** via FeishuClient
   - Full support for Feishu message types
   - Proper authentication with app credentials
   - Rate limiting and error handling
   - Support for rich content formatting

3. **Name Lookup with Caching**
   - `get_chat_name()` resolves chat IDs to display names
   - `get_user_name()` resolves user IDs to display names
   - In-memory caching to reduce API calls
   - Used by thread name derivation and message display

4. **Multi-format Message Support**
   - Markdown formatting (primary)
   - Plain text fallback
   - HTML support for complex formatting
   - Automatic format detection and conversion

5. **Thread Management**
   - Thread name derivation from chat metadata
   - Message pattern matching for routing
   - Conversation context preservation
   - Cross-channel thread compatibility

6. **Error Handling and Recovery**
   - Comprehensive FeishuError enum
   - Automatic token refresh on expiration
   - WebSocket reconnection on failure
   - Graceful degradation when features unavailable

### Configuration

Feishu channel configuration in `config.toml`:

```toml
[channels.feishu]
type = "feishu"

[channels.feishu.config]
app_id = "your-app-id"
app_secret = "your-app-secret"
# Optional: WebSocket configuration
websocket.enabled = true
websocket.reconnect_delay_ms = 5000
```

### Implementation Phases

The Feishu channel was implemented in multiple phases:

**Phase 1: Foundation**
- Basic FeishuConfig structure
- Channel type registration
- Skeleton adapters

**Phase 2: Client Implementation**
- FeishuClient with openlark SDK integration
- Authentication and token management
- Basic message sending capabilities

**Phase 3: WebSocket Integration**
- Real-time message reception
- WebSocket connection management
- Event parsing and routing

**Phase 4: Complete Adapter Implementation**
- Full InboundAdapter implementation
- Complete OutboundAdapter implementation
- Message formatting and validation

**Phase 5: Production Readiness**
- Comprehensive error handling
- Unit test coverage
- Configuration validation
- Performance optimization

### Integration with Core System

The Feishu channel integrates seamlessly with the core JYC architecture:

- Uses the same `InboundMessage` and `OutboundMessage` types
- Follows the same pattern matching system
- Integrates with the thread manager and queue system
- Supports all existing AI features and command system
- Compatible with MCP reply tool and alert service

### Testing

Comprehensive unit tests cover:
- Client initialization and authentication
- Message sending and receiving
- WebSocket connection management
- Error handling and recovery
- Message formatting and parsing

All Feishu channel tests pass as part of the 283 total tests in the test suite.

## GitHub Channel Implementation

### Architecture Overview

The GitHub channel enables multi-agent AI workflows on GitHub issues and pull requests. It uses a **polling-based** inbound adapter (GitHub REST API) and posts comments as the outbound mechanism.

```
┌─────────────────────────────────────────────────────────────────────────┐
│                     GitHub Channel Architecture                         │
│                                                                         │
│  ┌──────────────────┐         ┌──────────────────┐                     │
│  │  GithubInbound   │         │  GithubOutbound   │                    │
│  │  (REST polling)  │         │  (Comment poster)  │                   │
│  │                  │         │                    │                    │
│  │ poll_once():     │         │ send_reply():      │                   │
│  │  list open issues│         │  post comment with │                   │
│  │  list comments   │         │  [Role] prefix     │                   │
│  │  detect @j:role  │         │                    │                   │
│  │  detect closes   │         │ send_heartbeat():  │                   │
│  │  detect edits    │         │  post progress     │                   │
│  └────────┬─────────┘         └──────────┬─────────┘                   │
│           │                              │                              │
│  ┌────────▼─────────┐         ┌──────────▼─────────┐                   │
│  │  GithubMatcher   │         │   GithubClient     │                   │
│  │                  │         │  (REST API v3)      │                   │
│  │ match_message(): │         │                    │                    │
│  │  role matching   │         │  list_open_issues()│                    │
│  │  self-loop check │         │  list_comments()   │                    │
│  │  rules_match():  │         │  post_comment()    │                    │
│  │   github_type    │         │  get_auth_user()   │                    │
│  │   labels         │         │                    │                    │
│  │   assignees      │         │                    │                    │
│  └──────────────────┘         └────────────────────┘                   │
└─────────────────────────────────────────────────────────────────────────┘
```

### Multi-Agent Workflow

JYC supports a planner/developer/reviewer workflow on GitHub issues and PRs:

```
User creates issue
    │
    ├── @j:planner comment
    │       ↓
    │   [Planner] analyzes codebase, discusses with user
    │   [Planner] creates empty PR with implementation plan
    │   [Planner] posts @j:developer comment on PR
    │       ↓
    ├── @j:developer triggered on PR
    │       ↓
    │   [Developer] implements code per plan
    │   [Developer] pushes commits, posts @j:reviewer comment
    │       ↓
    ├── @j:reviewer triggered on PR
    │       ↓
    │   [Reviewer] reviews code changes
    │   [Reviewer] approves or requests changes via @j:developer
    │       ↓
    └── Cycle continues until PR is approved and merged
```

Each role maps to a pattern with a `role` field and a template that defines the agent's behavior. Templates are in `templates/github-{planner,developer,reviewer}/`.

### Mention-Driven Routing

Routing is driven by `@j:<role>` mentions in comments:

1. **Detection**: `poll_once()` scans comments for the regex `@j:(\w+)` via `extract_mention_role()`
2. **Metadata**: The extracted role is stored as `handover_role` in message metadata
3. **Matching**: `GithubMatcher::match_message()` iterates patterns, matching `pattern.role` against `handover_role` (case-insensitive)
4. **Self-loop prevention**: If the comment's `[Role]` prefix matches the target pattern's role, the pattern is skipped (prevents a `[Developer]` comment with `@j:developer` from re-triggering the developer)
5. **Rule filtering**: After role matching, `rules_match()` validates `github_type`, `labels`, and `assignees` rules (AND logic between rules, OR logic within each rule)

### Pattern Rule Filtering

When a `@j:<role>` mention matches a pattern's role, the pattern's rules are additionally checked:

```
@j:developer detected → find patterns with role="Developer"
    │
    ├── Pattern "developer" (rules: github_type=["pull_request"], assignees=["alice"])
    │       ↓
    │   Check github_type: message is "pull_request" → ✓
    │   Check assignees: PR assigned to "bob" → ✗
    │   → SKIP (rules don't match)
    │
    ├── Pattern "developer-default" (rules: github_type=["pull_request"])
    │       ↓
    │   Check github_type: message is "pull_request" → ✓
    │   No assignees rule → ✓
    │   → MATCH
    │
    └── Route to thread with "developer-default" pattern
```

Rules:
- **`github_type`**: `"issue"` or `"pull_request"` — OR logic within the list
- **`labels`**: Match if ANY label on the issue/PR is in the pattern's label list — case-insensitive
- **`assignees`**: Match if ANY assignee on the issue/PR is in the pattern's assignee list — case-insensitive
- All present rules use **AND logic** (all must pass). `None` rules are considered matched.

### Close Detection

The adapter detects issue/PR close events by comparing cached state:

1. Each poll cycle fetches all open issue/PR numbers
2. Previously-open issues not in the current set are considered closed
3. Closed threads trigger the `on_thread_close` callback (deletes thread directory)
4. Fallback: `list_closed_since()` checks recently-closed issues as a backup

### Persistent Comment Tracking

Comments are tracked with composite keys (`{comment_id}:{updated_at}`) to detect edits:

- New comment: key not in processed set → process and track
- Edited comment: same ID but new `updated_at` → re-process
- Compaction: when the set exceeds 5000 entries, keep only the 2000 highest IDs
- Backward-compatible with legacy `processed-comments.txt` format (plain comment IDs)

### Configuration

```toml
[channels.my_repo]
type = "github"

[channels.my_repo.github]
owner = "kingye"
repo = "jyc"
token = "${GITHUB_TOKEN}"         # PAT scopes: repo, read:user
api_url = "https://api.github.com"  # Default; set for GitHub Enterprise
poll_interval_secs = 60           # Default: 60

[[channels.my_repo.patterns]]
name = "planner"
enabled = true
role = "Planner"
template = "github-planner"

[channels.my_repo.patterns.rules]
github_type = ["issue"]

[[channels.my_repo.patterns]]
name = "developer"
enabled = true
role = "Developer"
template = "github-developer"

[channels.my_repo.patterns.rules]
github_type = ["pull_request"]
assignees = ["alice", "bob"]      # Optional: restrict to specific assignees

[[channels.my_repo.patterns]]
name = "reviewer"
enabled = true
role = "Reviewer"
template = "github-reviewer"

[channels.my_repo.patterns.rules]
github_type = ["pull_request"]
```

### Thread Naming

| GitHub Type | Pattern | Thread Name |
|-------------|---------|-------------|
| Issue | any | `issue-{N}` |
| Pull Request | developer | `pr-{N}` |
| Pull Request | reviewer | `review-pr-{N}` |

The reviewer gets a separate thread prefix so developer and reviewer can work on the same PR concurrently without context collision.

### Testing

Comprehensive unit tests (14 tests for rule filtering alone) cover:
- Mention-driven routing (role matching, case-insensitive, unknown role)
- Self-loop prevention (own-role skip, cross-role pass)
- Rule filtering (github_type, labels, assignees — AND/OR logic, case-insensitive)
- Pattern fallback (skip first pattern on rule failure, match second)
- Thread name derivation (issue, PR, reviewer prefix)
- Persistent comment tracking (track, reload, edit detection, compaction)
- Trigger message building (issue and PR variants)

## Core Types & Traits

### Channel Abstractions

```rust
/// Channel type identifier
pub type ChannelType = String; // "email", "feishu", "slack", etc.

/// Channel-agnostic normalized message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub id: String,                           // Internal UUID
    /// Channel name from config (e.g., "jiny283", "work")
    pub channel: ChannelType,
    pub channel_uid: String,                  // Channel-specific ID (IMAP UID, feishu msg ID)
    pub sender: String,                       // Display name
    pub sender_address: String,               // Canonical address
    pub recipients: Vec<String>,              // To addresses/IDs
    pub topic: String,                        // Subject (email) / title (feishu)
    pub content: MessageContent,
    pub timestamp: DateTime<Utc>,
    pub thread_refs: Option<Vec<String>>,     // Email: References header
    pub reply_to_id: Option<String>,          // Email: In-Reply-To
    pub external_id: Option<String>,          // Email: Message-ID
    pub attachments: Vec<MessageAttachment>,
    pub metadata: HashMap<String, Value>,     // Channel-specific extra data
    pub matched_pattern: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageContent {
    pub text: Option<String>,                 // Plain text
    pub html: Option<String>,                 // HTML (email)
    pub markdown: Option<String>,             // Markdown (feishu, slack)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageAttachment {
    pub filename: String,
    pub content_type: String,
    pub size: usize,
    #[serde(skip)]
    pub content: Option<Vec<u8>>,             // Binary content (transient, not serialized)
    pub saved_path: Option<PathBuf>,          // Set after saving to disk
}

/// Pattern matching result
#[derive(Debug, Clone)]
pub struct PatternMatch {
    pub pattern_name: String,
    pub channel: ChannelType,
    pub matches: HashMap<String, String>,     // Channel-specific match details
}

/// Inbound adapter trait — one per channel type
/// Channel-specific message matching and thread name derivation.
///
/// Pure-logic trait used by MessageRouter. Every channel type implements this.
/// Separated from InboundAdapter to allow use without the lifecycle (start/stop).
/// Stateless implementations (EmailMatcher, FeishuMatcher) can be cheaply created.
pub trait ChannelMatcher: Send + Sync {
    fn channel_type(&self) -> &str;

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
        pattern_match: Option<&PatternMatch>,
    ) -> String;

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch>;
}

/// Inbound adapter trait — adds connection lifecycle on top of ChannelMatcher.
#[async_trait]
pub trait InboundAdapter: ChannelMatcher {
    async fn start(
        &self,
        options: InboundAdapterOptions,
        cancel: CancellationToken,
    ) -> Result<()>;
}

/// Options passed to an inbound adapter's `start()` method.
pub struct InboundAdapterOptions {
    /// Callback for each received message (fire-and-forget)
    pub on_message: Box<dyn Fn(InboundMessage) -> Result<()> + Send + Sync>,
    /// Callback for thread close events (e.g., Feishu chat.disbanded)
    pub on_thread_close: Option<Box<dyn Fn(String) -> Result<()> + Send + Sync>>,
    /// Callback for errors
    pub on_error: Box<dyn Fn(anyhow::Error) + Send + Sync>,
    /// Attachment download configuration
    pub attachment_config: Option<InboundAttachmentConfig>,
}

/// Outbound adapter trait — one per channel type.
/// Owns the full reply lifecycle: format + send + store.
#[async_trait]
pub trait OutboundAdapter: Send + Sync {
    fn channel_type(&self) -> &str;

    async fn connect(&self) -> Result<()>;
    async fn disconnect(&self) -> Result<()>;

    /// Strip channel-specific artifacts from a message body.
    /// Email: strips quoted reply history. Feishu: trims whitespace.
    fn clean_body(&self, raw_body: &str) -> String;

    /// Send a reply with full lifecycle management (format + send + store).
    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        thread_path: &Path,
        message_dir: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult>;

    async fn send_alert(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
    ) -> Result<SendResult>;

    async fn send_heartbeat(
        &self,
        original: &InboundMessage,
        message: &str,
    ) -> Result<SendResult>;
}

#[derive(Debug)]
pub struct SendResult {
    pub message_id: String,
}
```

### Channel Pattern Rules

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelPattern {
    pub name: String,
    pub channel: ChannelType,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub rules: PatternRules,
    pub attachments: Option<AttachmentConfig>,
    pub template: Option<String>,             // Thread template name
    pub thread_name: Option<String>,          // Fixed thread name override
    pub role: Option<String>,                 // Agent role (e.g., "Planner", "Developer", "Reviewer")
    pub trigger_mode: Option<TriggerMode>,    // Trigger mode: Pattern, Mention, or Both
    #[serde(default = "default_true")]
    pub live_injection: bool,                 // Inject into active AI session (default: true)
}

/// Controls when a pattern triggers on incoming messages.
///
/// - **Pattern** (default for GitHub): Only pattern rules (github_type, labels, assignees) need to match.
///   No `@j:<role>` mention required. Used for auto-trigger on new issues/PRs.
/// - **Mention** (default for backward compatibility): Requires `@j:<role>` mention in comment.
///   Pattern rules are optional filters.
/// - **Both**: Both pattern rules match AND `@j:<role>` mention required. Used for reviewer patterns
///   that should only trigger when explicitly requested AND the PR is in review state.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TriggerMode {
    Pattern,
    Mention,
    Both,
}

/// Channel-agnostic pattern matching rules.
/// All present rules must match (AND logic).
/// Each channel's ChannelMatcher only checks the fields relevant to it:
/// - Email checks: `sender`, `subject`
/// - Feishu checks: `mentions`, `keywords`, `sender`, `chat_name`
/// - GitHub checks: `github_type`, `labels`, `assignees`
#[derive(Debug, Clone, Deserialize)]
pub struct PatternRules {
    // --- Shared rules ---
    pub sender: Option<SenderRule>,           // Sender matching (email address, feishu user ID)

    // --- Email rules ---
    pub subject: Option<SubjectRule>,         // Subject matching (email only)

    // --- Feishu rules ---
    pub mentions: Option<Vec<String>>,        // @mention user/bot IDs or names (OR logic)
    pub keywords: Option<Vec<String>>,        // Keywords in message body (OR, case-insensitive)
    pub chat_name: Option<Vec<String>>,       // Group chat names (OR, case-insensitive)

    // --- GitHub rules ---
    pub github_type: Option<Vec<String>>,     // Entity type: "issue" or "pull_request" (OR logic)
    pub labels: Option<Vec<String>>,          // Labels to match (OR logic)
    pub assignees: Option<Vec<String>>,       // Assignees to match (OR logic, case-insensitive)
}

#[derive(Debug, Clone, Deserialize)]
pub struct SenderRule {
    pub exact: Option<Vec<String>>,           // Case-insensitive exact match
    pub domain: Option<Vec<String>>,          // Domain match (email only)
    pub regex: Option<String>,                // Regex match
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubjectRule {
    pub prefix: Option<Vec<String>>,          // Prefix match (stripped from thread name)
    pub regex: Option<String>,                // Regex match
}
```

### Thread Name Derivation

Each channel's `ChannelMatcher` implements `derive_thread_name(message, patterns, pattern_match)` with channel-specific logic:

- **Email**: Strip reply prefixes (Re:, Fwd:, 回复:, 转发:), strip configured subject prefix (e.g., "Jiny:"), sanitize for filesystem. Supports broad separator recognition (`:`, `-`, `_`, `~`, `|`, `/`, `&`, `$`, etc.)
- **FeiShu**: Derive from chat name (via `get_chat_name` with caching) or message content
- **GitHub**: `issue-{N}`, `pr-{N}`, or `review-pr-{N}` (for reviewer pattern). Thread name is derived from issue/PR number and type.
- **Slack** (future): Derive from channel name + thread topic

## Async Event Queue Architecture

### Overview

JYC uses **Tokio** as its async runtime. The message processing pipeline is built on a hierarchy of `tokio::sync::mpsc` channels and a `Semaphore` for bounded concurrency.

### Channel & Task Topology

```
                    ┌─────────────────────┐
                    │  Tokio Runtime       │
                    │  (multi-threaded)    │
                    └─────────────────────┘
                              │
              ┌───────────────┼───────────────┐
              ▼               ▼               ▼
     ┌────────────────┐ ┌────────────────┐ ┌────────────────┐
     │ tokio::spawn   │ │ tokio::spawn   │ │ tokio::spawn   │
     │ IMAP Monitor   │ │ FeiShu Monitor │ │ Alert Service  │
     │ (channel: work)│ │ (WebSocket)    │ │ (flush timer)  │
     └───────┬────────┘ └───────┬────────┘ └────────────────┘
             │                  │
             ▼                  ▼
      mpsc::Sender ─────> mpsc::Receiver
      (bounded, 256)      (MessageRouter task)
                                │
                          ┌─────┴──────┐
                          │ match_msg  │
                          │ derive_thr │
                          └─────┬──────┘
                                │
              ┌─────────────────┼─────────────────┐
              ▼                 ▼                  ▼
    ┌──────────────┐  ┌──────────────┐   ┌──────────────┐
    │ Thread Queue  │  │ Thread Queue  │   │ Thread Queue  │
    │ "thread-A"   │  │ "thread-B"   │   │ "thread-C"   │
    │ mpsc(10)     │  │ mpsc(10)     │   │ mpsc(10)     │
    └──────┬───────┘  └──────┬───────┘   └──────┬───────┘
           │                 │                   │
    ┌──────▼───────┐  ┌──────▼───────┐   ┌──────▼───────┐
    │ tokio::spawn │  │ tokio::spawn │   │ tokio::spawn │
    │ Worker task  │  │ Worker task  │   │ Worker task  │
    │ (semaphore   │  │ (semaphore   │   │ (semaphore   │
    │  acquired)   │  │  acquired)   │   │  acquired)   │
    └──────────────┘  └──────────────┘   └──────────────┘
           │
     ┌─────┴──────────────────────────────┐
     │  Semaphore (3 permits)             │
     │  Controls max concurrent workers   │
     │                                    │
     │  Worker lifecycle:                 │
     │  1. acquire_permit().await         │
     │  2. loop { recv().await → process }│
     │  3. channel closed → drop permit   │
     └───────────────────────────────────┘
```

### Thread Manager: Semaphore + Per-Thread mpsc

```rust
pub struct ThreadManager {
    /// Per-thread bounded mpsc channels
    thread_queues: Mutex<HashMap<String, mpsc::Sender<QueueItem>>>,

    /// Bounds concurrent thread workers
    semaphore: Arc<Semaphore>,

    /// Configuration
    max_queue_size: usize,              // mpsc buffer size (default: 10)

    /// Shared dependencies (wrapped in Arc for worker tasks)
    storage: Arc<MessageStorage>,
    outbound: Arc<dyn OutboundAdapter>, // Channel-agnostic outbound
    agent: Arc<dyn AgentService>,

    /// Thread-isolated event buses (Thread Event system)
    event_buses: Mutex<HashMap<String, ThreadEventBusRef>>,
    enable_events: bool,

    /// Heartbeat configuration (configurable via [heartbeat] config section)
    heartbeat_config: HeartbeatConfig,

    /// Per-channel heartbeat message template
    heartbeat_template: String,

    /// Graceful shutdown (child token — cancelling this does NOT cancel other channels)
    cancel: CancellationToken,

    /// Worker join handles for cleanup
    worker_handles: Mutex<Vec<JoinHandle<()>>>,
}
```

**Enqueue flow:**

```rust
impl ThreadManager {
    pub async fn enqueue(
        &self,
        message: InboundMessage,
        thread_name: String,
        pattern_match: PatternMatch,
    ) {
        let mut queues = self.thread_queues.lock().await;

        if let Some(sender) = queues.get(&thread_name) {
            // Thread queue exists — try_send (non-blocking)
            match sender.try_send(QueueItem { message, pattern_match }) {
                Ok(()) => return,
                Err(TrySendError::Full(_)) => {
                    tracing::warn!(thread = %thread_name, "Queue full, dropping message");
                    return;
                }
                Err(TrySendError::Closed(_)) => {
                    // Worker finished, remove stale queue and recreate below
                    queues.remove(&thread_name);
                }
            }
        }

        // Create new thread queue + spawn worker
        let (tx, rx) = mpsc::channel(self.max_queue_size_per_thread);
        tx.try_send(QueueItem { message, pattern_match }).ok();
        queues.insert(thread_name.clone(), tx);

        let handle = self.spawn_worker(thread_name.clone(), rx);
        self.worker_handles.lock().await.push(handle);
    }

    fn spawn_worker(
        &self,
        thread_name: String,
        mut rx: mpsc::Receiver<QueueItem>,
    ) -> JoinHandle<()> {
        let semaphore = self.semaphore.clone();
        let cancel = self.cancel.clone();
        let agent = self.agent.clone();
        let storage = self.storage.clone();
        // ... clone other Arc deps ...

        tokio::spawn(async move {
            // Acquire semaphore permit (blocks if all workers busy)
            let _permit = tokio::select! {
                permit = semaphore.acquire_owned() => permit.unwrap(),
                _ = cancel.cancelled() => return,
            };

            tracing::info!(thread = %thread_name, "Worker started");

            // Thread Event system setup
            let (current_message_tx, current_message_rx) = tokio::sync::watch::channel(None);
            let event_listener_handle = if enable_events {
                // Create thread-isolated event bus
                let event_bus = Arc::new(SimpleThreadEventBus::new(10));
                // Set event bus for agent service
                let _ = agent.set_thread_event_bus(&thread_name, Some(event_bus.clone())).await;
                
                // Start event listener with heartbeat control
                Some(tokio::spawn(async move {
                    Self::event_listener_with_heartbeat(
                        event_bus,
                        thread_name.clone(),
                        outbound.clone(),
                        current_message_rx,
                    ).await;
                }))
            } else {
                None
            };

            loop {
                let item = tokio::select! {
                    item = rx.recv() => match item {
                        Some(item) => item,
                        None => break, // Channel closed, queue drained
                    },
                    _ = cancel.cancelled() => break,
                };

                // Update current message for event listeners
                let _ = current_message_tx.send(Some(item.message.clone()));
                
                if let Err(e) = process_message(
                    &item, &thread_name, &opencode, &storage, /* ... */
                ).await {
                    tracing::error!(
                        thread = %thread_name,
                        error = %e,
                        "Failed to process message"
                    );
                }
                
                // Clear current message after processing
                let _ = current_message_tx.send(None);
            }

            tracing::info!(thread = %thread_name, "Worker finished");
            // _permit dropped here → semaphore slot freed
        })
    }
}
```

**Key properties:**
- **Bounded concurrency**: `Semaphore(3)` — at most 3 threads process messages simultaneously
- **Per-thread ordering**: Each thread's `mpsc::Receiver` ensures FIFO order. Messages arriving during AI processing are injected live into the session (not queued).
- **Back-pressure**: `mpsc::channel(10)` — `try_send` fails when queue is full (message dropped)
- **Graceful shutdown**: `CancellationToken` propagates to all workers and monitors
- **Automatic cleanup**: Worker tasks exit when their mpsc channel closes (all senders dropped) or on cancellation. Semaphore permits are released on `_permit` drop.
- **Thread Event System**: Each thread has isolated event bus and heartbeat control (default 10-minute intervals, configurable via `[heartbeat]` section)

**Thread Event System Integration:**
- **Event Listener with Heartbeat Control**:
  ```rust
  async fn event_listener_with_heartbeat(
      event_bus: ThreadEventBusRef,
      thread_name: String,
      outbound: Arc<dyn OutboundAdapter>,
      heartbeat_config: HeartbeatConfig,
      heartbeat_template: String,
      current_message_rx: watch::Receiver<Option<InboundMessage>>,
  ) {
      let mut receiver = event_bus.subscribe().await;
      let interval = Duration::from_secs(heartbeat_config.interval_secs);
      let min_elapsed = Duration::from_secs(heartbeat_config.min_elapsed_secs);
      let mut heartbeat_timer = tokio::time::interval(interval);
      let mut last_heartbeat_sent: Option<Instant> = None;
      let mut last_processing_state: Option<(u64, String, String)> = None;
      
      loop {
          tokio::select! {
              event = receiver.recv() => {
                  // Update processing state from ProcessingProgress events
                  if let ThreadEvent::ProcessingProgress { elapsed_secs, activity, progress, .. } = event {
                      last_processing_state = Some((elapsed_secs, activity, progress));
                  }
              }
              _ = heartbeat_timer.tick() => {
                  // Check heartbeat conditions and send if met
                  if let Some(message) = current_message_rx.borrow().clone() {
                      if let Some((elapsed_secs, ref activity, ref progress)) = last_processing_state {
                          if Duration::from_secs(elapsed_secs) >= min_elapsed {
                              let should_send = match last_heartbeat_sent {
                                  Some(last_sent) => last_sent.elapsed() >= interval,
                                  None => true,
                              };
                              if should_send {
                                  let formatted = format_heartbeat(&heartbeat_template, elapsed_secs, activity, progress);
                                  outbound.send_heartbeat(&message, &formatted).await;
                                  last_heartbeat_sent = Some(Instant::now());
                              }
                          }
                      }
                  }
              }
          }
      }
  }
  ```
- **Heartbeat Conditions**:
  1. ✅ Current message being processed
  2. ✅ Processing state available (from `ProcessingProgress` events)
  3. ✅ Processing elapsed ≥ `min_elapsed_secs` (default 1 minute)
   4. ✅ Time since last heartbeat ≥ `interval_secs` (default 10 minutes)
- **Thread Isolation**: Each thread maintains independent event bus and heartbeat state

### IMAP Monitor: State Machine

```
┌──────────────────────────────────────────────────────────┐
│                  ImapMonitor State Machine                 │
│                                                           │
│   ┌──────────┐    connect OK    ┌───────────────┐        │
│   │          │─────────────────>│               │        │
│   │ Starting │                  │  Connected    │        │
│   │          │<─────────────────│               │        │
│   └──────────┘    connect fail  └───────┬───────┘        │
│        ▲          (backoff)             │                 │
│        │                                ▼                 │
│        │                     ┌───────────────────┐       │
│        │                     │ check_for_new()   │       │
│        │                     │                   │       │
│        │                     │ count > last_seq? │       │
│        │                     │   YES → fetch new │       │
│        │                     │   NO  → skip      │       │
│        │                     │                   │       │
│        │                     │ count < last_seq? │       │
│        │                     │   YES → RECOVERY  │       │
│        │                     └────────┬──────────┘       │
│        │                              │                  │
│        │                    ┌─────────┴─────────┐        │
│        │                    ▼                   ▼        │
│        │          ┌──────────────┐    ┌──────────────┐   │
│        │          │  IDLE mode   │    │  Poll mode   │   │
│        │          │              │    │              │   │
│        │          │ client.idle()│    │ sleep(30s)   │   │
│        │          │  .await      │    │  .await      │   │
│        │          │              │    │              │   │
│        │          │ new mail     │    │ interval     │   │
│        │          │  notified    │    │  elapsed     │   │
│        │          └──────┬───────┘    └──────┬───────┘   │
│        │                 │                   │           │
│        │                 └─────────┬─────────┘           │
│        │                          │                      │
│        │                          ▼                      │
│        │               loop back to check                │
│        │                                                 │
│        │            ┌──────────────────┐                 │
│        └────────────│  RECOVERY mode   │                 │
│         reconnect + │                  │                 │
│         reprocess   │ load UIDs set    │                 │
│                     │ fetch ALL msgs   │                 │
│                     │ skip processed   │                 │
│                     │ process new only │                 │
│                     └──────────────────┘                 │
│                                                          │
│  CancellationToken → exits loop → disconnect             │
└──────────────────────────────────────────────────────────┘
```

### SSE Streaming: OpenCode AI Processing

```
┌─────────────────────────────────────────────────────────────────────┐
│              OpenCode SSE Stream Processing                          │
│                                                                      │
│  reqwest-eventsource                                                 │
│       │                                                              │
│       ▼                                                              │
│  EventSource::new(request)                                           │
│       │                                                              │
│       ▼                                                              │
│  ┌──────────────────────────────────────────────────────────┐       │
│  │  tokio::select! {                                        │       │
│  │                                                          │       │
│  │    event = sse.next() => {                               │       │
│  │      match event.type:                                   │       │
│  │        "server.connected"    → log, confirm alive        │       │
│  │        "message.updated"     → capture model info,       │       │
│  │                                update reply-context.json  │       │
│  │        "message.part.updated"→ accumulate parts,         │       │
│  │                                detect tool calls,        │       │
│  │                                update last_activity      │       │
│  │        "session.status"      → track busy/retry          │       │
│  │        "session.idle"        → DONE, collect result      │       │
│  │        "session.error"       → handle error:             │       │
│  │                                ContextOverflow → retry   │       │
│  │    }                                                     │       │
│  │                                                          │       │
│  │    new_msg = pending_rx.recv() => {                      │       │
│  │      // Live message injection                           │       │
 │  │      1. Store new message → append to chat log           │       │
│  │      2. Strip quoted history from body                   │       │
│  │      3. Update reply-context.json (new messageDir)       │       │
│  │      4. Send body via prompt_async (follow-up prompt)    │       │
│  │      → AI receives it in same conversation context       │       │
│  │    }                                                     │       │
│  │                                                          │       │
│  │    _ = activity_timeout_check => {                       │       │
│  │      // tokio::time::interval(5s)                        │       │
│  │      if now - last_activity > 30min (60min if tool) {    │       │
│  │        → timeout, break loop                             │       │
│  │      }                                                   │       │
│  │      if now - last_progress_log > 10s {                  │       │
│  │        → log progress                                    │       │
│  │      }                                                   │       │
│  │    }                                                     │       │
│  │                                                          │       │
│  │    _ = cancel.cancelled() => break                       │       │
│  │  }                                                       │       │
│  └──────────────────────────────────────────────────────────┘       │
│                                                                      │
│  Post-SSE checks:                                                    │
│  ┌──────────────────────────────────────────────────────────┐       │
│  │  1. Check accumulated parts for reply_message tool call  │       │
│  │  2. Check signal file (.jyc/reply-sent.flag)            │       │
│  │  3. Stale session detection (tool reported success but   │       │
│  │     signal file missing → delete session → retry once)   │       │
│  │  4. Fallback: if tool not used → direct send via adapter │       │
│  └──────────────────────────────────────────────────────────┘       │
└─────────────────────────────────────────────────────────────────────┘
```

**Thread Event Integration with SSE:**
- **SSE Event Conversion**: OpenCode Client converts SSE events to ThreadEvents
- **Event Types Converted**:
  - `ProcessingStarted` → `ThreadEvent::ProcessingStarted`
  - `ProcessingProgress` → `ThreadEvent::ProcessingProgress`
  - `ProcessingCompleted` → `ThreadEvent::ProcessingCompleted`
  - `ToolStarted` → `ThreadEvent::ToolStarted`
  - `ToolCompleted` → `ThreadEvent::ToolCompleted`
  - `server.heartbeat` → ignored (connection keep-alive only)
- **Event Publishing**: Events are published to thread-isolated event bus
- **Thread Manager Monitoring**: Listens for events and controls heartbeat rhythm

### Alert Service: Event-Driven Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                      Alert Service                                │
│                                                                   │
│  ┌───────────────┐                                               │
│  │  AppLogger    │  (unified logging + alerting handle)           │
│  │               │                                               │
│  │ .error() ─────┼──> tracing::error!() + mpsc::Sender<Event>   │
│  │ .info()  ─────┼──> tracing::info!()                           │
│  │ .reply_by_tool()──> tracing + mpsc (health stats)             │
│  └───────────────┘         │                                     │
│                            ▼                                     │
│              ┌─────────────────────────┐                         │
│              │  Alert Service Task     │                         │
│              │  (tokio::spawn)         │                         │
│              │  span: alert            │                         │
│              │                         │                         │
│              │  tokio::select! {       │                         │
│              │    event = rx.recv() => │                         │
│              │      match event:       │                         │
│              │        Error →          │                         │
│              │          buffer_error() │                         │
│              │        MessageReceived →│                         │
│              │          track_stats()  │                         │
│              │        ReplyByTool →    │                         │
│              │          track_stats()  │                         │
│              │                         │                         │
│              │    _ = flush_tick =>    │                         │
│              │      flush_errors()    │                         │
│              │      → send digest     │                         │
│              │                         │                         │
│              │    _ = health_tick =>   │                         │
│              │      send_health()     │                         │
│              │      → send report     │                         │
│              │                         │                         │
│              │    _ = cancel =>        │                         │
│              │      final_flush()     │                         │
│              │      break             │                         │
│              │  }                      │                         │
│              └─────────────────────────┘                         │
│                                                                   │
│  AppLogger sends structured AlertEvent variants via mpsc.        │
│  Self-protection: send failures use eprintln (not tracing).       │
└──────────────────────────────────────────────────────────────────┘
```

### Graceful Shutdown Sequence

```
Signal (SIGINT/SIGTERM)
       │
       ▼
 tokio::signal::ctrl_c()
       │
       ▼
 CancellationToken::cancel()
       │
       ├──> IMAP Monitors: exit IDLE/poll loop → disconnect
       │
       ├──> ThreadManager workers: finish current message → exit
       │    (in-queue messages are lost — IMAP re-fetch on restart)
       │
       ├──> Alert Service: final flush → send pending errors → exit
       │
       ├──> OpenCode Server: explicitly stopped via server.stop()
       │
       └──> SMTP connections: disconnect

 All JoinHandles awaited → process exits cleanly
```

### OpenCode Server Process Lifecycle on Shutdown

| Scenario | Server killed? | How? |
|----------|---------------|------|
| Ctrl+C (graceful) | Yes | `opencode_server.stop()` explicitly kills it during shutdown sequence |
| jyc panics | Yes | `kill_on_drop(true)` on the child process — Rust drop runs during unwind |
| SIGTERM to jyc | Yes | Same as panic — drop destructors run |
| SIGKILL (kill -9) to jyc | **No** — orphan | Destructors don't run. Opencode process stays alive on its ephemeral port. Harmless — next jyc start picks a new port. |

### Cancellation Token Hierarchy

```
root_cancel (top-level)
    │
    ├── imap_monitor_cancel (per channel)
    │       └── signals IMAP IDLE to abort
    │
    ├── thread_manager_cancel
    │       └── all worker tasks check this
    │
    ├── alert_service_cancel
    │       └── triggers final flush
    │
    └── opencode_service_cancel
            └── aborts SSE streams
```

## Thread Manager & Queue

### Per-Thread Queue with Semaphore-Bounded Concurrency

(See the Async Event Queue Architecture section above for the full `ThreadManager` design with code.)

**Key properties:**
- **Inbound channels run as concurrent tokio tasks** — Email monitor and FeiShu monitor listen simultaneously
- **Fire-and-forget enqueue** — MessageRouter sends into mpsc and returns immediately
- **Each thread has its own mpsc channel** — FIFO order preserved within a conversation
- **One worker per thread** — Sequential processing (order matters for conversation coherence)
- **Different threads process in parallel** — Up to `max_concurrent_threads` (default: 3) via `Semaphore`
- **In-memory queues** — Lost on restart; IMAP re-fetch handles recovery
- **Queue overflow** — Messages dropped with warning when mpsc buffer is full

### Live Message Injection

When a user sends a follow-up message while the AI is still processing the first message in the same thread, the follow-up is injected into the ongoing AI session rather than waiting in the queue.

**Behavior:**
- Message 2 arrives while AI processes Message 1 → Message 2 body injected into same session → user gets one combined reply
- Message 2 arrives after AI finished Message 1 → normal sequential processing → two separate replies

**How it works:**
1. The worker passes its queue receiver (`rx`) to `agent.process()` during processing
2. The agent passes `rx` through to the SSE streaming loop (`prompt_with_sse()`)
3. The SSE `tokio::select!` loop monitors `rx.recv()` alongside SSE events and timeout checks
4. When a new message arrives during streaming:
   - Append the new message to the daily chat log (`chat_history_YYYY-MM-DD.md`)
   - Process commands from the new message (e.g., `/model` switch)
   - Strip quoted history from the body
   - Update `.jyc/reply-context.json` with the new `incomingMessageDir`
   - Send the body as a follow-up prompt via `POST /session/:id/prompt_async`
5. The AI receives the follow-up in the same conversation context and adjusts its work

**Injection format:** Raw body only — no framing, no instructions. This matches how the OpenCode TUI handles messages sent during AI processing. The AI treats it as a natural follow-up in the conversation.

```
Please also add a chart to the PPT.
```

**OpenCode API support:** `POST /session/:id/prompt_async` can be called while a session is busy. OpenCode queues the message internally — this is the same mechanism the OpenCode TUI uses.

## Worker (OpenCode Service)

### Responsibility Separation: ThreadManager vs AgentService vs OutboundAdapter

The processing pipeline is split into three layers with distinct responsibilities:

**`AgentService` trait** (`src/services/agent.rs`) — Channel-agnostic AI brain:
```rust
#[async_trait]
pub trait AgentService: Send + Sync {
    async fn process(
        &self, message: &InboundMessage, thread_name: &str,
        thread_path: &Path, message_dir: &str,
        pending_rx: &mut mpsc::Receiver<QueueItem>,
    ) -> Result<AgentResult>;
}

pub struct AgentResult {
    pub reply_sent_by_tool: bool,   // MCP tool already handled full lifecycle
    pub reply_text: Option<String>, // Raw AI text for outbound adapter
}
```
Each agent mode implements this trait. Adding a new agent requires only implementing `AgentService` + a match arm in `cli/monitor.rs`.

**ThreadManager** (`src/core/thread_manager.rs`) — Orchestrator:
- Queue management: per-thread mpsc channels, semaphore-bounded concurrency
- Message storage: append to daily chat log (`chat_history_YYYY-MM-DD.md`), save attachments
- Command processing: parse/execute/strip email commands, send command results
- Agent dispatch: calls `agent.process()` via `Arc<dyn AgentService>`
- Fallback: passes raw AI text to outbound adapter if MCP tool wasn't used
- Does NOT know about: sessions, prompts, SSE, reply formatting, email quoting

**OutboundAdapter** (`src/channels/email/outbound.rs`) — Channel-specific reply lifecycle:
- Builds channel-formatted reply (email: `build_full_reply_text()` with quoted history)
- Sends via channel transport (SMTP with threading headers + attachments)
 - Appends reply to chat log
- Different channels (FeiShu, Slack) would implement different formatting + transport

**OpenCodeService** (`src/services/opencode/service.rs`) implements `AgentService`:
- Server lifecycle: ensure OpenCode server is running, health check, auto-restart
- Thread setup: write per-thread `opencode.json` with model, MCP tools, permissions
- Session management: reuse/create sessions, staleness detection
- Prompt building: system prompt + user prompt 
- SSE streaming: activity timeout, tool detection, progress logging
- Error recovery: ContextOverflow → new session, stale session → retry
- Returns raw AI text — does NOT format, send, or store replies

**StaticAgentService** (`src/services/static_agent.rs`) implements `AgentService`:
- Returns configured static text — does NOT format, send, or store

```rust
// ThreadManager dispatches to agent, then outbound:
let result = agent.process(&message, thread_name, thread_path, message_dir, &mut rx).await?;

if !result.reply_sent_by_tool {
    if let Some(ref text) = result.reply_text {
        // Outbound adapter owns: format + send + store
        outbound.send_reply(&message, text, thread_path, message_dir, None).await?;
    }
}
```

This separation:
- **Agent** is channel-agnostic — returns raw text, no email/FeiShu knowledge
- **OutboundAdapter** owns the full reply lifecycle — format + send + store
- **ThreadManager** is a thin orchestrator — dispatch to agent, pass result to outbound
- Adding a new channel requires only a new OutboundAdapter implementation
- Adding a new AI backend requires only a new AgentService implementation

### Session-Based Thread Management

Each thread has a dedicated OpenCode session persisted in `opencode-session.json`. This enables:
- **Memory** — AI remembers previous replies in the conversation
- **Coherence** — Consistent responses across the thread
- **Context** — Conversation history maintained by OpenCode session memory (not injected into prompt)
- **Debugging** — Can inspect/replay sessions in OpenCode TUI

### OpenCode Service Architecture

```rust
pub struct OpenCodeService {
    /// HTTP client for OpenCode API
    client: reqwest::Client,

    /// OpenCode server process
    server: Mutex<Option<OpenCodeServer>>,

    /// Configuration
    config: Arc<AppConfig>,

    /// Binary path for the reply tool subcommand
    reply_tool_command: Vec<String>,
}

struct OpenCodeServer {
    port: u16,
    process: Child,  // tokio::process::Child
}
```

**Server lifecycle:**
- Single shared OpenCode server handles all threads
- Started via `opencode serve --hostname=127.0.0.1 --port=<port>`
- Readiness detected by parsing stdout for `"opencode server listening on http://..."`
- Auto-started on first request, auto-finds free port (49152+)
- Health check before reuse: `GET /session` with 3s timeout
- Server lives until CLI exits

### OpenCode Server HTTP API Reference

Full API documentation: **https://opencode.ai/docs/server/**

JYC uses the following subset of the OpenCode server API:

| Method | Path | Purpose | Notes |
|--------|------|---------|-------|
| `GET` | `/session` | Health check / list sessions | Used for liveness probe |
| `POST` | `/session` | Create a new session | Body: `{ title }` |
| `GET` | `/session/:id` | Get session details | Returns 404 if not found |
| `POST` | `/session/:id/prompt_async` | Send prompt asynchronously | Body: `{ system, agent?, parts }`. Returns 204. |
| `POST` | `/session/:id/message` | Send prompt and wait (blocking) | Same body format. Returns `{ info, parts }`. |
| `GET` | `/event` | SSE event stream (global) | First event: `server.connected` |

**Key API conventions:**
- **Directory context**: Passed via `x-opencode-directory` HTTP header (URL-encoded path), NOT as a query parameter
- **Model selection**: Passed per-prompt via `PromptRequest.model` — session is preserved across model switches
- **Prompt body**: `{ system: string, model?: string, agent?: "plan", parts: [{ type: "text", text: string }] }`
- **SSE events**: Event type is in the JSON data field as `{ "type": "...", "properties": {...} }` — NOT in the SSE `event:` field

**SSE event types used:**

| Event Type | Purpose | Key Fields |
|------------|---------|------------|
| `server.connected` | Stream handshake | — |
| `message.updated` | Model info | `properties.info.{ sessionID, modelID, providerID }` |
| `message.part.updated` | Content/tool updates | `properties.part.{ id, sessionID, type, text, tool, state }` |
| `session.status` | Processing status | `properties.{ sessionID, status.type }` |
| `session.idle` | Prompt complete | `properties.sessionID` |
| `session.error` | Session error | `properties.{ sessionID, error.name }` |
| `step.finish` | Step completion with token counts | `properties.step.{ id, sessionID, cost, inputTokens, outputTokens, reason }` |

**Per-thread configuration:**
- Each thread gets its own `opencode.json` with model settings, MCP tool config, and permissions
- `permission: { "*": "allow", "question": "deny", "external_directory": "deny" }` — headless mode, no interactive terminal, no access outside thread directory
- Staleness check detects changes → rewrites config → restarts server
- Model and mode are passed per-prompt via `PromptRequest.model` and `PromptRequest.agent` — no session restart needed for switches

### OpenCode Server Architecture Diagram

```
┌─────────────────────────────────────────────────────────────┐
│                    OpenCodeService                          │
│                                                             │
│  Single Server (auto-port: 49152+)                          │
│       ↓                                                     │
│  Shared reqwest::Client                                     │
│       ↓                                                     │
│  ┌─────────────────────────────────────┐                    │
│  │ Sessions (per-thread directory)     │                    │
│  │                                     │                    │
│  │ Thread A → opencode-session.json + .opencode/│                    │
│  │ Thread B → opencode-session.json + .opencode/│                    │
│  │ Thread C → opencode-session.json + .opencode/│                    │
│  └─────────────────────────────────────┘                    │
│                                                             │
│  Server lives until CLI exits                               │
└─────────────────────────────────────────────────────────────┘
```

### Worker Processing Flow

```
┌─ ThreadManager (src/core/thread_manager.rs) ─────────────────────────┐
│                                                                       │
│  Worker picks message from thread queue                               │
│         │                                                             │
│         ▼                                                             │
│  ┌──────────────────────────────────────────┐                         │
│  │ 1. STORE                                 │                         │
│  │    MessageStorage::store(msg, thread)     │                         │
│  │    → chat_history_YYYY-MM-DD.md (appended)  │                         │
│  │    → save attachments (allowlisted)       │                         │
│  └──────────────┬───────────────────────────┘                         │
│                 │                                                     │
│                 ▼                                                     │
│  ┌──────────────────────────────────────────┐                         │
│  │ 2. COMMAND PROCESS                       │                         │
│  │    CommandRegistry::process_commands()    │                         │
│  │    → parse /model, /plan, /build          │                         │
│  │    → execute each command                 │                         │
│  │    → strip command lines from body        │                         │
│  │    → strip quoted history from cleaned    │                         │
│  └──────────────┬───────────────────────────┘                         │
│                 │                                                     │
│          commands found?                                              │
│           ╱          ╲                                                │
│         YES          NO                                               │
│          │            │                                               │
│          ▼            │                                               │
│  ┌──────────────────┐ │                                               │
│  │ 3. REPLY RESULTS │ │                                               │
│  │    Direct reply   │ │                                               │
│  │    with command   │ │                                               │
│  │    results        │ │                                               │
│  │    (always sent)  │ │                                               │
│  └────────┬─────────┘ │                                               │
│           │            │                                               │
│           ▼            ▼                                               │
│  ┌──────────────────────────────────────────┐                         │
│  │ 4. CHECK BODY                            │                         │
│  │    cleaned_body (after commands +         │                         │
│  │    quoted history stripped) empty?         │                         │
│  └──────────────┬───────────────────────────┘                         │
│           ╱          ╲                                                │
│        EMPTY      HAS CONTENT                                         │
│          │            │                                               │
│          ▼            ▼                                               │
│  ┌──────────┐  ┌──────────────────────────────────────────┐          │
│  │ STOP     │  │ 5. DISPATCH TO AGENT                     │          │
│  │ (no AI)  │  │    agent.process(pending_rx) → AgentResult  │          │
│  │ return   │  │                                          │          │
│  └──────────┘  │ 6. HANDLE RESULT                         │          │
│                │    If reply_sent_by_tool → done           │          │
│                │    If reply_text → pass to outbound:      │          │
│                │      outbound.send_reply(raw_text)        │          │
│                │      (outbound formats + sends + stores)  │          │
│                └──────────────────────────────────────────┘  │      │
│                                                              │      │
│  Worker picks next message from thread queue                  │      │
└───────────────────────────────────────────────────────────────┘      │
                                                                       │
┌─ OpenCodeService (src/services/opencode/service.rs) ─────────────────┘
│
│  1. Ensure OpenCode server is running (auto-start, health check)
│  2. ensure_thread_opencode_setup(thread_path)
│     → reads .jyc/model-override (if exists, takes priority over config)
│     → writes opencode.json with model, MCP config, permissions
│     → staleness check: skip write if unchanged
│  3. Get or create session (.jyc/opencode-session.json)
│     - Check token limit: if total_input_tokens > max_input_tokens → new session
│     - Update max_input_tokens: detect model context or use configured value
│     - Record if session reset due to token limit for prompt notification
│  4. Clean up stale signal file
│  5. Build system prompt (config + directory boundaries + system.md)
│     BUILD MODE prompt categorizes incoming messages:
│       - Information questions → use bash curl directly
│       - Coding tasks → use tools to edit files
│       - General conversation → reply from knowledge
│  6. Build user prompt (stripped body )
│  7. Check mode override (plan/build from .jyc/mode-override)
│         ↓
│  prompt_with_sse() (SSE streaming):
│    1. Subscribe to SSE events ({ directory: thread_path })
│    2. Fire prompt_async() (returns immediately)
│    3. Process events (filtered by session_id, deduped):
│        - server.connected → confirm SSE stream alive
│        - message.updated → capture model_id/provider_id, log model
│        - message.part.updated → accumulate parts, detect tool calls
│        - step.finish → track input/output tokens, persist to session state
│        - session.status → track busy/retry (deduped)
│        - session.idle → done, collect result
│        - session.error → handle (ContextOverflow → new session + retry)
│    4. Activity-based timeout: 30 min of silence (60 min when tool running)
│    5. Progress log every 10s (elapsed, parts, model, activity, silence)
│         ↓
│  OpenCode calls reply_message MCP tool
│         ↓
│  MCP Tool (jyc mcp-reply-tool subprocess):
│    1. Load .jyc/reply-context.json → get channel name + message timestamp
│    2. Load config from JYC_ROOT/config.toml
│    3. Write reply text to .jyc/reply.md
│    4. Write .jyc/reply-sent.flag (signal file)
│         ↓
│  Monitor detects signal file:
│    1. Read .jyc/reply.md
│    2. Build full_reply_text = AI reply + quoted history (email only)
│    3. Send via pre-warmed outbound adapter (eliminates cold-start timeouts)
│    4. MessageStorage::store_reply() → append to chat log
│         ↓
│  Handle result → return GenerateReplyResult:
│    - reply_sent_by_tool: true (SSE tool detection OR signal file) → done
│    - Stale session (tool reported success, signal file missing)
│        → delete session, create new, retry once
│    - ContextOverflow → new session + blocking retry
│    - SSE failure → blocking prompt fallback
│    - No tool used → return reply_text for ThreadManager fallback
│
└─ Returns GenerateReplyResult to ThreadManager ──────────────────────────
```

**Key flow rules:**
1. **Commands are always processed first** — before any AI interaction
2. **Command results are always sent** as a direct reply (if commands were found)
3. **Body emptiness is checked AFTER both command stripping AND quoted history stripping** — leftover quoted history from inline reply formats does not count as real content
4. **Empty body → stop** — no OpenCode server started, no AI processing, no wasted API calls
5. **Non-empty body → dispatch to agent mode** — static text or OpenCode AI

**Session lifecycle:**
- Sessions are created on first use per thread and persisted in `.jyc/opencode-session.json`
- Sessions are reused across messages, model switches, mode switches, and container restarts
- Sessions track input tokens (`total_input_tokens`) and maximum threshold (`max_input_tokens`)
- Sessions are automatically reset when token limit is exceeded
- Sessions are deleted for error recovery (ContextOverflow, stale session detection)
- On session reset: AI prompt includes notification and reference to chat history

### Context Management Strategy

The agent relies on OpenCode's built-in session memory for multi-turn conversation context. JYC does NOT inject conversation history into the prompt.

1. **OpenCode Session (Primary)** — Conversation memory maintained by OpenCode
    - Session is reused across messages in the same thread (`opencode-session.json`)
    - AI remembers previous messages and replies within the session
    - Session is deleted when token limit is exceeded or on ContextOverflow
    - New session created on server restart

 2. **Token-based Session Management** — Automatic session reset based on input tokens
    - **Token-based reset**:
      - Session accumulates input tokens (`total_input_tokens`) from each AI processing step
      - When accumulated tokens exceed `max_input_tokens` threshold, session is automatically reset
      - Old session is deleted and a new session is created
    - **Token tracking**:
      - Real-time token counting from SSE `step-finish` events
      - Immediate persistence to `opencode-session.json` after each step
      - Token usage displayed in reply footer (e.g., `Tokens: 20.7K/122K`)
    - **Intelligent threshold detection**:
      - Automatically detects model context limit (e.g., 128K, 200K, 1M tokens)
      - Uses 95% of model context as default threshold for safety margin
      - Configurable via `max_input_tokens` setting in `config.toml`
    - **Standardized units**:
      - Uses 1024 as K unit basis (1K = 1024 tokens)
      - Default threshold: 122,880 tokens (120K * 1024)
      - Display format: `{current}K/{max}K` with 0.1K precision
    - **Context preservation**:
      - Chat history is preserved in `chat_history_YYYY-MM-DD.md` files
      - AI can read chat history for context continuity after session reset
      - Session reset notification injected into AI prompt to reference chat history

 3. **Session State Data Structure** (`src/services/opencode/session.rs`)
    ```rust
    /// Default maximum input tokens per session before resetting
    pub const DEFAULT_MAX_INPUT_TOKENS: u64 = 120 * 1024; // 122,880 tokens (120K)

    /// Per-thread session state, persisted in `.jyc/opencode-session.json`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SessionState {
        #[serde(rename = "sessionId")]
        pub session_id: String,
        #[serde(rename = "createdAt")]
        pub created_at: String,
        #[serde(rename = "lastUsedAt")]
        pub last_used_at: String,
        /// Current input tokens (from latest step-finish SSE event)
        #[serde(rename = "totalInputTokens", default)]
        pub total_input_tokens: u64,
        /// Resolved max input tokens for this session
        #[serde(rename = "maxInputTokens", default)]
        pub max_input_tokens: u64,
    }
    ```

 4. **Configuration** (`config.toml`):
    ```toml
    [opencode]
    # Optional: Maximum input tokens per session before resetting
    # Default: 120*1024 = 122,880 tokens (95% of typical 128K model context)
    # When not set, automatically detects model context limit and uses 95%
    max_input_tokens = 122880
    ```

 5. **User Interface Enhancements**:
    - **Reply footer display**: Shows current token usage and threshold
      - Format: `Model: ark/deepseek-v3.2 | Mode: build | Tokens: 20.7K/122K`
      - Current tokens: Formatted in K units (1024 basis) with 0.1 precision
      - Max tokens: Shows actual reset threshold (model context 95% or configured value)
    - **Session reset notification**: When token limit is exceeded:
      - AI prompt includes notification about session reset
      - References `chat_history_<date>.md` for previous conversation context
      - Clear indication that this is a new session with fresh token counter

 6. **Incoming Message (Current)** — Latest message being processed
    - Body stripped of quoted reply history (`strip_quoted_history()`)
    - Topic cleaned of repeated Reply/Fwd prefixes (at ingest time by InboundAdapter)
    - Limited to 2,000 chars

 7. **Thread Files (Durable, for quoted history only)** — Markdown files stored in thread folder
    - Used by `build_full_reply_text()` for quoted history in reply emails
    - NOT loaded into the AI prompt

 **Context Limits:**
pub const MAX_BODY_IN_PROMPT: usize = 2000;
```

### ContextOverflow Recovery

```
AI Prompt → ContextOverflowError (detected via SSE session.error)
    ↓
Log warning with old session_id
    ↓
Create new session (clears history)
    ↓
Retry prompt with new session (blocking fallback)
    ↓
Thread files still provide recent conversation context
```

### Fallback Behavior

| Scenario | What Happens |
|----------|-------------|
| OpenCode uses `reply_message` tool successfully | Detected via SSE; `reply_sent_by_tool: true`, skips fallback |
| `reply_message` tool fails (e.g. MCP not implemented, invalid JSON) | AI generates text response instead; ThreadManager passes raw text to OutboundAdapter which formats, sends, and stores |
| AI returns text without using tool | `session.idle` fires; ThreadManager passes raw text to OutboundAdapter |
| AI takes very long but keeps working | SSE events keep arriving → no timeout; progress logged every 10s |
| AI goes silent for 30 minutes | Activity timeout (60 min if tool running) → checks signal file → error |
| SSE subscription fails | Falls back to blocking prompt with 5-min timeout |
| OpenCode server dies between messages | Health check detects it, restarts automatically |
| ContextOverflowError | Detected via SSE `session.error` → new session → retry (blocking) |
| Token limit exceeded | Detected at session creation → new session created → AI notified via prompt |
| Thread queue full | Message dropped with warning; IMAP re-fetch recovers on restart |

### Reply Text Building Pipeline

`build_full_reply_text()` (`src/core/email_parser.rs`) is the **single shared function** for assembling a complete reply email. It is called by:

1. **EmailOutboundAdapter::send_reply()** — the outbound adapter calls it internally when formatting replies (both fallback and command results)
2. **Monitor reply send path** — when the MCP tool writes reply.md and signal file, the monitor reads reply.md and calls `send_reply()` on the pre-warmed outbound adapter (which calls `build_full_reply_text()`)

This ensures all reply emails have the same format regardless of the send path. The MCP reply tool no longer sends messages directly — it only writes reply.md and signal file to disk. The agent (OpenCodeService/StaticAgentService) never calls this function — it's a channel-specific concern owned by the outbound adapter.

**Reply format:**
```
<AI reply text>

---
### Sender Name (2026-03-27 10:00)
> Subject
>
> Current message body (stripped of nested quotes)...

---
### AI Assistant (2026-03-27 09:55)
> Previous AI reply text...

---
### Sender Name (2026-03-27 09:50)
> Subject
>
> Earlier message body (stripped)...
```

**Building pipeline:**

```
build_full_reply_text(reply_text, thread_path, sender, timestamp, topic, body, message_ts)
    │
    ├── prepare_body_for_quoting(thread_path, current_message, max_history)
    │       │
    │       └── Read chat_history_*.md files for conversation context
    │               │
    │               ├── Current incoming message (stripped of quoted history)
    │               │
    │               └── Previous messages and replies from chat log
    │                   (interleaved chronologically, newest first)
    │
    ├── format_quoted_reply(sender, timestamp, subject, body) for each trail entry
    │       → "---\n### Sender (timestamp)\n> Subject\n>\n> Body quoted..."
    │
    └── Combine: "{reply_text}\n\n{quoted_blocks}"
```

**Trail ordering:** Conversation history is read from `chat_history_*.md` files. Messages and replies are interleaved chronologically, with the most recent entries first.

**Prompt echo stripping:** When the AI generates a fallback text response (because the MCP tool failed), it may echo parts of the prompt. `extract_text_from_parts()` strips these markers before building the full reply:
- `## Incoming Message`
- `reply-context.json=`
- `## Conversation history`

### Signal File (`.jyc/reply-sent.flag`)

Cross-process detection mechanism for when the MCP tool sends the reply but tool parts are missing from the prompt response (or the prompt times out).

**Format:** Single-line JSON
```json
{"sent_at":"2026-03-19T13:09:43Z","channel":"email","recipient":"user@example.com","message_id":"<123@smtp>","attachment_count":1}
```

**Lifecycle:**
1. **Cleanup**: Before starting a new prompt, `cleanup_stale_signal_file()` deletes any leftover file
2. Written by MCP reply-tool after successful outbound send
3. Read by `OpenCodeService::check_signal_file()` as fallback detection
4. Deleted immediately after detection to prevent stale signals

### SSE Event Logging

Events from `prompt_with_progress()` are logged with deduplication:
- **Step start**: Logged at INFO with step number and model name
- **Step finish**: Logged at DEBUG with cost, token counts, and reason
- **Tool calls**: Logged at INFO only on status change per part ID (pending → running → completed)
- **Tool input**: reply_message tool args logged at INFO on `running`
- **Tool output**: reply_message output logged at INFO on `completed`
- **Tool errors**: reply_message `completed` with error output logged at ERROR
- **Session status**: Logged at DEBUG only on status type change (avoids duplicates)
- **Progress**: Every 10s at INFO with elapsed time, part count, current activity, silence duration

## MCP Reply Tool

### Architecture: Single Binary, Hidden Subcommand

```
jyc binary
├── jyc monitor            ← main command
├── jyc config init        ← config management
├── jyc config validate
├── jyc state              ← show monitoring state
├── jyc patterns list      ← list patterns (shows all rule fields)
├── jyc dashboard          ← live TUI dashboard (connects to running monitor)
├── jyc mcp-reply-tool     ← hidden subcommand (MCP stdio server)
├── jyc mcp-vision-tool    ← hidden subcommand (vision analysis MCP server)
└── jyc mcp-question-tool  ← hidden subcommand (ask_user MCP server)
                              All MCP tools spawned by OpenCode as subprocesses
```

The reply tool shares types with the main binary (same Rust crate), eliminating the type drift risk of the two-binary TypeScript approach.

### Reply Context File (Disk-Based)

The reply context is saved to `.jyc/reply-context.json` per-thread before the AI prompt is sent. The MCP reply tool reads it from disk — the AI never sees or touches the context.

This replaces the old `reply-context.json=<base64>` approach where context was passed through the AI in the prompt text (prone to corruption by AI models).

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyContext {
    pub channel: String,              // Config channel name (routing key)
    pub thread_name: String,          // Thread directory name
    pub incoming_message_dir: String, // Timestamp identifier (e.g., "2026-03-19_23-02-20")
    pub uid: String,                  // Channel-specific message ID
    pub model: Option<String>,        // AI model used (e.g., "ark/deepseek-v3.2")
    pub mode: Option<String>,         // AI mode used (e.g., "build", "plan")
    pub created_at: String,           // When context was created
}

/// Save to .jyc/reply-context.json (called by OpenCodeService before prompt)
pub async fn save_reply_context(thread_path: &Path, ctx: &ReplyContext) -> Result<()>

/// Load from .jyc/reply-context.json (called by MCP reply tool from cwd)
pub async fn load_reply_context(thread_path: &Path) -> Result<ReplyContext>

/// Delete reply context file (used for tests and manual cleanup)
pub async fn cleanup_reply_context(thread_path: &Path)
```

**Lifecycle:**
1. `OpenCodeService` saves `.jyc/reply-context.json` before sending the prompt
2. AI calls `reply_message(message, attachments)` — no token parameter
3. MCP reply tool reads `.jyc/reply-context.json` from cwd (= thread directory)
4. After successful send, context file persists (not deleted) to allow multiple replies in same thread
5. Context file is overwritten on each new incoming message
6. `cleanup_reply_context()` is only used for tests and manual cleanup operations

**Why disk-based?** Zero corruption risk — the context never passes through the AI. The AI only receives the prompt text (incoming message body). All routing and metadata is on disk.

### MCP Tool: `reply_message`

```
MCP Server (rmcp, stdio transport, cwd = thread dir):
  Tool schema: message (string), attachments (string[] optional)

  1. Load .jyc/reply-context.json from cwd → get channel, message timestamp
  2. Load config from JYC_ROOT/config.toml
  3. Validate attachments (exclude .opencode/, .jyc/)
  4. Write reply text to .jyc/reply.md
  5. Write .jyc/reply-sent.flag (signal file)
  6. Return success message
  (Monitor process reads .jyc/reply.md and sends via pre-warmed outbound adapter.
   This eliminates cold-start timeouts for Feishu API calls.)
```

### Historical Message Quoting (Thread Trail)

`build_full_reply_text()` builds the reply with quoted history from the thread's `chat_history_*.md` files.

- **Chronological ordering**: Messages and replies are read from chat log files, ordered newest first.
- **Stripped bodies**: Received messages stripped of quoted history via `strip_quoted_history()`. Reply messages parsed to extract only the AI's response text.
- **Per-entry truncation**: Each quoted history entry is capped at 1024 characters (`MAX_QUOTED_BODY_CHARS`)
- **Limit**: `MAX_HISTORY_QUOTE = 6` entries for reply email quoted history
- **Timestamp format**: `YYYY-MM-DD HH:MM` in both quoted history headers and prompt context

### Per-Thread OpenCode Config (`opencode.json`)

Written by `ensure_thread_opencode_setup()` in each thread directory:

```json
{
  "$schema": "https://opencode.ai/config.json",
  "model": "SiliconFlow/Pro/zai-org/GLM-4.7",
  "small_model": "SiliconFlow/Qwen/Qwen2.5-7B-Instruct",
  "permission": {
    "*": "allow",
    "question": "deny",
    "external_directory": "deny"
  },
  "mcp": {
    "jiny_reply": {
      "type": "local",
      "command": ["/path/to/jyc", "mcp-reply-tool"],
      "environment": { "JYC_ROOT": "<root-dir>" },
      "enabled": true,
      "timeout": 60000
    }
  }
}
```

**Tool command resolution** (`get_reply_tool_command()`):
1. Use `std::env::current_exe()` to get current binary path
2. Return `["/path/to/jyc", "mcp-reply-tool"]`
3. Fallback: check common paths `/usr/local/bin/jyc`, `/usr/bin/jyc`

**Staleness check**: Rewrites `opencode.json` if model, tool path, JYC_ROOT, or permissions changed. Session is NOT deleted — model and mode are passed per-prompt.

## Configuration (TOML)

### Fresh Design

JYC uses TOML for configuration, taking advantage of TOML's native support for nested tables and inline comments. Environment variable substitution is supported via `${VAR}` syntax in string values.

```toml
# JYC Configuration

[general]
max_concurrent_threads = 3
max_queue_size_per_thread = 10

# --- Channels ---

[channels.work]
type = "email"

[channels.work.inbound]
host = "imap.company.com"
port = 993
tls = true
auth_timeout_ms = 30000
username = "me@company.com"
password = "${IMAP_PASSWORD}"

[channels.work.outbound]
host = "smtp.company.com"
port = 465
secure = true
username = "me@company.com"
password = "${SMTP_PASSWORD}"

[channels.work.monitor]
mode = "idle"                      # "idle" | "poll"
poll_interval_secs = 30
max_retries = 5
folder = "INBOX"

[[channels.work.patterns]]
name = "support"
enabled = true

[channels.work.patterns.rules.sender]
exact = ["kingye@petalmail.com"]

[channels.work.patterns.rules.subject]
prefix = ["jiny"]

[channels.work.patterns.attachments]
enabled = true
allowed_extensions = [".pdf", ".pptx", ".docx", ".xlsx", ".png", ".jpg", ".txt", ".md"]
max_file_size = "25mb"
max_per_message = 10

# --- Agent ---

[agent]
enabled = true
mode = "opencode"

[agent.opencode]
model = "SiliconFlow/Pro/zai-org/GLM-4.7"
small_model = "SiliconFlow/Qwen/Qwen2.5-7B-Instruct"
system_prompt = "You are an AI assistant. Respond professionally and concisely."



[agent.attachments]
enabled = true
max_file_size = "10mb"
allowed_extensions = [".ppt", ".pptx", ".doc", ".docx", ".txt", ".md"]

# --- Heartbeat ---

[heartbeat]
enabled = true
interval_secs = 600                # Default: 10 minutes (avoids SMTP rate limits)
min_elapsed_secs = 60              # Default: 1 minute before first heartbeat

# --- Alerting ---

[alerting]
enabled = true
recipient = "ops@example.com"
batch_interval_minutes = 5
max_errors_per_batch = 50
subject_prefix = "JYC Alert"
include_reply_tool_log = true
reply_tool_log_tail_lines = 50

[alerting.health_check]
enabled = true
interval_hours = 6
```

### Config Structs

```rust
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub general: GeneralConfig,
    pub channels: HashMap<String, ChannelConfig>,
    pub agent: AgentConfig,
    pub heartbeat: Option<HeartbeatConfig>,
    pub inspect: Option<InspectConfig>,                   // Inspect server config
    pub attachments: Option<UnifiedAttachmentConfig>,  // Global attachment config
    pub vision: Option<VisionConfig>,                  // Vision API config
}

#[derive(Debug, Deserialize)]
pub struct GeneralConfig {
    #[serde(default = "default_3")]
    pub max_concurrent_threads: usize,
    #[serde(default = "default_10")]
    pub max_queue_size_per_thread: usize,
}

#[derive(Debug, Deserialize)]
pub struct ChannelConfig {
    #[serde(rename = "type")]
    pub channel_type: String,                          // "email", "feishu", "github"
    pub inbound: Option<ImapConfig>,                   // Email inbound
    pub outbound: Option<SmtpConfig>,                  // Email outbound
    pub monitor: Option<MonitorConfig>,                // Email IMAP monitor
    pub feishu: Option<FeishuConfig>,                  // Feishu channel config
    pub github: Option<GithubConfig>,                  // GitHub channel config
    pub patterns: Option<Vec<ChannelPattern>>,
    pub agent: Option<AgentConfig>,                    // Channel-specific override
    pub heartbeat_template: Option<String>,            // Per-channel heartbeat message template
}

#[derive(Debug, Deserialize)]
pub struct ImapConfig {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_true")]
    pub tls: bool,
    pub auth_timeout_ms: Option<u64>,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct SmtpConfig {
    pub host: String,
    pub port: u16,
    #[serde(default = "default_true")]
    pub secure: bool,
    pub username: String,
    pub password: String,
}

#[derive(Debug, Deserialize)]
pub struct MonitorConfig {
    #[serde(default = "default_idle")]
    pub mode: String,                         // "idle" | "poll"
    #[serde(default = "default_30")]
    pub poll_interval_secs: u64,
    #[serde(default = "default_5")]
    pub max_retries: usize,
    #[serde(default = "default_inbox")]
    pub folder: String,
}

#[derive(Debug, Deserialize)]
pub struct AgentConfig {
    pub enabled: bool,
    pub mode: String,                         // "static" | "opencode"
    pub text: Option<String>,
    pub opencode: Option<OpenCodeConfig>,
    pub attachments: Option<AttachmentConfig>,
}

#[derive(Debug, Deserialize)]
pub struct OpenCodeConfig {
    pub model: Option<String>,
    pub small_model: Option<String>,
    pub system_prompt: Option<String>,
    // Note: include_thread_history is deprecated — conversation history
    // is no longer injected into the prompt. OpenCode session memory handles it.
}

#[derive(Debug, Deserialize)]
pub struct AlertingConfig {
    pub enabled: bool,
    pub recipient: String,
    #[serde(default = "default_5")]
    pub batch_interval_minutes: u64,
    #[serde(default = "default_50")]
    pub max_errors_per_batch: usize,
    pub subject_prefix: Option<String>,
    #[serde(default = "default_true")]
    pub include_reply_tool_log: bool,
    #[serde(default = "default_50")]
    pub reply_tool_log_tail_lines: usize,
    pub health_check: Option<HealthCheckConfig>,
}

#[derive(Debug, Deserialize)]
pub struct HealthCheckConfig {
    pub enabled: bool,
    #[serde(default = "default_24")]
    pub interval_hours: f64,
    pub recipient: Option<String>,            // Falls back to alerting.recipient
}

/// Heartbeat configuration — controls progress updates during long AI processing.
/// Configurable via [heartbeat] TOML section. Defaults: 10min interval, 60s min elapsed.
#[derive(Debug, Clone, Deserialize)]
pub struct HeartbeatConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_600")]
    pub interval_secs: u64,                   // Default: 600 (10 minutes)
    #[serde(default = "default_60")]
    pub min_elapsed_secs: u64,                // Default: 60 (1 minute)
}
```

### Environment Variable Substitution

```rust
/// Post-process TOML string values, replacing ${VAR} with env values.
/// Applied after toml::from_str() by walking the Value tree.
fn expand_env_vars(value: &mut toml::Value) {
    match value {
        toml::Value::String(s) => {
            let re = regex::Regex::new(r"\$\{(\w+)\}").unwrap();
            *s = re.replace_all(s, |caps: &regex::Captures| {
                std::env::var(&caps[1]).unwrap_or_default()
            }).to_string();
        }
        toml::Value::Table(t) => {
            for v in t.values_mut() { expand_env_vars(v); }
        }
        toml::Value::Array(a) => {
            for v in a.iter_mut() { expand_env_vars(v); }
        }
        _ => {}
    }
}
```

### State Files (Per-Channel)

```
<channel-name>/
├── .imap/
│   ├── .state.json                  # { last_sequence_number, last_processed_uid, uid_validity }
│   └── .processed-uids.txt         # One UID per line, append-only
```

Each channel manages its own state independently. For email, state tracks IMAP sequence numbers and processed UIDs.

## Directory Structure

### Runtime Data

```
<root-dir>/
├── config.toml                          # Master config (TOML)
├── <channel-name>/                      # Per-channel directory (e.g., "jiny283")
│   ├── .imap/
│   │   ├── .state.json                  # IMAP monitor state
│   │   └── .processed-uids.txt         # One UID per line, append-only
│   └── workspace/                       # Thread workspaces (hardcoded: <workdir>/<channel_name>/workspace/)
│       ├── <thread-dir-1>/              # OpenCode cwd for this thread
│       │   ├── chat_history_2026-03-19.md   # Daily chat log (messages + replies)
│       │   ├── chat_history_2026-03-20.md   # Next day's chat log
│       │   ├── attachments/                 # Saved inbound attachments (if configured)
│       │   │   └── report.pdf
│       │   ├── .jyc/
│       │   │   ├── opencode-session.json         # AI session state
│       │   │   ├── reply-context.json   # Reply routing context (disk-based)
│       │   │   ├── reply-tool.log       # MCP tool log
│       │   │   ├── reply-sent.flag      # Signal file (transient)
│       │   │   ├── model-override       # /model command override
│       │   │   └── mode-override        # /plan command override
│       │   ├── .opencode/               # OpenCode internal
│       │   ├── opencode.json            # Per-thread OpenCode config
│       │   └── system.md                # Optional thread-specific prompt
│       └── <thread-dir-2>/
│           └── ...
└── <channel-2>/
    └── ...
```

## Thread Template

Thread Template allows initializing new threads with predefined files and directories. Templates are defined at the pattern level and applied when a thread is first created.

### Configuration

Templates are configured at the pattern level in `config.toml`:

```toml
[channels.my_channel]
type = "email"

[[channels.my_channel.patterns]]
name = "urgent"
template = "urgent"  # Use templates/urgent/ for this pattern

[[channels.my_channel.patterns]]
name = "normal"
# No template - thread starts empty
```

Template directory structure (in workdir):

```
<root-dir>/
├── templates/
│   ├── urgent/
│   │   ├── agent.md      # OpenCode reads this as thread-specific prompt
│   │   ├── skills/
│   │   │   └── my_skill/
│   │   │       └── SKILL.md
│   │   └── custom_file.txt
│   └── default/
│       └── ...
```

### How It Works

1. **Pattern Matching**: When a message matches a pattern with a `template` field, the template name is stored in the message metadata.

2. **Thread Initialization**: On the first message to a thread, `ThreadManager` copies all files from `templates/{template_name}/` to the thread directory (skipping existing files).

3. **Pattern Tracking**: The pattern name is saved to `.jyc/pattern` for later reference.

4. **`/template` Command**: Users can run `/template` to re-apply the template to the current thread (copies missing files).

### Files Copied

- Template files are copied to the thread root directory (not `.jyc/`)
- Directories are created as needed
- Existing files are **not** overwritten (safe to re-run)

## Inspect Server & TUI Dashboard

### Architecture

```
┌──────────────────────┐         TCP (127.0.0.1:9876)  ┌──────────────────────┐
│   jyc monitor        │  ◄──────────────────────────► │   jyc dashboard      │
│                       │    JSON line protocol          │   (ratatui TUI)      │
│  ┌─────────────────┐ │                                │                      │
│  │ InspectServer    │ │   {"method":"get_state"}      │  polls every 500ms   │
│  │ [inspect] config │ │  ◄──────────────────────────  │  persistent TCP conn │
│  │                  │ │   {channels, threads, stats}   │  auto-reconnect      │
│  │ queries:         │ │  ──────────────────────────►  │                      │
│  │  ThreadManagers  │ │                                └──────────────────────┘
│  │  MetricsCollector│ │
│  └─────────────────┘ │
└──────────────────────┘
```

### Configuration

```toml
[inspect]
enabled = true                      # Default: false
bind = "127.0.0.1:9876"            # Default: 127.0.0.1:9876
```

For Docker (Podman Machine on macOS), use `bind = "0.0.0.0:9876"` and the SSH tunnel script `jyc-podman-tunnel.sh`.

### Protocol

JSON line protocol over TCP. Client sends one JSON object per line, server responds with one JSON object per line.

```json
// Request
{"method": "get_state"}

// Response
{
  "uptime_secs": 3600,
  "version": "0.1.11",
  "channels": [{"name": "emf", "channel_type": "github"}],
  "threads": [{"name": "issue-42", "channel": "emf", "pattern": "planner", "status": "processing", ...}],
  "stats": {"active_workers": 2, "total_threads": 3, "max_concurrent": 3, ...}
}
```

### TUI Dashboard Layout

```
┌─ JYC Dashboard ──────────────────────────────────────────────────┐
│ ┌─ Channels ──────────────────────────────────────────────────┐  │
│ │ ● emf (github)     ● networkcalc (github)                   │  │
│ └─────────────────────────────────────────────────────────────┘  │
│ ┌─ Threads (4) ───────────────────────────────────────────────┐  │
│ │ Thread          Channel     Pattern     Status     Tokens   │  │
│ │ issue-42        emf         planner     Processing 45K/120K │  │
│ │ pr-43           emf         developer   Idle       12K/120K │  │
│ │ review-pr-43    emf         reviewer    Queued     -        │  │
│ │ issue-55        networkcalc planner     Processing 89K/120K │  │
│ └─────────────────────────────────────────────────────────────┘  │
│ ┌─ Details: issue-42 ─────────────────────────────────────────┐  │
│ │ Channel: emf  Pattern: planner                               │  │
│ │ Model: anthropic/claude-opus-4-6  Mode: build                    │  │
│ │ Tokens: 45231 / 120000 (37%)                                 │  │
│ │ Status: Processing                                           │  │
│ └─────────────────────────────────────────────────────────────┘  │
│ 2 active / 4 threads │ 156 recv │ 2 err │ up 1h03m │ v0.1.11   │
└──────────────────────────────────────────────────────────────────┘
```

Key bindings: `q`/`Esc` quit, `↑`/`↓`/`j`/`k` select thread, `r` force refresh.

### MetricsCollector

Replaces the old `AlertService`. Components report events via `MetricsHandle`:

- `message_received(thread)`, `message_matched(thread)`
- `reply_by_tool(thread)`, `reply_by_fallback(thread)`
- `processing_error(thread, error)`, `queue_dropped(thread)`

Stats are accumulated in `Arc<Mutex<HealthStats>>`, queryable by the inspect server. No email dependency.

### Source Tree

```
jyc/
├── Cargo.toml
├── DESIGN.md
├── IMPLEMENTATION.md
├── src/
│   ├── main.rs                          # Entry point, clap CLI
│   ├── cli/
│   │   ├── mod.rs
│   │   ├── monitor.rs                   # `jyc monitor` — wiring
│   │   ├── dashboard.rs                 # `jyc dashboard` — ratatui TUI
│   │   ├── config.rs                    # `jyc config init/validate`
│   │   ├── patterns.rs                  # `jyc patterns list` (all rule fields)
│   │   ├── state.rs                     # `jyc state`
│   │   ├── mcp_reply.rs                 # `jyc mcp-reply-tool` (hidden)
│   │   ├── mcp_vision.rs               # `jyc mcp-vision-tool` (hidden)
│   │   └── mcp_question.rs             # `jyc mcp-question-tool` (hidden)
│   ├── config/
│   │   ├── mod.rs
│   │   ├── types.rs                     # Config structs (serde + toml)
│   │   └── validation.rs               # Config validation
│   ├── channels/
│   │   ├── mod.rs
│   │   ├── types.rs                     # InboundMessage, traits, patterns
│   │   ├── registry.rs                  # ChannelRegistry
│   │   ├── email/
│   │   │   ├── mod.rs
│   │   │   ├── inbound.rs              # EmailInboundAdapter
│   │   │   └── outbound.rs             # EmailOutboundAdapter
│   │   ├── feishu/
│   │   │   ├── mod.rs
│   │   │   ├── client.rs               # Feishu API client (auth, token mgmt)
│   │   │   ├── config.rs               # Feishu-specific config
│   │   │   ├── inbound.rs              # FeishuInboundAdapter (WebSocket)
│   │   │   ├── outbound.rs             # FeishuOutboundAdapter (API)
│   │   │   ├── websocket.rs            # LarkWsClient (WebSocket connection)
│   │   │   ├── types.rs                # Feishu event/message types
│   │   │   ├── formatter.rs            # Message formatting (markdown/text)
│   │   │   └── validator.rs            # Config & message validation
│   │   └── github/
│   │       ├── mod.rs
│   │       ├── client.rs               # GitHub REST API v3 client
│   │       ├── config.rs               # GithubConfig (owner, repo, token, api_url)
│   │       ├── inbound.rs              # GithubInboundAdapter (polling + matcher)
│   │       └── outbound.rs             # GithubOutboundAdapter (comment poster)
│   ├── core/
│   │   ├── mod.rs
│   │   ├── thread_manager.rs           # Per-thread queues + semaphore
│   │   ├── thread_manager_tests.rs     # ThreadManager integration tests
│   │   ├── thread_path.rs              # Central path resolution for threads
│   │   ├── thread_event_bus.rs         # Thread-isolated event bus
│   │   ├── thread_event.rs             # ThreadEvent types
│   │   ├── message_router.rs           # Pattern match → dispatch
│   │   ├── message_storage.rs          # Markdown file I/O
│   │   ├── chat_log_store.rs           # Chat log storage (daily log files)
│   │   ├── email_parser.rs             # Stripping, quoting, thread trail
│   │   ├── state_manager.rs            # UID tracking, state persistence
│   │   ├── metrics.rs                   # MetricsCollector (replaces AlertService)
│   │   ├── attachment_storage.rs       # Channel-agnostic attachment saving
│   │   ├── template_utils.rs           # Template file copying
│   │   ├── pending_delivery.rs         # Background reply delivery watcher
│   │   └── command/
│   │       ├── mod.rs
│   │       ├── registry.rs             # Command parsing + dispatch
│   │       ├── handler.rs              # CommandHandler trait
│   │       ├── model_handler.rs        # /model command
│   │       ├── mode_handler.rs         # /plan, /build commands
│   │       ├── reset_handler.rs        # /reset command
│   │       ├── close_handler.rs        # /close command (thread cleanup)
│   │       └── template_handler.rs     # /template command (re-apply template)
│   ├── services/
│   │   ├── mod.rs
│   │   ├── agent.rs                   # AgentService trait (process → AgentResult)
│   │   ├── static_agent.rs            # StaticAgentService (fixed text reply)
│   │   ├── opencode/
│   │   │   ├── mod.rs                 # OpenCode server manager (start/stop, port, health)
│   │   │   ├── service.rs            # OpenCodeService implements AgentService
│   │   │   ├── client.rs             # OpenCode HTTP + SSE client
│   │   │   ├── session.rs            # Session + opencode.json + signal file management
│   │   │   ├── prompt_builder.rs     # Prompt construction
│   │   │   └── types.rs              # API request/response + SSE event types
│   │   ├── imap/
│   │   │   ├── mod.rs
│   │   │   ├── client.rs             # async-imap wrapper
│   │   │   └── monitor.rs            # IDLE + poll + recovery
│   │   └── smtp/
│   │       ├── mod.rs
│   │       └── client.rs             # lettre SMTP, MD→HTML, file attachments
│   ├── mcp/
│   │   ├── mod.rs
│   │   ├── reply_tool.rs             # rmcp MCP server (reply_message tool)
│   │   ├── vision_tool.rs            # rmcp MCP server (analyze_image tool)
│   │   ├── question_tool.rs          # rmcp MCP server (ask_user tool)
│   │   └── context.rs                # ReplyContext serialization + validation
│   ├── inspect/
│   │   ├── mod.rs
│   │   ├── types.rs                  # InspectState, ChannelInfo, ThreadInfo, protocol
│   │   ├── server.rs                 # TCP inspect server (JSON line protocol)
│   │   └── client.rs                 # TCP client (persistent connection, auto-reconnect)
│   ├── security/
│   │   └── mod.rs                     # Path validation, file size/extension checks
│   └── utils/
│       ├── mod.rs
│       ├── helpers.rs                # Regex validation, file size parsing
│       ├── attachment_validator.rs   # Attachment validation (count, size, extension)
│       └── constants.rs              # Default configs, timeouts
```

### Chat Log Entry Format (`chat_history_YYYY-MM-DD.md`)

```yaml
---
channel: email
uid: "12345"
external_id: "<abc123@mail.example.com>"
matched_pattern: "support"
topic: "Help with feature X"
timestamp: "2026-03-19T23:02:20Z"
---
```

```markdown
## Sender Name (10:15 AM)

Message body content here (full body including quoted history preserved)

*Attachments:*
  - **report.pdf** (application/pdf, 52410 bytes) saved
  - **malware.exe** (application/x-msdownload, 12345 bytes) skipped
---
```

### Chat Log Storage

Messages and replies are appended to daily chat log files:
```
chat_history_2026-03-19.md     # All messages and replies for this date
chat_history_2026-03-20.md     # Next day's log (auto-rotated by date)
```

Each log file contains chronological entries:
- Incoming messages — appended with sender, timestamp, and body
- AI replies — appended after sending, with model and mode metadata
- Attachments — saved in the thread directory by the inbound adapter (if allowlist config enabled)

## Logging & Tracing

### Library Choice: `tracing` + `tracing-subscriber`

JYC uses the `tracing` ecosystem for all logging and diagnostics:

| Aspect | Detail |
|--------|--------|
| **Crate** | `tracing` 0.1.x + `tracing-subscriber` 0.3.x |
| **Why not `log`** | `tracing` provides structured fields, async-aware spans, and custom subscriber layers |
| **Span architecture** | Layered spans provide automatic context (component, channel, thread, model) on every log line |
| **Env filter** | `RUST_LOG=jyc=info,async_imap=warn` controls per-module verbosity |
| **CLI flags** | `--debug` sets `jyc=debug`, `--verbose` sets `jyc=trace,async_imap=debug` |

### Layered Span Architecture

Every log line automatically includes context from hierarchical `tracing` spans. Spans are layered from general to specific:

```
Layer 1: component     (always present — identifies the subsystem)
  Layer 2: channel     (present when processing a specific channel)
    Layer 3: thread    (present when processing a specific thread)
      Layer 4: model/mode  (present during AI session)
```

#### Span Definitions

| Span Name | Layer | Fields | Where Created | Propagation |
|-----------|-------|--------|---------------|-------------|
| `inbound` | L1+L2 | `channel` | `cli/monitor.rs` — per IMAP task | `tokio::spawn().instrument()` |
| `worker` | L1+L2+L3 | `channel`, `thread` | `thread_manager.rs` — per worker | `tokio::spawn().instrument()` |
| `metrics` | L1 | — | `metrics.rs` — background task | `tokio::spawn().instrument()` |

Logs within instrumented futures automatically inherit all parent span fields. For example, a log in `opencode/service.rs` called from within a `worker` span shows:

```
INFO worker{channel=jiny283, thread=weather}: Sending prompt to OpenCode mode=build
INFO worker{channel=jiny283, thread=weather}: AI model selected model=deepseek-v3.2
INFO worker{channel=jiny283, thread=weather}: Tool running tool=glob
INFO worker{channel=jiny283, thread=weather}: Session idle — prompt complete
```

#### How Spans Propagate in Async Code

```
cli/monitor.rs:
  tokio::spawn(async { ... }.instrument(info_span!("inbound", channel = %ch)))
    → imap/monitor.rs: start() — all logs inherit inbound{channel}
      → message_router.rs: route() — inherits inbound{channel}
        → thread_manager.rs: enqueue() — creates new worker span

  tokio::spawn(async { ... }.instrument(info_span!("worker", channel, thread)))
    → process_message() — inherits worker{channel, thread}
      → command/registry.rs: process_commands() — inherits worker{channel, thread}
      → agent.process() — inherits worker{channel, thread}
        → opencode/service.rs: generate_reply() — inherits worker{channel, thread}
          → opencode/client.rs: prompt_with_sse() — inherits worker{channel, thread}
            → handle_sse_event() — inherits (sync, called within instrumented future)
```

#### Log Output Examples

```
INFO inbound{channel=jiny283}: Starting IMAP monitor mode="poll" folder=INBOX
INFO inbound{channel=jiny283}: IMAP connected and authenticated host=imap.163.com
INFO inbound{channel=jiny283}: Message received uid=123 sender=kingye@petalmail.com
INFO inbound{channel=jiny283}: Pattern matched pattern=sap
INFO worker{channel=jiny283, thread=weather}: Worker started
INFO worker{channel=jiny283, thread=weather}: Message stored sender=kingye@petalmail.com
INFO worker{channel=jiny283, thread=weather}: Sending prompt to OpenCode mode=build
INFO worker{channel=jiny283, thread=weather}: AI model selected model=deepseek-v3.2
INFO worker{channel=jiny283, thread=weather}: Tool running tool=jiny_reply_reply_message
INFO worker{channel=jiny283, thread=weather}: Session idle — prompt complete
INFO worker{channel=jiny283, thread=weather}: Reply sent by MCP tool
INFO worker{channel=jiny283, thread=weather}: Agent complete reply_sent=true
INFO worker{channel=jiny283, thread=weather}: Worker finished
alert: Alert service stopped
```

#### Key Rules

- **`tokio::spawn` does NOT inherit parent spans** — each spawned task must be explicitly instrumented with `.instrument(span)`
- **`.instrument(span)` works across `.await` points** — unlike `span.enter()` which only works in sync code
- **Sync methods called within instrumented async blocks** inherit the parent span automatically (e.g., `handle_sse_event()`)
- **MCP reply tool** runs as a separate process — no span inheritance. Uses its own file-based logger.
- **Individual log calls** only include per-event fields (e.g., `tool`, `uid`, `error`). Context fields (channel, thread) come from the span.

### Log Levels

| Level | Usage |
|-------|-------|
| ERROR | Unrecoverable failures, processing errors, MCP tool errors |
| WARN | Recoverable issues: queue full, stale session, timeout, reconnection |
| INFO | Lifecycle: message received, matched, processed, reply sent, worker start/stop, step start, tool calls |
| DEBUG | SSE events, session status changes, step finish with costs, AI response text, config details |
| TRACE | IMAP polling, mailbox select, skipping heartbeat notifications |

### Alert Service Integration

The `AppLogger` provides a unified logging + alerting interface:

1. **Logging methods** (`info()`, `error()`, etc.) delegate to `tracing` for console output
2. **Structured event methods** (`message_received()`, `reply_by_tool()`, etc.) additionally send events to the alert service via `mpsc` channel
3. The alert service buffers errors and periodically flushes them as digest emails
4. Self-protection: alert send failures use `eprintln` (not tracing) to avoid feedback loops

## Command System

### Available Commands

| Command | Description | Example |
|---------|-------------|---------|
| `/model <id>` | Switch AI model for this thread | `/model SiliconFlow/Pro/deepseek-ai/DeepSeek-V3.2` |
| `/model` | List available models | `/model` |
| `/model reset` | Reset to default model from config | `/model reset` |
| `/plan` | Switch to plan mode (read-only, enforced by OpenCode) | `/plan` |
| `/build` | Switch to build mode (full execution, default) | `/build` |
| `/reset` | Clear the current OpenCode session (start fresh context) | `/reset` |
| `/close` | Close the current thread (deletes thread directory and state) | `/close` |
| `/template` | Re-apply the pattern's thread template files | `/template` |

### Command Handler Trait

```rust
#[async_trait]
pub trait CommandHandler: Send + Sync {
    fn name(&self) -> &str;            // e.g., "/model"
    fn description(&self) -> &str;

    async fn execute(&self, context: CommandContext) -> Result<CommandResult>;
}

pub struct CommandContext {
    pub args: Vec<String>,
    pub thread_path: PathBuf,
    pub config: Arc<AppConfig>,
    pub channel: String,
}

pub struct CommandResult {
    pub success: bool,
    pub message: String,               // User-facing result text
    pub requires_restart: bool,        // Whether OpenCode server needs restart
}
```

### Unified Command Processing

JYC unifies command parsing, execution, and body stripping into a single `process_commands()` method. This keeps all command-related concerns in one place.

```rust
/// Output of unified command processing
pub struct CommandOutput {
    /// Results from executed commands (for direct reply if body is empty)
    pub results: Vec<CommandResult>,
    /// Message body with command lines stripped
    pub cleaned_body: String,
    /// Whether the body was empty after stripping (command-only message)
    pub body_empty: bool,
}

impl CommandRegistry {
    /// Parse, execute, and strip commands from message body in a single pass.
    ///
    /// Commands must appear at the top of the body (before any non-command content).
    /// Lines starting with `/` that match a registered handler are treated as commands.
    /// Empty lines between commands are skipped. The first non-empty, non-command
    /// line ends the command block — everything from that line onward is the
    /// cleaned body.
    ///
    /// Returns executed results + cleaned body. ThreadManager does NOT need to
    /// know about command line syntax.
    pub async fn process_commands(
        &self,
        body: &str,
        context: CommandContext,
    ) -> Result<CommandOutput> {
        let mut results = Vec::new();
        let mut body_lines = Vec::new();
        let mut in_command_block = true;

        for line in body.lines() {
            let trimmed = line.trim();

            if in_command_block {
                if trimmed.is_empty() {
                    continue; // Skip blank lines in command block
                }
                if trimmed.starts_with('/') {
                    let parts: Vec<&str> = trimmed.split_whitespace().collect();
                    let cmd_name = parts[0].to_lowercase();
                    if let Some(handler) = self.handlers.get(&cmd_name) {
                        let args = parts[1..].iter().map(|s| s.to_string()).collect();
                        let ctx = CommandContext { args, ..context.clone() };
                        let result = handler.execute(ctx).await?;
                        results.push(result);
                        continue; // Command consumed, don't add to body
                    }
                }
                // First non-empty, non-command line → end command block
                in_command_block = false;
                body_lines.push(line);
            } else {
                body_lines.push(line);
            }
        }

        let cleaned_body = body_lines.join("\n");
        let body_empty = cleaned_body.trim().is_empty();

        Ok(CommandOutput { results, cleaned_body, body_empty })
    }
}
```

**ThreadManager usage** (simplified — no command syntax knowledge needed):

```rust
let output = command_registry.process_commands(
    &message.content.text.unwrap_or_default(),
    ctx,
).await?;

if output.body_empty && !output.results.is_empty() {
    // Command-only message → direct reply with results summary
    let summary = output.results.iter()
        .map(|r| r.message.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    send_direct_reply(&message, &summary, thread_path, message_dir).await?;
    return Ok(());
}

// Continue with output.cleaned_body for AI processing
message.content.text = Some(output.cleaned_body);
```

**Key design decisions:**
- **Single pass**: Commands are parsed, executed, and stripped in one scan of the body.
- **Single responsibility**: All command-related logic (parsing, executing, stripping) lives in `CommandRegistry`. `ThreadManager` only checks `body_empty` and `results`.
- **Testable**: One function in, one struct out. Easy to unit test without mocking ThreadManager.

### Model Override Persistence

The `/model` command writes the model ID to `.jyc/model-override` in the thread directory. This persists across messages — subsequent emails in the same thread use the overridden model until `/model reset` is sent.

### Plan/Build Mode

- **Plan mode**: OpenCode enforces read-only at the tool level — the AI cannot edit files or run modifying commands
- **Build mode**: Default. Full execution — AI can edit files, run tests, commit, etc.
- `.jyc/mode-override` contains `"plan"` when plan mode active; file absent = build mode

## Inbound Attachment Download

Configurable per pattern via `attachments` in the pattern config.

**Processing flow:**
1. `mail-parser` parses MIME and provides attachment bytes
2. Inbound adapter preserves bytes on the `MessageAttachment` object
3. Inbound adapter saves attachments before passing message to MessageRouter
4. For each attachment: check extension allowlist → check size limit → check count limit → sanitize filename → resolve collisions → write to disk
5. Bytes freed after write (`attachment.content = None`)
6. Attachment metadata is logged and included in the chat log entry

**Security measures:**
- Extension allowlist (not blocklist) — only explicitly permitted types saved
- File size limit per attachment (human-readable: `"25mb"`, `"150kb"`)
- Max attachments per message (prevents resource exhaustion)
- Filename sanitization: basename only, no path traversal, no hidden files, no null bytes, max 200 chars, Unicode NFC normalized
- Double extension defense: only the last extension is checked
- Collision handling: counter suffix (e.g. `report_2.pdf`)

## Stripping Strategy

`strip_quoted_history()` is applied at **AI prompt consumption time**, never at storage or reply time. Cleaning (`clean_email_body`) happens once at the InboundAdapter boundary.

| Stage | Where | Strips history? | Cleans? | Purpose |
|-------|-------|----|---------|---------|
| **Inbound** | `EmailInboundAdapter` | No | Yes | Clean at boundary |
| **Storage** | `MessageStorage::store()` | No | No | Append to daily chat log |
| **AI Prompt Body** | `PromptBuilder::build_prompt()` | Yes | No | Incoming message for AI |
| **Reply context** | `.jyc/reply-context.json` | N/A | N/A | Saved to disk before prompt, read by reply tool |
| **Reply Tool** | `mcp/reply_tool.rs` | No | No | Reads reply-context.json for routing metadata |
| **Outbound** | `SmtpClient` | No | No | Dumb transport + attachments |

## Security Considerations

- Environment variables for credentials (never commit passwords)
- Validate regex patterns at config load time to prevent ReDoS
- Rate limiting for AI API calls
- Path validation for all file operations (`PathValidator`)
- Attachment security: extension allowlist, size limit, filename sanitization
- MCP tool: validate context before processing
- `permission: { "*": "allow", "question": "deny", "external_directory": "deny" }` in opencode.json
- Rust's ownership model eliminates data races, use-after-free, and buffer overflows
- `system.md` per-thread customization — file permissions should restrict who can modify thread directories

## Crate Dependencies

| Crate | Version | Purpose |
|-------|---------|---------|
| `tokio` | 1.x (features: full) | Async runtime |
| `clap` | 4.x (features: derive) | CLI argument parsing |
| `async-imap` | 0.11.x (features: runtime-tokio) | IMAP client with IDLE |
| `async-native-tls` | 0.5.x | TLS for IMAP |
| `mail-parser` | 0.9.x | MIME email parsing |
| `lettre` | 0.11.x (features: tokio1-rustls-tls) | SMTP sending |
| `comrak` | 0.37.x | Markdown → HTML (GFM) |
| `htmd` | 0.5.x | HTML → Markdown |
| `reqwest` | 0.12.x (features: json, stream) | HTTP client |
| `reqwest-eventsource` | 0.6.x | SSE client |
| `rmcp` | 0.1.x (features: server, transport-io) | MCP server (stdio) |
| `serde` | 1.x (features: derive) | Serialization framework |
| `serde_json` | 1.x | JSON serialization |
| `toml` | 0.8.x | TOML config parsing |
| `tracing` | 0.1.x | Structured async-aware logging |
| `tracing-subscriber` | 0.3.x (features: env-filter, fmt) | Log output formatting + filtering |
| `anyhow` | 1.x | Application error handling |
| `thiserror` | 2.x | Typed library errors |
| `chrono` | 0.4.x (features: serde) | Date/time handling |
| `base64` | 0.22.x | Base64 encoding/decoding |
| `regex` | 1.x | Pattern matching |
| `uuid` | 1.x (features: v4) | Internal message IDs |
| `tokio-util` | 0.7.x | CancellationToken |
| `async-trait` | 0.1.x | Async trait support |

## Thread Event System

The Thread Event System is a core component for handling inter-thread event communication in JYC. It implements SSE event to ThreadEvent conversion with thread isolation and controlled heartbeat rhythm.

### Architecture

#### Core Components
1. **OpenCode Client** - SSE event conversion layer
2. **Thread Event Bus** - Thread-isolated event bus (publish/subscribe)
3. **Thread Manager** - Event listening and heartbeat control layer
4. **Outbound Adapter** - Heartbeat message sending layer (`send_heartbeat()` — pre-formatted from per-channel template)

#### Data Flow
```
SSE Events (OpenCode Server)
    ↓
OpenCode Client Conversion
    ├── ProcessingStarted → ThreadEvent::ProcessingStarted
    ├── ProcessingProgress → ThreadEvent::ProcessingProgress
    ├── ToolStarted/Completed → ThreadEvent::ToolStarted/Completed
    └── server.heartbeat → ignored (connection keep-alive only)
    ↓
Publish to Thread's Event Bus
    ↓
Thread Manager Event Listener
    ├── Receive events and update processing state
    ├── Check heartbeat conditions based on configurable interval (default 10min)
    ├── Format heartbeat message from per-channel template
    ├── Send heartbeat when conditions met
    └── Use send_heartbeat() with pre-formatted message
    ↓
User receives heartbeat email
```

### Thread Isolation Design

#### Key Features
1. **Per-thread isolated event bus** - Each thread uses a `SimpleThreadEventBus` instance
2. **No cross-thread event propagation** - Complete isolation
3. **Independent heartbeat state** - Each thread maintains its own heartbeat timer and state

#### Implementation
```rust
// ThreadManager creates isolated event bus for each thread
let event_bus = Arc::new(SimpleThreadEventBus::new(10));

// Event listener subscribes only to its thread's event bus
let mut receiver = event_bus.subscribe().await;
```

### Heartbeat Rhythm Control

#### Control Logic
Heartbeat rhythm is controlled by Thread Manager based on configurable `HeartbeatConfig`:
1. **interval_secs** (default 10 minutes) - Minimum interval between heartbeats
2. **min_elapsed_secs** (default 1 minute) - Minimum processing time before first heartbeat

#### Heartbeat Conditions
Send heartbeat when ALL conditions are met:
1. ✅ Current message being processed
2. ✅ Processing state available (from `ProcessingProgress` events)
3. ✅ Processing elapsed ≥ `min_elapsed_secs` (default 1 minute)
4. ✅ Time since last heartbeat ≥ `interval_secs` (default 10 minutes)

#### Implementation
```rust
// In event_listener_with_heartbeat function
let interval = Duration::from_secs(heartbeat_config.interval_secs);
let min_elapsed = Duration::from_secs(heartbeat_config.min_elapsed_secs);
let mut heartbeat_timer = tokio::time::interval(interval);

// Timer tick - check if we should send heartbeat
_ = heartbeat_timer.tick() => {
    if let Some(message) = current_message {
        if let Some((elapsed_secs, activity, progress)) = &last_processing_state {
            let processing_elapsed = Duration::from_secs(*elapsed_secs);
            if processing_elapsed >= min_elapsed {
                // Check heartbeat interval
                let should_send = match last_heartbeat_sent {
                    Some(last_sent) => last_sent.elapsed() >= interval,
                    None => true, // First heartbeat
                };
                
                if should_send {
                    let formatted = format_heartbeat(&heartbeat_template, *elapsed_secs, activity, progress);
                    outbound.send_heartbeat(&message, &formatted).await;
                }
            }
        }
    }
}
```

### ThreadEvent Types

#### Event Enumeration
```rust
pub enum ThreadEvent {
    // Heartbeat event (controlled by Thread Manager, pre-formatted message)
    Heartbeat {
        thread_name: String,
        message: String,
        timestamp: DateTime<Utc>,
    },
    
    // Processing state events (published by OpenCode Client)
    ProcessingStarted { ... },
    ProcessingProgress { ... },
    ProcessingCompleted { ... },
    
    // Tool execution events
    ToolStarted { ... },
    ToolCompleted { ... },
}
```

#### Event Sources
| Event Type | Publisher | Purpose |
|------------|-----------|---------|
| ProcessingStarted | OpenCode Client | Published when processing starts |
| ProcessingProgress | OpenCode Client | Periodic progress updates |
| ProcessingCompleted | OpenCode Client | Published when processing completes |
| ToolStarted/Completed | OpenCode Client | Tool start/complete events |
| Heartbeat | Thread Manager | Periodic heartbeat, user notification |

### Configuration

#### HeartbeatConfig (configurable via `[heartbeat]` TOML section)
```rust
/// Heartbeat timing configuration
pub struct HeartbeatConfig {
    pub enabled: bool,           // default: true
    pub interval_secs: u64,      // default: 600 (10 minutes, to avoid SMTP rate limits)
    pub min_elapsed_secs: u64,   // default: 60 (1 minute)
}

// Minimum interval between heartbeats to avoid flooding (30 seconds)
pub const MIN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
```

Per-channel heartbeat message template is configured via `heartbeat_template` field on `ChannelConfig`.

#### Thread Manager Initialization
```rust
// Enable Thread Event system by default
let thread_manager = ThreadManager::new_with_options(
    max_concurrent,
    max_queue_size,
    storage,
    outbound,
    agent,
    heartbeat_config,
    heartbeat_template,
    cancel.child_token(), // child token — prevents cascade shutdown
    true, // enable_events: true (Thread Event system)
);
```

### Error Handling

#### Event Publishing Errors
- Event publishing failures do not block the main process
- Asynchronous non-blocking event publishing
- Appropriate logging for failures

#### Heartbeat Sending Errors
- Heartbeat sending failures log warning
- No retry for failed heartbeats
- Next heartbeat cycle will continue trying

### Performance Considerations
1. **Asynchronous non-blocking** - All event publishing is asynchronous
2. **Thread-local state** - Each thread maintains independent heartbeat state
3. **Lightweight events** - Event structures remain simple
4. **Limited queues** - Event buses use limited capacity queues

### Testing Strategy

#### Unit Tests
- Event type serialization/deserialization
- Event bus basic functionality
- Heartbeat condition judgment logic

#### Integration Tests
- SSE event to ThreadEvent conversion
- Inter-thread event isolation
- Heartbeat rhythm control

#### End-to-End Tests
- Complete event flow: SSE → ThreadEvent → Heartbeat email
- Multi-thread concurrent processing
- Error scenario handling

### Deployment Notes
1. **Configuration adjustment** - Adjust heartbeat interval based on actual needs
2. **Monitoring** - Monitor event publishing and heartbeat sending frequency
3. **Log levels** - Adjust event log levels appropriately in production
4. **Resource limits** - Pay attention to memory usage of event queues

### Invoice Download Proxy

Some invoice platforms (e.g., 51fapiao.cn) use Alibaba Cloud WAF that blocks
requests from non-mainland China IPs. The invoice-processing skill includes a
download proxy to handle this:

**Architecture:**
```
Overseas Server (HK)                    Mainland China Server (Shanghai)
┌────────────────────┐                  ┌─────────────────────────┐
│ JYC + OpenCode     │                  │ download_proxy.py :8765 │
│                    │  HTTP request    │                         │
│ proxy_download.py ─┼─────────────────►│  fetch?url=<encoded>    │
│                    │  file content    │  ↓                      │
│ ◄──────────────────┼─────────────────┤  urllib → 51fapiao.cn   │
│                    │                  │                         │
└────────────────────┘                  └─────────────────────────┘
```

**Components:**
- `scripts/download_proxy.py` — HTTP proxy service on Shanghai server (150.158.50.252:8765).
  Whitelisted domains only. Rate limited. Auto-starts via crontab `@reboot`.
- `scripts/proxy_download.py` — Client script on the JYC server. Tries direct download
  first; falls back to proxy if direct fails and `INVOICE_DOWNLOAD_PROXY` env var is set.

**Environment variable:**
- `INVOICE_DOWNLOAD_PROXY` — Set on overseas servers only. Not needed on mainland China.
  Example: `export INVOICE_DOWNLOAD_PROXY=http://150.158.50.252:8765`
  Configured in `~/.zshrc.local` on the HK server (sourced by `deploy.sh`).

**Deployment scenarios:**
| Server Location | Env Var Needed? | Behavior |
|-----------------|-----------------|----------|
| Mainland China (e.g., Shanghai) | No | Direct download works |
| Overseas (e.g., Hong Kong) | Yes | Direct fails → proxy fallback |

### Templates & Skills

Agent templates define the role, instructions, and skills for each thread type.
Templates live in the repo under `templates/`, skills under `.opencode/skills/`.
A deploy script composes them at deploy time.

**Repository structure:**
```
templates/                      ← AGENTS.md only (role + instructions)
  invoice-processing/AGENTS.md
  jyc-dev/AGENTS.md
  jyc-review/AGENTS.md
  github-planner/AGENTS.md
  github-developer/AGENTS.md
  github-reviewer/AGENTS.md

.opencode/skills/               ← Single source of truth for all skills
  invoice-processing/           ← Invoice extraction workflow
  dev-workflow/                 ← Branching, commits, releases
  incremental-dev/              ← Small-step development methodology
  plan-solution/                ← Structured implementation planning
  pr-review/                    ← Read-only PR review
  jyc-deploy-bare/              ← Bare metal deployment
  jyc-deploy-docker/            ← Docker deployment
```

**Skill → Template mapping:**

| Template | Skills | Purpose |
|----------|--------|---------|
| invoice-processing | invoice-processing | Invoice data extraction and Excel recording |
| jyc-dev | plan-solution, dev-workflow, incremental-dev, jyc-deploy-bare | JYC self-development |
| jyc-review | pr-review | Code review |
| github-planner | dev-workflow | Discuss requirements, create PRs |
| github-developer | incremental-dev, dev-workflow | Implement code from PR spec |
| github-reviewer | pr-review | Review PRs, approve/request changes |

**Deployment:**
```bash
# Compose templates + skills and deploy to server data directory
./deploy-templates.sh /home/jiny/projects/jyc-data/templates
```

The script copies each template's `AGENTS.md` and its referenced skills from
`.opencode/skills/` into the target directory. When a thread is created with
a template, the `AGENTS.md` and `.opencode/skills/` are copied to the thread's
workspace directory.

**Adding a new template:**
1. Create `templates/<name>/AGENTS.md` with role instructions
2. Add the skill mapping to `deploy-templates.sh` (`get_skills` function)
3. Run `./deploy-templates.sh <target>` to deploy

## References

- [SYSTEMD.md](SYSTEMD.md) - systemd service management for process supervision and self-bootstrapping
- [IMPLEMENTATION.md](IMPLEMENTATION.md) - Implementation phases and progress
- [CHANGELOG.md](CHANGELOG.md) - Version history and changes
