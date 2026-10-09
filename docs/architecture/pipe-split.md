# Pipe Process Split (`jyc` / `jyc-pipe`)

**Status:** Step 1 done (websocket protocol extensions: `metadata` on
inbound `message` frames, `topic_event` server frames, `close_topic`
client frame — see [api.md](../api.md) §3). Step 2 done (`jyc-pipe`
binary: config resolution shared via `jyc_utils::config_resolve`, hub
ws client with inspect auth token and reconnect backoff; adapter
wiring lands in step 3, feishu first). The websocket channel adapter
moved from `jyc-channels` into `jyc-inspect` (`server::websocket`) —
it is hub frontend (dashboard UI + pipe endpoint), not a peripheral
channel; `jyc-channels` is now the pipe crate and no longer depends on
`jyc-inspect`. Step 3 done (feishu
migrated end-to-end: `jyc-pipe` hosts the feishu adapter — inbound
`message` frames, reply relay with completion footer + attachment
download, live status cards fed by `topic_event` frames (mode/model/
context segments omitted — no `TopicManager` in the pipe), chat disband
→ `close_topic`). Channel ownership between the two processes is derived
from the pipe's adapter capabilities
(`pipe::SUPPORTED_CHANNEL_TYPES`), not from any config section: the pipe
claims every configured channel of a type it can run and `jyc serve`
skips exactly those in-process. (Step 2 briefly introduced a `[pipe]`
config section listing channel names; that was a design mistake — it
required users to coordinate both sides by editing config, and the
capability rule replaces it.) Known
gap: inbound attachments are not relayed (the hub `message` frame has no
attachments field). Step 4 in progress (one PR per channel type, in
order): **github and gitee migrated** — `pipe/github.rs` / `pipe/gitee.rs`
host the poller adapters (poll + dedup/cursor state stays under
`<data_dir>/channels/<channel>/.github/` / `.gitee/`), reply relays post
`[Role]`-prefixed comments, issue/PR close events forward `close_topic`
frames; the hub-side feishu/github/gitee in-process wiring is deleted, so
the `jyc` binary no longer references those adapters (adapter-only
dependencies leave its link graph). wecom_bot follows: the aibot WS
callback adapter (streaming indicator, keep-alive spinner, proactive
fallback + attachment relay) also runs in `jyc-pipe`, deleting the
hub-side wiring; reply attachments download through the hub's files
endpoint with the pipe's bearer token instead of the `jyc-inspect`
client (no new dependency in `jyc-channels`). Remaining: wecom (and
wecomkf, which shares the hub's webhook server with wecom — migrating
either alone collides on the bind port, so they move together or the
pipe takes the server).

## Goal

Split the single `jyc` binary into two processes with distinct lifecycles:

- **`jyc`** — the clean agents tool: TUI, agent runtime, the synthesized
  agent websocket channel (hub), inspect server. Standalone-capable.
- **`jyc-pipe`** — a pure peripheral message-pipe process: hosts pipe-only
  channel adapters (feishu, wecom, email, github, gitee, wecom_bot, ...),
  translating platform events and forwarding messages in both directions.
  It owns no topics, no agents, no core state — only pipe adapters.

Motivation: peripheral channels are exposed to the internet and churn more
often than the agent core; isolating them in their own process means a
channel crash or upgrade never takes down the TUI/agents, and the `jyc`
binary stops linking heavy channel dependencies (IMAP/SMTP, feishu SDK,
wecom crypto).

Crate layout stays as-is: `jyc-channels` remains the library crate; the new
binary target inside it is named `jyc-pipe`. The name matches the existing
domain language ("pipe target", "pipe-only adapter", see
[overview.md](overview.md)).

## Current state

Every channel type other than the synthesized agent websocket is already
pipe-only in-process (see overview.md). A pipe-only adapter's only
couplings to the core are:

| Coupling | In-process today | Cross-process replacement |
|---|---|---|
| Inbound | `route_into_pipe_target` → hub `MessageRouter` | ws client sends `{"type":"message", topic, text, sender...}` — protocol exists |
| Reply relay | subscribes to hub `broadcast::Sender<String>`, receives `{"type":"reply",...}` frames | hub ws server already forwards the channel broadcast to connected clients — mechanism exists |
| Attachments | inspect HTTP client (`loopback_addr`) | already network-capable; point at the core's inspect address |

