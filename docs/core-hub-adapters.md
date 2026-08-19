# Core / Hub / Adapters — The Pipe Architecture

**Status:** Target architecture. Feishu, wecom_bot, and email are migrated.
The remaining channels will follow; the end state is that JYC supports *only* this
architecture.

## Context

Historically, every JYC channel type was a full channel: its own inbound
adapter, pattern matcher, MessageRouter, TopicManager, agent service, state
manager, and outbound adapter. Each new channel re-implemented the same
plumbing, and channel-specific behavior leaked into places it didn't belong.

The pipe architecture splits the system into three layers with strict
responsibilities:

```
┌────────────────────────────────────────────────────────────┐
│ Adapters (outer) — protocol only                            │
│   feishu · wecom_bot · email · github · wecom · wechat      │
│   platform events ⇄ InboundMessage; relay replies back      │
└───────────────────────────┬────────────────────────────────┘
                            │ pipe = { channel, topic, pattern }
┌───────────────────────────▼────────────────────────────────┐
│ Hub (middle) — the websocket channel                        │
│   owns topics, routing, agents; outbound broadcast bus      │
│   TUI / dashboard connect here                              │
└───────────────────────────┬────────────────────────────────┘
                            │ MessageRouter → TopicManager
┌───────────────────────────▼────────────────────────────────┐
│ Core (inner) — jyc-core                                     │
│   per-topic message queues + worker tasks                   │
│   (semaphore-bounded concurrency), agent service,           │
│   message storage, templates, scheduler                     │
└────────────────────────────────────────────────────────────┘
```

## Layer 1 — Core

The core is channel-agnostic. It never sees platform-specific data — only
`InboundMessage`, topics, and reply text.

- Per-topic message queues drained by worker tasks; concurrency bounded by a
  semaphore.
- Agent service (in-process agent), message storage, template resolution, job
  scheduler.
- One core instance per hub channel; adapters have no core presence.

## Layer 2 — Hub

The hub is the websocket channel — the **only** channel type that owns topics,
routing, and agent wiring.

- Inbound adapter + outbound broadcast bus (`tokio::sync::broadcast`) per hub
  channel.
- Replies are broadcast as `{"type":"reply","topic","text","attachments":[...]}`
  payloads.
- Humans (TUI, dashboard chat panes) and adapters are symmetric spokes of the
  hub: both submit messages into topics and both can subscribe to replies.

## Layer 3 — Adapters

An adapter speaks exactly one platform protocol and nothing else.

**An adapter MUST:**

- translate platform events into `InboundMessage` (with platform metadata:
  chat_id, chat_name, mentions, sender),
- match its own patterns and re-target matching messages into a hub channel
  via `pipe = { channel, topic?, pattern? }`,
- record the resolved hub topic → platform address mapping for reply relay,
- subscribe to the pipe target's broadcast and relay replies (text +
  attachments) back to the platform.

**An adapter MUST NOT:**

