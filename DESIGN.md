# JYC: Channel-Agnostic AI Agent (Rust)

## Overview

JYC is a channel-agnostic AI agent that operates through messaging channels. Users interact with the agent by sending messages (email, FeiShu, Slack, etc.), and the agent responds autonomously using an in-process agent. The agent maintains conversation context per topic, enabling coherent multi-turn interactions.

**Core Concept:** Messaging channels are the interface; AI is the brain. The architecture is channel-agnostic — adding a new channel requires only implementing an inbound and outbound adapter trait.

**Why Rust:** Single static binary, zero runtime dependencies, memory safety without GC, and predictable low-latency performance for long-running server processes.

## Use Cases

- **Support Agent** — Automatically respond to support inquiries with context-aware replies
- **Task Automation** — Execute tasks requested via messages and respond with results
- **Notification Processor** — Process notifications and take action based on content
- **Personal Assistant** — Manage schedules, reminders, and information requests via messaging
- **Cross-Channel Agent** — Same AI agent accessible through multiple channels (email, FeiShu, etc.)

## Architecture

> **Architecture:** JYC follows the three-layer **channels / agents / core+AI**
> model — pipe-only channels (Feishu, GitHub, Gitee, WeCom Bot, WeCom, WeCom KF,
> Email), agents (first-class entities; own topics, synthesize a websocket
> channel), and a channel-agnostic core (per-topic queues + workers + `[ai]` block).
> See [docs/architecture/overview.md](docs/architecture/overview.md). Sections
> below that describe per-channel TopicManagers and outbound adapters reflect
> the legacy architecture and apply to the websocket channel synthesized by
> an agent only.
>
> **Config surface:** an explicit `[channels.<name>]` with `type = "websocket"`
> is rejected — the synthesized "agents" channel is the only websocket channel
> in the hub. A pipe channel carries routing only (`rules` to match, `pipe` to
> target); the processing configuration (model, mode, skills, mcps,
> live_injection, topic_path, template, …) belongs to the agent that handles
> the message, and config validation rejects those keys on a pipe channel.
>
> **Processes:** every pipe-only channel runs in a separate process,
> `jyc-pipe`, which connects to the hub's websocket endpoint and owns no
> topics, agents, or core state; the hub (`jyc` serve) claims only the
> synthesized agent/websocket channel, and hosts the inspect server. The
> ownership rule is the channel type
> (`jyc_types::channel::SUPPORTED_CHANNEL_TYPES`), not a config section;
> the WeCom webhook listener (`[wecom].bind_addr`) and the
> email mailbox cursor (`<data dir>/channels/<name>/.imap/`) belong to the pipe.
> The two processes are paired by `jyc serve --pipe`, which spawns `jyc-pipe` as a
> child of the hub — same data dir and resolved config file — and stops it when the
> hub exits (SIGTERM, then SIGKILL after a grace period); the child is deliberately
> not restarted, since a channel crash must not take the agent core down. Running
> `jyc-pipe` separately (systemd, by hand) stays possible, but the shipped
> deployment is the hub-owned child.
> Inbound attachment bytes travel the same way as reply attachments, over the
> bearer-gated REST API (staged under `<data dir>/.inbound/`, then named in
> the `message` frame and moved into the routed topic). The staging directory
> is hub-level on purpose: it must sit outside every topic root, or a topic
> named `workspace` would share it.
> See [docs/architecture/pipe-split.md](docs/architecture/pipe-split.md).

### High-Level Flow

```
User sends message (any channel) → Pattern Match → Topic Queue → Worker (AI) → Reply via originating channel
                                                         ↓
                                               Topic-based context
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
│  │ derive_topic│  │ derive_topic│  │ derive_topic│  │ derive_topic│                 │
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
│  - Delegates topic naming to adapter.derive_topic_name()               │
│  - Reads patterns from live ArcSwap<AppConfig> (supports hot-reload)     │
│  - Sends to TopicManager via mpsc channel (fire-and-forget)             │
└────────────────────────┬────────────────────────────────────────────────┘
                         │ send (non-blocking)
                         ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                       TopicManager                                      │
│  max_concurrent_topics: 3 (Semaphore-bounded)                           │
│  max_queue_size_per_topic: 10                                           │
│                                                                          │
│  ┌─────────────────────────────────────────────────────┐                │
│  │        Topic Queues (HashMap<String, TopicQueue>)  │                │
│  │                                                      │                │
│  │  "topic-A" → mpsc::Receiver ← [msg2, msg3]         │                │
│  │  "topic-B" → mpsc::Receiver ← [msg4]               │                │
│  │  "topic-C" → mpsc::Receiver ← []                   │                │
│  └─────────────────────────────────────────────────────┘                │
│                                                                          │
│  Tokio Semaphore (3 permits):                                            │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐                     │
│  │ Worker A    │  │ Worker B    │  │ Worker C    │                     │
│  │ (permit 1)  │  │ (permit 2)  │  │ (permit 3)  │                     │
│  │ processing  │  │ processing  │  │ idle        │                     │
│  │ topic-A/m1 │  │ topic-B/m4 │  │             │                     │
│  │   ┌─────┐   │  │   ┌─────┐   │  │             │                     │
│  │   │Event│   │  │   │Event│   │  │             │                     │
│  │   │Bus A│   │  │   │Bus B│   │  │             │                     │
│  │   └─────┘   │  │   └─────┘   │  │             │                     │
│  └─────────────┘  └─────────────┘  └─────────────┘                     │
│                                                                          │
│  Topic Event System (per topic):                                       │
│  ┌─────────────────────────────────────────────────────┐                │
│  │  • Topic-isolated event bus                        │                │
│  │  • SSE → TopicEvent conversion (agent client)   │                │
│  │  • Processing state tracking                        │                │
│  └─────────────────────────────────────────────────────┘                │
│                                                                          │
│  New topic arrives → tokio::spawn → acquire semaphore permit            │
│  Worker loop: recv from mpsc → process (rx passed to agent for           │
│    live injection) → recv next                                           │
│  Topic queue empty + no pending → release permit, task exits            │
└────────────────────────┬────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                    Worker (per message) — TopicManager                   │
│                                                                          │
│  0. If event support enabled: create topic event bus and start listener │
 │  1. MessageStorage::store(msg) → append to chat_history_YYYY-MM-DD.jsonl   │
│  2. Save inbound attachments (allowlisted)                               │
│  3. CommandRegistry::process_commands(body, ctx)                         │
│     → parse, execute, strip in single pass → cleaned body + results      │
│  4. If body empty after commands → direct reply with results, return     │
│  5. Dispatch to agent mode:                                              │
│     - "static" → send configured text via OutboundAdapter                │
│     - "agent" → AgentService::generate_reply(msg)                  │
│     - "agent" → JycAgentService::process(msg) [in-process, no ext. deps] │
│  6. If agent returns fallback text → send via OutboundAdapter            │
│  7. Worker picks next message from topic queue                          │
└────────────────────────┬────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────────────────┐
│             AgentService::generate_reply() (agent-specific)           │
│                                                                          │
│  1. Ensure agent service is running (auto-start)                       │
│  2. Setup per-topic agent config (model, MCP tools, permissions)       │
│  3. Resolve the model for this turn's mode:                              │
│     session_state::resolve_model (the one implementation — see the       │
│     Merge & Lookup Rules section)                                        │
│  4. Build system prompt:                                                 │
│     - Directory boundary rules                                           │
│     - AGENTS.md (topic-local + repo/AGENTS.md if exist)                 │
│     - Discovered skills section (lazy: name + description, full content  │
│       loaded on demand via read tool)                                    │
│     - Reply instructions                                                 │
│  5. Build user prompt (incoming message)                                 │
│     - Text blocks + optional ContentBlock::Image for inbound attachments │
│       when inject_inbound_images=true and model supports images          │
│  6. Build tool registry (bash, read, write, edit, grep, webfetch        │
│     + read_image + reply_message bridge)                                 │
│     - Apply per-pattern disabled_builtin_tools and per-pattern mcps      │
│  6. Run agent loop:                                                      │
│     a. Send messages to LLM (streaming)                                  │
│     b. Collect response (text + tool_calls)                              │
│     c. If tool_calls → execute tools → loop back                         │
│     d. If text only → done                                               │
│     e. If reply_message tool called → reply delivered synchronously      │
│        via the channel's outbound adapter (file relay as fallback) → done│
│  7. Return AgentResult                                                   │
│                                                                          │
│  Advantages over the external-server agent mode:                                          │
│  - No external process (no SSE fragility, no version coupling)           │
│  - ~10MB per session vs ~300MB (external-server per-session overhead)           │
│  - Instant startup (no server boot)                                      │
│  - Direct tool execution (no MCP subprocess spawning)                    │
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
│  │ References hdrs│  │ feishu msg    │  │ [Role] prefix │  │ for slack ││
│  └───────────────┘  └───────────────┘  └───────────────┘  └───────────┘│
└─────────────────────────────────────────────────────────────────────────┘
```

### Components

1. **Inbound Adapters** — Channel-specific message receivers (Email/IMAP, FeiShu/WebSocket, GitHub/REST polling)
2. **Outbound Adapters** — Channel-specific reply senders (Email/SMTP, FeiShu/API, GitHub/REST)
3. **Message Router** — Receives messages from all channels, delegates matching to adapters, routes to TopicManager
4. **Topic Manager** — Per-topic queues with semaphore concurrency control, worker spawn/manage
5. **Topic Event Bus** — Topic-isolated event bus for publishing and subscribing to processing events (SSE → TopicEvent conversion)
6. **Topic Event System** — Per-topic isolated event bus for progress events (ProcessingStarted/Progress/Completed, ToolStarted/Completed, Thinking, SessionStatus). Used by the inspect dashboard for realtime monitoring.
7. **Prompt Builder** — Builds channel-agnostic prompts from InboundMessage; supports multimodal first turns with ContentBlock::Image
8. **Reply Delivery** — the agent's final message at the end of the turn IS the reply (no reply tool). `deliver_reply` sends it synchronously via the channel's pre-warmed outbound adapter when a reply target is live; otherwise (or on send failure) it writes `reply.md` + signal file for the watcher/monitor to deliver
9. **MCP SendMessage Tool** — `jyc_send_message` in-process tool, sends a channel message with optional attachments to any recipient via the pre-warmed outbound adapter. Advertised with the core tools; used for proactive alerts, cross-channel messages, and attachment-bearing replies
10. **MCP Vision Tool** — `analyze_image` tool via `rmcp`, analyzes images using OpenAI-compatible vision API. Configure via `[[mcps]]` in `config.toml`
11. **Question Tool** — `ask_user` renders the user's decision options on whichever channel the message arrived on: a `question` frame to a channel with a question box (the TUI), the same set as a plain-text message on the ordinary reply path everywhere else (feishu, email, any pipe-only channel). Up to 5 questions go in one call and come back in one answer. It never waits: the tool ends the turn the moment the questions are handed over, and the user's reply — a numbered message on the TUI, free text anywhere else — arrives as the input to the next turn. On the TUI each question also takes a typed answer: the option list ends with a `✎` row whose one-line draft answers for that question on its own line (`Q2: their own words`, carrying no option number, so a pick and a typed answer are told apart), which is what makes the free text the other channels always had reachable on the one surface that draws a box. Skipping one question (`d`) or closing the box (`Esc`) are client-side choices with no protocol of their own, which is why there is no hub, no timeout, and no answer frame on any channel.
12. **Pending Delivery Watcher** — Background task that runs alongside SSE stream, watches for signal files and delivers messages immediately
13. **Message Storage** — Unified chat log storage: daily log files (`chat_history_YYYY-MM-DD.jsonl`) with JSON metadata
14. **State Manager** — Track processed UIDs per channel, handle migrations
15. **Security Module** — Path validation, file size/extension checks for attachments; tool boundary checks for write/edit/grep/bash/read_image
16. **Attachment Storage** — Channel-agnostic attachment saving with path traversal protection
17. **Inspect Server + Dashboard** — TCP JSON line protocol for runtime state queries, TUI dashboard for live monitoring
18. **MetricsCollector** — Lightweight stats accumulation for monitoring topic/channel activity
19. **Command System** — `/command` parsing and execution (`/model`, `/plan`, `/build`, `/reset`, `/close`, `/template`)
20. **Topic Lifecycle** — Channel-agnostic topic close mechanism via `on_topic_close` callback
21. **Template System** — Initialize new topics with predefined files from `templates/` directory
22. **AgentService** — Unified agent dispatch trait for static and in-process agent modes; resolves effective model from pattern/channel/global config
23. **Channel Orchestrator** — Manages channel lifecycle across config reloads. Registers per-channel status (topic manager, cancel token). On reload, diffs old/new config: cancels removed channels gracefully, warns on new channels (requires restart). Updates shared `InspectContext` state (`topic_managers`, `channels`, `workspace_dirs`) via `ArcSwap`.
24. **BillingLogStore** — Durable central cost ledger. Appends one line per LLM call to `<data dir>/billing/bill-YYYY-MM-DD.jsonl` (topic label + token counts + computed cost + currency). Central rather than per-topic because topic directories are deleted on close, which would destroy cost history. Day-stamped files bound the dashboard's per-poll read to a single day; never rotated or truncated, unlike the bounded `activity.jsonl` debug buffer. Complements `session_cost` in `agent-session.json`, which is session-scoped and zeroes on reset.

### Design Principles: Component Responsibilities

Each component has a single, clear responsibility. Data flows through the system with transformations happening at well-defined boundaries.

**InboundAdapter** (e.g., `EmailInboundAdapter`)

- Boundary between the external world and the internal system
- Parses raw data from the channel (e.g., raw email bytes via `mail-parser`)
- Cleans and normalizes data at the boundary: strips redundant `Re:/回复:` from subject, cleans bracket-nested duplicates
- Produces a clean `InboundMessage` — all downstream consumers receive clean data

**MessageStorage**

- Unified chat log storage: appends messages and replies to daily log files
- JSONL metadata: stores timestamp, type, matched, sender, sender_name, channel, external_id, topic, model, mode in JSON
- Dual-write support: during migration, maintains compatibility with legacy format
- No transformation or business logic - stores messages exactly as given
- Chat logs are stored in `chat_history_YYYY-MM-DD.jsonl` files
- Format: one JSON object per line (JSONL)

**PromptBuilder**

- Builds the user prompt from the incoming message
- Strips quoted history from body and truncates to fit AI token budget
- Note: Reply context is saved to disk (.jyc/reply-context.json), NOT embedded in the prompt
- Does NOT include conversation history in the prompt (in-process agent session memory handles multi-turn context)

**Reply Tool** (MCP `reply_message`)

- Orchestrator for the reply flow
- Reads `.jyc/reply-context.json` from disk to get routing info (channel name, message timestamp)
- Reads ALL message metadata (sender, recipient, topic, References headers) from reply-context.json — NOT from the AI prompt
- Builds the full reply in markdown: AI reply text + quoted history (`prepare_body_for_quoting`)
- Delegates sending to OutboundAdapter (passes the full markdown reply)
- Delegates storage to MessageStorage (appends to daily chat log file)
- Chat log entries reflect exactly what was sent to the recipient

**SmtpClient** (and other transport services)

- Dumb transport: receives markdown, converts to HTML (via `comrak`), adds email headers, sends via `lettre`
- Adds `Re:` to subject, sets `In-Reply-To` and `References` headers for References
- Does NOT build quoted history, does NOT clean or transform content
- **Structured error handling**: Uses lettre's structured SmtpError API for error classification: permanent errors (5xx) fail immediately, transient errors (4xx) retry with exponential backoff (3 attempts, 5-60s), connection/timeout errors reconnect with backoff (2 attempts).
- **Shared instance**: A single `SmtpClient` is created by the pipe-only email channel (`jyc-pipe`, `crates/jyc-channels/src/pipe/email.rs`) at startup and shared by its reply forwarders (see `docs/architecture/overview.md`)

**Topic Event System**

- **Topic Event Bus** - Topic-isolated event bus for SSE → TopicEvent conversion
- **Topic Isolation** - Each topic has independent event bus state
- **Event Types**:
  - `ProcessingStarted`, `ProcessingProgress`, `ProcessingCompleted`
  - `ToolStarted`, `ToolCompleted`
- **Event Flow**: SSE events → agent client conversion → Topic Event Bus → subscribers (inspect dashboard)

**Reply context** saved to `.jyc/reply-context.json` — the AI never sees it

