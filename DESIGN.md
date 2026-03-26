# JYC: Channel-Agnostic AI Agent (Rust)

## Overview

JYC is a Rust rewrite of jiny-m — a channel-agnostic AI agent that operates through messaging channels. Users interact with the agent by sending messages (email, FeiShu, Slack, etc.), and the agent responds autonomously using OpenCode AI. The agent maintains conversation context per thread, enabling coherent multi-turn interactions.

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
┌─────────────────────────────────────────────────────────────────────────┐
│                  Inbound Channels (tokio tasks, run concurrently)        │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐                  │
│  │ Email Inbound│  │FeiShu Inbound│  │ Slack Inbound│ (future)         │
│  │  (IMAP/TLS)  │  │  (WebHook)   │  │  (WebHook)   │                  │
│  │              │  │              │  │              │                  │
│  │ match_message│  │ match_message│  │ match_message│                  │
│  │ derive_thread│  │ derive_thread│  │ derive_thread│                  │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘                  │
└─────────┼──────────────────┼──────────────────┼────────────────────────┘
          │                  │                  │
          ▼                  ▼                  ▼
    InboundMessage     InboundMessage     InboundMessage
    (channel:"email")  (channel:"feishu") (channel:"slack")
          │                  │                  │
          └────────┬─────────┘──────────────────┘
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
│  └─────────────┘  └─────────────┘  └─────────────┘                     │
│                                                                          │
│  New thread arrives → tokio::spawn → acquire semaphore permit            │
│  Worker loop: recv from thread's mpsc → process → recv next              │
│  Thread queue empty + no pending → release permit, task exits            │
└────────────────────────┬────────────────────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                    Worker (per message)                                   │
│                                                                          │
│  0. If agent.progress enabled: ProgressTracker::start()                  │
│  1. MessageStorage::store(msg) → messages/<ts>/received.md               │
│  2. Save inbound attachments (allowlisted)                               │
│  3. CommandRegistry::process_commands(body, ctx)                         │
│     → parse, execute, strip in single pass → cleaned body + results      │
│  4. If body empty after commands → direct reply with results, return     │
│  5. PromptBuilder::build_prompt(msg) → prompt with <reply_context>       │
│  6. OpenCodeService::generate_reply(msg) — SSE streaming                 │
│                                                                          │
│  ┌─────────────────────────────────────────┐                            │
│  │  MCP Tool: reply_message (subprocess)   │                            │
│  │  Binary: jyc mcp-reply-tool             │                            │
│  │  Transport: stdio (rmcp)                │                            │
│  │                                         │                            │
│  │  1. Decode base64 context token         │                            │
│  │  2. Read received.md → full body        │                            │
│  │  3. Build full reply (AI + quoted hst)  │                            │
│  │  4. Instantiate OutboundAdapter         │                            │
│  │  5. adapter.send_reply(full_reply_text) │                            │
│  │  6. storage.store_reply(full_reply_text)│                            │
│  │  7. Write signal file                   │                            │
│  └──────────────────┬──────────────────────┘                            │
│                     │                                                    │
│  7. Fallback: ThreadManager sends via OutboundAdapter                    │
│  8. ProgressTracker::stop()                                              │
│  9. Worker picks next message from thread queue                          │
└─────────────────────┼───────────────────────────────────────────────────┘
                      │
                      ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                     Outbound Channels (Reply)                            │