So the split boundary is not new code structure — it is process separation
over the existing hub websocket protocol.

## Protocol gaps (Step 1)

Three additive extensions to the hub websocket protocol, none of which
break existing clients (TUI, agents channel):

1. **`metadata` on inbound `message` frames** — optional string map,
   carrying pipe hints (e.g. `pipe_pattern`) that in-process pipes set via
   `InboundMessage.metadata`.
2. **`topic_event` server frames** — hub streams typed `TopicEvent`s
   (ProcessingStarted/Thinking/ToolStarted/ProcessingCompleted, ...) to
   connected clients. Needed for feishu-style status cards, which today
   consume the in-process `TopicManager` event bus.
3. **`close_topic` client frame** — a pipe process asks the hub to close a
   topic (feishu chat disband, GitHub issue/PR closed). Today this calls
   the hub `TopicManager` directly.

## Incremental steps

### Step 1 — Hub websocket protocol extensions (additive)

Extend `ClientMessage` / server frames in
`crates/jyc-inspect/src/server/websocket/inbound.rs` (+ `jyc-core` topic event
serialization). Existing clients unaffected.

### Step 2 — `jyc-pipe` binary skeleton

New `[[bin]] name = "jyc-pipe"` in `crates/jyc-channels`: loads config,
runs a tokio runtime, and implements a hub ws client
(connect/subscribe/reconnect). Chooses adapters by capability: it claims
every configured channel whose type it can run.

### Step 3 — Migrate feishu (first channel, end-to-end proof)

Move the wiring from `crates/jyc-cli/src/cli/serve/channels/feishu.rs`
into `jyc-pipe`: inbound adapter → ws `message` frames (topic→chat_id map
stays local in `jyc-pipe`); ws reply/event stream → `FeishuClient` relay +
status cards (progress watcher consumes ws events instead of the
in-process bus); chat disband → `close_topic`. `jyc serve` skips
in-process spawning of any channel whose type the pipe can run — the two
sides derive ownership from the same capability list
(`pipe::SUPPORTED_CHANNEL_TYPES`), with no config involvement.

### Step 4 — Migrate remaining pipe-only channels, one PR each

Order by coupling: github → gitee → wecom_bot → wecom. Each reuses the
Step 3 pattern. The first migration PR should also delete the feishu
in-process wiring (`serve/channels/feishu.rs` + `spawn_feishu_adapter`):
the `jyc` binary only shrinks when in-process spawn paths are *removed*,
not when the pipe path is added. Attach a before/after `jyc` release
artifact size comparison to that PR — adapter-exclusive dependencies
(`openlark-client`, wecom crypto `aes`/`cbc`/`md5`/`sha1`/`hex`) drop out
of the link graph entirely once unreferenced; the big shared deps
(tokio, reqwest, serde, tungstenite, axum) do not.

### Step 5 — Email, decided separately

Email has protocol state (IMAP cursor) and possibly own-topic semantics;
migrate last with a dedicated design or keep in-process.

Note: email's client dependencies (`async-imap`, `lettre`) live in
`jyc-services`, not `jyc-channels` — alongside `job_scheduler.rs`, which
stays in-process. Migrating email to the pipe therefore also requires
moving the `imap/` and `smtp/` clients out of `jyc-services` (e.g. into
the pipe side); otherwise they stay linked into the `jyc` binary.

### Step 6 — Cleanup

Slim `jyc` dependencies (drop in-process channel spawn paths), Docker /
systemd two-process orchestration, update overview.md / docs/channels/ /
CHANGELOG.md.

## Open decisions

1. **Service discovery** — `jyc-pipe` reads the hub ws address from config
   (e.g. `ws://127.0.0.1:8080/ws/agents`); startup ordering handled by
   client-side reconnect.
2. **Multiple hubs** — one `jyc-pipe` process may connect to several hub
   channels (one ws connection each). Leaning yes.
3. **TUI transport** — confirm whether the TUI is in-process or already
   ws-based (affects Step 6 slimming scope).