- Only 5 fields: `channel`, `topicName`, `incomingMessageDir`, `uid`, `_nonce`
- Channel-agnostic — no email-specific fields (no sender, recipient, topic, References headers)
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
{"ts":"2026-04-07T01:18:31.002+00:00","type":"received","matched":true,"sender":"ou_c36ae8bf58a1d727fffd2289467fefce","channel":"feishu_bot","external_id":"om_x100b5271f8a044a0b4ca586517f9e5d","topic":"self-hosting-jyc","from":"ou_c36ae8bf58a1d727fffd2289467fefce","content":"部署完成了吗？"}
```

#### Migration Strategy

1. **Dual-write Phase**: Messages written to both legacy directories and new logs
2. **Email Parser Enhancement**: `email_parser` reads from logs first, falls back to directories
3. **Directory Removal**: Legacy directories gradually phased out after verification

#### AI Access Pattern

- AI uses `grep` or `rg --files` to find `chat_history_*.jsonl` log files
- `read "chat_history_2026-04-07.jsonl"` to access specific logs
- `grep "keyword" --include "chat_history_*.jsonl"` to search history

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
│ IMAP │  │ Inbound  │  │ Message  │  │  Topic  │  │ Prompt   │  │ in-process agent │  │  Reply   │  │  SMTP    │
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
- **PromptBuilder** strips quoted history from body for the AI prompt; does NOT include conversation history (the agent session memory handles that)
- **`build_footer()`** builds the model/mode/tokens footer — NOTE: it currently has no production caller, so `[channels.<name>.footer]` is inert (dead code; removal is a follow-up)
- **SmtpClient** is a dumb transport: markdown→HTML + headers + attachments + send
- **reply-context.json** is a minimal routing token (5 fields) — all message metadata comes from chat log frontmatter
- **Chat log entries** = exactly what the recipient receives (minus HTML formatting)

## Terminology

The vocabulary this document, the code comments and the agent-facing prompts all
use with one meaning. Most of the confusion this file has seen came from using
these names loosely, so they are fixed here.

| Term | What it is | Where it is | In code |
|---|---|---|---|
| **config dir** | The user-edited input root | `$XDG_CONFIG_HOME/jyc` or `~/.config/jyc`; Windows `%APPDATA%\jyc`. Holds `config.toml`, `skills/`, `templates/`, `subagents/`, `hooks`, `scripts/` | `jyc_utils::paths::config_dir()` |
| **plugin bundle** | The installable, shareable form of the four user-editable collections: a gzipped tar (or plain directory) holding `skills/`, `subagents/`, `hooks/` and `templates/` at its root — the layout `jyc plugins install` reads. Built by the release workflow as `jyc-plugins.tar.gz` and published with every release, nightly included; installing it is a copy into the collections' places under the config dir, never a merge into `config.toml` (which no bundle carries) | the release asset `dist/jyc-plugins.tar.gz`; any directory laid out that way | `cli/plugins.rs` |
| **data dir** | The generated-state root — the instance root | `$XDG_DATA_HOME/jyc` or `~/.local/share/jyc`; Windows `%LOCALAPPDATA%\jyc`. The default `--data-dir` | `jyc_utils::paths::data_dir()` |
| **channel** | The origin/destination of messages (email, github, gitee, feishu, websocket, …). A channel has **no topic directory**: topics never live under a channel | `[channels.<name>]` in the config; binds a pipe (`agent` + `topic` template) or is a hub channel | — |
| **channels dir** | The root of every channel's own runtime state | `<data dir>/channels` | `jyc_utils::paths::channels_dir()` |
| **channel dir** | One channel's runtime state — its IMAP / GitHub / Gitee cursors. Not a topic root, and never scanned as one | `<data dir>/channels/<channel>/` (`.imap/`, `.github/`, `.gitee/`) | `jyc_utils::paths::channels_dir(data_dir).join(<channel>)` |
| **agents dir** | The root of every agent, topic and state dir | `<data dir>/agents` (`~/.local/share/jyc/agents`) | `topic_path::resolve_agents_dir()`, `topic_path::agents_dir()`, `TopicManager::resolve_agents_dir()` |
| **agent dir** | One agent's home: the directory its default topic's state dir and default working dir live in | `<agents dir>/<agent>/` (`.jyc` + `.data`) | — |
| **home** | The directory a topic belongs to under the agents dir. It holds that topic's state dir and its default working dir, and it is chosen **once** — at the topic's first activation. A topic created before the `.data` split keeps the directory it was created with (`topic_work_dir`) | `<agents dir>/<agent>/<topic>/` for a pipe topic, `<agents dir>/<name>/` for an agent-keyed one (1:1 topic, `/spawn`); `.jyc` and `.data` are siblings inside it | `topic_path::{default_topic_dir, topic_home, topic_work_dir, topic_work_dir_at, record_topic_work_dir}` |
| **default topic** | The one topic every agent has, named after the agent — a message sent to the agent lands on it | home `<agents dir>/<agent>/`: `topic_path` is its `.data` unless pinned; state dir its `.jyc` | 1:1 topics in `TopicManager` |
| **topic** (non-default) | A topic a pipe creates from its rule's template (`plan-42`, `review-7`, …) | home `<agents dir>/<agent>/<topic>/`: `topic_path` is its `.data`, state dir its `.jyc` | router dynamic topics |
| **subagent type** | A named **role** a spawn hands its child: a topic config selected by name (`type = "review"`), defined by `[subagents.<type>]` in the config or by `<config dir>/subagents/<type>/config.toml`, and expanded into the child's own `.jyc/config.toml` at spawn time. Not a topic, not a channel, not a pattern: nothing routes to it, and nothing about it is inherited by anything else. Only the L1 config dir is scanned — unlike skills, a type is not layered. Resolution happens in the entry points, against the config the instance is *running* with (never a fresh read of the default path), and an installed type may also carry a `description` (one line of "when to spawn me", listed in the spawn tool's `type` argument and as `/spawn`'s popup row for the type, with its declared start defaults appended) and `[spawn]` (its defaults for the two start flags). The repo ships three at `subagents/review/`, `subagents/ci-watch/` and `subagents/develop/` (`cp -r subagents/<type> <config dir>/subagents/`) | `[subagents.<type>]`; `<config dir>/subagents/<type>/config.toml` | `jyc_types::load_subagent_type`, `spawn_handler::{resolve_type_arg, resolve_subagent_type, SubagentType}` |
| **subagent** | An agent `/spawn` (or `jyc_spawn_subagent`) created: an agent with its own default topic, not a second kind of topic | `<agents dir>/<subagent>/`, its state dir as any default topic's — or the parent's dir when the spawn was asked to share it (`/spawn -s/--share`, the tool's `share_topic_path`, or a role's own `[spawn]` default) | `spawn_handler` |
| **topic_path** | A topic's **working directory**: where the agent runs, what `read`/`write`/`bash` see, and what a parent is granted when it spawns a subagent. **Any** directory — a pin, a checkout, a `/spawn -d <dir>` child — not necessarily under the agents dir | its home's `.data` by default (`<agents dir>/<agent>/.data`, `<agents dir>/<agent>/<topic>/.data`); a pin is the working dir itself | `TopicManager::topic_path(name)` |
| **state dir** | A topic's `.jyc`: transcript, `agent-context.jsonl` (append-only raw context journal), `tasks`, `pattern`, `mode-override`, the lineage `spawned-from`, … — always under the agents dir, located by **topic name**, and always the home's `.jyc`: never inside a `topic_path` (which is the home's `.data`, a sibling) and never inside a pinned dir | see the table below | `jyc_types::state_dir::resolve_state_dir(name, topic_dir)` |
| **inbound dir** | Where the hub stages inbound attachment bytes between their upload and the message announcing them, deliberately outside every topic and channel dir | `<data dir>/.inbound` | `topic_path::inbound_dir()` |
| **L1 / L2 / L3** | Override layers only, not ownership | L1 = config dir; L2 = data dir; L3 = one topic (`topic_path` + state dir) | — |
| **cache hit rate** | The share of a topic's input tokens served from the provider's prompt cache — cache-hit tokens ÷ total input tokens. "Cache util" is the older name for the same figure | the info pane's "Turn" / "Session" rows under its "Cache:" header, `/info`, `/bill` | `jyc_types::cache_util_pct` |

Readings this table exists to rule out:

1. `--data-dir` (and "data dir" in older notes) means the **data dir**, not any
   topic's `topic_path`.
2. A **topic never lives under a channel**. `<data dir>/channels/<channel>/`
   holds cursors; the older `<data dir>/<channel>/workspace/<topic>/` shape is a
   pre-pipe leftover with no live writer (dormant data — its migration is a
   separate change).
3. The older wording "topic dir" means `topic_path`; and
   `resolve_state_dir(name, topic_dir)` returns the **state dir** — never
   `<topic_path>/.jyc`, which is no longer a fallback for anything: an
   unregistered lookup panics.
4. Two roots answer to "the agents dir" in code and coincide only by default:
   `topic_path::resolve_agents_dir()` (topic dirs; prefers the platform data
   home) and `topic_path::agents_dir(data_dir)` = `<data dir>/agents` (adopted
   state dirs; deliberately data-dir-based so multi-instance and test setups stay
   isolated). Where both are in play (cluster close and purge, both of which
   weigh a dir against either root) the data-dir-based one is spelled out at
   the call site as `agents_dir(&self.data_dir)`. With an explicit
   `--data-dir` the two differ.
5. "Cache util" and **cache hit rate** are the same number — the cache-hit
   tokens as a share of the total input tokens. The UI says "Turn" and
   "Session" under the info pane's "Cache:" header; the code keeps the
   older name (`cache_util_pct`).
6. **thread** is the older name for a **topic** and is gone from the
   user-facing surfaces: the dashboard's status chip counts topics
   (`2 active / 5 topic`), and the config layer rejects the older keys
   (`max_concurrent_threads`, `max_queue_size_per_thread`) instead of
   accepting them silently.

## Topic State Directory (.jyc)

Each topic keeps its runtime state (agent session, chat history,
exchange files, L3 topic config) in a `.jyc` directory resolved
through `jyc_types::state_dir::resolve_state_dir(topic_name, topic_dir)` — a process-global
registry keyed by **topic name** (the identity that distinguishes topics).
Find a topic among a listed set through `jyc_types::find_topic` /
`topic_index` / `has_topic` — by name, narrowed by channel when the caller
knows one — and never by `topic_path`, which a child sharing its parent's
directory also carries.
**Every topic's state must be registered before any state access: an
unregistered lookup panics instead of falling back to `<topic_dir>/.jyc`.**
That fallback once silently created state inside pinned user repos (#825).
Keying by name is what lets several agents pin the same directory while each
keeps its own state dir.

| Topic kind | topic_path (working dir) | state dir (`.jyc`) |
|---|---|---|
| Agent's default topic, unpinned | `<agents dir>/<agent>/.data` | `<agents dir>/<agent>/.jyc`, registered at startup restore |
| Agent's default topic, pinned (`topic_path`) | the pinned dir (e.g. a repo) | `<agents dir>/<agent>/.jyc` |
| Ad-hoc pinned topic (`jyc open -p`, non-agents pin) | the pinned dir | `<agents dir>/_<path-escaped>/.jyc` |
| Pipe topic (dynamic) | `<agents dir>/<agent>/<topic>/.data` | `<home>/.jyc`, registered at activation |
| Spawned topic (`/spawn`) | a fresh `<agents dir>/<name>/.data`, or any dir `-d/--dir` names outside the source topic's own dir (a non-empty checkout included — its files are not touched; nesting is refused) — or, with `-s/--share`, the **parent's** own dir | `<agents dir>/<name>/.jyc` — located by name, never `<topic_path>/.jyc` |
| Subagent (`jyc_spawn_subagent`) | the same shape a fresh-name `/spawn` gets — `<agents dir>/<name>/.data` with the parent's state seeded into it — or, with `share_topic_path`, the **parent's** dir (the `-s/--share` shape: that is what lets a reviewer read the work as it is) | `<agents dir>/<name>/.jyc` — located by name, never in the shared dir |
| Legacy channel workspace topic | `<data dir>/<channel>/workspace/<topic>/` | `<topic_path>/.jyc` — no live writer; it is dormant pre-pipe data, and its migration is a separate change |

Registrations happen at four points: startup restore (config pins adopt,
agent-keyed state dirs re-register from their breadcrumbs), runtime pins
(`set_topic_path`, dashboard `open -p`), spawn, and **activation** — every
enqueue path (router, `jyc_send_to_topic`, dashboard proxy)
registers a jyc-owned dir's **home** `.jyc` before any state access, which for
a working directory is `<home>/.data`'s parent. A user-owned dir (a pin)
without a registration is a bug: state access fails loudly rather than writing
into the user's directory.

A spawn's access grant for its parent is rebuilt by that same startup restore,
from the child's `spawned-from` breadcrumb and on the dir the `topic-path`
breadcrumb beside it names: the grant is process-lifetime, the breadcrumb is
not, so a restarted parent would otherwise stop finding its subagents
(`/close` would offer none) and lose its reach into their dirs. A `/grant` gets
no such rebuild — `-p` is what persists one.

### Working directory: the home's `.data`

A topic's default working directory is its **home's** `.data` — a sibling of
the state dir, never the home itself. That is why every framework-owned
directory is dot-prefixed (`.jyc` state, `.data` working dir, `.inbound`
staging, `.attachments` delivered files):

- **A name a user can own stays free.** The checkouts this system creates —
  a GitHub/Gitee init, a delegated developer's worktree — *are* the topic's
  working directory, so whatever the framework writes there lands in
  somebody's repository: a bare `attachments/` is a name real projects have,
  while `.attachments/` cannot be one.
- **The state is never inside the work.** `<home>/.jyc` and `<home>/.data`
  are siblings, so no working directory contains framework state at all and
  the "never commit `.jyc`" hazard has nothing left to guard.

The directory is chosen **once**, at the topic's first activation, and then
**recorded**: `<home>/.jyc/topic-path` holds it and
`topic_path::topic_work_dir_at` reads that record before the filesystem. The
record is what keeps the choice stable — `<home>/.jyc` is where the *new*
layout keeps the state dir, so its presence can no longer be read as "the home
is the working directory". A topic with no record — anything created before
this change — keeps the directory it was created with: its state in the home
itself, or nested at `<home>/<topic>/.jyc`, with the state dir untouched. Only
a fresh topic gets `<home>/.data`. A mixed instance is therefore expected after
this change, and no existing topic moves under an agent because of it.

Multiple agent rows may pin the same topic dir (e.g. one repo shared by
`jyc` and `jyc_git_planner`); they still get one state dir per topic name. A
`/spawn -s/--share` — or the tool's `share_topic_path`, or a role that declares
it — is that mechanism used deliberately: the child works on the parent's
files, so it co-pins the parent's dir while keeping its own state — its own
transcript as well, now that inheriting the conversation is opt-in
(`-i/--inherit`; see the subagent section for why). It is
therefore *not* subject to the "the dir is the topic" note below — its state was
registered by name before the dir was pinned (`adopt_state_dir` →
`set_topic_path`, and the latter keeps an existing registration). The name still
occupies a top-level entry under `agents/`, which is why a shared spawn refuses
a name that is already a topic.

A `/spawn` is the same mechanism pointed at a dir of its own, and it moves no
files: the parent's *settings* are what comes along (`--inherit` adds the
conversation and the transcript), so the destination may already hold
data (a checkout made elsewhere) but never sits inside the parent's dir —
nesting would inherit the parent's git status, file listings, and close
semantics — and the only
things a spawn creates are a missing destination and `<agents dir>/<name>/.jyc`.
Its state is registered by name even in that default shape, where the state dir is
the home's `.jyc` — a sibling of the `.data` the topic works in — so `close_topic`
removes the emptied working dir with its state (and the home once that leaves it
empty); a dir that has since gained files is kept, like a pin's. Purge is
the explicit "and this one too", refusing whenever the dir is not the topic's own
to remove: one another topic also uses (a workspace topic counts, not just a
runtime pin — a shared child works in its parent's dir), contains another topic's dir, is
one of jyc's own roots, or holds another topic's state directly under one of
those roots — which is why a spawn's default dir under `agents/` is never purged.

- **Identity.** Registrations key on the topic *name*. Config-key agents keep
  their `[agents.<key>]` name for the state dir — TOML keys are unique by
  construction. Pinned dirs without an agent key get a path-derived name:
  `_` is escaped to `__`, then `/` (and
  Windows `:`) become `_`, and the leading `/` is kept as `_`. Names around
  separator boundaries stay distinct (`/a/b_c` → `_a_b__c` ≠ `_a__b_c` ←
  `/a_b/c`); the only residual collision is a component ending in `_`
  directly adjacent to one starting with `_` (`/a_/b` vs `/a/_b`), which
  would make such sibling pins share one state dir. The `_` prefix is a
  reserved namespace that cannot collide with config keys.
- **Adoption.** `topic_path::adopt_state_dir(topic_name,
  topic_dir, state_dir)` registers the name→state mapping, creates the
  state dir, and writes the `topic-path` breadcrumb. **First adoption
  wins**: when the topic already has a registration (a runtime pin adopted
  before the config restore ran, or the reverse), the existing state dir is
  kept and the candidate is ignored — a topic's state location is chosen
  once and never moved. Sibling pins of one dir each start with their own
  clean state (no ping-pong). Repos stay clean; the "never commit .jyc"
  hazard disappears. Config pins adopt at startup inside
  `TopicManager::restore_custom_topic_paths`; runtime pins (`set_topic_path`,
  dashboard `open -p`) adopt immediately.
- **Restore.** Each adopted state dir carries `topic-name` and
  `topic-path` breadcrumbs. At startup `restore_state_registry` scans
  `<data dir>/agents/*/.jyc` and re-registers each mapping by the
  `topic-name` breadcrumb (falling back to the folder name for
  config-key dirs predating the breadcrumb; derived `_` dirs with no
  breadcrumb are skipped — they are re-adopted deterministically when the
  dir is re-opened). Runs before channels start and also bootstraps
  config-less external subprocesses (hooks and CLI-spawned helpers, which
  receive the topic name via `JYC_TOPIC_NAME` at spawn); config pins
  additionally adopt from config — identical mappings, idempotent.
- **Close.** `/close --force` on an adopted topic deletes the state dir
  registered for that topic *name* **and the home it lived in**, contents and
  all: the `<home>` a topic works in (its `.data` and whatever was written
  there), the `<agents dir>/<topic>/` shell a pin left behind, the default dir
  a `/spawn` gets (not one a `-d` or a pin handed it). Two dirs are never
  taken: one **outside the agents
  tree** — a checkout a `-d` or a pin points at is the user's, and only its
  state goes — and one **another topic works in or lives inside** (a
  `/spawn -s` child, a fork, the topics of the pipe that owns the dir; the last
  of these is read off the disk, because the pin map only knows the topics that
  are live). Nothing *above* the home goes either: a pipe's own dir
  (`<agents dir>/<pipe>/`) stays when one of its topics closes — it is the home
  only of the pipe topic named after it, which takes it with it; the boundary
  is the home, not the pipe. The registration itself is kept
  so a reopened topic reuses the same state location (never the topic dir). A
  sibling topic co-pinning the same dir keeps its own state. Unregistered
  topics keep the legacy whole-dir deletion.
  An **upstream close event** (issue/PR closed, chat disbanded) goes the same
  way — the home/pipe boundary is drawn in one teardown, shared by every close
  source — and puts one rule in front of it: a topic whose working dir is not
  under the agents tree is skipped whole, not even closed. A topic reopened
  upstream (issue closed, then reopened) starts with an empty dir.
  Closing also closes the topic's **subagents**, before anything of its own is
  touched: they exist to serve its work, the grant is what makes them findable
  at all, and a shared one (below) would otherwise point into a directory this
  close is about to remove. That is also what keeps the guards above from
  tripping over them: a shared subagent's pin is a *borrowed* one, so
  `other_topic_dirs` ignores a pin whose topic is marked closed (which the
  cascade itself did) and one held by a subagent of the topic being purged
  (which is the one path that purges first: `/close --force --purge`). A
  shared child's co-pin never gets that far: the cascade closes it first.
  Ad-hoc pins (no config key) derive their state name from the dir, so two
  runtime topics opening one dir intentionally share — and closing either
  destroys — that single state: "the dir is the topic". Closing also
  **freezes** the topic's state (a process-global closed set in
  `jyc_types::state_dir`): bookkeeping writes that land after the deletion —
  the close reply's chat-history line, activity-log entries from event
  subscribers — are dropped rather than recreating the deleted directory.
  The freeze lifts when the topic reopens (its worker starts).
- **Agent access.** When the state dir lies outside the topic dir it is
  appended to the agent's additional read/write roots (parity with the
  pre-refactor in-dir `.jyc`), and the system prompt's Chat History
  section shows the absolute path.

### Subagents (`jyc_spawn_subagent`)

A topic's agent can create a **subagent**: a sibling topic that inherits this
topic's settings and runs on its own queue. `/spawn` and the `jyc_spawn_subagent`
tool are one implementation (`spawn_handler::spawn_under`) with two triggers —
the user typed the command, or the agent called the tool. Two things about the
child's *starting point* are the caller's to decide, and both default to the
more independent choice: `inherit_context` (or `/spawn -i/--inherit`) copies the
conversation and transcript as well, and `share_topic_path` (or `/spawn
-s/--share`; a role may also default it) puts the
child in the parent's directory — the shape a reviewer of the work as it is
needs. A **type** may state those two defaults for itself
(`[spawn]` in its own file), and an argument passed at spawn time overrides them:
the role knows how it has to run. The tool's argument can say otherwise in
either direction, an explicit `false` included; `/spawn`'s `-i` and `-s` can
only turn the defaults on, never off (see `AGENTS.md` on that asymmetry).

1. Resolve the name and topic_path (`/spawn` from its flags — the type is its one
   positional, `-n/--name` and `-d/--dir` the name and the directory, and the
   rest fall to the role's `[spawn]` defaults — the tool from a single name under
   `agents dir`, or from the parent's own dir when it was asked to share), then
   create that dir and adopt its state dir: always
   `<agents root>/<name>/.jyc`, even when the child was pointed at a named path
   (one order for every trigger: `adopt_state_dir` first, so `set_topic_path`
   keeps the in-dir state instead of deriving `_<path>` state under
   `agents_dir(data_dir)`).
2. Seed the parent's state with `seed_state(…, "spawned-from")` — the seed
   allow-list minus the conversation-shaped entries (pattern, backlog,
   `mode-override`, model/context/MCP/skill overrides; the task list is never
   inherited, so a child plans its own work), never a state directory
   and never `agent-session.json`, and never the workspace. The conversation
   (`agent-context.jsonl`) and the transcript (`chat_history_*.jsonl`) are copied
   only when they were asked for: a child starting on a task of its own would
   otherwise pay for the parent's whole train of thought, in tokens, on every
   turn — and arrive anchored to it, which is the opposite of the independent
   view a delegated review is for. [`.jyc` ownership][state] stays one topic per directory:
   the seeded copy is the child's own, and the breadcrumb
   `<name>/.jyc/spawned-from` = the parent's topic name is the only file added
   on top of it.
3. Grant the parent read+write access to the child's **topic_path** through the
   existing runtime grant registry (`jyc_types::access_grants`, the `/grant`
   mechanism), so both access-root resolvers in `jyc-agent/src/service/prompt.rs`
   pick it up on the next turn with no resolver change. The entry also records
   the child's **name** (`grant_subagent`), which is what
   `spawn_handler::subagents_of` reads: the path cannot identify a child, since a
   shared topic_path is the parent's own dir and may hold more than one child —
   they are separate grants, and `/grant`'s own unnamed entries are access, not
   lineage. The state dir is not granted and must not be: it is the child's own.
4. Report the subagent list, which the system prompt also renders every turn
   (`format_subagents_section`) so the model continues an existing subagent with
   `jyc_send_to_topic` instead of spawning a duplicate.

A **subagent type** (`type = "review"`) changes what step 2 hands over: the
role's settings are written to the child's `.jyc/config.toml`, a config layer the
child's own `/mode`, `/model` and `/skill` still outrank, and of the runtime
overrides `seed_state` copied, only the ones whose key the type sets itself are
removed — each of those sits *above* a topic config in its chain, so a kept one
would silently outrank the role, while a key the type has no opinion about keeps
what the parent was running. The mapping is key-for-key with no exceptions: the
chain reads `.jyc/<mode>-model-override` for the mode the caller asks for (a
declared `mode` changes which file that is), so a declared `plan_model` takes out
the plan file, a declared `build_model` the build one, `[ai].model` both, and
nothing else. The other mode's file is left in place and dormant: a later
`/mode` switch inherits the parent's choice on that side.

An **installed** type (`<config dir>/subagents/<type>/config.toml`) carries two
keys a topic config has no place for, and both exist for the agent that has to
*choose* a role: `description`, one line of "when to spawn me" listed beside the
name in `jyc_spawn_subagent`'s own `type` argument (so a parent that was never
told about a role can still recognize the moment for it — the list, the refusal
for a bad name and the spawn's own resolution all read the same two sources), and
`[spawn]`, the role's defaults for the two start flags above. `[subagents.<type>]`
in the config is a topic config and has neither: a type defined only there is
listed by name alone.

The child's **starting model** is filled in the same step, from the parent's own:
the trigger resolves it with `session_state::resolve_effective_model` (the
effective mode, then `resolve_model` for it) and `seed_start_model` writes it to
`.jyc/model-override` — the mode-agnostic fallback, read whenever the
mode-specific file for the child's own mode is missing, so a leftover file for a
*different* mode cannot hide it (the file for its own mode is the parent's choice
for that mode, and wins) — unless the type names a model of its own (the same
three `[ai]` keys). `seed_state` alone cannot do this: it copies *files*, and a parent
whose model comes from a topic config, a pattern or the global config has none to
copy, while the child resolving its own chain may land somewhere else — a typed
child's mode comes from its config, so it looks up the topic-config layer rather
than the file its build-mode parent read. It is a starting point, not a pin: the
child's own `/model` still wins over it.

A **shared** subagent (`share_topic_path`) takes the parent's dir and nothing
else of it: its own state stays in `<agents root>/<name>/.jyc`, which is what
keeps the close from ever touching that directory when the child closes. The
child's pin is a borrowed one: it is closed with the parent (below), and the
guards above read a pin's lineage, not just its presence, so a shared child
neither blocks its parent's dir from going nor claims it as somebody else's.

Because the grant comes from the same place `/spawn` writes it, a hand-typed
spawn is a subagent too: it shows up in the list, in the parent's read+write
boundary, and the two forms are one feature rather than two. That holds whichever
form the command took, because both readers (`subagents_of`, `forget_subagent`)
locate the lineage by resolving the child's **state dir** from the granted dir's
name (`spawn_handler::state_dir_of`) rather than assuming `<granted dir>/.jyc` —
a `/spawn -d ../test` child runs beside its parent while its state stays under the
agents root. The same lookup is what `/close` uses to revoke, so a path-form
spawn gives the parent's access back when it closes.

Everything after that is the parent's own work with tools it already has:
`write`/`bash` seed the directory, `jyc_send_to_topic` (with
`require_reply=true`) sends the task and carries the result back as a new message
to the parent's topic. Reconfiguring the child needs no new tool either: the
worker runs commands for *every* queued message, so sending `/build`, `/model`,
`/context`, … as the body of a message to the child topic switches it, and an
empty remaining body stops before the model call — the child spends no turn on
it. A command's receipt is broadcast to the child's own chat pane (the websocket
outbound keys on the original topic), so the parent reads
`<child>/.jyc/mode-override` (which it may read) if it wants to confirm.

Guardrails, all of them the tool's: the name is validated here — `spawn_under`
tells a name from a path by shape, so an unchecked `../evil` would be resolved as
a path, off the agents root. Nothing bounds how many subagents a topic spawns or
how deep they go: a subagent may spawn its own subagent, and the close cascade
below is depth-first because of it. A closed subagent revokes its parent's grant
in `TopicManager::close_topic_under` — which `auto_close_topic` and `purge` route
through, so every teardown path revokes — and a closed *parent* closes its
subagents there too (depth-first, children first, so a nested chain goes whole),
which is what stops an orphan from pointing into a directory the close
took with it.

One deliberate non-guardrail: because the worker runs commands for *every* queued
message, any topic's agent can send itself a command body and switch its own mode
(`/build`) — the plan mode is a working convention, not a sandbox. The same
mechanism is what lets a parent reconfigure a child from outside.

Known boundaries: grants are process-global and in-memory (same "temporary"
semantics as `/grant`), so a restart drops the access, not the subagent; the
cap and the prompt list then go quiet too, since both read the same grants. A
restarted hub can be re-granted by the user's `/grant` or by a fresh spawn. The
subagent's queue is in-memory as well: a task enqueued but not yet consumed when
the process stops is lost. Its topic_path is its own by default and the parent's
when it was asked to share — either way the state is the child's, and either way
the child goes when the parent does. `/grant`ing a child's directory again after
a restart re-lists it only when that directory is named after the child, which is
the shape every default spawn has and a shared one does not: a shared child that
a restart forgot is reached by its name (the parent knows it) or replaced by a
fresh spawn, not re-discovered from its parent's own directory. The registry is also keyed by the parent's
**name alone**, with no channel in the key: a topic of the same name in another
channel reads the same subagent list (shows it, counts it against the cap, and
now closes and deletes it), which is the price of one process-wide registry
shared by `access_grants` and `/grant`. The fix, when it is worth paying for, is
a channel-scoped grant key rather than a local guard here.

[state]: #topic-state-directory-jyc
## Lifecycle Hooks

Operator-configured external commands (`[[hooks]]` global +
`[[agents.<name>.hooks]]`, merged in that order exactly like
`[[commands]]`) run at agent-layer chokepoints via argv spawn with the
event JSON on stdin; exit 2 blocks the action, everything else fails
open. Design decisions:

- **Agent layer only** (two-layer deployment: outer channels forward, the
  websocket-based agent does the AI work): `message_received` +
  `session_start(startup)` gate in `worker::process_message`, tool events
  wrap `ToolRegistry::execute` (hooks attached to the cached, per-agent
  registry so config reload refreshes them), `reply_send` gates every AI
  reply delivery path (watcher/tool/fallback), session events fire in the
  `/reset`/`/new`/`/close` handlers (`session_end` before teardown so
  hooks can archive).
- **Dual dialect**: the config event spelling (jyc snake_case vs Claude
  Code PascalCase) selects the stdin payload shape — CC-shaped payloads
  make existing CC hook scripts reusable. Best-effort fields: the jyc
  dialect always carries `hook_event_name`/`agent`/`topic`/`cwd` (CC:
  `hook_event_name`/`session_id`/`cwd`); everything else — channel,
  sender, `metadata` (forwarded senders ride it), tool/reply fields —
  appears only when the site has it.
- **Executor** lives in `jyc-utils::hooks` (both `jyc-core` and
  `jyc-agent` depend on it); the vocabulary + config types live in
  `jyc-types`. Hook sets compile per message (regex cost ≈ µs, same
  per-message build pattern as the command registry).
- **Trust boundary**: config-file only — never topic-local `.jyc/`
  definitions (the agent writes that directory; loading commands from it
  would be prompt-injection with a shell). Payload is untrusted input for
  the script. Full docs: `docs/hooks.md`.

## Image Input & Multimodal Support

JYC supports vision-capable models through two complementary paths:

### 1. Inbound Auto-Injection

When a pattern sets `inject_inbound_images = true` and the active model has `supports_images = true`, image attachments on matching messages are automatically base64-encoded and appended as `ContentBlock::Image` to the first user turn.

**Flow:**
```
InboundMessage with image attachment
  → MessageRouter stores attachment to topic dir
  → TopicManager calls agent.process()
  → Agent checks: supports_images? && pattern.inject_inbound_images?
  → build_user_blocks() reads image bytes from saved_path
  → base64-encodes → ContentBlock::Image
  → Provider::format_user_message() emits provider-specific wire format
     - Anthropic: {"type":"image","source":{"type":"base64",...}}
     - OpenAI-compat: {"type":"image_url","image_url":{"url":"data:image/..."}}
```

### 2. Agent-Driven `read_image` Tool

Registered automatically when the provider has `supports_images() == true`. The model can call it mid-loop with either `path` or `url`.

**Boundary checking**: `path` is validated against the working directory + the system temp dir (always allowed, so tools have scratch space; a degenerate `$TMPDIR` of `/` is ignored) + configured read roots (`additional_read_roots`) + write roots (`additional_write_roots`, since write implies read). Roots are populated from per-pattern `access.read` and `access.write` config. `url` must be http/https.

**Side-channel queue**: Images loaded mid-loop are pushed onto `ToolContext.pending_images`. The agent loop drains this queue after each tool batch and emits a synthetic user-role turn carrying the image blocks. This avoids embedding base64 in `tool_result` content (unsupported by most OpenAI-compat servers for `role: "tool"`).

### 3. Vision Fallback (Non-Vision Models)

When the active model does NOT have `supports_images = true`, `read_image` falls back to a vision-capable provider (e.g. DeepSeek-VL) for OCR/description, then returns the extracted text. This lets text-only models still reason about images.

### Configuration

```toml
[agents.my_bot]
inject_inbound_images = true

[agent.providers.deepseek]
type = "openai-compatible"
base_url = "https://api.deepseek.com"
api_key = "${DEEPSEEK_API_KEY}"
supports_images = true

[agent.providers.deepseek.models.deepseek-v4-flash]
context_window = 64000
supports_images = true      # Model-level overrides provider-level
```

## Feishu Channel Implementation

Feishu (飞书) and WeCom Bot (`wecom_bot`) are migrated to the **channels / agents / core+AI**
architecture — see [docs/architecture/overview.md](docs/architecture/overview.md). They
are pipe-only channels: they speak the channel protocol and pipe messages into
an agent's websocket channel; they have no TopicManager, agent service, state, or
outbound adapter of their own. Feishu was the first; wecom_bot follows the
same pattern (with a streaming-reply twist for the WeCom passive reply window).

### Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    Feishu Platform (Cloud)                   │
│  ┌──────────────┐    API Calls    ┌────────────────────┐    │
│  │  Feishu API  │◄───────────────►│ Feishu WebSocket   │    │
│  │   (REST)     │                 │    (Real-time)     │    │
│  └──────▲───────┘                 └─────────┬──────────┘    │
└─────────┼────────────────────────────────────┼──────────────┘
          │ relay replies                      │ WebSocket events
          │ (text + attachments)               ▼
┌─────────┴───────────────────────────────────────────────────┐
│                 JYC Feishu Adapter (pipe-only)               │
│                                                              │
│  FeishuWebSocket (websocket.rs)                              │
│   • LarkWsClient connection, reconnect, event parsing        │
│   • im.message.receive_v1 → InboundMessage (+ name lookups)  │
│                                                              │
│  FeishuMatcher (inbound.rs)                                  │
│   • pattern rules: mentions / keywords / chat_name / sender  │
│                                                              │
│  Pipe wiring (jyc-cli serve/channels/)                       │
│   • apply_pipe_retarget: rewrite channel/topic, resolve      │
│     ${msg.chat_name}, record topic→chat_id mapping           │
│   • route through the agent channel's own MessageRouter     │
│   • reply forwarder: subscribe agent broadcast, relay via    │
│     FeishuClient (attachments via inspect files endpoint)    │
│                                                              │
│  FeishuClient (client.rs)                                    │
│   • auth/token management, send text/image/file, upload      │
│   • name lookup: get_chat_name, get_user_name (cached)       │
└─────────────────────────────────────────────────────────────┘
```

### Configuration

```toml
[channels.feishu_bot]
type = "feishu"

[channels.feishu_bot.feishu]
app_id = "${FEISHU_APP_ID}"
app_secret = "${FEISHU_APP_SECRET}"
base_url = "https://open.feishu.cn"

[[channels.feishu_bot.patterns]]
name = "mention_bot"
enabled = true
# Required: pipe into an agent. A matched pattern without `pipe` drops the
# message with a warning.
pipe = { agent = "jin", topic = "${msg.chat_name}" }

[channels.feishu_bot.patterns.rules]
mentions = ["jyc"]
```
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
│  │  detect labels  │         │                    │                   │
│  │  detect closes   │         │                    │                   │
│  │  detect edits    │         │                    │                   │
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

JYC supports a planner/developer/reviewer workflow on GitHub issues and PRs. Agent hand-off is driven by **labels**, not mentions:

```
User creates issue with "jyc:plan" label
    │
    ├── [Planner] analyzes codebase, discusses with user
    │   [Planner] creates empty PR with implementation plan
    │   [Planner] adds "jyc:develop" label to PR
    │       ↓
    ├── "jyc:develop" label detected → Developer triggered
    │       ↓
    │   [Developer] implements code per plan
    │   [Developer] pushes commits, adds "jyc:review" label to PR
    │       ↓
    ├── "jyc:review" label detected → Reviewer triggered
    │       ↓
    │   [Reviewer] reviews code changes
    │   [Reviewer] approves or requests changes by adding "jyc:develop" label
    │       ↓
    └── Cycle continues until PR is approved and merged
```

Each role maps to a pattern with a `role` field and a template that defines the agent's behavior. Templates are in `templates/github-{planner,developer,reviewer}/`.

### Label-Based Routing

Routing is driven by **labels** on issues and PRs. Patterns with a `role` field get an implicit routing label:

| Role        | Routing Label |
| ----------- | ------------- |
| `Developer` | `jyc:develop` |
| `Reviewer`  | `jyc:review`  |
| `Planner`   | `jyc:plan`    |

1. **Auto-label**: When a pattern has a `role` field, the routing label is automatically included in label matching (OR-merged with any explicit `labels` in pattern config)
2. **Matching**: `GithubMatcher::match_message()` iterates patterns, checking `rules_match()` against github_type, labels, and assignees
3. **Self-loop prevention**: If the comment's `[Role]` prefix matches the target pattern's role, the pattern is skipped (prevents a `[Developer]` comment from re-triggering the developer)
4. **Rule filtering**: After label matching, `rules_match()` validates `github_type`, `labels`, and `assignees` rules (AND logic between rules, OR logic within each rule)

### Label Change Detection

Adding labels to existing issues/PRs triggers routing:

1. Each poll cycle tracks seen issues with key `number:sorted_labels`
2. When labels change (e.g., user adds `jyc:plan`), the new key is not in the seen set
3. A trigger message is generated for the issue/PR, allowing Pattern-mode patterns to match on the new label
4. This enables users to manually add routing labels to route work to agents

### Pattern Rule Filtering

When a pattern's rules are evaluated, all rules must match (AND logic):

```
Pattern "developer" (rules: github_type=["pull_request"], labels=[["ready-for-dev"]])
    │
    ├── Check github_type: message is "pull_request" → ✓
    ├── Check labels: PR has "ready-for-dev" label → ✓
    │   (auto-label "jyc:develop" is OR-merged with explicit labels)
    │   → MATCH
    │
Pattern "developer-default" (rules: github_type=["pull_request"])
    │
    ├── Check github_type: message is "pull_request" → ✓
    ├── No labels rule → ✓ (auto-label still applied for routing)
    │   → MATCH
    │
Pattern "restricted-dev" (rules: github_type=["pull_request"], assignees=["alice"])
    │
    ├── Check github_type: message is "pull_request" → ✓
    ├── Check assignees: PR assigned to "bob" → ✗
    │   → SKIP (rules don't match)
```

````

Rules:
- **`github_type`**: `"issue"` or `"pull_request"` — OR logic within the list
- **`labels`**: `LabelRule` — supports flat OR (`["bug", "enhancement"]`) or nested AND-OR/CNF (`[["bug", "enhancement"], ["test"]]`), case-insensitive. Auto-label from `role` is OR-merged with explicit labels.
- **`assignees`**: Match if ANY assignee on the issue/PR is in the pattern's assignee list — case-insensitive
- **`exclude_labels`**: If ANY label on the issue/PR matches, the pattern is excluded — OR logic
- All present rules use **AND logic** (all must pass). `None` rules are considered matched.

### Close Detection

The adapter detects issue/PR close events by comparing cached state:

1. Each poll cycle fetches all open issue/PR numbers
2. Previously-open issues not in the current set are considered closed
3. Closed topics trigger the `on_topic_close` callback (deletes the topic directory — for a pipe topic that is `<agents dir>/<agent>/<topic>` with its contents, empty or not)
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
pipe = { agent = "planner" }

[channels.my_repo.patterns.rules]
github_type = ["issue"]

[[channels.my_repo.patterns]]
name = "developer"
enabled = true
pipe = { agent = "developer" }

[channels.my_repo.patterns.rules]
github_type = ["pull_request"]
assignees = ["alice", "bob"]      # Optional: restrict to specific assignees

[[channels.my_repo.patterns]]
name = "reviewer"
enabled = true
pipe = { agent = "reviewer" }

[channels.my_repo.patterns.rules]
github_type = ["pull_request"]

# The agents carry the topic configuration (a pipe pattern carries
# routing only).
[agents.planner]
role = "Planner"
template = "github-planner"

[agents.developer]
role = "Developer"
template = "github-developer"

[agents.reviewer]
role = "Reviewer"
template = "github-reviewer"
````

### Topic Naming

Topic name is `{prefix}-{N}` where `{prefix}` comes from the matched
pattern's optional `topic_prefix` config field, falling back to a default
based on event type when no prefix is configured.

| GitHub Type  | Pattern config              | Topic Name     |
| ------------ | --------------------------- | --------------- |
| Issue        | (no `topic_prefix`)        | `issue-{N}`     |
| Pull Request | (no `topic_prefix`)        | `pr-{N}`        |
| any          | `topic_prefix = "plan"`    | `plan-{N}`      |
| any          | `topic_prefix = "review-pr"` | `review-pr-{N}` |

When two patterns can match the same GitHub identity (e.g., both target
issues but are distinguished by labels), they MUST declare distinct
`topic_prefix` values. Otherwise both patterns route to the same workspace
directory; the second pattern's template / `AGENTS.md` would silently be
dropped. jyc detects this on the second message and refuses to dispatch it
with a `TemplateMismatch` error.

The reviewer pattern in particular must declare `topic_prefix = "review-pr"`
explicitly so it does not collide with the developer pattern (default
`pr-{N}`) on the same PR.

### Testing

Comprehensive unit tests (14 tests for rule filtering alone) cover:

- Label-based routing (auto-label matching, role-to-label mapping)
- Self-loop prevention (own-role skip, cross-role pass)
- Rule filtering (github_type, labels, assignees — AND/OR logic, case-insensitive)
- Pattern fallback (skip first pattern on rule failure, match second)
- Topic name derivation (issue, PR, reviewer prefix)
- Persistent comment tracking (track, reload, edit detection, compaction)
- Label change detection (new label triggers routing)
- Trigger message building (issue and PR variants)

## Configuration Layering

JYC separates **user-edited configuration** from **generated data** and supports three-level layering of `config.toml`, `skills/`, and `templates/` (#393).

### Platform Paths

Resolved via the `dirs` crate (`jyc-utils/src/paths.rs`). On Unix (Linux/macOS) the XDG Base Directory convention is used; on Windows native `dirs` paths.

| Platform | Config dir (L1) | Data dir (L2) |
|---|---|---|
| Linux/macOS | `$XDG_CONFIG_HOME/jyc` (`~/.config/jyc`) | `$XDG_DATA_HOME/jyc` (`~/.local/share/jyc`) |
| Windows | `%APPDATA%\jyc` | `%LOCALAPPDATA%\jyc` |

- **Config dir** holds user-edited files: `config.toml`, `skills/`, `templates/`.
- **Data dir** (data root, the instance root) holds generated state: the agents dir (`agents/<agent>/<topic>/`), the channels dir (`channels/<channel>/.imap/`, `channels/<channel>/.github/`), the inbound dir (`.inbound/`).
- `jyc serve` without `--data-dir` uses the data dir (never the current directory). Without `--config`, the config is `<config dir>/config.toml`; with an explicit `--data-dir` but no `--config`, it's `<data dir>/config.toml`.
- **First run**: if the default config is missing on a flag-less invocation, jyc creates it from `config.example.toml` (plus empty `skills/` and `templates/`), prints edit instructions, and exits.

### The Three Levels

- **L1 (global)**: `<config dir>/` — `config.toml`, `skills/`, `templates/`, `subagents/<type>/config.toml` (subagent types; see Terminology — L1 only, types are not layered)
- **L2 (data dir / data root)**: `--data-dir` if given, else the data dir — `config.toml` (via `--config`), `skills/`, `templates/`, plus all generated state
- **L3 (topic)**: the topic's state dir — `<home>/.jyc/` (see Terminology: `topic_path` is the working dir, `.data` beside it) — `config.toml` (the topic's own overrides, see the table below), `skills/`, `templates/`, sessions, chat history, task list (`tasks.json`), the agent's own lessons (`lessons.md`)

### Merge & Lookup Rules

| Asset | Rule |
|---|---|
| `config.toml` (L1/L2) | Deep merge at `toml::Value` level (`merge_toml`): tables merge recursively, arrays/scalars replaced by L2. `${VAR}` expansion after merge. Applies to startup and hot-reload (`load_config_layered`). |
| `config.toml` (L3) | `TopicConfig`: `[ai]` models (`model`, `plan_model`, `build_model`, `small_model`), `[[mcps]]` + `mcps_replace`, and the topic's own `mode`, `skills` (allow-list), `disabled_tools`, `instructions`. Invalid files are ignored with a warning. |
| Model precedence | `.jyc/<mode>-model-override` file (then the legacy `.jyc/model-override`) > L3 `config.toml` > pattern > L2/L1 config. One implementation for every consumer — see **One model resolver** below. |
| Mode precedence | `.jyc/mode-override` (the user's `/mode`) > L3 `config.toml` > pattern > build. A configured mode is never written to `.jyc/mode-override`, which stays reserved for explicit user overrides. |
| Skill filters | Allow-list only, and **opt-in**: L3 `skills` **replaces** pattern/channel (it is the narrowest statement of intent); no layer declaring `skills` means *none* — the default is an empty list, not "everything found". The list is what the system prompt lists (context cost); the runtime `/skill` toggle layers on top of it (`on` adds, `off` removes, both per topic). Every discovered skill stays invocable via `/skill:<name>`, whatever the list says. |
| `disabled_tools` | Union of channel ∪ pattern ∪ L3 `config.toml` — subtractive at every layer. |
| Subagent types | `[subagents.<type>]` in the config, or `<config dir>/subagents/<type>/config.toml` (L1 only); both are `TopicConfig`, merged key by key with the table winning. `spawn_handler::resolve_type_arg` resolves the name against the **live** config — the one the instance was started with (`--config` included) and reloads into — from the two entry points that hold it (`/spawn`'s `CommandContext`, the tool's shared `ArcSwap`); a refusal before anything is created, `apply_subagent_type` expands it into the child's state dir at spawn time. |
| `skills/` | All levels scanned low→high; same-named skills are overridden by higher levels. Order: `~/.config/jyc/skills`, `~/.claude/skills` → L1 → L2 → topic repo → L3. |
| `templates/` | Lookup L3 → L2 → L1 (`TemplateDirs::resolve_with_topic`); first match wins. |
| Custom `topic_path` | Absolute/`~` paths used as-is (outside the data root); **relative paths resolve against the data root** (previously process cwd). L3 applies to any topic directory, including ad-hoc topics (`jyc open <path>`). |

> **Merge limitation**: L2 can override any L1 value but cannot *remove* entries (e.g. a channel defined in L1 cannot be deleted by L2). If removal is needed, either omit the channel from L1 or add it with `enabled = false` in L2.

**One model resolver.** `jyc_core::session_state::resolve_model(topic_name,
topic_path, config, channel, mode, pattern_name, topic_agent_cfg)` is the only
implementation of the model row above, and the `mode` it is asked for is the one
`resolve_effective_mode` resolved for the topic (file > topic config > pattern >
build). Its callers: the agent runtime, which builds the provider with the value
it returns; the display source `TopicManager::topic_display_state`, so the
dashboard topic list, the Feishu status card and `/info` render the model the
agent actually runs; the context-window resolver; and the `/spawn` and
spawn-tool triggers, which use `resolve_effective_model` (mode, then model) to
hand a child its starting model.

Before this the chain existed three times and had drifted: the display had no
topic-config layer at all and put the channel's generic `model` above the global
per-mode fields, and each consumer picked the `<mode>-model-override` file by its
own notion of "mode" (the runtime by the file's mode, `/model` and the display by
the effective one). That last difference was user-visible: in a topic whose mode
comes from a config layer — a typed subagent — `/model X` wrote
`plan-model-override` while the turn read `build-model-override`, so the command
reported success, the pane showed X and the agent kept running the old model.

**Self-containment invariant**: all generated state follows the *effective data root* — a custom `--data-dir` instance never touches the platform data dir, keeping parallel instances and tests isolated.

## Config Hot-Reload Architecture

JYC supports live configuration reload via the Dashboard `R` key without restarting the monitor process. The architecture uses a layered approach based on the "thermal volatility" of each config type:

```
config.toml (disk)
    │
    ▼
[Reload] → handle_reload_config()
    │
    ├── ArcSwap<AppConfig>  ← Config atomically replaced
    │       │
    │       ▼
    │   MessageRouter::route()  ← reads patterns from live config
    │   (pattern changes take effect immediately)
    │
    └── reload_callback
            │
            ▼
        ChannelOrchestrator::reload()
            │
            ├── removed channels → cancel + graceful shutdown
            ├── new channels → warn (requires monitor restart)
            └── InspectContext updated via ArcSwap
```

### What Reload Covers

| Change | Effect |
|--------|--------|
| Add/modify pattern | ✅ Takes effect immediately |
| Delete channel | ✅ Channel cancelled gracefully |
| Add new channel | ⚠️ Requires monitor restart |
| Change connection params (host/port) | ⚠️ Requires monitor restart |
| Change `general.max_concurrent_topics` | ⚠️ Requires process restart |

### Components

- **ChannelOrchestrator** — Manages channel task lifecycle. Holds per-channel `ChannelHandle` (cancel token, topic manager ref). On reload, computes diff between old and new config section names, cancels removed channels with a 5-second grace period for cleanup. New channels are detected and logged with a warning (the monitor must be restarted to spawn new channel tasks).
- **InspectContext (ArcSwap)** — `channels` and `workspace_dirs` are wrapped in `Arc<ArcSwap<Vec<...>>>`, allowing the orchestrator to atomically swap updated lists after reload; `topic_managers` is the process's `Arc<TopicManagers>` (see the `jyc_send_to_topic` section). The dashboard reads the latest snapshot on each poll cycle.
- **MessageRouter** — Holds `Arc<ArcSwap<AppConfig>>` and the channel name. On each `route()` call, reads `config.load().channels[name].patterns` dynamically. No cached pattern snapshot — pattern changes take effect on the next message after reload.

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
    pub references: Option<Vec<String>>,     // Email: References header
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
    pub staged_path: Option<PathBuf>,         // Pipe-uploaded file, moved into the topic by the worker
}

/// Pattern matching result
#[derive(Debug, Clone)]
pub struct PatternMatch {
    pub pattern_name: String,
    pub channel: ChannelType,
    pub matches: HashMap<String, String>,     // Channel-specific match details
}

/// Inbound adapter trait — one per channel type
/// Channel-specific message matching and topic name derivation.
///
/// Pure-logic trait used by MessageRouter. Every channel type implements this.
/// Separated from InboundAdapter to allow use without the lifecycle (start/stop).
/// Stateless implementations (EmailMatcher, FeishuMatcher) can be cheaply created.
pub trait ChannelMatcher: Send + Sync {
    fn channel_type(&self) -> &str;

    fn derive_topic_name(
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
    /// Callback for topic close events (e.g., Feishu chat.disbanded)
    pub on_topic_close: Option<Box<dyn Fn(String) -> Result<()> + Send + Sync>>,
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
        topic_path: &Path,
        message_dir: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult>;

    async fn send_message(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
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
    pub template: Option<String>,             // Topic template name
    pub topic_name: Option<String>,          // Fixed topic name override
    pub topic_prefix: Option<String>,        // Topic name prefix (e.g. "issue", "pr")
    pub topic_path: Option<String>,          // Custom filesystem path for topic dir
    pub role: Option<String>,                 // Agent role (e.g., "Planner", "Developer", "Reviewer")
    #[serde(default = "default_true")]
    pub live_injection: bool,                 // Inject into active AI session (default: true)
    pub inject_inbound_images: bool,          // Auto-inject image attachments (default: false)
    pub model: Option<String>,                // Per-pattern model override
    pub plan_model: Option<String>,           // Model override for plan mode
    pub build_model: Option<String>,          // Model override for build mode
    pub small_model: Option<String>,          // Per-pattern small_model override
    pub mcps: Option<Vec<McpServerConfig>>,   // Per-pattern MCP server configs
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
    pub labels: Option<LabelRule>,            // Label matching (OR or AND-OR logic, see LabelRule)
    pub assignees: Option<Vec<String>>,       // Assignees to match (OR logic, case-insensitive)
    pub exclude_labels: Option<Vec<String>>,  // Labels that must NOT be present (OR logic)
}

/// Label matching rule supporting flat OR logic and nested AND-OR (CNF) logic.
///
/// Uses `#[serde(untagged)]` for backward compatibility: a flat `["bug"]`
/// deserializes as `Flat`, while `[["bug"], ["test"]]` deserializes as `Nested`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LabelRule {
    /// Flat: `["bug", "enhancement"]` → OR logic (at least one label must match)
    Flat(Vec<String>),
    /// Nested: `[["bug", "enhancement"], ["test"]]` → CNF (AND of ORs)
    /// Each inner group must have at least one matching label.
    Nested(Vec<Vec<String>>),
}

#[derive(Debug, Clone, Deserialize)]
pub struct SenderRule {
    pub exact: Option<Vec<String>>,           // Case-insensitive exact match
    pub domain: Option<Vec<String>>,          // Domain match (email only)
    pub regex: Option<String>,                // Regex match
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubjectRule {
    pub prefix: Option<Vec<String>>,          // Prefix match (stripped from topic name)
    pub regex: Option<String>,                // Regex match
}
```

### Topic Name Derivation

Each channel's `ChannelMatcher` implements `derive_topic_name(message, patterns, pattern_match)` with channel-specific logic:

- **Email**: Strip reply prefixes (Re:, Fwd:, 回复:, 转发:), strip configured subject prefix (e.g., "Jiny:"), sanitize for filesystem. Supports broad separator recognition (`:`, `-`, `_`, `~`, `|`, `/`, `&`, `$`, etc.)
- **FeiShu**: Derive from chat name (via `get_chat_name` with caching) or message content
- **GitHub**: `{prefix}-{N}` where `{prefix}` is the matched pattern's `topic_prefix` config value, or `issue` / `pr` by default based on event type. See "Topic Naming" above for the full rule.
- **Slack** (future): Derive from channel name + topic topic

## Async Event Queue Architecture

### Overview

JYC uses **Tokio** as its async runtime. The message processing pipeline is built on a hierarchy of `tokio::sync::mpsc` channels and a `Semaphore` for bounded concurrency.

### Channel & Task Topology

```
                    ┌─────────────────────┐
                    │  Tokio Runtime       │
                    │  (multi-topiced)    │
                    └─────────────────────┘
                              │
              ┌───────────────┴───────────────┐
              ▼                               ▼
     ┌────────────────┐               ┌────────────────┐
     │ tokio::spawn   │               │ tokio::spawn   │
     │ IMAP Monitor   │               │ FeiShu Monitor │
     │ (channel: work)│               │ (WebSocket)    │
     └───────┬────────┘               └───────┬────────┘
             │                                │
             ▼                                ▼
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
    │ Topic Queue  │  │ Topic Queue  │   │ Topic Queue  │
    │ "topic-A"   │  │ "topic-B"   │   │ "topic-C"   │
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

### Topic Manager: Semaphore + Per-Topic mpsc

```rust
pub struct TopicManager {
    /// Per-topic bounded mpsc channels
    topic_queues: Mutex<HashMap<String, mpsc::Sender<QueueItem>>>,

    /// Bounds concurrent topic workers
    semaphore: Arc<Semaphore>,

    /// Configuration
    max_queue_size: usize,              // mpsc buffer size (default: 10)

    /// Shared dependencies (wrapped in Arc for worker tasks)
    storage: Arc<MessageStorage>,
    outbound: Arc<dyn OutboundAdapter>, // Channel-agnostic outbound
    agent: Arc<dyn AgentService>,

    /// Topic-isolated event buses (Topic Event system)
    event_buses: Mutex<HashMap<String, TopicEventBusRef>>,
    enable_events: bool,

    /// Graceful shutdown (child token — cancelling this does NOT cancel other channels)
    cancel: CancellationToken,

    /// Worker join handles for cleanup
    worker_handles: Mutex<Vec<JoinHandle<()>>>,
}
```

**Enqueue flow:**

```rust
impl TopicManager {
    pub async fn enqueue(
        &self,
        message: InboundMessage,
        topic_name: String,
        pattern_match: PatternMatch,
    ) {
        let mut queues = self.topic_queues.lock().await;

        if let Some(sender) = queues.get(&topic_name) {
            // Topic queue exists — try_send (non-blocking)
            match sender.try_send(QueueItem { message, pattern_match }) {
                Ok(()) => return,
                Err(TrySendError::Full(_)) => {
                    tracing::warn!(topic = %topic_name, "Queue full, dropping message");
                    return;
                }
                Err(TrySendError::Closed(_)) => {
                    // Worker finished, remove stale queue and recreate below
                    queues.remove(&topic_name);
                }
            }
        }

        // Create new topic queue + spawn worker
        let (tx, rx) = mpsc::channel(self.max_queue_size_per_topic);
        tx.try_send(QueueItem { message, pattern_match }).ok();
        queues.insert(topic_name.clone(), tx);

        let handle = self.spawn_worker(topic_name.clone(), rx);
        self.worker_handles.lock().await.push(handle);
    }

    fn spawn_worker(
        &self,
        topic_name: String,
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

            tracing::info!(topic = %topic_name, "Worker started");

            // Topic Event system setup
            let (current_message_tx, current_message_rx) = tokio::sync::watch::channel(None);
            let event_listener_handle = if enable_events {
                // Create topic-isolated event bus
                let event_bus = Arc::new(SimpleThreadEventBus::new(10));
                // Set event bus for agent service
                let _ = agent.set_topic_event_bus(&topic_name, Some(event_bus.clone())).await;

                // Start event listener (forwards events to subscribers)
                Some(tokio::spawn(async move {
                    Self::event_listener(
                        event_bus,
                        topic_name.clone(),
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
                    &item, &topic_name, &agent, &storage, /* ... */
                ).await {
                    tracing::error!(
                        topic = %topic_name,
                        error = %e,
                        "Failed to process message"
                    );
                }

                // Clear current message after processing
                let _ = current_message_tx.send(None);
            }

            tracing::info!(topic = %topic_name, "Worker finished");
            // _permit dropped here → semaphore slot freed
        })
    }
}
```

**Key properties:**

- **Bounded concurrency**: `Semaphore(3)` — at most 3 topics process messages simultaneously
- **Per-topic ordering**: Each topic's `mpsc::Receiver` ensures FIFO order. Messages arriving during AI processing are injected live into the session (not queued).
- **Back-pressure**: `mpsc::channel(10)` — `try_send` fails when queue is full (message dropped)
- **Graceful shutdown**: `CancellationToken` propagates to all workers and monitors
- **Automatic cleanup**: Worker tasks exit when their mpsc channel closes (all senders dropped) or on cancellation. Semaphore permits are released on `_permit` drop.
- **Topic Event System**: Each topic has an isolated event bus; events are forwarded to subscribers (e.g. inspect dashboard).

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

### SSE Streaming: Agent Processing

```
┌─────────────────────────────────────────────────────────────────────┐
│              agent SSE stream processing                          │
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