│  context.channel → ChannelRegistry → OutboundAdapter                     │
│  ┌───────────────┐  ┌───────────────┐  ┌───────────────┐               │
│  │ Email Outbound│  │FeiShu Outbound│  │ Slack Outbound│ (future)      │
│  │  (SMTP/TLS)   │  │  (API)        │  │  (API)        │               │
│  │               │  │               │  │               │               │
│  │ markdown→HTML │  │ format for    │  │ format for    │               │
│  │ threading hdrs│  │ feishu msg    │  │ slack blocks  │               │
│  └───────────────┘  └───────────────┘  └───────────────┘               │
└─────────────────────────────────────────────────────────────────────────┘
```

### Components

1. **Inbound Adapters** — Channel-specific message receivers (Email/IMAP, FeiShu/WebHook, etc.)
2. **Outbound Adapters** — Channel-specific reply senders (Email/SMTP, FeiShu/API, etc.)
3. **Channel Registry** — Lookup adapters by channel type (uses `Arc<dyn>` trait objects)
4. **Message Router** — Delegates matching/naming to adapters, dispatches to thread queues
5. **Thread Manager** — Per-thread mpsc queues with semaphore-bounded concurrency
6. **Worker (OpenCode Service)** — AI processing with SSE streaming, session management
7. **Progress Tracker** — Periodic progress update emails during long AI operations
8. **Prompt Builder** — Builds channel-agnostic prompts from InboundMessage
9. **MCP Reply Tool** — `reply_message` tool via `rmcp`, routes replies through OutboundAdapter
10. **Message Storage** — Persist messages and replies as markdown files per thread
11. **State Manager** — Track processed UIDs per channel, handle migrations
12. **Security Module** — Path validation, file size/extension checks for attachments
13. **Alert Service** — Error alert digests + periodic health check reports via email
14. **Command System** — Email `/command` parsing and execution (e.g., `/model` for model switching)

### Design Principles: Component Responsibilities

Each component has a single, clear responsibility. Data flows through the system with transformations happening at well-defined boundaries.

**InboundAdapter** (e.g., `EmailInboundAdapter`)
- Boundary between the external world and the internal system
- Parses raw data from the channel (e.g., raw email bytes via `mail-parser`)
- Cleans and normalizes data at the boundary: strips redundant `Re:/回复:` from subject, cleans bracket-nested duplicates
- Produces a clean `InboundMessage` — all downstream consumers receive clean data

**MessageStorage**
- Pure storage: reads and writes files to disk via `tokio::fs`
- No transformation, no cleaning, no business logic
- Stores `received.md` and `reply.md` exactly as given
- `received.md` = the clean inbound message (cleaned by InboundAdapter)
- `reply.md` = the full reply as sent (built by Reply Tool)

**PromptBuilder**
- Read-only consumer of stored data
- Reads `received.md` and `reply.md` for conversation history
- Strips quoted history (`strip_quoted_history`) and truncates to fit AI token budget
- Builds the user prompt with stripped body + opaque base64 context token
- The context token contains only metadata references (`incoming_message_dir`), never real content

**Reply Tool** (MCP `reply_message`)
- Orchestrator for the reply flow
- Decodes the opaque context token to get metadata (channel, recipient, `incoming_message_dir`, etc.)
- Reads `received.md` to get the full message body (the clean source of truth)
- Builds the full reply in markdown: AI reply text + quoted history (`prepare_body_for_quoting` includes recent historical messages)
- Delegates sending to OutboundAdapter (passes the full markdown reply)
- Delegates storage to MessageStorage (stores the same full reply as `reply.md`)
- `reply.md` reflects exactly what was sent to the recipient

**SmtpClient** (and other transport services)
- Dumb transport: receives markdown, converts to HTML (via `comrak`), adds email headers, sends via `lettre`
- Adds `Re:` to subject, sets `In-Reply-To` and `References` headers for threading
- Does NOT build quoted history, does NOT clean or transform content
- **Auto-reconnect**: wraps send with one-retry on connection errors containing "connect", "timeout", etc.
- **Shared instance**: A single `SmtpClient` (via `EmailOutboundAdapter`) is created at monitor startup and shared across ThreadManager fallback, MCP reply tool (creates its own instance), and AlertService

**ProgressTracker**
- Manages timing and thresholds for progress notifications
- Starts background `tokio::time::interval` checking every 5 seconds
- Sends progress updates at configured intervals (default: 180s, 360s, 540s, 720s, 900s)
- Includes time elapsed, current activity, and estimated completion in email body
- Stops and cleans up when processing completes
- Uses channel-specific outbound adapter to send progress emails via `send_progress_update()`

**ReplyContext** (base64 opaque token)
- Metadata-only: contains channel type, sender, recipient, subject, `incoming_message_dir`, threading IDs
- Never contains real content (no message body, no preview)
- The AI passes it through unchanged (opaque base64 string)
- The Reply Tool decodes it to locate the stored message and reconstruct threading metadata

### Data Flow Summary

```
Email arrives
  → InboundAdapter: parse, clean subject + body → clean InboundMessage
    → MessageStorage: store as-is → received.md (clean source of truth)
      → PromptBuilder: read received.md, strip + truncate for AI → prompt
        → AI: receives stripped body + opaque context token
          → Reply Tool: decode context, read received.md (full body)
            → prepare_body_for_quoting(): AI reply + full quoted history
            → SmtpClient: markdown→HTML, add headers, send via SMTP
            → MessageStorage: store full reply → reply.md (= what was sent)
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
   │           │        received.md        │             │             │             │             │
   │           │             │             │             │             │             │             │
   │           │             │             │ build_prompt()            │             │             │
   │           │             │             ├────────────>│             │             │             │
   │           │             │             │             │             │             │             │
   │           │             │  read received.md + reply.md           │             │             │
   │           │             │  (conversation history)   │             │             │             │
   │           │             │<────────────┤             │             │             │             │
   │           │             │             │             │             │             │             │
   │           │             │        strip_quoted_history│             │             │             │
   │           │             │        + truncate for      │             │             │             │
   │           │             │          token budget      │             │             │             │
   │           │             │             │             │             │             │             │
   │           │             │        serialize_context() │             │             │             │
   │           │             │        → base64 token      │             │             │             │
   │           │             │          (metadata only)   │             │             │             │
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
   │           │             │             │             │        read received.md    │             │
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
- **MessageStorage** stores data as-is, no transformation
- **PromptBuilder** is the only place where history is stripped (for AI token budget)
- **Reply Tool** is the only place where the full reply is assembled (AI text + quoted history)
- **SmtpClient** is a dumb transport: markdown→HTML + headers + send
- **ReplyContext** is an opaque base64 token carrying only metadata references, never content
- **reply.md** = exactly what the recipient receives (minus HTML formatting)

## Core Types & Traits

