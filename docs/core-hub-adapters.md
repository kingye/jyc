# Core / Hub / Adapters — The Pipe Architecture

**Status:** Target architecture. Feishu is the first migrated channel. All other
channels will follow; the end state is that JYC supports *only* this
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
│   feishu · github · wecom_bot · wecom · wechat · (email)    │
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

- create a TopicManager, agent service, state manager, or outbound adapter,
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

## Migrating other channels

To be documented per channel when migration starts (github, wecom_bot, wecom,
wechat; email last). The feishu cleanup serves as the checklist template:
strip everything except protocol code + pipe wiring.