**Topic Event Integration with SSE:**

- **SSE Event Conversion**: agent client converts SSE events to TopicEvents
- **Event Types Converted**:
  - `ProcessingStarted` → `TopicEvent::ProcessingStarted`
  - `ProcessingProgress` → `TopicEvent::ProcessingProgress`
  - `ProcessingCompleted` → `TopicEvent::ProcessingCompleted`
  - `ToolStarted` → `TopicEvent::ToolStarted`
  - `ToolCompleted` → `TopicEvent::ToolCompleted`
  - `server.heartbeat` → ignored (connection keep-alive only)
- **Event Publishing**: Events are published to topic-isolated event bus
- **Topic Manager Monitoring**: Listens for events and forwards them to subscribers (inspect dashboard).

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
        ├──> TopicManager workers: finish current message → exit
        │    (in-queue messages are lost — IMAP re-fetch on restart)
        │
        ├──> agent server: explicitly stopped via server.stop()
        │
        └──> SMTP connections: disconnect

 All JoinHandles awaited → process exits cleanly
```

### Agent Server Process Lifecycle on Shutdown

| Scenario                 | Server killed?  | How?                                                                                                                   |
| ------------------------ | --------------- | ---------------------------------------------------------------------------------------------------------------------- |
| Ctrl+C (graceful)        | Yes             | `agent_server.stop()` explicitly kills it during shutdown sequence                                                  |
| jyc panics               | Yes             | `kill_on_drop(true)` on the child process — Rust drop runs during unwind                                               |
| SIGTERM to jyc           | Yes             | Same as panic — drop destructors run                                                                                   |
| SIGKILL (kill -9) to jyc | **No** — orphan | Destructors don't run. agent process stays alive on its ephemeral port. Harmless — next jyc start picks a new port. |

### Cancellation Token Hierarchy

```
root_cancel (top-level)
    │
    ├── imap_monitor_cancel (per channel)
    │       └── signals IMAP IDLE to abort
    │
    ├── topic_manager_cancel
    │       └── all worker tasks check this
    │
    └── agent_service_cancel
            └── aborts SSE streams