### Channel Abstractions

```rust
/// Channel type identifier
pub type ChannelType = String; // "email", "feishu", "slack", etc.

/// Channel-agnostic normalized message
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundMessage {
    pub id: String,                           // Internal UUID
    pub channel: ChannelType,                 // "email" | "feishu" | ...
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
#[async_trait]
pub trait InboundAdapter: Send + Sync {
    fn channel_type(&self) -> &str;

    fn derive_thread_name(
        &self,
        message: &InboundMessage,
        pattern_match: Option<&PatternMatch>,
    ) -> String;

    fn match_message(
        &self,
        message: &InboundMessage,
        patterns: &[ChannelPattern],
    ) -> Option<PatternMatch>;

    async fn start(
        &self,
        options: InboundAdapterOptions,
        cancel: CancellationToken,
    ) -> Result<()>;
}

/// Outbound adapter trait — one per channel type
#[async_trait]
pub trait OutboundAdapter: Send + Sync {
    fn channel_type(&self) -> &str;

    async fn connect(&self) -> Result<()>;
    async fn disconnect(&self) -> Result<()>;

    async fn send_reply(
        &self,
        original: &InboundMessage,
        reply_text: &str,
        attachments: Option<&[OutboundAttachment]>,
    ) -> Result<SendResult>;

    async fn send_alert(
        &self,
        recipient: &str,
        subject: &str,
        body: &str,
    ) -> Result<SendResult>;

    async fn send_progress_update(
        &self,
        original: &InboundMessage,
        elapsed_ms: u64,
        activity: &str,
    ) -> Result<SendResult>;
}

#[derive(Debug)]
pub struct SendResult {
    pub message_id: String,
}
```

### Email Channel Pattern Rules

```rust
#[derive(Debug, Clone, Deserialize)]
pub struct ChannelPattern {
    pub name: String,
    pub channel: ChannelType,
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub rules: PatternRules,
    pub attachments: Option<AttachmentConfig>,
}

/// Email-specific pattern rules
#[derive(Debug, Clone, Deserialize)]
pub struct PatternRules {
    pub sender: Option<SenderRule>,
    pub subject: Option<SubjectRule>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SenderRule {
    pub exact: Option<Vec<String>>,           // Case-insensitive exact match
    pub domain: Option<Vec<String>>,          // Domain match
    pub regex: Option<String>,                // Regex match
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubjectRule {
    pub prefix: Option<Vec<String>>,          // Prefix match (stripped from thread name)
    pub regex: Option<String>,                // Regex match
}
```

### Thread Name Derivation

Each inbound adapter implements `derive_thread_name()` with channel-specific logic:

- **Email**: Strip reply prefixes (Re:, Fwd:, 回复:, 转发:), strip configured subject prefix (e.g., "Jiny:"), sanitize for filesystem. Supports broad separator recognition (`:`, `-`, `_`, `~`, `|`, `/`, `&`, `$`, etc.)
- **FeiShu** (future): Derive from group name, topic, or message content
- **Slack** (future): Derive from channel name + thread topic

## Async Event Queue Architecture

### Overview