- create a TopicManager, agent service, or outbound adapter,
  and no conversation state manager (a *protocol* cursor is allowed — see
  email's IMAP `StateManager`),
- store conversation history or own topics,
- appear in the channel orchestrator / dashboard channel list.

A matched pattern without `pipe` is a configuration error: the message is
dropped with a warning at runtime, and startup logs a warning for each such
pattern.

## Message flow

**Inbound** (feishu example):

```
Feishu platform ──WS──► adapter: parse event → InboundMessage (+ metadata)
                        → FeishuMatcher pattern match
                        → apply_pipe_retarget: rewrite channel/topic,
                          resolve ${msg.chat_name}, record topic→chat_id
                        → hub channel's MessageRouter → core worker → agent
```

**Outbound:**

```
agent reply → hub's WebsocketOutboundAdapter → broadcast {"type":"reply",...}
            → adapter's reply forwarder (subscribed to that broadcast)
            → topic→chat_id lookup → FeishuClient.send_text_message
            → attachments: download via inspect files endpoint,
              apply [attachments.outbound] policy, re-upload to platform
```

Proactive sends follow the same path: `jyc_send_message` to the hub channel
broadcasts a `reply` payload keyed by topic, which the adapter's forwarder
relays. Known limitation: the topic→address mapping is in-memory and rebuilt
on inbound traffic, so proactive sends right after restart are dropped until
the next inbound message. Persisting the mapping (e.g. a `.jyc/` file in the
topic directory) is a planned follow-up.

## Design rules

1. Core MUST NOT contain platform-specific code.
2. The hub is the only place where topics, agents, and templates exist.
3. New platform support = new adapter, never a new full channel.
4. Adapters run **in-process** today (in-process re-targeting + broadcast
   subscription). The adapter↔hub seam — the pipe re-target contract and the
   broadcast payload schema — is the designated boundary where an
   **external-process adapter** (e.g. a Node.js adapter connecting over real
   WebSocket) may attach in the future. Changes to this seam must keep that
   option open; do not couple adapters to core internals beyond this contract.

## Feishu — first migration

Feishu is the reference implementation. After cleanup, the adapter retains
only:

- `client.rs` — Feishu API client (send text/image/file, upload, name lookups)
- `websocket.rs` — event stream → `InboundMessage`
- `inbound.rs` — pattern matching (`FeishuMatcher`)
- pipe wiring in `jyc-cli/src/cli/serve/channels.rs` — re-targeting, address
  mapping, reply forwarder

Removed in the migration: `FeishuOutboundAdapter` (direct-mode delivery),
`formatter.rs` and `validator.rs` (never wired in), the non-pipe routing
fallback, the channel's own TopicManager/agent/state/orchestrator
registration, and topic-close handling (a no-op for piped topics).

## WeCom Bot — second migration

`wecom_bot` (WeCom Smart Robot, WebSocket long connection) follows the
same pipe-only migration as feishu. The adapter retains only:

- `client.rs` — WebSocket lifecycle (connect, subscribe, heartbeat, reconnect).
- `inbound.rs` — WebSocket frames → `InboundMessage` (with attach-download via
  `media::process_bot_attachments` for image/file/mixed).
- `outbound.rs` — wire-format helpers (streaming reply, attachment upload,
  media body) re-exported as `pub` so the pipe adapter can drive them
  directly. The full `WecomBotOutboundAdapter` struct is kept (public API)
  for callers that still want the full reply-lifecycle surface (footer,
  chat-log storage, etc.).
- The new `spawn_wecom_bot_adapter` in `crates/jyc-cli/src/cli/serve/channels.rs`.

Removed in the migration: the `WecomBotOutboundAdapter` registration in
`build_outbound_adapter`, the `"wecom_bot"` arm in `InboundSpawner::spawn`,
the `wecom_bot_handle_arc` plumbing, and the channel-specific processing
indicator / progress spinner code in `TopicManager::worker` (the core
stays channel-agnostic — the pipe adapter owns the streaming reply).

**Placeholders.** Unlike feishu, wecom_bot does not populate a
`chat_name` on the inbound message. The pipe topic template uses
`${msg.<key>}` against any metadata key (channel_uid, chatid, userid,
chat_name, ...) — `channel_uid` unifies group chat (chatid) and single
chat (userid) in one template (`topic = "bot-${msg.channel_uid}"`).

**Streaming reply.** Because the WeCom passive reply window is short and
the agent can take minutes, the adapter sends a `finish=false` streaming
indicator immediately on inbound (the user-visible "thinking…" message).
The reply forwarder completes the stream with `finish=true` and sends
any attachments via proactive `aibot_send_msg` (no window constraint).

## Email — third migration

Email (IMAP in, SMTP out) follows the same pipe-only migration. The adapter
retains only:

- `jyc-services/src/imap/` — IMAP client, monitor loop (IDLE/poll), raw email
  parsing. The monitor takes an `on_message` callback (like
  `InboundAdapterOptions::on_message`) instead of owning a router + matcher.
- `jyc-services/src/smtp/client.rs` — SMTP wire format (reply threading,
  attachments).
- `jyc-channels/src/email/inbound.rs` — pattern matching only (`EmailMatcher`).
- The new `spawn_email_adapter` in `crates/jyc-cli/src/cli/serve/channels.rs`.

Removed in the migration: `EmailOutboundAdapter` (whole file), the dead
`EmailInboundAdapter` + duplicate `parse_raw_email` in `email/inbound.rs`,
`email_parser::build_full_reply_text`, and the `"email"` arms in
`build_outbound_adapter` / `InboundSpawner::spawn`.

**Mailbox cursor state.** Unlike feishu/wecom_bot, the email adapter keeps a
`StateManager` at `<workdir>/channels/<channel_name>/.imap/` — the IMAP
sequence number + processed UIDs. That is protocol-level dedup state, not
conversation state, so it stays with the adapter. `--reset` clears it;
`--no-idle` forces poll mode. (The generic per-channel `StateManager` in
`serve/mod.rs` went away with this migration: email was its only consumer.)

**Topic identity.** Email's natural topic is its subject, so a `pipe` without
an explicit `topic` falls back to the subject-derived topic name
(`EmailMatcher::derive_topic_name`, i.e. prefixes stripped). An explicit
`pipe.topic` (including `${msg.<key>}` templates) wins.

**Reply threading.** The forwarder keeps a `topic → { recipient, subject,
in_reply_to, references }` map recorded on inbound, so replies land in the
original mail thread (`In-Reply-To` = original `Message-ID`, `References` =
original chain + `Message-ID`). Same in-memory limitation as feishu: rebuilt
from inbound traffic after a restart, and two senders sharing one subject
share one entry (last writer wins).

**No footer.** Email replies are plain agent text (trailing `---` separators
stripped); the model/mode/token footer is gone, so `[channels.<name>.footer]`
no longer applies to email channels. Per-pattern
`[channels.<name>.patterns.attachments]` also stops applying at route time
(the hub channel's patterns are matched there) — the global
`[attachments.inbound]` policy still applies, same as feishu/wecom_bot.

## Migrating other channels

To be documented per channel when migration starts (github, wecom,
wechat). The feishu/wecom_bot/email cleanup serves as the checklist
template: strip everything except protocol code + pipe wiring.