```

## Topic Manager & Queue

### Per-Topic Queue with Semaphore-Bounded Concurrency

(See the Async Event Queue Architecture section above for the full `TopicManager` design with code.)

**Key properties:**

- **Inbound channels run as concurrent tokio tasks** — Email monitor and FeiShu monitor listen simultaneously
- **Fire-and-forget enqueue** — MessageRouter sends into mpsc and returns immediately
- **Each topic has its own mpsc channel** — FIFO order preserved within a conversation
- **One worker per topic** — Sequential processing (order matters for conversation coherence)
- **Different topics process in parallel** — Up to `max_concurrent_topics` (default: 3) via `Semaphore`
- **In-memory queues** — Lost on restart; IMAP re-fetch handles recovery
- **Queue overflow** — Messages dropped with warning when mpsc buffer is full

### Live Message Injection

When a user sends a follow-up message while the AI is still processing the first message in the same topic, the follow-up is injected into the ongoing turn rather than waiting in the queue.

**Behavior:**

- Message 2 arrives while AI processes Message 1 → Message 2 joins the running turn → the model sees it in the next call it makes → one combined reply
- Message 2 arrives after AI finished Message 1 → normal sequential processing → two separate replies

**The flag:** `live_injection`, per channel pattern, default `true`. With `false` the follow-up waits in the queue for a turn of its own — the behavior every release had before the flag was wired up.

**The backlog:** the messages the turn took in and the model has not read yet are part of the topic's backlog — the user sent them and the model has not seen them — so the surfaces that report the backlog (the info pane's **Status** row, `/info`) report them too. The turn ends the count with itself.

**How it works (in-process agent):**

1. The worker owns the topic's queue receiver and selects on it while the turn runs. A slash command is executed immediately through the command registry (`/cancel`, `/model` — instant feedback even during a 429 retry). A plain message is handed to the agent through a per-turn injection channel.
2. The agent loop drains that channel at the top of every iteration — the whole batch, in arrival order — and folds each message into the conversation as a user message: the internal history and the provider-formatted raw context both grow by one message.
3. The next LLM call therefore carries the injected messages. The model reads them as a natural follow-up and adjusts its work; the turn's reply covers them.
4. Each folded message is appended to the topic's chat log, so the transcript shows what the user said.
5. A message the turn never picks up — the turn ended before its next drain point — is drained by the worker and re-queued, so it becomes a turn of its own instead of being lost.

**Injection format:** The body only — no framing, no instructions. This matches how the agent TUI handles messages sent during AI processing. The AI treats it as a natural follow-up in the conversation. The body the model reads is cleaned the way the turn's own body is cleaned (the email channel strips the quoted history of a reply), so a follow-up email does not feed the model the whole quoted thread; the transcript keeps the raw words. A body that cleans to nothing keeps its raw words, so the model is never blind to a message the transcript shows.

```
Please also add a chart to the PPT.
```

**Only plain text is injected:** a message carrying attachments or a template needs the full turn pipeline — its own storage, hooks and reply target — so it keeps a turn of its own. A message headed for another directory (its topic path points elsewhere than the running turn's) keeps its own turn too. A topic path that merely names the topic's own directory is not a reason to wait: the worker registers that directory the first time it stores a message, so every message after the topic's first carries one.

**On the wire:** the Anthropic protocol carries a tool result as a `user` message, so an injected message would put two `user` turns back to back. The Anthropic provider merges consecutive `user` turns into one before sending — what the Messages API does anyway, and what the strict routes built on it (Bedrock, Vertex, and the gateways that re-implement the validation) require: `messages: roles must alternate between "user" and "assistant"`. The same shape arises from an image a tool loaded, so the merge covers both.

**The moderation gate still applies:** a `message_received` hook that blocks the message keeps it out of the running turn — it waits for a turn of its own, where the same hook blocks it again and the chat log records it. An injected message reaches the model just as much as a message that starts a turn, so it passes the same gate.

**Ordering:** an injected message overtakes the messages already waiting. A message that cannot be injected (an attachment, a template, a topic path pointing elsewhere than the running turn's) and arrived earlier is re-queued and processed *after* the injected content was folded into the running turn and answered: the absorbed turn comes first, the queued one follows.

## Worker (Agent Service)

### Responsibility Separation: TopicManager vs AgentService vs OutboundAdapter

The processing pipeline is split into three layers with distinct responsibilities:

**`AgentService` trait** (`src/services/agent.rs`) — Channel-agnostic AI brain:

```rust
#[async_trait]
pub trait AgentService: Send + Sync {
    async fn process(
        &self, message: &InboundMessage, topic_name: &str,
        topic_path: &Path, message_dir: &str,
        pending_rx: &mut mpsc::Receiver<QueueItem>,
    ) -> Result<AgentResult>;
}

pub struct AgentResult {
    pub reply_sent_by_tool: bool,   // MCP tool already handled full lifecycle
    pub reply_text: Option<String>, // Raw AI text for outbound adapter
}
```

Each agent mode implements this trait. Adding a new agent requires only implementing `AgentService` + a match arm in `cli/serve.rs`.

**TopicManager** (`src/core/topic_manager.rs`) — Orchestrator:

- Queue management: per-topic mpsc channels, semaphore-bounded concurrency
- Message storage: append to daily chat log (`chat_history_YYYY-MM-DD.jsonl`), save attachments
- Command processing: parse/execute/strip email commands, send command results
- Agent dispatch: calls `agent.process()` via `Arc<dyn AgentService>`
- Fallback: passes raw AI text to outbound adapter if MCP tool wasn't used
- Does NOT know about: sessions, prompts, SSE, reply formatting, email quoting

**OutboundAdapter** (e.g. `crates/jyc-inspect/src/server/websocket/outbound.rs`) — Channel-specific reply lifecycle:

- Builds channel-formatted reply (footer, formatting)
- Sends via channel transport
  (email is now a pipe-only channel — see `docs/architecture/overview.md`)
- Appends reply to chat log
- Different channels (FeiShu, Slack) would implement different formatting + transport

**AgentService** (`src/services/agent/service.rs`) implements `AgentService`:

- Server lifecycle: ensure agent service is running, health check, auto-restart
- Topic setup: write per-topic `agent config` with model, MCP tools, permissions
- Session management: reuse/create sessions, staleness detection
- Prompt building: system prompt + user prompt
- SSE streaming: activity timeout, tool detection, progress logging
- Error recovery: ContextOverflow → new session, stale session → retry
- Returns raw AI text — does NOT format, send, or store replies

**StaticAgentService** (`src/services/static_agent.rs`) implements `AgentService`:

- Returns configured static text — does NOT format, send, or store

```rust
// TopicManager dispatches to agent, then outbound:
let result = agent.process(&message, topic_name, topic_path, message_dir, &mut rx).await?;

if !result.reply_sent_by_tool {
    if let Some(ref text) = result.reply_text {
        // Outbound adapter owns: format + send + store
        outbound.send_reply(&message, text, topic_path, message_dir, None).await?;
    }
}
```

This separation:

- **Agent** is channel-agnostic — returns raw text, no email/FeiShu knowledge
- **OutboundAdapter** owns the full reply lifecycle — format + send + store
- **TopicManager** is a thin orchestrator — dispatch to agent, pass result to outbound
- Adding a new channel requires only a new OutboundAdapter implementation
- Adding a new AI backend requires only a new AgentService implementation

### Session-Based Topic Management

Each topic has a dedicated agent session persisted in `agent-session.json`. This enables:

- **Memory** — AI remembers previous replies in the conversation
- **Coherence** — Consistent responses across the topic
- **Context** — Conversation history maintained by agent session memory (not injected into prompt)
- **Debugging** — Can inspect/replay sessions in in-process agent TUI

### in-process agent Service Architecture

```rust
pub struct AgentService {
    /// HTTP client for in-process agent API
    client: reqwest::Client,

    /// agent service process
    server: Mutex<Option<in-process agentServer>>,

    /// Configuration
    config: Arc<AppConfig>,

    /// Binary path for the reply tool subcommand
    reply_tool_command: Vec<String>,
}

struct in-process agentServer {
    port: u16,
    process: Child,  // tokio::process::Child
}
```

**Server lifecycle:**

- Single shared agent service handles all topics
- Started via `jyc serve --hostname=127.0.0.1 --port=<port>`
- Readiness detected by parsing stdout for `"jyc server listening on http://..."`
- Auto-started on first request, auto-finds free port (49152+)
- Health check before reuse: `GET /session` with 3s timeout
- Server lives until CLI exits

### in-process agent Server HTTP API Reference

Full API documentation: **<https://agent.ai/docs/server/>**

JYC uses the following subset of the agent service API:

| Method | Path                        | Purpose                         | Notes                                           |
| ------ | --------------------------- | ------------------------------- | ----------------------------------------------- |
| `GET`  | `/session`                  | Health check / list sessions    | Used for liveness probe                         |
| `POST` | `/session`                  | Create a new session            | Body: `{ title }`                               |
| `GET`  | `/session/:id`              | Get session details             | Returns 404 if not found                        |
| `POST` | `/session/:id/prompt_async` | Send prompt asynchronously      | Body: `{ system, agent?, parts }`. Returns 204. |
| `POST` | `/session/:id/message`      | Send prompt and wait (blocking) | Same body format. Returns `{ info, parts }`.    |
| `GET`  | `/event`                    | SSE event stream (global)       | First event: `server.connected`                 |

**Key API conventions:**

- **Directory context**: Passed via `x-agent-directory` HTTP header (URL-encoded path), NOT as a query parameter
- **Model selection**: Passed per-prompt via `PromptRequest.model` — session is preserved across model switches
- **Prompt body**: `{ system: string, model?: string, agent?: "plan", parts: [{ type: "text", text: string }] }`
- **SSE events**: Event type is in the JSON data field as `{ "type": "...", "properties": {...} }` — NOT in the SSE `event:` field

**SSE event types used:**

| Event Type             | Purpose                           | Key Fields                                                                   |
| ---------------------- | --------------------------------- | ---------------------------------------------------------------------------- |
| `server.connected`     | Stream handshake                  | —                                                                            |
| `message.updated`      | Model info                        | `properties.info.{ sessionID, modelID, providerID }`                         |
| `message.part.updated` | Content/tool updates              | `properties.part.{ id, sessionID, type, text, tool, state }`                 |
| `session.status`       | Processing status                 | `properties.{ sessionID, status.type }`                                      |
| `session.idle`         | Prompt complete                   | `properties.sessionID`                                                       |
| `session.error`        | Session error                     | `properties.{ sessionID, error.name }`                                       |
| `step.finish`          | Step completion with token counts | `properties.step.{ id, sessionID, cost, inputTokens, outputTokens, reason }` |

**Per-topic configuration:**

- Each topic gets its own `agent config` with model settings, MCP tool config, and permissions
- `permission: { "*": "allow", "question": "deny", "external_directory": "deny" }` — headless mode, no interactive terminal, no access outside topic directory
- Staleness check detects changes → rewrites config → restarts server
- Model and mode are passed per-prompt via `PromptRequest.model` and `PromptRequest.agent` — no session restart needed for switches

### in-process agent Server Architecture Diagram

```
┌─────────────────────────────────────────────────────────────┐
│                    AgentService                          │
│                                                             │
│  Single Server (auto-port: 49152+)                          │
│       ↓                                                     │
│  Shared reqwest::Client                                     │
│       ↓                                                     │
│  ┌─────────────────────────────────────┐                    │
│  │ Sessions (per-topic directory)     │                    │
│  │                                     │                    │
│  │ Topic A → agent-session.json + .agent/│                    │
│  │ Topic B → agent-session.json + .agent/│                    │
│  │ Topic C → agent-session.json + .agent/│                    │
│  └─────────────────────────────────────┘                    │
│                                                             │
│  Server lives until CLI exits                               │
└─────────────────────────────────────────────────────────────┘
```

### Worker Processing Flow

```
┌─ TopicManager (src/core/topic_manager.rs) ─────────────────────────┐
│                                                                       │
│  Worker picks message from topic queue                               │
│         │                                                             │
│         ▼                                                             │
│  ┌──────────────────────────────────────────┐                         │
│  │ 1. STORE                                 │                         │
│  │    MessageStorage::store(msg, topic)     │                         │
│  │    → chat_history_YYYY-MM-DD.jsonl (appended)  │                         │
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
│  Worker picks next message from topic queue                  │      │
└───────────────────────────────────────────────────────────────┘      │
                                                                       │