JYC uses **Tokio** as its async runtime. The message processing pipeline is built on a hierarchy of `tokio::sync::mpsc` channels and a `Semaphore` for bounded concurrency. This replaces jiny-m's callback-based `ThreadManager` with an idiomatic Rust async design.

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
     │ (channel: work)│ │ (future)       │ │ (flush timer)  │
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
    max_concurrent_threads: usize,      // Semaphore permits (default: 3)
    max_queue_size_per_thread: usize,   // mpsc buffer size (default: 10)

    /// Shared dependencies (wrapped in Arc for worker tasks)
    opencode: Arc<OpenCodeService>,
    storage: Arc<MessageStorage>,
    command_registry: Arc<CommandRegistry>,
    channel_registry: Arc<ChannelRegistry>,

    /// Graceful shutdown
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
        let opencode = self.opencode.clone();
        let storage = self.storage.clone();
        // ... clone other Arc deps ...

        tokio::spawn(async move {
            // Acquire semaphore permit (blocks if all workers busy)
            let _permit = tokio::select! {
                permit = semaphore.acquire_owned() => permit.unwrap(),
                _ = cancel.cancelled() => return,
            };

            tracing::info!(thread = %thread_name, "Worker started");

            loop {
                let item = tokio::select! {
                    item = rx.recv() => match item {
                        Some(item) => item,
                        None => break, // Channel closed, queue drained
                    },
                    _ = cancel.cancelled() => break,
                };

                if let Err(e) = process_message(
                    &item, &thread_name, &opencode, &storage, /* ... */
                ).await {
                    tracing::error!(
                        thread = %thread_name,
                        error = %e,
                        "Failed to process message"
                    );
                }
            }

            tracing::info!(thread = %thread_name, "Worker finished");
            // _permit dropped here → semaphore slot freed
        })
    }
}
```

**Key properties:**
- **Bounded concurrency**: `Semaphore(3)` — at most 3 threads process messages simultaneously
- **Per-thread ordering**: Each thread's `mpsc::Receiver` ensures FIFO order within a conversation
- **Back-pressure**: `mpsc::channel(10)` — `try_send` fails when queue is full (message dropped)
- **Graceful shutdown**: `CancellationToken` propagates to all workers and monitors
- **Automatic cleanup**: Worker tasks exit when their mpsc channel closes (all senders dropped) or on cancellation. Semaphore permits are released on `_permit` drop.

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
│  │        "message.updated"     → capture model info        │       │
│  │        "message.part.updated"→ accumulate parts,         │       │
│  │                                detect tool calls,        │       │
│  │                                update last_activity      │       │
│  │        "session.status"      → track busy/retry          │       │
│  │        "session.idle"        → DONE, collect result      │       │
│  │        "session.error"       → handle error:             │       │
│  │                                ContextOverflow → retry   │       │
│  │    }                                                     │       │
│  │                                                          │       │
│  │    _ = activity_timeout_check => {                       │       │
│  │      // tokio::time::interval(5s)                        │       │
│  │      if now - last_activity > 30min (60min if tool) {    │       │
│  │        → timeout, break loop                             │       │
│  │      }                                                   │       │
│  │      if now - last_progress_log > 10s {                  │       │
│  │        → log progress, call on_progress callback         │       │
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

### Alert Service: Event-Driven Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                      Alert Service                                │
│                                                                   │
│  ┌───────────────┐                                               │
│  │ tracing Layer │  (custom tracing subscriber layer)             │
│  │               │                                               │
│  │ on_event() ───┼──> mpsc::Sender<LogEvent>                     │
│  └───────────────┘         │                                     │
│                            ▼                                     │
│              ┌─────────────────────────┐                         │
│              │  Alert Service Task     │                         │
│              │  (tokio::spawn)         │                         │
│              │                         │                         │
│              │  tokio::select! {       │                         │
│              │    event = rx.recv() => │                         │
│              │      match level:       │                         │
│              │        ERROR →          │                         │
│              │          buffer_error() │                         │
│              │        _ →              │                         │
│              │          track_stats()  │                         │
│              │          update_context │                         │
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
│  Note: Uses tracing subscriber layer instead of EventEmitter.    │
│  The layer filters for ERROR events and sends them through mpsc. │
│  Health stats tracking pattern-matches on event messages.         │
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
       ├──> OpenCode Service: close server → delete session files
       │
       └──> SMTP connections: disconnect

 All JoinHandles awaited → process exits cleanly
```

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

## Worker (OpenCode Service)

### Session-Based Thread Management

Each thread has a dedicated OpenCode session persisted in `session.json`. This enables:
- **Memory** — AI remembers previous replies in the conversation
- **Coherence** — Consistent responses across the thread
- **Context** — Full conversation history available
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
- Auto-started on first request, auto-finds free port (49152+)
- Health check before reuse: HTTP ping with 3s timeout
- Server lives until CLI exits

**Per-thread configuration:**
- Each thread gets its own `opencode.json` with model settings, MCP tool config, and permissions
- `permission: { "*": "allow", "question": "deny" }` — headless mode, no interactive terminal
- Staleness check detects changes → rewrites config → restarts server
- Model is NOT passed per-prompt — OpenCode reads from project config

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
│  │ Thread A → session.json + .opencode/│                    │
│  │ Thread B → session.json + .opencode/│                    │
│  │ Thread C → session.json + .opencode/│                    │
│  └─────────────────────────────────────┘                    │
│                                                             │
│  Server lives until CLI exits                               │
└─────────────────────────────────────────────────────────────┘
```

### Worker Processing Flow

```
Worker picks message from thread queue
       ↓
MessageStorage::store(msg, thread_name)
  → creates messages/<timestamp>/ directory
  → saves allowlisted inbound attachments
  → writes received.md
  → returns { message_dir, thread_path }
       ↓
CommandRegistry::process_commands(body, ctx)
  → single pass: parse /commands, execute, strip from body
  → returns CommandOutput { results, cleaned_body, body_empty }
  → if body_empty + has results → send_direct_reply(results) → return
       ↓
ensure_thread_opencode_setup(thread_path)
   → reads .jyc/model-override (if exists, takes priority over config)
   → writes opencode.json with:
     - model from override or config
     - MCP config: jiny_reply server
     - permission: { "*": "allow", "question": "deny" }
   → staleness check: rewrites if model, tool path, JYC_ROOT, or tools changed
   → if config changed: restart OpenCode server + create new session
       ↓
OpenCodeService::generate_reply(msg, thread_path, message_dir)
       ↓