┌─ AgentService (src/services/agent/service.rs) ─────────────────┘
│
│  1. Ensure agent service is running (auto-start, health check)
│  2. ensure_topic_agent_setup(topic_path)
│     → resolves the topic's model for its mode (`.jyc/<mode>-model-override`,
│       else `.jyc/model-override`, else the config layers — see
│       "One model resolver"), takes priority over the configured default
│     → writes agent config with model, MCP config, permissions
│     → staleness check: skip write if unchanged
│  3. Get or create session (.jyc/agent-session.json)
│     - Record the model's context window (the budget the next call fits into)
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
│    1. Subscribe to SSE events ({ directory: topic_path })
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
│  in-process agent finishes: the final assistant text IS the reply
│         ↓
│  Agent loop delivers it (crates/jyc-agent/src/tools/mod.rs::deliver_reply):
│    1. With a live reply target: build full_reply_text = AI reply +
│       quoted history (email only), send directly via the pre-warmed
│       outbound adapter (eliminates cold-start timeouts)
│    2. Without one (background turns, tests): write .jyc/reply.md +
│       .jyc/reply-sent.flag signal files for the file relay
│    3. MessageStorage::store_reply() → append to chat log
│         ↓
│  Handle result → return GenerateReplyResult:
│    - reply_delivered: true (direct send, or signal file queued) → done
│    - Stale session (tool reported success, signal file missing)
│        → delete session, create new, retry once
│    - ContextOverflow → new session + blocking retry
│    - SSE failure → blocking prompt fallback
│    - No reply text → nothing to deliver
│
└─ Returns GenerateReplyResult to TopicManager ──────────────────────────
```

**Key flow rules:**

1. **Commands are always processed first** — before any AI interaction
2. **Command results are always sent** as a direct reply (if commands were found)
3. **Body emptiness is checked AFTER both command stripping AND quoted history stripping** — leftover quoted history from inline reply formats does not count as real content
4. **Empty body → stop** — no agent service started, no AI processing, no wasted API calls
5. **Non-empty body → dispatch to agent mode** — static text or in-process agent

**Session lifecycle:**

- Sessions are created on first use per topic and persisted in `.jyc/agent-session.json`
- Sessions are reused across messages, model switches, mode switches, and container restarts
- Sessions track the context size (`context_input_tokens` + `context_output_tokens`) and the model's context window (`context_window`)
- Sessions are never deleted automatically — the anchored sliding-window history is bounded by turns (`window` / `max_window`), with `max_history_tokens` as a second slide trigger (uncapped when unset)
- On session reset: the session state and the raw context are cleared — the next turn starts from an empty context



1. **in-process agent Session (Primary)** — Conversation memory maintained by in-process agent
   - Session is reused across messages in the same topic (`agent-session.json`)
   - AI remembers previous messages and replies within the session
   - Session is deleted only on an explicit reset (`/reset`, dashboard) — never automatically
   - Session state and raw context are loaded from disk on server restart

2. **Context-window accounting** — How full the model's context window is
   - **The figure**:
     - The context size is the most recent LLM call's `usage.input_tokens` plus that call's `usage.output_tokens` — the size of the context the *next* call will carry
     - The budget is the model's own context window (`context_window`), detected from the model config
     - The share is `(input + output) / context_window`, shown as a percentage
   - **Token tracking**:
     - Real-time token counting from SSE `step-finish` events
     - Immediate persistence to `agent-session.json` after each step
     - Token usage displayed in the reply footer and the info pane (e.g., `Tokens: 20.7K/122K`)
   - **No auto-reset**: the sliding-window strategy slides — drops whole turns off its head — when it outgrows its turn bound, or on the token budget at the top of that cycle (unbounded when `max_history_tokens` is unset); the session is never reset automatically
   - **Standardized units**:
     - Uses 1024 as K unit basis (1K = 1024 tokens)
     - Display format: `{current}K/{max}K` with 0.1K precision
   - **Context preservation**:
     - Chat history is preserved in `chat_history_YYYY-MM-DD.jsonl` files
     - AI can read chat history for context continuity after session reset

3. **Session State Data Structure** (`src/services/agent/session.rs`)

   ```rust
   /// Per-topic session state, persisted in `.jyc/agent-session.json`.
   #[derive(Debug, Clone, Serialize, Deserialize)]
   pub struct SessionState {
       pub created_at: String,
       /// The input the most recent LLM call was sent (latest step-finish SSE event)
       #[serde(default)]
       pub context_input_tokens: u64,
       /// The output tokens of that same call — the reply appended to the context
       #[serde(default)]
       pub context_output_tokens: u64,
       /// The model's context window — the whole budget the next call fits into
       #[serde(default, alias = "max_input_tokens")]
       pub context_window: u64,
       // ... the accumulating counters (totals, cache hits, cost, latency)
   }
   ```

4. **Configuration** (`config.toml`):

   ```toml
   [agent]
   # The context window is detected from the model config
   # (`[agent.providers.<p>.models.<m>] context_window`); no session
   # threshold is configured any more.
   ```

5. **User Interface Enhancements**:
   - **Reply footer display**: Shows the context size and the context window
     - Format: `Model: ark/deepseek-v3.2 | Mode: build | Tokens: 20.7K/122K`
     - Context size: the last call's input + output, in K units (1024 basis) with 0.1 precision
     - Window: the model's own context window

6. **Incoming Message (Current)** — Latest message being processed
   - Body stripped of quoted reply history (`strip_quoted_history()`)
   - Topic cleaned of repeated Reply/Fwd prefixes (at ingest time by InboundAdapter)
   - Limited to 2,000 chars

7. **Topic Files (Durable, for quoted history only)** — Markdown files stored in topic folder
   - Used by `build_full_reply_text()` for quoted history in reply emails
   - NOT loaded into the AI prompt

8. **Wire Payload Shaping (Context Strategy)** — How the prior `agent-context.jsonl`
   is shaped into the wire payload on each LLM request. The on-disk
   file always stores the full raw context unchanged; only the request
   payload is shaped. Configured per-pattern / per-agent / globally
   (`context_strategy`), overridable at runtime via `.jyc/context-strategy.json`
   (written by `/context full | sliding [N] | reset`).

   * `full` (default) — send the entire `agent-context.jsonl`.
   * `sliding_window` — anchored: `agent-session.json` persists
     `context_offset`, the journal byte offset where the window's oldest
     retained turn begins, and each load reads the journal forward from
     there. The wire payload is the history block — the prior turns as
     text-only pairs, append-only, never trimmed — followed by the
     current turn in two parts: once it passes 16 assistant steps, the
     older steps fold into a text-only checkpoint (same representation as
     history, so their reasoning and tool results leave the wire), and
     the recent suffix stays verbatim with structured tool calls and
     results intact. The checkpoint boundary advances one group of 16 at
     a time, so it is byte-stable between advances and a cold build equals
     a cached one. The window
     grows by append until it holds more than `max_window = window +
     round(extend_rate × window)` turns — or has reached that bound and
     exceeds `max_history_tokens`' weight, the budget being a second
     trigger for the same slide rather than a per-turn one — and then
     **slides**: the anchor moves forward so the last `window` turns
     remain, whole turns at a time.
     Slides are the only way history shrinks — a per-turn trim of the
     head is what destroys the cross-turn prefix cache. The prefix is
     therefore append-only up to the end of the history block; within a
     turn only the recent suffix is re-encoded from one call to the next,
     and the checkpoint changes at most once per 16 assistant steps. A
     cold start (fresh topic, reset, no anchor, or an anchor no longer on
     a turn boundary) reads the journal once from its head and drops the
     bad anchor; the next slide persists a fresh one.

   Resolution chain: runtime override file > the topic's own config
   (`[context_strategy]` in `<topic>/.jyc/config.toml` — including one a
   subagent type expanded into the child at spawn time, which is how a role
   starts with a small window) > matched pattern (or
   synthesized `[agents.<name>]` pattern) > first pattern fallback
   > global `[ai].context_strategy` > default. The chain itself is
   `jyc_core::session_state::resolve_context_strategy` (its callers consult the
   runtime file first, and hand it the topic's own table);
   `build_send_context` (`crates/jyc-agent/src/agent_loop/context.rs`) only
   shapes what that returned:
   for `full` it returns `Cow::Borrowed(&raw_context)`; for
   `sliding_window` it returns `Cow::Owned` of `[windowed_prior,
   current_turn_checkpoint, recent_current_turn]`. Synthetic messages use
   the provider's
   own formatter, so Anthropic (`content: [...]`) and OpenAI-compat
   (`content: "..."`) wire formats both stay valid.

   Full mechanism reference (turn pairing, what a request looks like,
   token safety nets, configuration): [docs/architecture/context.md](docs/architecture/context.md).

   **`max_history_tokens` (2026-10-18; corrected 2026-10-28).** `window`
   counts turns, so `window = 10` costs 2k tokens on a quiet topic and
   200k on one whose turns carry huge tool results. The budget is the
   orthogonal lever, but it may not trim: an earlier version dropped the
   oldest history per message on every turn, which moved the window's
   head on every turn and destroyed the cross-turn prefix cache (and could
   never get under the budget anyway, since folding turns into notes left
   the total unchanged). It is now a **second trigger for the same
   slide**: when the window has already reached `max_window` turns and its
   estimated weight exceeds the budget, it slides back to `window` turns —
   whole turns, anchor forward, exactly what the turn bound does. Below
   `max_window` it never fires, because a window whose `window` turns
   alone exceed the budget cannot get under it. The current turn is
   excluded: the turn in flight has `turn_tool_result_cap` for a single
   result and the 16-assistant-step checkpoint for older steps. The estimate is a
   heuristic, not a tokenizer — CJK characters ×1, other bytes ÷4, plus a
   small per-message constant — because a tokenizer dependency would buy
   precision this knob does not need. Unset by default.

   **System Prompt Stability (2026-10-18).** The system prompt is
   **unchanged by runtime churn on purpose** — everything except the
   boundary's access-grant lines, which track enforcement by design.
   Providers that cache by prefix (OpenAI-compat: DashScope/DeepSeek/
   MiniMax/Kimi automatic context caching; Anthropic: explicit
   `cache_control` breakpoints) only reuse a cached span when its bytes
   are unchanged. Anything else that changes while the instance runs
   therefore belongs in
   the turn's **user message**, which is new every turn anyway and so
   sits past the cached prefix regardless of how often it churns:

   | Content | Home | Why |
   |---------|------|-----|
   | Access boundaries, skills, `AGENTS.md`, reply/history/VCS rules, static cross-topic prose | system prompt | Changes only when config or files do — **except** the boundary's runtime grant lines, below |
   | Mode block (`<mode>`) | user message | `BUILD`↔`PLAN` switches per turn |
   | Subagent roster (`Your subagents (N): …`) | user message | A child is spawned or closed |
   | Channel/topic roster | — | Not in the prompt at all: the `list_topics` tool fetches it on demand |

   **The extended tools.** A tool schema rides in every request, so a topic
   that never delegates, never publishes, never fetches a URL, never browses
   its transcript, never records a lesson and never needs a multi-step plan
   pays for those schemas on every call. The twelve tools most topics use
   rarely — `jyc_spawn_subagent`, `jyc_send_to_topic`, `list_topics`,
   `jyc_publish_file`, `read_image`, `webfetch`, `context_browse`,
   `lesson_add`, `lesson_read`, `task_list`, `task_create`, `task_update` —
   are therefore **registered but not advertised**: the registry filters them
   out of the `tools` array until the model calls `load_extended_tools`, whose
   description is the index of what the group holds. `jyc_send_message` stays
   in the core advertised set: an attachment-bearing reply must be
   discoverable without a loader call. The task tools are in the group
   because whether work needs a plan is a project convention, not a
   framework-wide cost; the loader points multi-step work at them, and that
   sentence is omitted when `disabled_tools` removes every task tool. The
   loader names both groups explicitly. The flag lives in the registry, which
   is cached per topic, so a load sticks for the rest of that topic's registry
   lifetime (a config reload rebuilds it and starts over). Only
   *advertisement* is gated: the tools stay executable, so a model that names
   one before loading it gets a working call instead of an error — the group
   is a cost optimization, and `disabled_tools` remains the boundary that
   removes a tool for good, the loader included (the loader is registered after
   the exclusion pass, so the pass is applied to it once more). The advertised
   set went from 6,925 chars of schema text to about 2,974 (−57%), and the
   tools array is re-read on every agent loop iteration, so the schemas are in
   the *next* request of the *same* turn.

   **The MCP tools.** The same argument applies to an MCP server's tools, and
   harder: a server with fifty tools puts fifty schemas in the request of a
   topic that wanted one of them. The registry therefore builds one **group
   per MCP server** at registry-build time, and the group's tools are
   **registered but not advertised** until the model calls `load_mcp_schema`
   with that server's name. The loader's description is the index: each
   server's name, and each of its tools with a one-line description — enough
   to decide *which* server to load, and none of the schemas themselves. The
   expansion is per tool, not per server (`tool_names` expands a subset), so
   a server with a hundred tools can be brought in a few at a time. The
   groups live in the registry, which is cached per topic, so an expansion
   sticks for the rest of that topic's registry lifetime.

   The boundary is the same as for the extended group, and the same rule
   applies: only *advertisement* is gated. The MCP tools stay registered and
   executable, so a model that names one before expanding it gets a working
   call instead of an error; `disabled_tools` and `disabled_mcps` remain the
   boundary that removes a tool or a server for good, the loader included. A
   server removed that way is not in the index either — the index is built
   from what actually landed in the registry, so a tool excluded by plain
   name is in no group.

   **One name, one owner.** The registry is keyed by tool name, so a name
   belongs to whatever holds it last. Ownership is therefore read from the
   live registry — `Tool::source()`, which an MCP-backed tool answers with its
   server's name and a built-in or bridge tool answers with `None` — rather
   than from a snapshot taken when the groups were built. An MCP server that
   exposes a built-in's name has taken that name over: the tool belongs to
   the MCP group and stays hidden until the model expands that server, and
   the built-in's group does not claim it as well — otherwise the tool would
   need *both* groups loaded, and each loader would report it as available
   while it is still hidden. The loader names are reserved for the same
   reason: `load_mcp_schema` and `load_extended_tools` stay the framework's,
   and an MCP server exposing a tool under one of those names has that tool
   skipped — the loaders are how the model reaches the MCP tools at all.

   What this does **not** change is the connection policy: the index needs
   the servers' tool names, so the servers are still connected at registry
   build time. The saving is the per-request schema cost, not the startup
   handshake or the subprocess.

   **What still moves the prompt, and why that is correct.** Spawning a
   child calls `access_grants::grant_subagent()`, which registers the
   child's directory as a read+write grant; the root resolvers fold
   granted paths into the additional roots and `format_access_boundary()`
   prints each as its own line. Closing the child drops the grant and the
   line with it, so `prompt_hash` moves. That text must describe exactly
   the roots the tool layer enforces — a boundary that drifts from
   enforcement is a security bug, not a cache problem — so it stays where
   it is. (`share_topic_path = true` children are already excluded: their
   directory is the parent's own, and the resolver skips it.) The win is
   therefore specifically the churn that changes **no** access: topics
   appearing and disappearing, which is the frequent case.

   One mover is config-driven: the cross-topic section names
   `load_extended_tools` only when the registry holds it, so a config
   reload that disables the loader drops that line. The registry is the
   only truth about which tools exist, so the prompt asks the registry
   instead of resolving `disabled_tools` a second time — a role that
   removed the loader must not be told to call a tool it does not have.

   What travels in the `<runtime-state>` block (`build_turn_reference()`) is
   the subagent roster alone, prepended to the user message and framed as an
   auto-generated snapshot that is not user text and may go stale within a
   long turn. `build_system_prompt()` emits neither roster.

   Only the subagent roster rides that block now. The channel/topic roster
   is fetched by the `list_topics` tool when the model wants to address
   someone: it changed whenever a topic appeared or closed, and as a
   per-turn block it was repeated in the history of every later turn — a
   standing cost for a table that is only needed when addressing.

   The per-turn `prompt_hash` in the `System prompt built` trace line
   is the tripwire for this property: on an otherwise idle topic the
   hash must not move. A hash that changes with no `/grant`, no spawn
   or close, and no config reload behind it means volatile state leaked
   back into the system prompt and is paying for cache misses.

   **Tool schemas sit in that same prefix, so their text is
   budgeted.** The `tools` array is sent on every call, which makes
   every character of a tool description a standing per-call cost
   even for a tool the topic never invokes.
   `crates/jyc-agent/tests/tool_schema_budget.rs` derives the
   expected built-in set from the same calls
   `JycAgentService::build_tool_registry` makes (so registering a
   tool without budgeting it fails the test) and **budgets** its
   description text: the 2026-10-18 slimming pass took the natural
   language from 10,108 to 8,349 chars (-17.4%) across those 20 tool
   files — the 21 tools the budget covers, bridge tool included, run
   about 9,000 — by cutting examples
   and redundant prose while leaving every name, parameter, type and
   `required` list untouched. Editing a description thus costs one
   round of prefix-cache misses — make such an edit its own change,
   where the diff shows it on its own. Every built-in tool is
   budgeted, including `jyc_send_message` — the MCP **bridge** tool
   `register_mcp_tools` adds to every registry, whose schema is ours
   rather than a server's. Two things sit outside the budget: MCP
   **server** tools, whose schemas belong to whoever runs the server,
   and `jyc_spawn_subagent`'s `type` description, which ends with the
   role catalog of the machine the agent runs on — configuration, not
   text we may pin.

**Payload Layout After The Change:**

```
tokens ↓
┌─ tools ───────────────────────────────── ~2-3k ─┐
│  ~20 内建工具 + MCP 工具的 schema，按名排序      │ ◇断点1  跨 topic 全进程一致
├─ system prompt ──────────────────────── ~6-9k ─┤
│  · File access boundaries                        │ ◇断点2  ★稳态¹
│  · Skills 清单 + AGENTS.md(项目指令)             │
│  · Reply instructions / History format / VCS     │
├─ messages ──────────────────────────────────────┤
│  历史 旧轮次 (text-only pairs)                   │
│  └ user/assistant 文本原样, tool call/result 丢弃 │ ← 只追加: 前缀字节不变
│                                                   │    ★不做任何裁剪★
│                                                   │
│  ⟵ 窗口超 max_window 轮, 或已到 max_window 且超   │
│     max_history_tokens 时, 前移锚点、丢整轮        │ ← 每 (max_window-window) 轮一次
│                                                   │
│  当前轮            0 → 增长                      │
│  ├ user: <runtime-state> 参考块 ~300B ★易变★     │ ★新位置★
│  │        + 入站消息(含 <mode> 块) + 附件          │
│  ├ 每 16 个 assistant 步: 旧步折叠为 text-only    │ ★checkpoint★
│  │     checkpoint (reasoning/tool result 丢弃)    │   每16步一次
│  ├ recent suffix: assistant/tool 原样             │ ◇断点3 (n-3)
│  ├ tool: result                                 │ ◇断点4 (n-2)
│  │     默认原样携带 ★turn_tool_result_cap★       │  ★置了则 ≤ N 字节:
│  │     设了则留头2/3 + 标记 + 尾1/3               │  头 2/3 + 缺口标记 + 尾 1/3
│  ├   …(一轮内多轮工具调用, 每块结果各按各的帽)…    │
│  └ assistant: 最终回复(不进后续 call 的输入)      │
└──────────────────────────────────────────────────┘
```

`◇` = Anthropic `cache_control` breakpoint; `★` = 本次改动的位置。
¹ 稳态是指"不随运行时变动"，access boundary 的 grant 行除外——它对应
  工具层强制的 roots，spawn/close 与 `/grant` 会移动它(见上文)。
参考块虽易变，但落在当前轮最前——这个位置反正每轮都是新的，
所以它的易变性对缓存的损失为零，这正是选这个注入点的原因。

**Cross-Turn Cache Reuse (prefix-caching providers):**

```
改动前 (易变内容在 system 内)              改动后 (易变内容随 user 消息)
turn N+1 的第一个 call:                    turn N+1 的第一个 call:
┌────────────────┐                         ┌────────────────┐
│ tools      ✗全价│  前缀匹配 = 0           │ tools      ✓缓存│
│ system     ✗全价│  (topic 生灭/子 agent   │ system     ✓缓存│ ← 字节全同*
│                │   变化使其改变)         │ 历史头块   ✗全价│ ← 滑窗滑动,
│ 历史头块   ✗全价│                        │ …          ✗全价│    无法避免
│ …          ✗全价│                        │                │
└────────────────┘                         └────────────────┘
命中 0                                     命中 ~8-12k (tools+system)
全价 ~53-64k                               全价 ~45-55k
```

* 两张图都假设这两个 call 之间**没有** spawn/close 或 `/grant` —— 那类变动
仍会移动 boundary 段（见上文），属预期而非回归。

对应 Anthropic：改动前断点 2 随任一易变项失效，A+B 两个 span 同时
作废；改动后 span A+B 跨轮稳定，span C/D 照旧在轮内逐 call 点亮。
改动**没有**减少历史块的体量(本话题实测最大的一块)——那是 token
预算窗口的课题。

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
Topic files still provide recent conversation context

```

### Fallback Behavior

| Scenario | What Happens |
|----------|-------------|
| in-process agent uses `reply_message` tool successfully | Detected via SSE; `reply_sent_by_tool: true`, skips fallback |
| `reply_message` tool fails (e.g. MCP not implemented, invalid JSON) | AI generates text response instead; TopicManager passes raw text to OutboundAdapter which formats, sends, and stores |
| AI returns text without using tool | `session.idle` fires; TopicManager passes raw text to OutboundAdapter |
| AI takes very long but keeps working | SSE events keep arriving → no timeout; progress logged every 10s |
| AI goes silent for 30 minutes | Activity timeout (60 min if tool running) → checks signal file → error |
| SSE subscription fails | Falls back to blocking prompt with 5-min timeout |
| agent service dies between messages | Health check detects it, restarts automatically |
| ContextOverflowError | Detected via SSE `session.error` → new session → retry (blocking) |
| Token limit exceeded | Detected at session creation → new session created → AI notified via prompt |
| Topic queue full | Message dropped with warning; IMAP re-fetch recovers on restart |

### Reply Text Building Pipeline

All non-websocket channels are pipe-only channels: they relay the agent's
reply text as-is from the agent broadcast (see docs/architecture/overview.md).
`build_footer()` (`crates/jyc-channels/src/email_parser.rs`) builds the
model/mode/tokens footer but currently has **no production caller** —
`[channels.<name>.footer]` is inert. The agent
(AgentService/StaticAgentService) never calls it either.

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

build*full_reply_text(reply_text, topic_path, sender, timestamp, topic, body, message_ts)
│
├── prepare_body_for_quoting(topic_path, current_message, max_history)
│ │
│ └── Read chat_history*\*.jsonl files for conversation context
│ │
│ ├── Current incoming message (stripped of quoted history)
│ │
│ └── Previous messages and replies from chat log
│ (interleaved chronologically, newest first)
│
├── format_quoted_reply(sender, timestamp, subject, body) for each trail entry
│ → "---\n### Sender (timestamp)\n> Subject\n>\n> Body quoted..."
│
└── Combine: "{reply_text}\n\n{quoted_blocks}"

````

**Trail ordering:** Conversation history is read from `chat_history_*.jsonl` files. Messages and replies are interleaved chronologically, with the most recent entries first.

**Prompt echo stripping:** When the AI generates a fallback text response (because the MCP tool failed), it may echo parts of the prompt. `extract_text_from_parts()` strips these markers before building the full reply:
- `## Incoming Message`
- `reply-context.json=`
- `## Conversation history`

### Signal File (`.jyc/reply-sent.flag`)

Cross-process detection mechanism for when the MCP tool sends the reply but tool parts are missing from the prompt response (or the prompt times out).

**Format:** Single-line JSON
```json
{"sent_at":"2026-03-19T13:09:43Z","channel":"email","recipient":"user@example.com","message_id":"<123@smtp>","attachment_count":1}
````

**Lifecycle:**

1. **Cleanup**: Before starting a new prompt, `cleanup_stale_signal_file()` deletes any leftover file
2. Written by MCP reply-tool after successful outbound send
3. Read by `AgentService::check_signal_file()` as fallback detection
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
├── jyc serve              ← main command
├── jyc config init        ← config management
├── jyc config validate
├── jyc state              ← show monitoring state
└── jyc dashboard          ← live TUI dashboard (connects to running jyc serve)
```

The reply delivery path shares types with the main binary (same Rust crate), eliminating the type drift risk of the two-binary TypeScript approach.

### Reply Delivery Context

There is no reply tool: the agent's final message at the end of the turn
IS the reply (the norm for code agents — Claude Code, Codex, etc.). The
loop receives its routing context in-process via `ToolContext::reply_target`
(`ReplyTarget`: the inbound message + pre-warmed outbound adapter, injected
by the service before the prompt is sent) — no context file, and the AI
never sees or touches routing metadata. Mid-turn text alongside tool calls
is internal narration and is never delivered.

### Reply delivery (`deliver_reply` in `tools/mod.rs`)

Called by the agent loop for the final text, pre-question narration, and
the cycle-boundary heartbeat — the single delivery path, no tool involved.

  1. With a live reply target: gated by `reply_send` hooks, then sent
     directly through the pre-warmed outbound adapter (eliminates
     cold-start timeouts); the caller publishes the `ReplySent` event
  2. Without one (background turns, tests), or when the direct send
     failed: write `.jyc/reply.md` + `.jyc/reply-sent.flag` for the
     file relay (worker/background watcher delivers, gated by
     `reply_send` hooks there)
  3. Empty final text → nothing is sent (absorbs the old `silent` mode)

### MCP Tool: `jyc_send_message`

Sends a channel message with optional attachments to any recipient. It is part of the core advertised tool set, not the on-demand group, because an attachment-bearing reply must be discoverable without a loader call. Ordinary text-only replies still use the turn's final message.

```
In-process tool (jyc-agent/src/tools/mcp_bridge.rs):
  Tool schema: recipient, subject (optional), message, channel (optional),
               attachments (optional)

  1. Validate recipient format (channel-specific, e.g. "wecomkf:kf001:wmE8OcHAAA...")
  2. Resolve the current or named outbound adapter
  3. Build an OutboundMessage with any attachment files from the topic directory
  4. Call OutboundAdapter::send_proactive() and return the delivery result
```

**ToolContext outbound injection**: `ToolContext` carries an optional `outbound: Arc<dyn OutboundAdapter>` field. When the agent loop builds the tool registry, it injects the current topic's outbound adapter into `ToolContext`. The `jyc_send_message` tool uses this adapter directly for immediate delivery — no signal file indirection needed.

**Usage constraints** (enforced by AGENTS.md rules):
- Use for proactive alerts, cross-channel messages, and attachment-bearing replies
- Use the turn's final message for ordinary text-only replies
- Must NOT be used to spam users
- Recipient format is channel-specific; the tool validates format before attempting delivery

### Built-in Tool: `jyc_send_to_topic` and `/send`

Injects a message into another agent's topic queue for agent processing. Unlike `jyc_send_message` which bypasses agent processing (direct outbound delivery), `jyc_send_to_topic` enqueues an `InboundMessage` into the target topic's `TopicManager`, so the target topic's agent picks it up and processes it.

**Addressing is by agent, not by channel.** A topic is identified by its name — the identity the state dir registry keys on — and the manager that serves it is resolved rather than named:

- `agent` (required): the addressee.
- `topic` (optional): one of that agent's topics (e.g. `invoice_processing`), whose home is `<agents dir>/<agent>/<topic>/`.

The name is matched against topic names first, because that is the address: a name some topic already owns belongs to that topic. A name no topic owns is **refused**, not created — a send reaches topics that exist, it does not make them. An agent whose name another channel's topic already holds is therefore reached through that topic, not by a second topic of the same name.

An agent's own topic is named after the agent, so `topic` carrying the agent's own name addresses that same default topic — the two spellings mean one thing, and both land on `<agents dir>/<agent>/`.

The command form is `/send <agent> [--topic|-t <topic>] [--reply] [--attach <file>] [--recipient <id>] <message>`. Both triggers parse their own input and call the same `jyc_core::command::send_handler::send_to_topic` — one implementation with two entries, exactly as `/spawn` and `jyc_spawn_subagent`.

Which manager serves the name is then answered by looking, in four steps:

- **Running it** — a manager whose queue map holds the topic. A topic exists because a channel routed to it (which enqueued), so a live queue is the first answer.
- **Its own state** — `channel` in the topic's `.jyc/topic-meta.json`, written on the message that first routed to it and backfilled on a later one when the record predates the key. This is what keeps a topic idle since the last restart addressable instead of making it wait for its next inbound message. The state dir comes from the state registry, or — a sub-topic's home being two directories down, past what the startup scan walks — from probing `<agents dir>/<agent>/<topic>/.jyc`. A recorded channel this process does not run is refused rather than guessed at.
- **Its own home** — a topic that exists but has never been routed to. `/spawn` registers the child's state and pins its directory, but a topic only gets a queue — and with it the routing record above — once something sends to it, so between those two moments the topic is there and unreachable. The home tells it apart: an agent-keyed home (`<agents dir>/<name>/`) is an agent's own topic and the agents channel serves it, while a pipe-routed home (`<agents dir>/<agent>/<topic>/`) belongs to the channel that routed it. A closed topic keeps its registration but loses its directory, so it is refused like a name nobody owns.
- **Nobody owns it** — the name is refused. A send never creates the topic it addresses: a topic is made by the thing that routes to it — `/spawn` for an agent, a channel rule for a channel topic — and one created here would be a name no rule owns, under a home that collides with the names that do (state dirs are keyed by topic name process-wide). A `topic` equal to the `agent` is not a named topic — it names the agent's own topic — and is refused the same way.

```
Topic A (feishu_bot / "greenfield 下单")
  │
  │  jyc_send_to_topic(
  │    agent="invoice",
  │    topic="invoice-processing",
  │    message="Please process this invoice...",
  │    attachments=["invoice.pdf"],
  │    require_reply=true
  │  )
  │
  ▼
Topic "invoice-processing" (of agent "invoice")
  │
  │  InboundMessage enqueued with metadata:
  │    source_channel: "feishu_bot"
  │    source_topic: "greenfield 下单"
  │    require_reply: true
  │
  │  Agent sees in incoming message prompt:
  │    **Source:** channel "feishu_bot", topic "greenfield 下单"
  │      (⚠️ Reply requested - use jyc_send_to_topic to send results back)
  │
  │  Agent processes task, then calls:
  │    jyc_send_to_topic(
  │      agent="greenfield 下单",   # the source topic's name is the address
  │      message="Done: invoice processed..."
  │    )
  │
  ▼
Topic A receives results
```

**`require_reply` flag** (default: `false`): When `true`, the target agent's incoming message prompt includes a "⚠️ Reply requested" indicator, instructing it to send results back via `jyc_send_to_topic` when done — addressed at the source topic's own name, which is what resolution matches.

**The reply guard**: `require_reply` is a request, not a promise — the target is the one that has to answer it, and nothing checked that it did. A target that ended its turn without replying (a small model that forgot the tool call, a turn that died on a provider error) left the requester with silence, and silence is indistinguishable from "still working". So the target checks at the end of its turn: when a message it handled asked for a reply, and nothing this turn went to the asking topic, the target's own last words are sent to the asking topic in its place — the requester then hears the answer, or the fact that there was none. The turn's own record of where it sent (each `jyc_send_to_topic` delivery is recorded as a `(channel, topic)` pair) is what tells an answered request from an unanswered one, so a turn that did reply is not forwarded on top of. Only the sends made *after* the turn folded the request in count as its answer: the model cannot have answered a request it had not been shown yet, so each request carries the turn's send count at the moment the request joined the conversation — a send made before that is no answer to it. A request that arrived while the turn was running counts too: it joins the conversation with the same "⚠️ Reply requested" header, and the guard owes it an answer as well — including when the turn dies, since the loop hands the requests it still owed back with the error. Two limits, both narrow: a turn that died *after* answering may still get a second, redundant notice (the record is lost with the turn), and the static agent mode has no guard at all — it has no topic managers to forward through.

**Source metadata**: The tool sets `source_channel`, `source_topic`, and `require_reply` in the `InboundMessage.metadata` HashMap. The `build_user_prompt_text()` function reads these fields and renders a `**Source:**` header in the target agent's prompt.