PromptBuilder::build_prompt(msg, thread_path, message_dir)
  → build_prompt_context(): reads messages/*/ (stripped + truncated)
  → Incoming message body (stripped)
  → <reply_context> with channel + channel_metadata + incoming_message_dir
  → Reply instructions: use reply_message tool
  → Mode instructions: plan mode vs build mode
  → Thread-specific system.md (if exists)
       ↓
prompt_with_progress() (SSE streaming):
  1. Subscribe to SSE events ({ directory: thread_path })
  2. Fire prompt_async() (returns immediately)
  3. Process events (filtered by session_id, deduped):
      - server.connected → confirm SSE stream alive
      - message.updated → capture model_id/provider_id
      - message.part.updated → accumulate parts, detect tool calls
      - session.status → track busy/retry (deduped)
      - session.idle → done, collect result
      - session.error → handle (ContextOverflow → new session + retry)
  4. Activity-based timeout: 30 min of silence → timeout (60 min when tool running)
  5. Progress log every 10s (elapsed, parts, activity, silence)
       ↓
OpenCode calls reply_message MCP tool
       ↓
MCP Tool (jyc mcp-reply-tool subprocess):
  1. Decode base64 context token → validate required fields
  2. Load config from JYC_ROOT/config.toml
  3. Instantiate OutboundAdapter for context.channel
  4. Read messages/<incoming_message_dir>/received.md for full body
  5. Build full_reply_text = AI reply + prepare_body_for_quoting()
  6. adapter.send_reply(original_message, full_reply_text, attachments)
  7. MessageStorage::store_reply(full_reply_text) → reply.md
  8. Write .jyc/reply-sent.flag (signal file)
       ↓
Check reply_sent_by_tool:
  1. SSE parts → tool call detected in real-time
  2. check_tool_used(accumulated_parts) — post-hoc
  3. check_signal_file(.jyc/reply-sent.flag) — last-resort fallback
       ↓
Stale session detection:
  If reply_sent_by_tool=true but signal file missing:
    → Session is stale (OpenCode replayed cached results)
    → Delete session file, create new session, retry prompt once
       ↓
If tool NOT used → ThreadManager fallback:
  → Get OutboundAdapter for message.channel (shared instance)
  → adapter.send_reply(message, reply_text, attachments)
  → storage.store_reply()
       ↓
Worker picks next message from thread queue
```

**Session lifecycle:**
- Sessions are created on first use per thread and persisted in `.jyc/session.json`
- On shutdown (SIGINT/SIGTERM): all session files are deleted to prevent stale sessions on restart
- On stale session detection: session file is deleted and a new session is created for retry

### Context Management Strategy

To balance context depth with token limits, the agent uses a multi-layered approach:

1. **Thread Files (Durable)** — Last 10 markdown files stored in thread folder
   - Includes both received messages and AI auto-replies
   - Files store full body (including quoted history) as canonical record
   - When loaded into prompt context, `strip_quoted_history()` + truncation applied
   - Files are limited to 400 chars each (2,000 chars total) in prompt

2. **OpenCode Session (Ephemeral)** — Conversation memory maintained by OpenCode
   - Persists only while server instance is alive
   - Deleted on shutdown to prevent stale sessions on restart
   - Contains condensed message history

3. **Incoming Message (Current)** — Latest message being processed
   - Body stripped of quoted reply history
   - Topic cleaned of repeated Reply/Fwd prefixes (at ingest time by InboundAdapter)
   - Limited to 2,000 chars

**Context Limits:**
```rust
pub const MAX_FILES_IN_CONTEXT: usize = 10;
pub const MAX_BODY_IN_PROMPT: usize = 2000;
pub const MAX_PER_FILE: usize = 400;
pub const MAX_TOTAL_CONTEXT: usize = 2000;
pub const MAX_TOTAL_PROMPT: usize = 6000;
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
| `reply_message` tool fails (e.g. invalid JSON) | SSE shows `completed` but output starts with "Error:" → stays false |
| AI returns text without using tool | `session.idle` fires; ThreadManager sends via OutboundAdapter directly |
| AI takes very long but keeps working | SSE events keep arriving → no timeout; progress logged every 10s |
| AI goes silent for 30 minutes | Activity timeout (60 min if tool running) → checks signal file → error |
| SSE subscription fails | Falls back to blocking prompt with 5-min timeout |
| OpenCode server dies between messages | Health check detects it, restarts automatically |
| ContextOverflowError | Detected via SSE `session.error` → new session → retry (blocking) |
| Thread queue full | Message dropped with warning; IMAP re-fetch recovers on restart |

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
├── jyc monitor          ← main command
├── jyc config init      ← config management
├── jyc config validate
├── jyc state            ← show monitoring state
├── jyc patterns list    ← list patterns
└── jyc mcp-reply-tool   ← hidden subcommand (MCP stdio server)
                            spawned by OpenCode as subprocess
```

The reply tool shares types with the main binary (same Rust crate), eliminating the type drift risk of the two-binary TypeScript approach.

### Reply Context

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplyContext {
    pub channel: ChannelType,
    pub thread_name: String,
    pub sender: String,
    pub recipient: String,
    pub topic: String,
    pub timestamp: String,
    pub incoming_message_dir: Option<String>,
    pub external_id: Option<String>,          // Email: Message-ID
    pub thread_refs: Option<Vec<String>>,     // Email: References
    pub uid: String,
    #[serde(rename = "_nonce")]
    pub nonce: Option<String>,                // Integrity nonce
    pub channel_metadata: Option<HashMap<String, Value>>,
}

/// Serialize context for embedding in AI prompt
pub fn serialize_context(
    message: &InboundMessage,
    thread_name: &str,
    incoming_message_dir: Option<&str>,
) -> String {
    // struct → JSON → base64url
    let ctx = ReplyContext { /* ... */ };
    let json = serde_json::to_string(&ctx).unwrap();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

/// Deserialize and validate context from AI tool call
pub fn deserialize_context(encoded: &str) -> Result<ReplyContext> {
    // base64 → JSON → struct, with integrity validation
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(encoded)?;
    let json = String::from_utf8(bytes)?;
    // Check for tampering indicators (backticks, escaped newlines, escaped quotes)
    if json.contains('`') || json.contains("\\n") || json.contains("\\\"") {
        anyhow::bail!("token appears modified — DO NOT decode or modify the token");
    }
    let ctx: ReplyContext = serde_json::from_str(&json)?;
    // Validate required fields
    if ctx.channel.is_empty() || ctx.recipient.is_empty() {
        anyhow::bail!("missing required fields in context");
    }
    Ok(ctx)
}
```

### Token Integrity Validation

To prevent AI tampering with the opaque token:
- **Nonce field**: `serialize_context()` adds a `_nonce` field (timestamp + random suffix)
- **Formatting detection**: `deserialize_context()` scans for backticks, escaped newlines, escaped quotes
- **Stronger system prompt**: AI receives explicit instructions not to decode, modify, or re-encode the token

### MCP Tool: `reply_message`

```
MCP Server (rmcp, stdio transport, cwd = thread dir):
  Tool schema: message (string), token (string, opaque base64), attachments (string[] optional)

  1. Decode base64 context token → validate required fields
  2. Load config from JYC_ROOT/config.toml
  3. Instantiate OutboundAdapter for context.channel
  4. Validate attachments via PathValidator (exclude .opencode/, .jyc/)
  5. Read messages/<incoming_message_dir>/received.md → extract full body
  6. Build full reply: AI reply text + prepare_body_for_quoting()
  7. adapter.send_reply(original_message, full_reply_text, attachments)
  8. MessageStorage::store_reply(thread_path, full_reply_text, message_dir)
  9. Write .jyc/reply-sent.flag (signal file)
  10. Return success message
```

### Historical Message Quoting (Thread Trail)

`build_thread_trail()` reads interleaved received/reply messages from the thread's `messages/` directory.

- **Per-directory ordering**: Within each message directory, **reply comes before received** (the AI responded after receiving, so the reply is more recent). Overall ordering is most-recent directory first.
- **Full trail order**:
  ```
  current received (folder 5)     ← the message being replied to now
  folder 4 reply                  ← AI's previous response
  folder 4 received               ← user's message that AI responded to
  folder 3 reply                  ← AI's earlier response
  folder 3 received               ← user's earlier message
  ...
  ```
- **Stripped bodies**: Received messages stripped of quoted history via `strip_quoted_history()`. Reply messages parsed with `parse_stored_reply()` to extract only the AI's response text.
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
    "question": "deny"
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

**Staleness check**: Rewrites `opencode.json` if model, tool path, JYC_ROOT, or `permission.question` changed. When config changes, the OpenCode server is restarted and a new session is created.

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
workspace = "./workspace"

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
include_thread_history = true

[agent.progress]
enabled = true
initial_delay_secs = 180
interval_secs = 180
max_messages = 5

[agent.attachments]
enabled = true
max_file_size = "10mb"
allowed_extensions = [".ppt", ".pptx", ".doc", ".docx", ".txt", ".md"]

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
    pub alerting: Option<AlertingConfig>,
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
    pub channel_type: String,
    pub workspace: Option<String>,
    pub inbound: Option<ImapConfig>,
    pub outbound: Option<SmtpConfig>,
    pub monitor: Option<MonitorConfig>,
    pub patterns: Option<Vec<ChannelPattern>>,
    pub agent: Option<AgentConfig>,           // Channel-specific override
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
    pub progress: Option<ProgressConfig>,
    pub attachments: Option<AttachmentConfig>,
}

#[derive(Debug, Deserialize)]
pub struct OpenCodeConfig {
    pub model: Option<String>,
    pub small_model: Option<String>,
    pub system_prompt: Option<String>,
    #[serde(default = "default_true")]
    pub include_thread_history: bool,
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
│   └── workspace/                       # Thread workspaces
│       ├── <thread-dir-1>/              # OpenCode cwd for this thread
│       │   ├── messages/
│       │   │   ├── 2026-03-19_23-02-20/
│       │   │   │   ├── received.md      # Incoming message
│       │   │   │   ├── reply.md         # AI reply
│       │   │   │   └── report.pdf       # Saved attachment
│       │   │   └── 2026-03-19_23-10-00/
│       │   │       ├── received.md
│       │   │       └── reply.md
│       │   ├── .jyc/
│       │   │   ├── session.json         # AI session state
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
│   │   ├── config.rs                    # `jyc config init/validate`
│   │   ├── patterns.rs                  # `jyc patterns list/add`
│   │   ├── state.rs                     # `jyc state`
│   │   └── mcp_reply.rs                 # `jyc mcp-reply-tool` (hidden)
│   ├── config/
│   │   ├── mod.rs
│   │   ├── types.rs                     # Config structs (serde + toml)
│   │   └── validation.rs               # Config validation
│   ├── channels/
│   │   ├── mod.rs
│   │   ├── types.rs                     # InboundMessage, traits, patterns
│   │   ├── registry.rs                  # ChannelRegistry
│   │   └── email/
│   │       ├── mod.rs
│   │       ├── config.rs               # Email-specific config
│   │       ├── inbound.rs              # EmailInboundAdapter
│   │       └── outbound.rs             # EmailOutboundAdapter
│   ├── core/
│   │   ├── mod.rs
│   │   ├── thread_manager.rs           # Per-thread queues + semaphore
│   │   ├── message_router.rs           # Pattern match → dispatch
│   │   ├── message_storage.rs          # Markdown file I/O
│   │   ├── email_parser.rs             # Stripping, quoting, thread trail
│   │   ├── state_manager.rs            # UID tracking, state persistence
│   │   ├── alert_service.rs            # Error digests + health reports
│   │   ├── progress_tracker.rs         # Progress update emails
│   │   └── command/
│   │       ├── mod.rs
│   │       ├── registry.rs             # Command parsing + dispatch
│   │       ├── handler.rs              # CommandHandler trait
│   │       ├── model_handler.rs        # /model command
│   │       └── mode_handler.rs         # /plan, /build commands
│   ├── services/
│   │   ├── mod.rs
│   │   ├── opencode/
│   │   │   ├── mod.rs
│   │   │   ├── client.rs              # OpenCode HTTP + SSE
│   │   │   ├── session.rs             # Session lifecycle
│   │   │   └── prompt_builder.rs      # Prompt construction
│   │   ├── imap/
│   │   │   ├── mod.rs
│   │   │   ├── client.rs             # async-imap wrapper
│   │   │   └── monitor.rs            # IDLE + poll + recovery
│   │   └── smtp/
│   │       ├── mod.rs
│   │       └── client.rs             # lettre SMTP, MD→HTML
│   ├── mcp/
│   │   ├── mod.rs
│   │   ├── reply_tool.rs             # rmcp stdio MCP server
│   │   └── context.rs                # ReplyContext serialization
│   ├── security/
│   │   ├── mod.rs
│   │   └── path_validator.rs         # File path/extension/size checks
│   └── utils/
│       ├── mod.rs
│       ├── helpers.rs                # Regex validation, file size parsing
│       └── constants.rs              # Default configs, timeouts
```