**`TopicManagers`**: the process's running managers, in one place. `ChannelOrchestrator` owns the single instance — it is the only thing that creates managers — and publishes the current set on every channel change; each manager holds a `Weak` back-pointer to it (a strong one would be an `Arc` cycle), so a command handler, built with only its own manager, resolves an addressee through `manager.managers()`. `ToolContext` and `InspectContext` carry the same `Arc<TopicManagers>`, and the tool carries `current_channel` / `current_topic` for the source metadata. Before this, the same information existed twice: a channel-keyed `HashMap` derived in the CLI for the agent layer, and ad-hoc `channel_name()` scans in the inspect server.

### Historical Message Quoting (Topic Trail)

`build_full_reply_text()` builds the reply with quoted history from the topic's `chat_history_*.jsonl` files.

- **Chronological ordering**: Messages and replies are read from chat log files, ordered newest first.
- **Stripped bodies**: Received messages stripped of quoted history via `strip_quoted_history()`. Reply messages parsed to extract only the AI's response text.
- **Per-entry truncation**: Each quoted history entry is capped at 1024 characters (`MAX_QUOTED_BODY_CHARS`)
- **Limit**: `MAX_HISTORY_QUOTE = 6` entries for reply email quoted history
- **Timestamp format**: `YYYY-MM-DD HH:MM` in both quoted history headers and prompt context

## Configuration (TOML)

### Fresh Design

JYC uses TOML for configuration, taking advantage of TOML's native support for nested tables and inline comments. Environment variable substitution is supported via `${VAR}` syntax in string values.

```toml
# JYC Configuration

[general]
max_concurrent_topics = 3
max_queue_size_per_topic = 10

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
mode = "agent"

[agent.agent]
model = "SiliconFlow/Pro/zai-org/GLM-4.7"
small_model = "SiliconFlow/Qwen/Qwen2.5-7B-Instruct"
system_prompt = "You are an AI assistant. Respond professionally and concisely."



[agent.attachments]
enabled = true
max_file_size = "10mb"
allowed_extensions = [".ppt", ".pptx", ".doc", ".docx", ".txt", ".md"]
```

### Config Structs

```rust
#[derive(Debug, Deserialize)]
pub struct AppConfig {
    pub general: GeneralConfig,
    pub channels: HashMap<String, ChannelConfig>,
    pub agent: AgentConfig,
    pub inspect: Option<InspectConfig>,                   // Inspect server config
    pub attachments: Option<UnifiedAttachmentConfig>,  // Global attachment config
    pub vision: Option<VisionConfig>,                  // DEPRECATED: Use [[mcps]] instead
    pub mcps: Vec<McpServerConfig>,                    // Named MCP server definitions
}

#[derive(Debug, Deserialize)]
pub struct GeneralConfig {
    #[serde(default = "default_3")]
    pub max_concurrent_topics: usize,
    #[serde(default = "default_10")]
    pub max_queue_size_per_topic: usize,
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
    pub mode: String,                         // "static" | "agent"
    pub text: Option<String>,
    pub agent: Option<in-process agentConfig>,
    pub attachments: Option<AttachmentConfig>,
}

#[derive(Debug, Deserialize)]
pub struct in-process agentConfig {
    pub model: Option<String>,
    pub small_model: Option<String>,
    pub system_prompt: Option<String>,
    // Note: include_topic_history is deprecated — conversation history
    // is no longer injected into the prompt. in-process agent session memory handles it.
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
channels/<channel-name>/
├── .imap/
│   ├── .state.json                  # { last_sequence_number, last_processed_uid, uid_validity }
│   └── .processed-uids.txt         # One UID per line, append-only
```

Only the email adapter keeps state: the IMAP mailbox cursor (sequence number +
processed UIDs), a protocol-level dedup concern. It lives under
`<data dir>/channels/<channel_name>/.imap/`.

### Per-Agent Overrides

An agent overrides the global behavior for the topics it handles:

```toml
[agents.developer]
role = "Developer"
template = "github-developer"
model = "deepseek/deepseek-v4-flash"           # Override global [agent].model
small_model = "deepseek/deepseek-v4-flash"     # Override global [agent].small_model
inject_inbound_images = true                   # Auto-inject image attachments
mcps = ["mcp-a", "mcp-b"]                      # Only these MCP servers; [] = none
disabled_builtin_tools = ["bash", "write"]     # Remove these built-in tools
```

The pipe pattern that routes into it carries matching only:

```toml
[[channels.github.patterns]]
name = "developer"
pipe = { agent = "developer" }

[channels.github.patterns.rules]
github_type = ["pull_request"]
assignees = ["kingye"]
```

**Resolution priority**

`model` — one implementation, `session_state::resolve_model` (see **One model
resolver**), every step keyed by the mode the topic is in:

1. `.jyc/<mode>-model-override` file, then the legacy `.jyc/model-override`
2. Topic-level `[agent]` `plan_model` / `build_model`, then `model`
3. Agent-level `plan_model` / `build_model`, then `model`
4. Global `plan_model` / `build_model`, then the channel-over-global `model`

`small_model` has no file layer; the runtime resolves it per turn:

1. Topic-level `[agent] small_model`
2. Agent-level `small_model`
3. Global `[agent] small_model`

**MCP scoping**: When `mcps` is present on an agent, only those named servers are loaded for matching topics. When `mcps = []`, no MCP servers are loaded (fully restricted). When omitted, the global `[[mcps]]` list is used.

**Built-in tool disable**: `disabled_builtin_tools` removes named tools from the registry before the agent loop starts. Tool names match `Tool::name()`: `bash`, `write`, `edit`, `grep`, `read`, `webfetch`, `read_image`.

## Directory Structure

### Runtime Data

```
<config dir>/                               # ~/.config/jyc — user-edited (L1)
├── config.toml                             # Master config (TOML)
├── skills/                                 # Global skills
└── templates/                              # Topic templates

<data dir>/                                 # ~/.local/share/jyc — generated state (L2)
├── channels/<channel-name>/                # That channel's own runtime state
│   └── .imap/                              # IMAP adapter cursor
│       ├── .state.json                     # Monitor state
│       └── .processed-uids.txt             # One UID per line, append-only
└── agents/<agent>/<topic>/                 # Topic home (agents dir)
    ├── .jyc/                               # State dir — never inside the working dir
    │   ├── agent-session.json              # AI session state
    │   ├── agent-context.jsonl             # Raw context journal, one message per line (a pre-journal .json array is not read)
    │   ├── chat_history_2026-03-19.jsonl   # Daily chat log (messages + replies)
    │   ├── model-override                  # /model command override
    │   └── mode-override                   # /plan command override
    ├── .data/                              # Default working directory (topic_path)
    │   └── .attachments/                   # Saved inbound attachments (if configured)
    │       └── report.pdf
    └── .inbound/                           # Inbound staging (framework)
```

## Topic Template

Topic Template allows initializing new topics with predefined files and directories. Templates are defined at the agent level and applied when a topic is first created.

### Configuration

Templates are configured per agent in `config.toml`:

```toml
[agents.urgent]
template = "urgent"  # Use templates/urgent/ for this agent

[agents.normal]
# No template - topic starts empty
```

Template directory structure (in data dir):

```
<root-dir>/
├── templates/
│   ├── urgent/
│   │   ├── agent.md      # in-process agent reads this as topic-specific prompt
│   │   ├── skills/
│   │   │   └── my_skill/
│   │   │       └── SKILL.md
│   │   └── custom_file.txt
│   └── default/
│       └── ...
```

### How It Works

1. **Pattern Matching**: When a message matches a pattern with a `template` field, the template name is stored in the message metadata.

2. **Topic Initialization**: On the first message to a topic, `TopicManager` copies all files from `templates/{template_name}/` to the topic directory (skipping existing files).

3. **Pattern Tracking**: The pattern name is saved to `.jyc/pattern` for later reference.

4. **`/template` Command**: Users can run `/template` to re-apply the template to the current topic (copies missing files).

### Files Copied

- Template files are copied to the topic root directory (not `.jyc/`)
- Directories are created as needed
- Existing files are **not** overwritten (safe to re-run)

## Inspect Server & TUI Dashboard

### Architecture

```
┌──────────────────────┐         TCP (127.0.0.1:9876)  ┌──────────────────────┐
│   jyc serve          │  ◄──────────────────────────► │   jyc dashboard      │
│                       │   HTTP/1.1 + WS upgrade       │   (ratatui TUI)      │
│  ┌─────────────────┐ │                                │                      │
│  │ InspectServer    │ │   GET  /api/state             │  polls every 1s      │
│  │ (axum router)   │ │  ◄──────────────────────────  │  reqwest + Bearer    │
│  │                  │ │   200 + {channels, topics…}  │  auto-reconnect      │
│  │ routes:         │ │  ──────────────────────────►  │                      │
│  │  /api/state     │ │                                └──────────────────────┘
│  │  /api/state/... │ │
│  │  /api/topics/* │ │  /ws, /ws/<ch>, /ws/<ch>/<t>  ← WebSocket upgrade
│  │  /api/...       │ │     Authorization: Bearer …
│  │  /ws/*          │ │     → 101 Switching Protocols
│  │  (require_bearer│ │
│  │   middleware)   │ │
│  └─────────────────┘ │
└──────────────────────┘
```

### Configuration

```toml
[inspect]
enabled = true                      # Default: false
bind = "127.0.0.1:9876"            # Default: 127.0.0.1:9876
# auth_token = "..."                # Optional; when set, every request
                                    # (REST and WS upgrade) must carry
                                    # `Authorization: Bearer <token>`.
```

For Docker (Podman Machine on macOS), use `bind = "0.0.0.0:9876"` and the SSH tunnel script `scripts/jyc-podman-tunnel.sh`.

### Remote Serve over SSH

`jyc connect <[user@]host>` keeps the hub where the work is and the TUI where
the user is: `jyc serve` runs on the remote machine, `jyc dashboard` runs
locally, and the two are joined by an `ssh -N -L` child process.

```
local                                            remote
  jyc dashboard ──HTTP/WS──► 127.0.0.1:9876 ~~ssh -L~~► 127.0.0.1:9876 ──► jyc serve
```

What the design buys, and what it deliberately does not:

- **A tunnel child, not a new transport.** ssh owns authentication — keys,
  agent, `~/.ssh/config`, jump hosts, `ProxyCommand` — and the dashboard keeps
  speaking the plain loopback HTTP/WS it already spoke. No TLS, no new client
  code, no change to the server.
- **The remote keeps its loopback bind.** `jyc connect` adds no exposed port:
  the tunnel is local on both ends. That is strictly safer than
  `bind = "0.0.0.0"` with a bearer token over plaintext HTTP, and it is why
  the `.ssh/config` alias of a bastion is enough to reach a hub behind it.
- **Same port first.** Links the server generates are absolute
  (`http://127.0.0.1:<port>/exchange/<channel>/<topic>/<name>?token=…`, from
  `jyc_publish_file` and `/exchange`) and carry a per-topic token, so the port
  in them is part of the answer. The tunnel prefers the remote's own port when
  it is free locally, and leaves those links untouched. When the port is taken
  here, an ephemeral port is used and message text is translated from the
  remote prefix to the tunnel's as it enters the pane — the rewrite is applied
  in one place (`ChatState::push_message`), so every ingestion path gets it.
  The prefix that gets rewritten is the one the *server* builds links from
  (`effective_base_url`): `http://<--remote-bind host>:<port>`. Two setups are
  out of its reach and rely on the same-port preference alone — a wildcard
  bind (the server substitutes its LAN IP, which this machine cannot know) and
  a configured `[inspect] base_url` (which may name anything, including a
  proxy). `jyc connect` emits no rewrite in those two cases rather than one
  that cannot match.
- **The token is read per session, never copied.** `jyc connect` runs
  `jyc token show` on the remote and keeps the result in memory. A copy in the
  local data dir would be a second secret to keep in sync, and the dashboard
  would silently keep sending a stale one after a `jyc token reset` there.
- **Starting the hub is opt-in (`-s/--serve`).** Connecting never mutates the
  remote by default: the dashboard probes `GET /api/state/overview` through
  the tunnel and, when nothing answers, fails with that probe's error — which
  names a 401 when the server is up but the token is stale, and a transport
  error when nothing is listening — plus the two remedies. With `-s` it starts
  `nohup jyc serve` (SIGHUP ignored, so the closed session cannot take the hub
  down), prefixed with `setsid` where that exists to also drop the controlling
  terminal — macOS ships no `setsid`, and `nohup` plus redirected stdio is
  enough there — and waits up to 60 s.
- **Compression is on by default (`ssh -C`).** The traffic is JSON that barely
  changes between frames, and the worst case is structural: a `Thinking` event
  re-sends the whole reasoning every 500 ms, so a long turn puts O(n²) bytes on
  the wire if nothing compresses them. zlib sees each frame as a prefix of the
  one before it and collapses the repeated prefix to near nothing — the option
  that decides whether a thin uplink is usable or smeared (how much it buys on
  a given link is a measurement, not a number claimed here). It costs CPU on
  both ends and is overridable with `--ssh-opt Compression=no`.
- **`--ssh-opt KEY=VALUE` (repeatable) for everything else.** Ciphers,
  keepalives, `Compression=no`: what fits a link is something only the user can
  see, and the option is appended after the defaults here, so it wins —
  including over the survival options (`BatchMode`, `ConnectTimeout`), which
  guard against a password prompt with nowhere to appear. Overriding those is
  permitted and self-inflicted.
- **Exiting ends the tunnel only.** The hub may be serving other dashboards
  and channels; `jyc connect` owns the forward, not the server.
  (A dashboard killed with SIGKILL never runs its `Drop`, so the `ssh -N` it
  leaves behind holds the local port until it is killed by hand.)
- **Non-interactive ssh is a requirement, not a preference.** Bootstrap and
  tunnel both run before the TUI takes the terminal, so a password prompt
  nobody can answer would hang in raw mode; `BatchMode=yes` turns that into a
  fast failure whose text says to check `ssh <host> true` and the agent.
  Every ssh command also carries an overall deadline (30 s), because
  `ConnectTimeout` bounds only the TCP connect — name resolution, a
  `ProxyCommand`, an sshd that never sends a banner, or a remote command that
  never exits would otherwise hang the same way.
- **A POSIX-ish remote login shell.** The bootstrap runs `printf '%s' "$HOME"`
  and a `command -v setsid` test, and quoting assumes POSIX single-quote rules
  — `sh`, `bash`, `zsh`, `dash` all fine, `csh`/`fish` not.

### Protocol

HTTP/1.1 REST over TCP, with WebSocket upgrades on the same port. One
Bearer token (`auth_token`) gates both REST and WS routes via a single
`require_bearer` middleware. JSON request and response bodies.

```bash
# REST
curl -H 'Authorization: Bearer <token>' http://127.0.0.1:9876/api/state
# → 200 + { "uptime_secs": 3600, "version": "0.2.1", "channels": [...], ... }

# WebSocket upgrade
wscat -c ws://127.0.0.1:9876/ws/feishu_bot/issue-42 \
      -H "Authorization: Bearer <token>"
# → 101 Switching Protocols, then live activity / chat events stream
```

> **Full reference.** This section is a high-level overview only. For
> every route, response shape, server-pushed event, type schema, and
> worked example, see [`docs/api.md`](docs/api.md).

### Topic data: one list, one detail

A topic's figures reach a reader through exactly one of two shapes, and the
two never carry the same figure:

| Shape | Endpoint | In-process API | Carries | Read by |
|-------|----------|----------------|---------|----------|
| **List** | `GET /api/state/overview` | `TopicManager::list_topics_without_git` | identity (name, channel, agent, pattern), `status`, context usage, `last_active_at`, `parent`, `commands` | the overview table, the topic explorer |
| **Detail** | `GET /api/topics/{channel}/{topic}/info` | `TopicManager::topic_info` | model, mode, token and cost figures, latency, task list, branch, changed files, `status`, `last_active_at`, the **subagent tree** | the topic info pane, `/info` |
| **Progress** | `WS /ws/<channel>/<topic>` | per-topic `TopicEventBus` | activity entries, chat messages, thinking text, the processing frame | the chat pane, the progress indicators (TUI, feishu), pipe processes |

The rule behind the table: **one figure, one source**. A figure is either in
the list or in the detail, and the two consumers that want the same figure
read the same shape. The info pane and `/info` are the same consumer of the
detail — the pane over HTTP, the command in-process through the same
`TopicManager::topic_info` call — so they cannot report different numbers
for one topic.

The subagent tree is part of the **detail**, not of the list: it says who
*this* topic spawned, so it travels with the topic's own shape and both
readers of it — the info pane and `/info` — draw the same tree out of the
same call. The list carries only `parent`, the one field the explorer needs
to nest every topic of every channel under the one that spawned it; the
list shape does not build a tree of its own, because a poll would then pay
a tree per topic.

The lineage behind both is the `spawned-from` breadcrumb each subagent wrote
into its own state dir (`jyc_core::command::spawn_handler::spawned_from`),
and the **topic manager owns it**: it reads the breadcrumbs once per list and
answers with the parents *and* the tree alike (`TopicManager::lineage`), so
the list's `parent`, the detail's `subagents` and the tree the explorer
draws are the same data — the inspect server takes the parent from the
manager's answer instead of reading the breadcrumb a second time. The rules
that turn a lineage into a tree live in one place,
`jyc_core::topic_manager::topic_tree`: who is under whom, in what order,
and how a hand-edited breadcrumb cycle is kept from looping.

`status` is the one field both the list and the detail carry, and they carry
the same one: the runtime's **live status**
(`jyc_core::topic_manager::live_status`). The runtime writes it as turns
start and end — the agent loop when it publishes `ProcessingStarted` /
`ProcessingCompleted`, the queue when a turn fails — and every reader
(`/info`, the overview table, the info pane, the progress frame) reports
that value. No consumer computes a status of its own, so "the topic is
working" cannot be true in one view and false in another.

A tool that fails mid-turn counts only when the model does not recover from
it. The agent loop keeps the tools whose last call failed
(`jyc_agent::agent_loop::tool_failures`) and reports them to the manager
once, at the end of the turn: a tool that works again leaves no mark, so a
turn that stumbled and carried on ends as a turn that worked. The recovery
is counted per tool, not per call — another call of the same tool that
works counts too, because the model may have given up on the call that
failed — and a failure that stands still leaves the topic in error until
the next turn starts.

Both writes — the report and the turn's end — follow the "if known" rule: a
topic closed mid-turn has no status entry any more, and re-creating one
would report a topic that is gone as failed, and a name reused later would
inherit that failure. The guard that closes a turn's "processing" flag does
so only when the turn left it open, so a turn that died without saying so
cannot leave the topic "Processing" forever.

`last_active_at` is the other field both carry, and they carry the same one:
the event stream's stamp for the topic. The state dir's mtime is only the
fallback, for a topic the tracker has not seen yet.

### TUI Dashboard Layout

The TUI has two screens and one set of pane renderers: the **overview** (topics
table, topic info pane, activity pane) and the **chat screen** (explorer,
conversation, and the same two panes). Nothing is drawn inside a frame of its
own — a pane draws only the edges that separate it from a neighbour, and leaves
the ones that sit on the screen boundary open. Every border goes through
`pane_border_style`: `PANE_BORDER` (DarkGray) at rest, `PANE_BORDER_FOCUSED`
(cyan) while that pane owns the keys. No pane carries a title.

#### Overview

```
Not connected: connection refused

  Agent              Topic            Pattern      Status     Context           Last Active
───────────────────────────────────────────────────────────────────────────────────────────
→ jyc                jyc              pipe         idle       0/117.19K         2m ago     │ Topic:       issue-42                
  invoice            invoice          feishu       processing 44.17K/117.19K    now        │ Pattern:     planner                 
                                                                                           │ Model:       claude-opus-4-8         
                                                                                           │ Mode:        build                   
                                                                                           │ Branch:      feat/issue-42           
                                                                                           │ Status:      Processing · queue ▊▊▊▊▊
                                                                                           │              2 / 10
                                                                                           │ Tokens:      ────────··············  
                                                                                           │              44.17K / 117.19K (38%)  
                                                                                           │ Cache:                               
                                                                                           │ Turn:        ──────────────────····  
                                                                                           │              12.00K / 15.00K (80%)   
                                                                                           │ Session:     ───────────────────···  
                                                                                           │              78.24K / 88.19K (88%)   
                                                                                           │ Files (2):                           
                                                                                           │   crates/…/mod.rs                    
                                                                                           │ ──────────────────────────────────── 
                                                                                           │ JYC AI v0.7.1                        
                                                                                           │ 2 active / 4 topic                   
──────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────────
2m  reply sent · emf
2m  err  ·  emf  ·  unhandled
```

- **Status strip** — one line at the very top, drawn *only* when there is
  something to report: `Not connected: …` in red while the poll is failing,
  `Connecting…` before the first poll lands. A loaded overview leaves the row
  blank and the table starts below it.
- **Topics table** (80% wide, borderless, no title) — one blank row of top
  padding, then the header, then a full-width rule under it in `PANE_BORDER`,
  then the topics. The `→ ` gutter plus `DIM` marks the selected row, and the
  gutter is reserved on every row so a selection never shifts the columns.
  Columns: Agent, Topic, Pattern, Status, Context, Last Active. Agent and Topic
  share the width the dropped Channel column gave up — the Agent column gives
  two of those cells back to the Context column. The Context column is wide
  enough for the widest pair a real context window produces —
  `1022.46K/1023.02K`, seventeen cells — so a context is never cut off
  mid-number; Status needs only its longest value and Last Active only its
  header.
- **Topic info pane** (right 20%) — left border only, one row of top padding.
  The topic detail (pattern, model, mode, branch, token counts, changed
  files) scrolls; the status sub-pane is pinned to the bottom behind a
  gray top edge, carrying `JYC AI v<version>` plus the server stats in gray. A
  transient message replaces that block with one yellow line for a few seconds.
  Every label of the pane is padded to one width — thirteen cells, the
  length of the longest label (`Today (all): `) — so, in the table layout,
  every value starts in the same column and the pane reads as one table
  rather than a ragged list. The percentage rows draw their share as a bar
  — `─` for the filled part (bold yellow), `·` for the rest (dim gray):
  the bar has the label's row to itself and the figures follow on the
  next one, in the same column —
  `Tokens:      ────────··············` with `44.17K / 117.19K (38%)` under
  it, and `Session:   ───────────────────···` with
  `78.24K / 88.19K (88%)` under it; the bars start in that column too, so the
  shares can be compared by eye. The figures have a row of their own so
  that the bar can reach for the edge: the bar is as wide as the column
  allows — what the label's room leaves, less the two cells it keeps to
  the pane's edge — and the wrapping never cuts it in half: a narrow
  column gets a shorter bar. The fill rounds to the nearest cell, so a bar
  of five cells is within ten points of the share; below five cells the
  rounding error grows past that and the bar would misreport the share, so
  a column that narrow gets the figures alone — only a column of fewer
  than five cells gets there, since the table layout always leaves the
  bar the label's room. The percentage is printed either way, so the bar is
  a glance aid and never the only source. The sketch above draws the right
  column wider than the layout's 20% share — thirty-seven cells, enough for
  the table layout — so its bars are longer than a terminal that narrow
  would give. The second row used to read "Cache
  util"; cache util and cache hit rate are the same number. The pane lays
  its rows out one way, decided by its inner width: while the column can
  hold the label column and a value column (31 cells or more), every row
  keeps its label and its value on one line — the table layout; a narrower
  column stacks them instead, the label on a row of its own and the value
  flush left under it, so a narrow column cuts no value short — each field
  then takes two rows, so a narrow column shows fewer fields at once. The
  bar follows the layout: in the table layout it shares its row with the
  label, in the stacked one it has the row to itself — either way it stops
  two cells short of the column's edge — so a bar appears wherever the
  column can carry an
  honest one, not only in a wide terminal. The **Status** row carries the
  topic's status and its backlog: the status word, then the blocks that
  say how much of the topic's queue is taken — one block per message the
  queue can hold, `▊` for a message waiting for a turn of its own (bold
  yellow), `▊` in dim gray for a free slot — so the row shows both what
  the topic holds and the room it has left; the blocks are a quarter
  short of a cell, so they keep a gap between them and read as blocks
  rather than as a solid bar. The word `queue` names them, so they
  cannot be read as the progress of the turn the status word names:
  `Status: Processing · queue ▊▊▊ 2 / 10`. The word and the blocks are
  what the row gives up as the column narrows: first the figures (`2 /
  10`) move to the row under it, then the blocks go — and the word with
  them, for it names blocks that are no longer there — then the figures
  move under the status word; nothing is dropped. The backlog is what the
  model has not read yet, counted
  where the messages wait: the ones still in the queue are the channel's
  own depth, the ones the running turn pulled aside for a turn of their
  own are that turn's buffer, and the ones the running turn took in and
  the model has not read yet are that turn's injection channel. The
  three are read at different moments, so a poll that
  lands while a turn hands its messages back can count one of them twice
  or not at all — the gap is one poll wide, and the next poll has it
  right. The backlog can exceed the capacity — the running turn holds
  messages the queue gave up room for — and the blocks then fill the row
  without covering it; the figures say what is really waiting. The
  backlog is left out when the topic reports no capacity — an
  older peer, or a source that does not know the queue. The pane's other
  sections — the
  task list, the changed files — are not `label: value` rows and keep their
  own shape. A value too long for its column wraps *inside* it: the lines
  that continue it are indented to the column, so the column keeps the
  values it holds instead of dropping them back to the column's edge. The
  table layout needs a column of 31 cells, and the column is 20% of the
  terminal, so it is a terminal of roughly 160 columns that gets the table;
  a narrower terminal gets the stacked layout, where every value starts in
  the column's first cell. The pane's subagent section — the topics this one
  spawned, as a tree, each with its own subagents under it — comes out of
  the topic's detail, the same shape `/info` prints, so the pane and the
  command show the same tree.
- **Activity pane** (bottom 20%) — top border only, because the other edges are
  the screen edge or sit against the pane above. Internal entries and thinking
  heartbeats are filtered out: they are dozens of identical markers that would
  crowd out the events worth reading.

#### Chat screen

```
┌────────────────────┬───────────────────────────────────────┬──────────────┐
│ explorer           │ conversation (borderless)             │ topic info   │
│ ● issue-42         │ message                               │ Topic: …     │
│   └ sub-42         │ reply                                 │ Channel: …   │
│ ● issue-55         │ progress tail                         │ Model: …     │
│                    │ input                                 │              │
├────────────────────┤──────────────────────────────────────────────────────┤
│                    │ activity pane (20% / 80% / full)
└────────────────────┴──────────────────────────────────────────────────────┘
```

- **Explorer** (left 20%, leader `e`) — the topic tree, with a subagent nested
  under the topic that spawned it.
- **Conversation** (80%) — borderless; messages, the progress tail and the
  input field.
- **Topic info pane** and **activity pane** are the same renderers the overview
  uses. The activity pane takes 20% / 80% / the full height (leader `a`), and
  there is no bottom status bar here: the status sub-pane lives inside the info
  pane, and zen mode (leader `z`) hides every pane and restores them on exit.

#### Keys

Overview: `↑`/`↓`/`j`/`k` move the cursor, `Enter` opens the selected topic's
chat, `Tab` cycles focus (table → info pane → activity pane), `R` reloads the
server config, `gg`/`G` jump to the top/bottom of the focused pane and
`PageUp`/`PageDown`/`Ctrl+B`/`Ctrl+F` scroll it. `Ctrl+P` opens the leader
popup, `Ctrl+Q` quits.

Leader (`Ctrl+P`): `c` open chat (focus chat on the chat screen) · `r` reload
config · `q` quit · `e` explorer · `a` activity pane · `i` info pane · `s`
status sub-pane · `z` zen · `m` mouse capture · `t` thinking · `T` tool detail ·
`p` minimal progress · `o` open in `$EDITOR` · `x` clear input · `d` back to the
overview · `gg`/`G` scroll to top/bottom. Chat adds `Ctrl+C` to cancel the
topic's in-flight turn.

### Dashboard Cross-Channel Topic Chat

When the user selects a non-websocket topic (email, feishu, github) in the overview pane and presses `Enter`, the dashboard enters a detail/chat mode showing live messages and allowing message injection into the topic. This closes the gap where previously only websocket topics supported interactive chat from the dashboard.

#### Live Message Flow

```
                              ┌──────────────────────────────────┐
                              │         External World            │
                              │                                  │
                              │  Email │ Feishu │ GitHub │ ...    │
                              └───────┬──────┼────────┼──────────┘
                                      │      │        │
                       InboundAdapter │      │        │
                       (IMAP/WS/poll) │      │        │
                                      ▼      ▼        ▼
                              ┌──────────────────────────────────┐
                              │       MessageRouter              │
                              │  match → derive_topic → route   │
                              └───────────────┬──────────────────┘
                                              │
                                              ▼
                              ┌──────────────────────────────────┐
                              │       TopicManager              │
                              │                                  │
                              │  enqueue(message)                │
                              │    │                             │
                              │    ├─ publish TopicEvent::      │
                              │    │   IncomingMessage           │
                              │    │   on TopicEventBus (1)     │
                              │    │                             │
                              │    ▼                             │
                              │  Worker processes                │
                              │    │                             │
                              │    ▼                             │
                              │  outbound.send_reply(text)       │
                              │    │                             │
                              │    └─ publish TopicEvent::      │
                              │       ReplySent                  │
                              │       on TopicEventBus (2)      │
                              └───────────────┬──────────────────┘
                                              │
                            TopicEventBus    │
                                              │
                    ┌─────────────────────────┘
                    │
                    ▼
┌───────────────────────────────────────────────────────────────────┐
│                      ActivityTracker                               │
│  (subscribes to TopicEventBus per topic)                         │
│                                                                   │
│  TopicEvent::IncomingMessage → store in                           │
│    TopicActivityState.recent_messages as ChatMessageEntry         │
│                                                                   │
│  TopicEvent::ReplySent → store in                                 │
│    TopicActivityState.recent_messages as ChatMessageEntry         │
└───────────────────────────────┬───────────────────────────────────┘
                                │
                                ▼
┌───────────────────────────────────────────────────────────────────┐
│                  InspectServer::build_state()                      │
│                                                                   │
│  For each topic:                                                 │
│    • Drain TopicActivityState.recent_messages                     │
│    • Populate TopicInfo.recent_messages                           │
│    • Return in InspectState (JSON)                                 │
└───────────────────────────────┬───────────────────────────────────┘
                                │
                                │  TCP poll (1s)
                                ▼
┌───────────────────────────────────────────────────────────────────┐
│                  Dashboard (detail/chat mode)                      │
│                                                                   │
│  Chat conversation (borderless; no outer box):                     │
│                                                                   │
│   14:32 ─────────────────────────────────────────────────────────  │
│   **You:** 帮我查一下这个订单的状态                                 │
│   **AI:** 正在查询订单 #12345...                                   │
│   ─────────────────────────────────────────────── 12s              │
│                                                                   │
│   14:35 ─────────────────────────────────────────────────────────  │
│   **User (remote):** 订单创建好了吗？                              │
│   > _                                                            │
│   ───────────────────────────────────────────────  8s              │
│                                                                   │
│  Auxiliary UI (info + status visible by default):                  │
│   Ctrl+P s/i toggle status / info panes individually;              │
│   Ctrl+P z zen mode hides all panes and restores the exact         │
│   pre-zen state on exit. Ctrl+P c focuses the message area;        │
│   typing from any focused pane refocuses the input.                │
│  ┌─────────────┐                                                 │
│  │ Topic Info │ (20% wide right pane)                            │
│  │ (bordered)  │                                                 │
│  └─────────────┘                                                 │
│  [Tab]focus [↑↓]scroll [^P]leader [Esc]back [^Q]quit              │
└───────────────────────────────────────────────────────────────────┘
```

#### TopicEvent Additions

Two new variants enable the live message feed:

```rust
pub enum TopicEvent {
    // ... existing variants ...

    /// A new message arrived in this topic (from any source: remote user,
    /// dashboard injection, cross-topic).
    IncomingMessage {
        topic_name: String,
        sender: String,        // "user", display name, etc.
        text: String,          // Truncated message body preview
        timestamp: DateTime<Utc>,
    },

    /// The AI sent a reply for this topic.
    ReplySent {
        topic_name: String,
        text: String,          // The AI reply text
        timestamp: DateTime<Utc>,
    },
}
```

#### Unified WebSocket Chat Pane

The dashboard's chat pane uses a unified WebSocket endpoint that works
for **any** channel type, regardless of whether the channel itself has
a WebSocket transport. This replaces the previous design that routed
non-websocket channels through a REST `inject_message` fallback.

```
┌──────────────────────────────────────────────────────────────────────────┐
│                    /ws/<channel>/<topic> (any channel type)              │
│                                                                          │
│  Dashboard  ──────►  InspectServer::handle_client                        │
│                          │                                             │
│                          │  extract_ws_route("/ws/<channel>/<topic>")   │
│                          ▼                                             │
│                    WsRoute::Topic { channel, name }                     │
│                          │                                             │
│                          │  if channel in websocket_handlers:            │
│                          ▼                                             │
│                    ┌─────────────────────────┐                          │
│                    │ ScopedWsHandler         │  (websocket-type channel) │
│                    │ pre-send {type:subscribe}│  wraps WebsocketInbound- │
│                    │ then delegate            │  Adapter                  │
│                    └─────────────────────────┘                          │
│                                                                          │
│                          │  else (any other channel):                    │
│                          ▼                                             │
│                    ┌─────────────────────────┐                          │
│                    │ TopicProxyHandler      │  (any other channel)      │
│                    │ - inbound: load topic- │  reads .jyc/topic-       │
│                    │   meta.json, build      │  meta.json for routing    │
│                    │   InboundMessage,       │  metadata                  │
│                    │   tm.enqueue()          │                            │
│                    │ - outbound: subscribe to │                            │
│                    │   InspectContext.broad- │                            │
│                    │   cast, filter by       │                            │
│                    │   (channel, topic),    │                            │
│                    │   forward events        │                            │
│                    │ - resync on Lagged      │                            │
│                    └─────────────────────────┘                          │
└──────────────────────────────────────────────────────────────────────────┘
```

**Three URL routes:**

- `GET /ws/<channel>/<topic>` — unified chat for any (channel, topic) pair
  (routes to `ScopedWsHandler` for websocket channels, `TopicProxyHandler` otherwise)
- `GET /ws/<channel>` — adhoc topic on a websocket-type channel only
  (returns error if `<channel>` is not a websocket channel)
- `GET /ws` — use the first registered websocket channel (legacy)

**Initial hydration (cold start):**

```
Dashboard  ──REST──►  GET /topics/<channel>/<topic>/activity
                       GET /topics/<channel>/<topic>/chat
                       ↓
                       seed_live(channel, topic, activity, chat) on the
                       dashboard's ChatState.live_activity / live_chat
                       BTreeMaps (keyed by (channel, topic))
```

**Live updates (warm path):**

```
TopicManager  ──publish──►  TopicEventBus
                                 │
                                 ▼
                            ActivityTracker  ──fanout──►  InspectContext.broadcast
                                 │                              │
                                 │                              ▼
                                 ▼                       ScopedWsHandler
                          state.entries /                  │
                          state.recent_messages            │ filter
                                 │                         │ by (channel, topic)
                                 │                         ▼
                                 └────► also drained     ws.write
                                          into the same         │
                                          buffers                ▼
                                                              Dashboard
```

**WebSocket protocol (outbound events from server):**

```json
{"type":"activity",     "channel":"...", "topic":"...", "id":N, "entry":{...}}
{"type":"chat_message", "channel":"...", "topic":"...", "id":N, "entry":{...}}
{"type":"thinking",     "channel":"...", "topic":"...", "text":"..."}
{"type":"processing",   "channel":"...", "topic":"...", "is_processing":bool, "has_error":bool}
{"type":"resync",       "channel":"...", "topic":"...", "dropped":N}
```

**WebSocket protocol (inbound messages from dashboard):**

```json
{"type":"message",       "text":"..."}
{"type":"disconnect"}
```

`channel` and `topic` are NOT in the payload — they're bound at handler
construction time from the URL path.

**Dedup & backpressure:**

- Each entry carries a monotonic `id: u64` assigned by `ActivityTracker`
  on push (per-topic counter in `TopicActivityState::next_id`).
- Client tracks `last_seen_id[(channel, topic)]` and drops `id <= last_seen_id`.
- On `Lagged(n)`, the server sends `{"type":"resync", "dropped": n}` and the
  client clears the live buffer and re-hydrates via REST.

#### Non-interactive Send (`jyc send`)

A cron job or script has no dashboard and no source topic, so it cannot send
through the chat WebSocket. `jyc send <agent> [--topic <topic>] <message>...`
connects to the running inspect server and calls `POST /api/send`, which
invokes `send_handler::send_to_topic_with_target` — the same address resolution
and queue entry point as `/send` and `jyc_send_to_topic`. The target must
already exist; the send never creates a topic.

```
cron/script
  └── jyc send <agent> [--topic <topic>] <message>...
        └── POST /api/send (bearer auth)
              └── send_to_topic_with_target
                    └── TopicManager::enqueue
                          └── target topic's FIFO worker
```

The CLI loops one HTTP request per message, awaiting each response before the
next, and stops at the first rejection. Messages already accepted stay queued;
there is no batch transaction or rollback. It never waits for an AI reply. The
injected message carries no source channel/topic and no attachments, so a
target reply follows the topic's normal outbound path rather than being routed
back to the caller.

`send_to_topic_with_target` now uses `TopicManager::try_enqueue`, so a full
bounded queue reaches the caller as `QueueFull` instead of the historical
silent drop; `/api/send` maps it to `429`. Other inbound paths keep
`TopicManager::enqueue`, whose drop-on-full policy is unchanged.

This is deliberately not the removed dashboard `inject_message` fallback: that
was a channel-scoped direct injection in which the dashboard supplied routing
metadata. `/api/send` has no channel in its request — it addresses a topic name
through the live managers and refuses an unknown target.

#### Component Map

```
TopicEvent (crates/jyc-core/src/topic_event.rs)
  ├── +IncomingMessage variant
  └── +ReplySent variant

TopicManager (crates/jyc-core/src/topic_manager.rs)
  ├── enqueue(): publish IncomingMessage on TopicEventBus
  ├── worker reply path: publish ReplySent after send_reply() succeeds
  └── process_message(): write .jyc/topic-meta.json on first message
      (routing metadata for TopicProxyHandler)

TopicActivityState (crates/jyc-inspect/src/server.rs)
  ├── +recent_messages: VecDeque<ChatMessageEntry>  (capped at ~50)
  └── +next_id: u64 (monotonic per-topic counter, activity rows only —
      chat rows carry the number the chat log allocated, see chat_seq)

ActivityTracker (crates/jyc-inspect/src/server.rs)
  ├── subscriber loop: capture IncomingMessage/ReplySent → recent_messages
  └── fanout: publish activity/chat_message/thinking/processing events
      to InspectContext.broadcast (capacity 256)

InspectContext (crates/jyc-inspect/src/server.rs)
  └── +inspect_broadcast: Arc<broadcast::Sender<String>>
      Per-channel broadcast bus fed by ActivityTracker; consumed by
      TopicProxyHandler and ScopedWsHandler (for filtering).

WsRoute (crates/jyc-inspect/src/server.rs)
  ├── Bare: GET /ws
  ├── Channel(name): GET /ws/<channel>
  └── Topic { channel, name }: GET /ws/<channel>/<topic>

TopicProxyHandler (crates/jyc-inspect/src/topic_proxy.rs)
  ├── inbound {message, disconnect}
  ├── outbound: filtered events from inspect_broadcast
  └── emits resync on Lagged

ScopedWsHandler (crates/jyc-inspect/src/scoped_ws.rs)
  └── wraps any WebsocketHandler; pre-sends {type:"subscribe", topic}
      before delegating to the inner handler (used to bind the inner
      WebsocketInboundAdapter to a specific topic from the URL path)

InspectOverview / TopicSummary (crates/jyc-types/src/inspect.rs)
  ├── Slim overview payload — same shape as InspectState but without
  │   activity / recent_messages / thinking_text per topic, and with
  │   branch / changed_files left null (no per-topic, per-poll git probe)
  └── Used by the dashboard's polling loop to keep payloads small

InspectState / TopicInfo (crates/jyc-types/src/inspect.rs)
  ├── ActivityEntry { id, text, timestamp, severity }
  ├── ChatMessageEntry { id, sender, text, timestamp, client_msg_id? }
  │   (id = the topic's chat sequence number, written into the log line)
  └── TopicInfo.{activity, recent_messages, thinking_text, ...} (full payload,
      retained for backward compat with any external clients)

InspectServer (crates/jyc-inspect/src/server.rs)
  ├── build_state(): full snapshot with activity/messages
  ├── build_overview_state(): slim snapshot (no activity/messages)
  ├── get_topic_activity: read .jyc/activity.jsonl
  ├── get_topic_chat: read chat_history_*.jsonl
  ├── get_topic_info: one topic's full TopicInfo, git probe included
  ├── get_state_overview / get_state: handled by handle_request
  └── reload_config: handled by handle_request

InspectClient (crates/jyc-inspect/src/client.rs)
  ├── get_state() → InspectState (full, retained for compatibility)
  ├── get_overview() → InspectOverview (slim, used by dashboard poll)
  ├── get_topic_activity(channel, topic, since, limit) → Vec<ActivityEntry>
  ├── get_topic_chat(channel, topic, since, limit) → Vec<ChatMessageEntry>
  ├── get_topic_info(channel, topic) → TopicInfo (branch + changed_files)
  └── reload_config() → (bool, String)

Dashboard (crates/jyc-cli/src/cli/dashboard/)
  ├── main poll: client.get_overview() every 1s (slim payload)
  ├── detail poll: client.get_topic_info() for the shown topic every 4s, its
  │   branch/changed_files stamped onto that row of the polled summaries
  ├── on topic selection: REST hydrate (get_topic_activity + get_topic_chat)
  │   + WS connect to /ws/<channel>/<topic>
  ├── ChatState.live_activity / live_chat / live_thinking / live_processing:
  │   BTreeMap<(channel, topic), ...>; populated by hydrate + WS events
  ├── Activity pane + chat progress read from these live buffers
  │   (rendering logic unchanged from before the refactor)
  └── handle_live_event() appends WS events to the live buffers with a
      per-stream watermark (activity ids and chat sequence numbers are
      separate spaces), and folds the chat pane on receipt
      (`sync_live_if_visible`) — no timer between a reply and the screen
```

#### Key Design Decisions

| Decision | Rationale |
|---|---|
| `TopicEventBus` over new broadcast channel | Reuses existing infrastructure; events already scoped per-topic; ActivityTracker already subscribes |
| `IncomingMessage` published in `TopicManager::enqueue()` | Captures ALL message sources (remote IMAP, Feishu WS, GitHub poll, dashboard) in one place |
| `ReplySent` published after `send_reply()` succeeds | Guarantees the reply was actually delivered; same timing as existing metrics counters |
| `recent_messages` in `TopicActivityState`, not `TopicInfo` directly | TopicInfo is rebuilt from scratch on each poll; ActivityTracker maintains rolling buffer |
| Single WebSocket endpoint for all channels (`/ws/<channel>/<topic>`) | Decouples dashboard from channel type; `TopicProxyHandler` handles non-WS channels uniformly |
| Per-channel broadcast bus (`InspectContext.inspect_broadcast`) | One shared bus; handlers filter by `(channel, topic)` from each payload |
| `id: u64` monotonic per-topic for dedup | Survives WS reconnect / Lagged recovery; small field, backward compat via `#[serde(default)]` |
| Chat row ids allocated by `chat_seq` (one owner: `jyc-core`), not by the inspect counter | The live row and the logged row are the same row only if both carry the same number; the number has to exist before either consumer runs (enqueue for a message, delivery for a reply), so the log cannot mint it and the observer cannot either. Sharing the activity counter instead made one watermark describe two number spaces |
| Chat dedup by membership on screen, not by a high-water mark | Numbers are handed out in arrival order, but a row is written when it is delivered or injected: a message that arrives mid-turn is logged *after* replies that took higher numbers, so neither the log nor a hydrate is in id order. A high-water mark skipped that row for the rest of the session, and seeding from the file's last line could hand out a number already used |
| `resync` on `Lagged(n)` instead of panicking | Client re-hydrates via REST; same code path as cold start |
| REST hydrate on selection + WS for live | Cold start from disk (authoritative); warm path push-based (low latency) |
| `chat.progress` and `activity` pane read from the **same** `live_activity` buffer | Single source of truth; both panes show identical data |
| Reuse `TopicManager::enqueue` for both REST-injected and WS-injected messages | Same path as `send_to_topic` tool; no duplicate routing logic |
| `ChatMessageEntry` struct shared in `jyc_types` | Lets both inspect server and dashboard use the same type without coupling |
| Drop `inject_message` REST method | Replaced by WS; no consumer since only the TUI used it |
| Per-topic broadcast capacity 256 (vs. 64 for reply bus) | Activity events burstier than AI replies; 256 absorbs 5-min TUI offline gaps |

### MetricsCollector

Components report events via `MetricsHandle`:

- `message_received(topic)`, `message_matched(topic)`
- `reply_by_tool(topic)`, `reply_by_fallback(topic)`
- `processing_error(topic, error)`, `queue_dropped(topic)`

Stats are accumulated in `Arc<Mutex<HealthStats>>`, queryable by the inspect server. No email dependency.

### Source Tree

> Historical: this tree is the pre-split single-crate layout (`src/channels`,
> `src/core`, `src/services`, …). Those modules now live in separate crates —
> see [docs/architecture/overview.md](docs/architecture/overview.md) and
> [docs/architecture/pipe-split.md](docs/architecture/pipe-split.md).

```
jyc/
├── Cargo.toml
├── DESIGN.md
├── src/
│   ├── main.rs                          # Entry point, clap CLI
│   ├── cli/
│   │   ├── mod.rs
│   │   ├── serve.rs                     # `jyc serve` — wiring
│   │   ├── dashboard.rs                 # `jyc dashboard` — ratatui TUI
│   │   ├── config.rs                    # `jyc config init/validate`
│   │   ├── state.rs                     # `jyc state`
│   │   └── mcp.rs                       # `jyc mcp` — MCP server probing
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
│   │   │   └── inbound.rs              # EmailMatcher (pattern matching)
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
│   │       └── inbound.rs              # GithubInboundAdapter (polling + matcher)
│   ├── core/
│   │   ├── mod.rs
│   │   ├── topic_manager.rs           # Per-topic queues + semaphore
│   │   ├── topic_manager_tests.rs     # TopicManager integration tests
│   │   ├── topic_path.rs              # Central path resolution for topics
│   │   ├── topic_event_bus.rs         # Topic-isolated event bus
│   │   ├── topic_event.rs             # TopicEvent types
│   │   ├── message_router.rs           # Pattern match → dispatch
│   │   ├── message_storage.rs          # Markdown file I/O
│   │   ├── chat_log_store.rs           # Chat log storage (daily log files)
│   │   ├── email_parser.rs             # Stripping, quoting, topic trail
│   │   ├── state_manager.rs            # UID tracking, state persistence
│   │   ├── metrics.rs                   # MetricsCollector
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
│   │       ├── close_handler.rs        # /close command (topic cleanup)
│   │       └── template_handler.rs     # /template command (re-apply template)
│   ├── services/
│   │   ├── mod.rs
│   │   ├── agent.rs                   # AgentService trait (process → AgentResult)
│   │   ├── static_agent.rs            # StaticAgentService (fixed text reply)
│   │   ├── agent/
│   │   │   ├── mod.rs                 # agent service manager (start/stop, port, health)
│   │   │   ├── service.rs            # AgentService implements AgentService
│   │   │   ├── client.rs             # in-process agent HTTP + SSE client
│   │   │   ├── session.rs            # Session + agent config + signal file management
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
│   │   ├── types.rs                  # InspectState, ChannelInfo, TopicInfo, protocol
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

### Chat Log Entry Format (`chat_history_YYYY-MM-DD.jsonl`)

Each line is a JSON object with the following fields:

```json
{"ts":"2026-03-19T23:02:20Z","type":"received","matched":true,"sender":"user@example.com","sender_name":"User Name","channel":"email","external_id":"<abc123@mail.example.com>","topic":"Help with feature X","from":"User Name (user@example.com)","content":"Message body"}
```

```json
{"ts":"2026-03-19T23:02:21Z","type":"reply","matched":true,"sender":"jyc-bot","channel":"jyc","model":"ark/deepseek-v3.2","mode":"build","content":"AI reply text"}
```

### Chat Log Storage

Messages and replies are appended to daily chat log files:

```
chat_history_2026-03-19.jsonl     # All messages and replies for this date
chat_history_2026-03-20.jsonl     # Next day's log (auto-rotated by date)
```

Each log file contains chronological entries:

- Incoming messages — appended with sender, timestamp, and body
- AI replies — appended after sending, with model and mode metadata
- Attachments — saved in the topic directory by the inbound adapter (if allowlist config enabled)

## Logging & Tracing

### Library Choice: `tracing` + `tracing-subscriber`

JYC uses the `tracing` ecosystem for all logging and diagnostics:

| Aspect                | Detail                                                                                        |
| --------------------- | --------------------------------------------------------------------------------------------- |
| **Crate**             | `tracing` 0.1.x + `tracing-subscriber` 0.3.x                                                  |
| **Why not `log`**     | `tracing` provides structured fields, async-aware spans, and custom subscriber layers         |
| **Span architecture** | Layered spans provide automatic context (component, channel, topic, model) on every log line |
| **Env filter**        | `RUST_LOG=jyc=info,async_imap=warn` controls per-module verbosity                             |
| **CLI flags**         | `--debug` sets `jyc=debug`, `--verbose` sets `jyc=trace,async_imap=debug`                     |

### Layered Span Architecture

Every log line automatically includes context from hierarchical `tracing` spans. Spans are layered from general to specific:

```
Layer 1: component     (always present — identifies the subsystem)
  Layer 2: channel     (present when processing a specific channel)
    Layer 3: topic    (present when processing a specific topic)
      Layer 4: model/mode  (present during AI session)