### Message Markdown Format (Unified)

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

### Message Directory Naming

Per-message directories use the message timestamp:
```
messages/2026-03-19_23-02-20/     # Timestamp from message
messages/2026-03-19_23-02-20_2/   # Collision: counter suffix added
```

Each directory contains:
- `received.md` — incoming message (always present)
- `reply.md` — AI reply (written when reply is sent)
- `<attachment>.pdf` — saved inbound attachments (if allowlist config enabled)

## Logging & Tracing

### Library Choice: `tracing` + `tracing-subscriber`

JYC uses the `tracing` ecosystem for all logging and diagnostics:

| Aspect | Detail |
|--------|--------|
| **Crate** | `tracing` 0.1.x + `tracing-subscriber` 0.3.x |
| **Why not `log`** | `tracing` provides structured fields, async-aware spans, and custom subscriber layers — all required by JYC |
| **Span context** | Each worker task runs inside a span with `thread` and `channel` fields — all logs inside automatically include these |
| **Custom Layer** | AlertService uses a custom `tracing::Layer` to capture ERROR events and forward them via mpsc to the alert task |
| **Env filter** | `RUST_LOG=jyc=info,async_imap=warn` controls per-module verbosity |
| **CLI flags** | `--debug` sets `jyc=debug`, `--verbose` sets `jyc=trace,async_imap=debug` |

### Structured Logging Convention

```rust
// Good: structured fields
tracing::info!(
    thread = %thread_name,
    channel = %channel,
    message_id = %msg.id,
    "Message processed"
);

// Good: span-based context (automatic for all logs inside)
#[tracing::instrument(fields(thread = %name, channel = %ch))]
async fn process_message(name: &str, ch: &str, /* ... */) -> Result<()> {
    tracing::info!("Starting"); // automatically includes thread + channel
    // ...
}
```

### Log Levels

| Level | Usage |
|-------|-------|
| ERROR | Unrecoverable failures, processing errors, MCP tool errors |
| WARN | Recoverable issues: queue full, stale session, timeout, reconnection |
| INFO | Lifecycle: message received, matched, processed, reply sent, worker start/stop, step start |
| DEBUG | SSE events, session status changes, step finish with costs, config details |
| TRACE | Raw SSE data, IMAP protocol, detailed parsing steps |

### Alert Service Integration

The AlertService registers a custom `tracing::Layer` that:
1. Captures all events at ERROR level (plus INFO/WARN for health stats)
2. Sends them through `mpsc::Sender<LogEvent>` to the alert task
3. The alert task buffers errors and periodically flushes them as digest emails
4. Self-protection: events with `alert_internal = true` field are skipped

```rust
struct AlertLayer {
    sender: mpsc::Sender<LogEvent>,
}

impl<S: Subscriber> Layer<S> for AlertLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        // Extract level, message, fields
        // Skip if alert_internal field is present
        // Send to alert service task via mpsc
        let _ = self.sender.try_send(log_event);
    }
}
```

## Email Command System

### Available Commands

| Command | Description | Example |
|---------|-------------|---------|
| `/model <id>` | Switch AI model for this thread | `/model SiliconFlow/Pro/deepseek-ai/DeepSeek-V3.2` |
| `/model` | List available models | `/model` |
| `/model reset` | Reset to default model from config | `/model reset` |
| `/plan` | Switch to plan mode (read-only, enforced by OpenCode) | `/plan` |
| `/build` | Switch to build mode (full execution, default) | `/build` |

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

Unlike jiny-m, which splits command parsing (`CommandRegistry.parseCommands`) and body stripping (`thread-manager.ts`) into two separate passes over the message body, JYC unifies these into a single `process_commands()` method. This eliminates duplicated scanning logic and keeps command-related concerns in one place.

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

**Key design differences from jiny-m:**
- **Single pass**: jiny-m scans the body twice (once in `parseCommands`, once in thread-manager for stripping). JYC does it once.
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
3. `MessageStorage::store()` calls `save_attachments()` before writing `received.md`
4. For each attachment: check extension allowlist → check size limit → check count limit → sanitize filename → resolve collisions → write to disk
5. Bytes freed after write (`attachment.content = None`)
6. Attachment metadata in `received.md` shows saved/skipped status

**Security measures:**
- Extension allowlist (not blocklist) — only explicitly permitted types saved
- File size limit per attachment (human-readable: `"25mb"`, `"150kb"`)
- Max attachments per message (prevents resource exhaustion)
- Filename sanitization: basename only, no path traversal, no hidden files, no null bytes, max 200 chars, Unicode NFC normalized
- Double extension defense: only the last extension is checked
- Collision handling: counter suffix (e.g. `report_2.pdf`)

## Stripping Strategy

`strip_quoted_history()` is only applied at **AI prompt consumption time**, never at storage or reply time. Cleaning (`clean_email_body`) happens once at the InboundAdapter boundary.

| Stage | Where | Strips history? | Cleans? | Purpose |
|-------|-------|----|---------|---------|
| **Inbound** | `EmailInboundAdapter` | No | Yes | Clean at boundary |
| **Storage** | `MessageStorage::store()` | No | No | Canonical record |
| **AI Prompt Context** | `PromptBuilder::build_context()` | Yes | No | Keep AI focused |
| **AI Prompt Body** | `PromptBuilder::build_prompt()` | Yes | No | Incoming for AI |
| **`<reply_context>`** | `serialize_context()` | N/A | N/A | Metadata only |
| **Reply Tool** | `mcp/reply_tool.rs` | No | No | Reads received.md, builds full reply |
| **Outbound** | `SmtpClient` | No | No | Dumb transport |

## Security Considerations

- Environment variables for credentials (never commit passwords)
- Validate regex patterns at config load time to prevent ReDoS
- Rate limiting for AI API calls
- Path validation for all file operations (`PathValidator`)
- Attachment security: extension allowlist, size limit, filename sanitization
- MCP tool: validate context before processing
- `permission: { "*": "allow", "question": "deny" }` in opencode.json
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

## Differences from jiny-m (TypeScript)

| Aspect | jiny-m (TypeScript/Bun) | jyc (Rust) |
|--------|------------------------|------------|
| Binary | Two binaries (`jiny-m` + `jiny-m-reply-tool`) | Single binary with hidden subcommand |
| Config | JSON with comments (`.jyc/config.json`) | TOML (`config.toml`) |
| Concurrency | Promise-based, manual worker counting | `tokio::Semaphore` + `mpsc` channels |
| Error handling | try/catch, errors swallowed in workers | `anyhow::Result`, `?` propagation with context |
| Logging | Custom Logger with EventEmitter | `tracing` with subscriber layers |
| Alert subscription | Logger EventEmitter `on('log')` | Custom `tracing::Layer` + mpsc |
| IMAP | `imapflow` (JS) | `async-imap` (Rust) |
| SMTP | `nodemailer` (JS) | `lettre` (Rust) |
| Email parsing | `mailparser` (JS) | `mail-parser` (Rust) |
| MD → HTML | `marked` (JS) | `comrak` (Rust, GFM support) |
| HTML → MD | `turndown` (JS) | `htmd` (Rust) |
| MCP | `@modelcontextprotocol/sdk` (JS) | `rmcp` (Rust) |
| SSE | `@opencode-ai/sdk` built-in | `reqwest-eventsource` |
| Shutdown | `process.on('SIGINT')` | `CancellationToken` hierarchy |
| Thread queues | Array + manual scheduling | `tokio::sync::mpsc` bounded channels |
| Memory | GC-managed | Ownership + RAII |
| Backward compat | 3 config formats + migrations | Clean start, TOML only |