```

#### Span Definitions

| Span Name | Layer    | Fields              | Where Created                    | Propagation                   |
| --------- | -------- | ------------------- | -------------------------------- | ----------------------------- |
| `inbound` | L1+L2    | `channel`           | `cli/serve.rs` — per IMAP task   | `tokio::spawn().instrument()` |
| `worker`  | L1+L2+L3 | `channel`, `topic` | `topic_manager.rs` — per worker | `tokio::spawn().instrument()` |
| `metrics` | L1       | —                   | `metrics.rs` — background task   | `tokio::spawn().instrument()` |

Logs within instrumented futures automatically inherit all parent span fields. For example, a log in `agent/service.rs` called from within a `worker` span shows:

```
INFO worker{channel=jiny283, topic=weather}: Sending prompt to in-process agent mode=build
INFO worker{channel=jiny283, topic=weather}: AI model selected model=deepseek-v3.2
INFO worker{channel=jiny283, topic=weather}: Tool running tool=grep
INFO worker{channel=jiny283, topic=weather}: Session idle — prompt complete
```

#### How Spans Propagate in Async Code

```
cli/serve.rs:
  tokio::spawn(async { ... }.instrument(info_span!("inbound", channel = %ch)))
    → imap/monitor.rs: start() — all logs inherit inbound{channel}
      → message_router.rs: route() — inherits inbound{channel}
        → topic_manager.rs: enqueue() — creates new worker span

  tokio::spawn(async { ... }.instrument(info_span!("worker", channel, topic)))
    → process_message() — inherits worker{channel, topic}
      → command/registry.rs: process_commands() — inherits worker{channel, topic}
      → agent.process() — inherits worker{channel, topic}
        → agent/service.rs: generate_reply() — inherits worker{channel, topic}
          → agent/client.rs: prompt_with_sse() — inherits worker{channel, topic}
            → handle_sse_event() — inherits (sync, called within instrumented future)
```

#### Log Output Examples

```
INFO inbound{channel=jiny283}: Starting IMAP monitor mode="poll" folder=INBOX
INFO inbound{channel=jiny283}: IMAP connected and authenticated host=imap.163.com
INFO inbound{channel=jiny283}: Message received uid=123 sender=kingye@petalmail.com
INFO worker{channel=jiny283, topic=weather}: Worker started
INFO worker{channel=jiny283, topic=weather}: Message stored sender=kingye@petalmail.com
INFO worker{channel=jiny283, topic=weather}: Sending prompt to in-process agent mode=build
INFO worker{channel=jiny283, topic=weather}: AI model selected model=deepseek-v3.2
INFO worker{channel=jiny283, topic=weather}: Tool running tool=jiny_reply_reply_message
INFO worker{channel=jiny283, topic=weather}: Session idle — prompt complete
INFO worker{channel=jiny283, topic=weather}: Reply sent by MCP tool
INFO worker{channel=jiny283, topic=weather}: Agent complete reply_sent=true
INFO worker{channel=jiny283, topic=weather}: Worker finished
```

#### Key Rules

- **`tokio::spawn` does NOT inherit parent spans** — each spawned task must be explicitly instrumented with `.instrument(span)`
- **`.instrument(span)` works across `.await` points** — unlike `span.enter()` which only works in sync code
- **Sync methods called within instrumented async blocks** inherit the parent span automatically (e.g., `handle_sse_event()`)
- **MCP reply tool** runs as a separate process — no span inheritance. Uses its own file-based logger.
- **Individual log calls** only include per-event fields (e.g., `tool`, `uid`, `error`). Context fields (channel, topic) come from the span.

### Log Levels

| Level | Usage                                                                                                  |
| ----- | ------------------------------------------------------------------------------------------------------ |
| ERROR | Unrecoverable failures, processing errors, MCP tool errors                                             |
| WARN  | Recoverable issues: queue full, stale session, timeout, reconnection                                   |
| INFO  | Lifecycle: message received, matched, processed, reply sent, worker start/stop, step start, tool calls |
| DEBUG | SSE events, session status changes, step finish with costs, AI response text, config details           |
| TRACE | IMAP polling, mailbox select, skipping heartbeat notifications                                         |

## Command System

### Available Commands

| Command        | Description                                                   | Example                                            |
| -------------- | ------------------------------------------------------------- | -------------------------------------------------- |
| `/model <id>`  | Switch AI model for this topic                               | `/model SiliconFlow/Pro/deepseek-ai/DeepSeek-V3.2` |
| `/model`       | List available models                                         | `/model`                                           |
| `/model reset` | Reset to default model from config                            | `/model reset`                                     |
| `/plan`        | Switch to plan mode (read-only by convention — the prompt; a system boundary needs `disabled_tools`)         | `/plan`                                            |
| `/build`       | Switch to build mode (full execution, default)                | `/build`                                           |
| `/reset`       | Clear the current in-process agent session (start fresh context)      | `/reset`                                           |
| `/close`       | Close the current topic, or one of its own direct subagents (`/close [subagent\|all]`): for a subagent the command is sent to that topic, which closes itself — `--force` required, `--purge` deletes the directory too | `/close --force`                                   |
| `/template`    | Re-apply the pattern's topic template files                  | `/template`                                        |

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
    pub topic_path: PathBuf,
    pub config: Arc<AppConfig>,
    pub channel: String,
}

pub struct CommandResult {
    pub success: bool,
    pub message: String,               // User-facing result text
    pub requires_restart: bool,        // Whether agent service needs restart
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
    /// Returns executed results + cleaned body. TopicManager does NOT need to
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

**TopicManager usage** (simplified — no command syntax knowledge needed):

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
    send_direct_reply(&message, &summary, topic_path, message_dir).await?;
    return Ok(());
}

// Continue with output.cleaned_body for AI processing
message.content.text = Some(output.cleaned_body);
```

**Key design decisions:**

- **Single pass**: Commands are parsed, executed, and stripped in one scan of the body.
- **Single responsibility**: All command-related logic (parsing, executing, stripping) lives in `CommandRegistry`. `TopicManager` only checks `body_empty` and `results`.
- **Testable**: One function in, one struct out. Easy to unit test without mocking TopicManager.

### Command Argument Values (`command/args.rs`)

The TUI's `/` popup is multi-level: `/model <id>`, `/thinking show`,
`/skill on <name>`, `/context dump on`. Which values a command accepts is
handler knowledge, so it lives server-side, not in the client:
`jyc_core::command::command_args(name, ctx)` returns a recursive
`Vec<CommandArg>` for one command, and `jyc-inspect` folds it into
`CommandInfo::args` while building the topic payload. The popup then walks
that tree and needs no command knowledge at all — `/model` is a row of the
table, not a special case in `jyc-cli`.

- **The field text is the path.** `/skill on po` means "values of
  `/skill on`, filtered by `po`", so no level state is kept anywhere; the
  popup's position is derived from the input field on every key.
- **Empty `args` means free text.** `/grant <path>` and `/backlog push <text>`
  complete nothing; the popup closes and the field sends what was typed.
- **A flag is declared once.** Its spellings, the value it takes and whether a
  repeat of it is refused come from one table (`jyc_core::command::flags`),
  which both the popup's flag level and the parser read — so the popup cannot
  offer a spelling a handler does not accept, and a short spelling is not prose
  in a description. What a value *means* stays with the command.
- **A command's argument shape and its values are one declaration.** The
  built-in table (`command/builtin.rs`) already named each command and
  described it; a row now also declares its `shape` (`command/spec.rs`: no
  arguments, fixed positionals, one positional then trailing text, or a
  subcommand) and the values its first argument level offers (`args`, one
  declaration per command in `command/args.rs`: key, description, and what the
  key opens) — and hands its flags to the table through the projection of the
  one typed declaration its handler matches on — a projection rather than a
  second copy, because only that command knows its own flag enum. One parser
  (`spec::parse`) reads a command line through that shape and **records**: which
  flags appeared with what values, which words were positional, where the
  trailing text begins. Which arguments a command *needs*, and what a value
  means, stay with the command — those sentences are its user interface. The
  refusals the shape itself can raise (a spelling nobody declared, a value that
  did not arrive, a repeat the declaration forbids, a value handed to a switch,
  a word too many) are worded once, in `spec::ArgError`, because they are the
  same mistake in every command — and where a line used to be quietly ignored,
  it is now refused. The usage line is derived from the same declaration, so the
  popup, `/?` and a handler's own refusals cannot advertise a syntax the parser
  refuses.
- **The value set is declared once, and read twice.** A subcommand's choice set
  used to be written in `Shape::Subcommand::values` *and* in the `command_args`
  match arm, with a guard test asserting the two lists agreed — the duplication
  was visible enough to need a test to hold it together. It is now one list
  (`CommandSpec::args`) with two projections: the popup resolves it against
  per-topic state (`ArgSource::Provided`/`Mixed` fill in the values only the
  topic knows — a topic's skills, the installed role names, a long model list),
  while the usage line names the same keys — for a `Shape::Args` row too, where
  the declared values are folded into the slot the label names (`/model`'s
  `reset` is read from `args`, not spelled a second time in the label). A level
  only per-topic state can produce takes its slot from the shape's label
  instead, since the usage line has no state to read. The one remaining seam is
  `usage_words` — words the popup *cannot* offer, like `/bill`'s `today` and the
  `YYYY-MM` pattern, which stand in for the months the popup offers instead —
  and a test refuses a word that also names a value the popup offers, at any
  level, which is what would make it a second copy.
- **Trade-off**: the values are computed per topic on every ~1 s overview
  poll and serialized per topic, so a long model list is repeated for every
  topic (`topics × models`). Leaf entries omit their empty `description`/`args`
  on the wire to keep that cheap — but `usage` is not empty for any row, so it
  rides the poll as one more string per command per topic: the field is always
  sent (`#[serde(default)]`, with no `skip_serializing_if`, unlike the `args`
  beside it). What an empty one does is leave `CommandInfo::summary` with the
  description alone — which is what a server older than the field sends, and why
  an older client still renders. The real fix is the lazy per-topic
  command fetch in the `TODO(perf)` in the inspect overview builder — which
  also removes the per-poll recompute.
  `/exchange` (a directory scan) and `/ungrant` (agent runtime state) are
  deliberately absent from the table until commands are fetched lazily per
  topic.

### Model Override Persistence

The `/model` command writes the model ID to `.jyc/model-override` in the topic directory. This persists across messages — subsequent emails in the same topic use the overridden model until `/model reset` is sent.

### Plan/Build Mode

- **Plan mode**: the in-process agent is told, in the prompt, that it is read-only — the AI is not supposed to edit files or run modifying commands, and it says so in the turn it answers with. This is **not** enforced at the tool level: `write`, `edit` and `bash` are still registered, so a role that must not change files gets that from `disabled_tools` (a plain prompt is a convention, and the model can be talked out of it)
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

| Stage              | Where                           | Strips history? | Cleans? | Purpose                                         |
| ------------------ | ------------------------------- | --------------- | ------- | ----------------------------------------------- |
| **Inbound**        | `EmailInboundAdapter`           | No              | Yes     | Clean at boundary                               |
| **Storage**        | `MessageStorage::store()`       | No              | No      | Append to daily chat log                        |
| **AI Prompt Body** | `PromptBuilder::build_prompt()` | Yes             | No      | Incoming message for AI                         |
| **Reply context**  | `.jyc/reply-context.json`       | N/A             | N/A     | Saved to disk before prompt, read by reply tool |
| **Reply Tool**     | `mcp/reply_tool.rs`             | No              | No      | Reads reply-context.json for routing metadata   |
| **Outbound**       | `SmtpClient`                    | No              | No      | Dumb transport + attachments                    |

## Security Considerations

- Environment variables for credentials (never commit passwords)
- Validate regex patterns at config load time to prevent ReDoS
- Rate limiting for AI API calls
- Path validation for all file operations (`PathValidator`)
- Attachment security: extension allowlist, size limit, filename sanitization
- MCP tool: validate context before processing
- `permission: { "*": "allow", "question": "deny", "external_directory": "deny" }` in agent config
- Rust's ownership model eliminates data races, use-after-free, and buffer overflows
- `system.md` per-topic customization — file permissions should restrict who can modify topic directories

## Crate Dependencies

| Crate                 | Version                                | Purpose                           |
| --------------------- | -------------------------------------- | --------------------------------- |
| `tokio`               | 1.x (features: full)                   | Async runtime                     |
| `clap`                | 4.x (features: derive)                 | CLI argument parsing              |
| `async-imap`          | 0.12.x (features: runtime-tokio)       | IMAP client with IDLE             |
| `async-native-tls`    | 0.6.x                                  | TLS for IMAP                      |
| `mail-parser`         | 0.11.x                                 | MIME email parsing                |
| `lettre`              | 0.11.x (features: tokio1-native-tls)   | SMTP sending                      |
| `comrak`              | 0.56.x                                 | Markdown → HTML (GFM)             |
| `htmd`                | 0.5.x                                  | HTML → Markdown                   |
| `reqwest`             | 0.13.x (hub: native-tls, pipe: rustls) | HTTP client                       |
| `rmcp`                | 3.x (client + child-process/streamable-http transports, auth; dev: server, transport-io) | MCP client (and a test MCP server) |
| `serde`               | 1.x (features: derive)                 | Serialization framework           |
| `serde_json`          | 1.x                                    | JSON serialization                |
| `toml`                | 1.x                                    | TOML config parsing               |
| `tracing`             | 0.1.x                                  | Structured async-aware logging    |
| `tracing-subscriber`  | 0.3.x (features: env-filter, fmt)      | Log output formatting + filtering |
| `anyhow`              | 1.x                                    | Application error handling        |
| `thiserror`           | 2.x                                    | Typed library errors              |
| `chrono`              | 0.4.x (features: serde)                | Date/time handling                |
| `base64`              | 0.23.x                                 | Base64 encoding/decoding          |
| `regex`               | 1.x                                    | Pattern matching                  |
| `uuid`                | 1.x (features: v4)                     | Internal message IDs              |
| `tokio-util`          | 0.7.x                                  | CancellationToken                 |
| `async-trait`         | 0.1.x                                  | Async trait support               |

## Topic Event System

The Topic Event System is a core component for handling inter-topic event communication in JYC. It implements SSE event to TopicEvent conversion with topic isolation.

### Architecture

#### Core Components

1. **in-process agent Client** - SSE event conversion layer
2. **Topic Event Bus** - Topic-isolated event bus (publish/subscribe)
3. **Topic Manager** - Event listening and forwarding layer

#### Data Flow

```
SSE Events (in-process agent Server)
    ↓
in-process agent Client Conversion
    ├── ProcessingStarted → TopicEvent::ProcessingStarted
    ├── ProcessingProgress → TopicEvent::ProcessingProgress
    ├── ToolStarted/Completed → TopicEvent::ToolStarted/Completed
    └── server.heartbeat → ignored (connection keep-alive only)
    ↓
Publish to Topic's Event Bus
    ↓
Topic Manager Event Listener
    └── Receive events and forward to subscribers (e.g. inspect dashboard)
```

### Topic Isolation Design

#### Key Features

1. **Per-topic isolated event bus** - Each topic uses a `SimpleThreadEventBus` instance
2. **No cross-topic event propagation** - Complete isolation

#### Implementation

```rust
// TopicManager creates isolated event bus for each topic
let event_bus = Arc::new(SimpleThreadEventBus::new(10));

// Event listener subscribes only to its topic's event bus
let mut receiver = event_bus.subscribe().await;
```

### TopicEvent Types

#### Event Enumeration

```rust
pub enum TopicEvent {
    // Processing state events (published by in-process agent Client)
    ProcessingStarted { ... },
    ProcessingProgress { ... },
    ProcessingCompleted { ... },

    // Tool execution events
    ToolStarted { ... },
    ToolCompleted { ... },
}
```

#### Event Sources

| Event Type            | Publisher       | Purpose                               |
| --------------------- | --------------- | ------------------------------------- |
| ProcessingStarted     | in-process agent Client | Published when processing starts      |
| ProcessingProgress    | in-process agent Client | Periodic progress updates             |
| ProcessingCompleted   | in-process agent Client | Published when processing completes   |
| ToolStarted/Completed | in-process agent Client | Tool start/complete events            |

### Configuration

#### Topic Manager Initialization

```rust
// Enable Topic Event system by default
let topic_manager = TopicManager::new_with_options(
    max_concurrent,
    max_queue_size,
    storage,
    outbound,
    agent,
    cancel.child_token(), // child token — prevents cascade shutdown
    true, // enable_events: true (Topic Event system)
);
```

### Error Handling

#### Event Publishing Errors

- Event publishing failures do not block the main process
- Asynchronous non-blocking event publishing
- Appropriate logging for failures

### Performance Considerations

1. **Asynchronous non-blocking** - All event publishing is asynchronous
2. **Topic-local state** - Each topic maintains independent event bus state
3. **Lightweight events** - Event structures remain simple
4. **Limited queues** - Event buses use limited capacity queues

### Testing Strategy

#### Unit Tests

- Event type serialization/deserialization
- Event bus basic functionality

#### Integration Tests

- SSE event to TopicEvent conversion
- Inter-topic event isolation

#### End-to-End Tests

- Complete event flow: SSE → TopicEvent → inspect dashboard / event subscribers
- Multi-topic concurrent processing
- Error scenario handling

### Deployment Notes

1. **Log levels** - Adjust event log levels appropriately in production
2. **Resource limits** - Pay attention to memory usage of event queues

### Invoice Download Proxy

Some invoice platforms (e.g., 51fapiao.cn) use Alibaba Cloud WAF that blocks
requests from non-mainland China IPs. The invoice-processing skill includes a
download proxy to handle this:

**Architecture:**

```
Overseas Server (HK)                    Mainland China Server (Shanghai)
┌────────────────────┐                  ┌─────────────────────────┐
│ JYC + in-process agent     │                  │ download_proxy.py :8765 │
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
  Configured in `~/.zshrc.local` on the HK server (sourced by `scripts/deploy.sh`).

**Deployment scenarios:**

| Server Location                 | Env Var Needed? | Behavior                      |
| ------------------------------- | --------------- | ----------------------------- |
| Mainland China (e.g., Shanghai) | No              | Direct download works         |
| Overseas (e.g., Hong Kong)      | Yes             | Direct fails → proxy fallback |

### Templates & Skills

Agent templates define the role, instructions, and skills for each topic type.
Templates live in the repo under `templates/`. Skills are discovered from multiple
priority paths at runtime, with topic-local paths overriding repo paths, which
override system paths. A deploy script composes them at deploy time.

**Repository structure:**

```
templates/                      ← AGENTS.md only (role + instructions)
  invoice-processing/AGENTS.md
  jyc-dev/AGENTS.md
  jyc-review/AGENTS.md
  github-planner/AGENTS.md
  github-developer/AGENTS.md
  github-reviewer/AGENTS.md

.agent/skills/               ← Default in-repo skills (lowest priority per-topic)
  invoice-processing/           ← Invoice extraction workflow
  dev-workflow/                 ← Branching, commits, releases
  incremental-dev/              ← Small-step development methodology
  plan-solution/                ← Structured implementation planning
  pr-review/                    ← Read-only PR review
  jyc-deploy-bare/              ← Bare metal deployment
  jyc-deploy-docker/            ← Docker deployment

.claude/skills/                 ← Alternative in-repo skills (overrides .agent/)
.jyc/skills/                    ← Topic-local override (highest in-repo priority)
```

**Multi-path skill discovery:**

When building an agent's system prompt, `discover_skills()` scans 9 paths in
priority order (lowest first, highest last). Same-named skills from
higher-priority paths override those from lower-priority paths, so a
`.jyc/skills/coding-principles/SKILL.md` completely replaces one in
`.agent/skills/coding-principles/SKILL.md`.

| Priority | Path | Scope |
|----------|------|-------|
| 1 (lowest) | `$HOME/.config/agent/skills/` | User system |
| 2 | `$HOME/.claude/skills/` | User system |
| 3 | `{jyc-data}/skills/` | JYC data directory |
| 4 | `{topic}/repo/.claude/skills/` | Topic repo |
| 5 | `{topic}/repo/.agent/skills/` | Topic repo |
| 6 | `{topic}/repo/.jyc/skills/` | Topic repo (highest in repo) |
| 7 | `{topic}/.claude/skills/` | Topic-local |
| 8 | `{topic}/.agent/skills/` | Topic-local |
| 9 (highest) | `{topic}/.jyc/skills/` | Topic-local override |

Topic-local `.jyc/skills/` is the highest priority path — it provides a
mechanism for overriding any skill on a per-topic basis without modifying the
shared repository.

**Runtime skill injection (lazy loading):**

Each `SKILL.md` must start with YAML frontmatter (delimited by `---`) containing
`name:` and `description:` fields. The frontmatter is parsed into `SkillMeta`:
- `name` — from frontmatter `name:` field
- `description` — from frontmatter `description:` field (supports `|`, `|-`, `>` block scalars)
- `source_path` — absolute path to the skill directory (filled in by the discoverer)

Skills without valid frontmatter are skipped silently.

Example `SKILL.md` header:
```yaml
---
name: plan-solution
description: |
  Create structured implementation plans with incremental steps.
  Use when: in plan mode, creating implementation plans, analyzing requirements.
---
```

When building the system prompt, `format_skills_section()` injects an `## Available Skills`
section that lists each discovered skill with name + description + path:

```markdown
## Available Skills

- **plan-solution** (at /home/jiny/.agent/skills/plan-solution)
  Create structured implementation plans with incremental steps.

- **invoice-processing** (at /home/jiny/projects/jyc-data/feishu_bot/workspace/invoice-processing/.jyc/skills/invoice-processing)
  Process invoices from messages (PDF/image attachments or URLs).

To load a skill's full instructions, use `read <skill-path>/SKILL.md`.
All file paths within a SKILL.md are relative to that skill's directory.
When running skill scripts: cd <skill-path> && <command>
```

This is **lazy loading**: only skill names + descriptions appear in the system
prompt, keeping it small. The LLM reads the full `SKILL.md` content on demand
via the `read` tool when it determines a skill is relevant. Skills with full
content can be 10-50KB each — embedding them all would bloat the prompt.

**Persistence for dashboard inspection:**

Discovered skill names are persisted to `.jyc/skills.json` per topic, allowing
the inspect server / dashboard to display which skills are active for each
topic without re-running discovery.

**Skill → Template mapping:**

| Template           | Skills                                                        | Purpose                                     |
| ------------------ | ------------------------------------------------------------- | ------------------------------------------- |
| invoice-processing | invoice-processing                                            | Invoice data extraction and Excel recording |
| jyc-dev            | plan-solution, dev-workflow, incremental-dev, jyc-deploy-bare | JYC self-development                        |
| jyc-review         | pr-review                                                     | Code review                                 |
| github-planner     | dev-workflow                                                  | Discuss requirements, create PRs            |
| github-developer   | incremental-dev, dev-workflow                                 | Implement code from PR spec                 |
| github-reviewer    | pr-review                                                     | Review PRs, approve/request changes         |

**Deployment:**

```bash
# Compose templates + skills and deploy to server data directory
./deploy-templates.sh /home/jiny/projects/jyc-data/templates
```

The script copies each template's `AGENTS.md` and its referenced skills from
the repository's skill directories into the target directory. When a topic is
created with a template, the `AGENTS.md` is copied to the topic workspace.
Skills are then discovered at runtime from all 9 priority paths (see table above)
via `discover_skills()`, with topic-local paths overriding any repo or system
skills of the same name.

**Adding a new template:**

1. Create `templates/<name>/AGENTS.md` with role instructions
2. Add the skill mapping to `deploy-templates.sh` (`get_skills` function)
3. Run `./deploy-templates.sh <target>` to deploy


## References

- [CHANGELOG.md](CHANGELOG.md) - Version history and changes
