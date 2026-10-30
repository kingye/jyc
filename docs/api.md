# JYC API Reference

Real-time protocols exposed by the JYC monitor on the inspect TCP port
(default `127.0.0.1:9876`):

- **§2 HTTP REST API** — JSON requests/responses over HTTP/1.1. State
  queries, topic / pattern management, config reload.
- **§3 WebSocket API** — upgrade on the same port for live chat and
  the activity / thinking / processing event stream.

Both protocols share **one TCP listener** and **one Bearer token**
generated automatically at startup (see §1.2). The same
`Authorization: Bearer <token>` header gates every route.

> **Source of truth.** The protocol types live in
> `crates/jyc-types/src/inspect.rs`, the server in
> `crates/jyc-inspect/src/server.rs`, the auth middleware in
> `crates/jyc-inspect/src/auth.rs`, the REST handlers in
> `crates/jyc-inspect/src/api.rs`, and the Rust client in
> `crates/jyc-inspect/src/client.rs`. This document is a derivative —
> if the two disagree, the code wins. Please file an issue or PR to
> fix the doc.

---

## §1 Overview

### 1.1 Process model

```
┌─────────────────────────────────────────────────────────────┐
│  jyc serve                                                  │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  InspectServer  (TCP, default 127.0.0.1:9876)        │  │
│  │  axum::Router::serve(TcpListener)                    │  │
│  │  ┌──────────────────────────────────────────────────┐│  │
│  │  │  require_bearer middleware                       ││  │
│  │  │  (Authorization: Bearer <token>)                ││  │
│  │  └────────────┬──────────────────────┬──────────────┘│  │
│  │               ▼                      ▼               │  │
│  │     /api/* REST handlers     /ws/* WebSocketUpgrade  │  │
│  └──────────────────────────────────────────────────────┘  │
└─────────────────────────────────────────────────────────────┘
                ▲                          ▲
                │ HTTP/1.1 + Bearer         │ WS upgrade + Bearer
                │                          │
        ┌───────┴────────┐         ┌───────┴────────┐
        │  jyc dashboard │         │  Browser / CLI │
        │  + 3rd-party   │         │  chat client   │
        └────────────────┘         └────────────────┘
```

### 1.2 Configuration

```toml
[inspect]
enabled = true
bind = "127.0.0.1:9876"  # Default; localhost only for security
```

| Field     | Required | Default              | Notes                                                                                |
|-----------|----------|----------------------|--------------------------------------------------------------------------------------|
| `enabled` | yes      | `false`              | When `false`, the inspect server is not started.                                     |
| `bind`    | no       | `127.0.0.1:9876`     | TCP bind address. Loopback by default; expose externally only with auth enabled.     |

When `enabled = true`, the server generates a random 256-bit authorization token
at startup, persists it to `<data dir>/auth.token` (mode `0600`), and uses it
to gate every REST and WebSocket request (see §2.2, §3.1). Retrieve it with
`jyc token show`; the dashboard auto-loads it from the same path.

### 1.3 Versioning

The protocol is **not yet formally versioned**. The server embeds its
own version in every state payload via `InspectState.version` (compiled
from `CARGO_PKG_VERSION`).

---

## §2 HTTP REST API

### 2.1 Wire format

- HTTP/1.1 over the same TCP port as the WebSocket endpoint.
- `Content-Type: application/json` for request/response bodies.
- Success: HTTP `200` (or `201 Created` for `POST /api/topics`) + JSON body.
- Error: HTTP `4xx`/`5xx` + JSON body `{"error": "<reason>"}`.
- REST has no client-side request envelope; each route is its own URL.

The Rust client `InspectClient` reuses `reqwest::Client` (which
internally pools connections).

### 2.2 Auth

When the server has `auth_token` configured, every request to `/api/*`
or `/ws/*` must carry:

```
Authorization: Bearer <token>
```

Scheme matching is case-insensitive per RFC 7235 §2.1. Missing or
mismatched token → HTTP `401 Unauthorized` with body
`{"error":"auth_failed"}`. The token is always set when the inspect
server is running (see §1.2); the no-auth path is only reached when
`inspect.enabled = false` and the server is not started at all.

The token is compared in constant time. A `WWW-Authenticate: Bearer`
challenge is not currently emitted.

Exception: `/exchange/*` routes are NOT gated by the bearer middleware.
Access control there is a per-topic `?token=` query parameter generated
by the `jyc_publish_file` tool (see §2.4), so share links work for end
users who have no dashboard token.

### 2.3 Route table

| Method | Path                                          | Purpose                                            |
|--------|-----------------------------------------------|----------------------------------------------------|
| GET    | `/api/state`                                  | Full runtime state snapshot.                       |
| GET    | `/api/state/overview`                         | Slim state (no per-topic activity/messages).      |
| GET    | `/api/topics/{channel}/{topic}/activity`    | Recent activity entries for a topic.              |
| GET    | `/api/topics/{channel}/{topic}/chat`        | Recent chat messages for a topic.                 |
| GET    | `/api/topics/{channel}/{topic}/info`        | One topic's full info, incl. branch/changed files. |
| GET    | `/api/channels/{channel}/patterns`            | Pattern names configured for a channel.            |
| GET    | `/api/topics/{channel}/{topic}/files/{file...}` | Topic-local file (bearer-gated; see §2.4.9).     |
| POST   | `/api/inbound?filename=`                     | Stage an inbound attachment for a pipe (see §2.4.10). |
| POST   | `/api/topics`                                | Register a new ad-hoc topic.                      |
| POST   | `/api/send`                                  | Enqueue one message for an existing agent/topic.  |
| POST   | `/api/config/reload`                          | Reload the layered config (global + data dir).      |
| GET    | `/exchange/{channel}/{topic}/{file...}?token=` | Agent-published file (no bearer auth; see §2.2).   |

WebSocket routes are documented in §3.

### 2.4 REST endpoints

#### 2.4.1 `GET /api/state`

Full runtime state including per-topic `activity`, `recent_messages`,
`thinking_text`. For polling, prefer `GET /api/state/overview`.

**Request:**

```bash
curl -H 'Authorization: Bearer <token>' http://127.0.0.1:9876/api/state
```

**Response (200):**

```json
{
  "uptime_secs": 1234,
  "version": "0.42.0",
  "channels": [
    { "name": "feishu_bot", "channel_type": "feishu",
      "active_workers": 1, "max_concurrent": 4 }
  ],
  "topics": [ /* TopicInfo — see §4 */ ],
  "stats": {
    "active_workers": 1, "total_topics": 12, "max_concurrent": 4,
    "available_workers": 3,
    "messages_received": 567, "messages_processed": 560, "errors": 2
  },
  "commands": [{ "name": "/model", "description": "...",
                 "args": [{ "value": "deepseek/deepseek-chat" },
                          { "value": "reset", "description": "Clear the override" }] }],
  "models":   [{ "name": "deepseek/deepseek-chat" }]
}
```

#### 2.4.2 `GET /api/state/overview`

The topic *list*: `topics` is `Vec<TopicSummary>`, which carries what the
overview table reads and nothing else — the topic's identity (`name`,
`channel`, `agent`, `pattern`), its `status`, its context usage
(`context_input_tokens`, `context_output_tokens`, `max_tokens`),
`last_active_at`, its `parent` and
the `commands` available in it. Every other figure (model, mode, tokens,
cost, latency, tasks, branch, changed files) is the topic's *detail* and
lives on §2.4.11 only.

The list carries no tree either: the lineage behind it is the one-hop
`parent`, and the tree a topic spawned lives in the detail (§2.4.11) — the
tree is built for the one topic on screen, not for every topic of every
poll.

The dashboard's polling loop uses this endpoint for the table, and fetches
§2.4.11 for the one topic it shows.

#### 2.4.3 `GET /api/topics/{channel}/{topic}/activity`

Returns recent activity entries from `.jyc/activity.jsonl`. Each entry
carries the raw topic event — the variant is the `event.type` field —
plus its envelope (`timestamp`, `id`); how (and whether) to render it is
the client's decision. High-frequency heartbeats (`ProcessingProgress`,
`LoopTick`) are dropped at write time and never appear. Rows written
before this shape carry `text` (backend-composed prose) instead of
`event`; clients render that verbatim until the log rotates.

**Query parameters:**

| Param   | Type    | Required | Default | Description                                       |
|---------|---------|----------|---------|---------------------------------------------------|
| `since` | string  | no       | none    | RFC 3339 timestamp; only entries `>= since`.      |
| `limit` | integer | no       | `180`   | Max entries to return.                            |

**Request:**

```bash
curl -H 'Authorization: Bearer <token>' \
  "http://127.0.0.1:9876/api/topics/feishu_bot/issue-42/activity?limit=50"
```

**Response (200):**

```json
[
  { "event": { "type": "tool_completed", "tool_name": "bash",
               "success": true, "duration_secs": 12, "output": "…" },
    "timestamp": "2026-07-31T08:12:34Z", "id": 142 }
]
```

**Errors:**

| Status | Body                              | Trigger                                |
|--------|-----------------------------------|----------------------------------------|
| `404`  | `{"error":"no topic manager…"}`  | `channel` not registered.              |
| `404`  | `{"error":"topic '…' not found"}`| Topic unknown.                        |
| `500`  | `{"error":"failed to load: …"}`   | I/O or parse error on `.jyc/activity.jsonl`. |

#### 2.4.4 `GET /api/topics/{channel}/{topic}/chat`

Returns recent chat messages from `chat_history_*.jsonl`. Includes
**both** user incoming messages and AI replies (the conversation
transcript).

**Query parameters:** same as activity (`since`, `limit`; default `100`).

**Response (200):**

```json
[
  { "sender": "user", "text": "explain this diff",
    "timestamp": "2026-07-31T08:00:11Z", "id": 3,
    "client_msg_id": "4812-1" },
  { "sender": "ai",   "text": "This PR fixes ...",
    "timestamp": "2026-07-31T08:00:14Z", "id": 4 }
]
```

`id` is the message's chat sequence number: one per topic, allocated where
the message enters the system (the enqueue funnel for an inbound message,
delivery for a reply), written into the log line, and carried by the live
`chat_message` event for the same message — so a client dedups on it alone.
Rows logged before the sequence existed have no `id` and read as `0`.
`client_msg_id` is present only on messages a dashboard client sent with its
own id (`metadata.jyc_client_msg_id` on the websocket message frame); it
comes back here and on the live event so the client can replace its
optimistic copy instead of drawing a second one.

#### 2.4.5 `GET /api/channels/{channel}/patterns`

Returns the **enabled** pattern names for a channel. Used by the
dashboard's `c` key to populate the pattern-select UI.

**Response (200):**

```json
{ "patterns": ["general", "coding-help"] }
```

**Errors:** `404` for unknown channel.

#### 2.4.6 `POST /api/topics`

Registers a new ad-hoc topic at a custom workspace path. The topic
name is validated against path traversal: names containing `..`, `/`,
or `\` are rejected with `400`.

**Request body:**

```json
{
  "channel": "feishu_bot",
  "topic":  "my-adhoc",
  "path":    "/home/me/projects/my-adhoc"
}
```

**Response (201):**

```json
{ "message": "topic 'my-adhoc' registered at /home/me/projects/my-adhoc" }
```

**Errors:**

| Status | Body | Trigger |
|--------|------|---------|
| `400`  | `{"error":"invalid topic_name: path traversal not allowed"}` | `topic` contains `..`, `/`, or `\`. |
| `404`  | `{"error":"no topic manager found for channel '…'"}`         | Unknown channel. |
| `500`  | `{"error":"failed to create topic: …"}`                       | Backend error. |

#### 2.4.7 `POST /api/config/reload`

Reloads the layered config from disk, validates it, atomically swaps
the in-memory `AppConfig`, and (if a reload callback is registered)
re-creates channel state.

**Response (200):**

```json
{ "message": "configuration reloaded" }
```

**Errors:**

| Status | Body | Trigger |
|--------|------|---------|
| `422`  | `{"error":"config reload not available (no config path)"}`  | No config path registered. |
| `422`  | `{"error":"failed to load config: …"}`                       | Layered config load failed (parse / IO). |
| `422`  | `{"error":"validation failed: …"}`                            | Config validation failed. |
| `500`  | `{"error":"config reloaded, but channel reload failed: …"}`  | Reload callback error.   |

#### 2.4.8 `GET /exchange/{channel}/{topic}/{file...}?token=`

Serves a file previously published by the `jyc_publish_file` agent tool
(stored under `<topic>/.jyc/exchange/`). NOT gated by the bearer
middleware (§2.2) — the per-topic `token` query parameter is the access
control. The token lives in `<topic>/.jyc/exchange-token`, is created on
first publish, and is deleted by `/reset` or `/new` (which also remove the
published files), invalidating previously shared links.

**Request:**

```bash
curl 'http://127.0.0.1:9876/exchange/email/weather/report.pdf?token=<64-hex>'
```

**Response (200):** raw file bytes with an extension-based `Content-Type`
(`text/html`, `application/pdf`, `image/png`, …, default
`application/octet-stream`).

**Behind a reverse proxy.** The link base is `[inspect] base_url`
(scheme required; port and subpath allowed) — the server cannot infer its
external scheme/host/port, and no request is in flight when a link is built.
Forward the path through unchanged, e.g. nginx:

```nginx
location /exchange/ { proxy_pass http://127.0.0.1:9876; }
```

No auth header injection is needed: `/exchange/*` is not bearer-gated, and
the `?token=` query parameter is the access control.

**Errors:**

| Status | Trigger |
|--------|---------|
| `403`  | Missing/wrong `token`, or no `exchange-token` for the topic. |
| `400`  | Path contains non-normal components (`..`, `.`, absolute). |
| `404`  | Unknown channel/topic, missing file, or path is a directory (no listing). |

#### 2.4.9 `GET /api/topics/{channel}/{topic}/files/{file...}`

Serves a file from the topic directory **in place** — no copy, no token
file. Unlike `/exchange/...` (reserved for agent-published files via
`jyc_publish_file`), this route IS gated by the bearer middleware (§2.2)
and is meant for dashboard clients and pipe reply forwarders downloading
reply attachments.

Websocket reply broadcasts reference these files via
`attachments[].path` (relative URL, already percent-encoded); prepend the
inspect base URL to download.

**Request:**

```bash
curl -H 'Authorization: Bearer <token>' \
  'http://127.0.0.1:9876/api/topics/local_dev/jyc/files/report.pdf'
```

**Response (200):** raw file bytes with an extension-based `Content-Type`
(same mapping as `/exchange/...`).

**Errors:**

| Status | Trigger |
|--------|---------|
| `403`  | Path under `.jyc/` (holds config, tokens, session state). |
| `400`  | Path contains non-normal components (`..`, `.`, absolute). |
| `404`  | Unknown channel/topic, missing file, directory, or a symlink resolving into `.jyc/`. |

#### 2.4.10 `POST /api/inbound?filename=...`

Stages one inbound attachment for a pipe process. The request body is the
raw file (at most 32 MiB — see the errors below); `filename` carries the
original name (percent-encoded), which is validated against the **global**
`[attachments.inbound]` (`allowed_extensions`, `max_file_size`) and kept,
sanitized, as the readable part of the stored name. A per-pattern
`attachments` section is enforced by the pipe before it uploads, not here.
`enabled = false` is refused.

Pipe processes (`jyc-pipe`) do not share a filesystem with the hub, so an
adapter uploads an attachment here and then names the returned `path` in
the `attachments` field of the `message` frame it sends next (§3.3). The
file is staged in the hub's staging directory `<data dir>/.inbound/` —
deliberately outside every topic directory, because the topic is only known
once the message has been routed. When the frame arrives, the topic worker
moves the file into the topic's attachment directory and the agent sees it
like any other inbound attachment. The move is a same-filesystem rename (no
copy); only when a pattern's `attachments.save_path` pins the directory to
another filesystem does it fall back to copy + delete of the staged
original. The endpoint is hub-level, not per-channel: the frame carries the
channel used for routing.

A file still staged once its message is long gone is an orphan: its frame
never arrived (a hub restart in between, a message the hub refused) or the
move failed. **Nothing collects orphans automatically** — check
`<data dir>/.inbound/` by hand and delete what you find there (a failed
move also logs the file's path).

**Request:**

```bash
curl -X POST -H 'Authorization: Bearer <token>' \
  -H 'Content-Type: application/pdf' --data-binary @invoice.pdf \
  'http://127.0.0.1:9876/api/inbound?filename=invoice.pdf'
```

**Response (200):**

```json
{ "path": "6f1c0f6e-5f4b-4c0a-9f5e-2b0f4d8f1c31-invoice.pdf" }
```

**Errors:**

| Status | Trigger |
|--------|---------|
| `400`  | Extension or size rejected by the inbound attachment policy. |
| `403`  | Inbound attachments are disabled (`[attachments.inbound].enabled = false`). |
| `413`  | Body larger than the 32 MiB ceiling. |
| `500`  | Staging directory or file could not be written. |

### 2.5 Example session (curl + Python)

```bash
# 1. Poll overview
curl -s -H 'Authorization: Bearer <token>' \
  http://127.0.0.1:9876/api/state/overview | jq '.topics | length'

# 2. Fetch activity for a topic
curl -s -H 'Authorization: Bearer <token>' \
  "http://127.0.0.1:9876/api/topics/feishu_bot/issue-42/activity?limit=20"

# 3. Create an ad-hoc topic
curl -s -H 'Authorization: Bearer <token>' -X POST \
  -H 'Content-Type: application/json' \
  -d '{"channel":"feishu_bot","topic":"my-adhoc","path":"/tmp/x"}' \
  http://127.0.0.1:9876/api/topics

# 4. Send one message to an existing agent (its own topic)
curl -s -H 'Authorization: Bearer <token>' -X POST \
  -H 'Content-Type: application/json' \
  -d '{"agent":"jyc","message":"Run the hourly report"}' \
  http://127.0.0.1:9876/api/send

# 5. Reload config
curl -s -H 'Authorization: Bearer <token>' -X POST \
  http://127.0.0.1:9876/api/config/reload
```

Python with `requests`:

```python
import requests
BASE = "http://127.0.0.1:9876"
H = {"Authorization": "Bearer <token>"}

overview = requests.get(f"{BASE}/api/state/overview", headers=H).json()
for t in overview["topics"]:
    print(t["channel"], t["name"], t["status"])
```

---

#### 2.4.11 `GET /api/topics/{channel}/{topic}/info`

One topic's full `TopicInfo` — the topic's *detail*: its model and mode, its
token and cost figures, its task list, its branch and changed files, its
`status`, the topic it was spawned by (`parent`), and the tree of subagents
it spawned (`subagents`). `branch` comes
from the checkout's `.git/HEAD`; `changed_files` runs `git diff` against the
repo's default branch (up to four invocations) — which is why the list
(§2.4.2) carries neither.

This is the one source for the topic's figures: the dashboard's topic info
pane reads it, and so does the `/info` command (in-process, through the same
`TopicManager::topic_info` call). The subagent tree rides along for the same
reason: the pane draws it and `/info` prints it, and both read this one
payload.

`last_active_at` is the event stream's stamp for the topic — the same value
the list (§2.4.2) carries, so the two shapes cannot report different times
for one topic. The state dir's mtime is only the fallback, for a topic the
tracker has not seen yet.

`404` when the channel has no topic manager, or that channel has no topic of
this name.

#### 2.4.12 `POST /api/send`

Enqueues one message for an existing agent/topic. This is the HTTP path behind
`jyc send` and uses the same resolver and queue as the `/send` command and the
`jyc_send_to_topic` tool: the target must already exist (a send never creates a
topic). The injected message has no source topic and no attachments, so no
reply route is recorded.

**Request body:**

```json
{
  "agent": "jyc",
  "topic": "reports",
  "message": "Generate the hourly report"
}
```

`topic` is optional; omitting it addresses the agent's own topic. Every field
is required except `topic`, and `message` must be non-empty.

**Response (200):**

```json
{ "message": "Message sent to topic 'reports'. The target topic will process it." }
```

**Errors:**

| Status | Body | Trigger |
|--------|------|---------|
| `401`  | `{"error":"auth_failed"}` | Missing or wrong bearer token. |
| `422`  | `{"error":"Message cannot be empty"}` | Empty or whitespace-only message. |
| `422`  | `{"error":"…"}` | Invalid name, unknown topic, or no manager currently serving it. |
| `429`  | `{"error":"topic '…' queue is full"}` | The target topic's bounded queue has no room. |

A successful response means the message entered the target's queue. The target
topic's single worker processes it in FIFO order; the response does not wait for
the AI reply. A full queue is a rejection, never a silent drop — `jyc send`
stops the batch with a non-zero exit.

## §3 WebSocket API

### 3.1 Connection

WebSocket upgrades are accepted on the same TCP port as REST.
Auth: same `Authorization: Bearer <token>` header (sent on the HTTP
upgrade request). On a 401 the upgrade is denied before the `101
Switching Protocols` response.

Browsers cannot set custom headers on `new WebSocket(url)` from JS.
Browser-based clients must use a reverse proxy that injects the header,
or use a cookie/query-param auth (not currently supported).

Note: the token also gates destructive frames — `close_topic` closes a
live topic (`auto_close_topic`). Holders of the token can already send
messages as any sender, so this adds no new trust class, but the
capability exists.

### 3.2 URL routes

| Path                          | Handler                                              | Use case                                                              |
|-------------------------------|------------------------------------------------------|-----------------------------------------------------------------------|
| `GET /ws`                     | First registered WS-type channel                     | Bare open; topic chosen per `message` payload.                       |
| `GET /ws/<channel>`           | That channel's `WebsocketInboundAdapter`             | Ad-hoc topic on a websocket channel.                                 |
| `GET /ws/<channel>/<topic>`  | If WS channel → `ScopedWsHandler`; else → `TopicProxyHandler` | Topic-scoped chat, the canonical dashboard path.                    |

> **Naming.** `<channel>` is the configured channel name (e.g.
> `feishu_bot`); `<topic>` is the topic name (e.g. `issue-42`).

### 3.3 Client → Server messages

All client messages are JSON text frames with a `type` discriminator.
There are **two** `ClientMessage` enums, depending on which handler
serves the route (see §3.2):

| Route                        | Handler                  | `message` payload                            |
|------------------------------|--------------------------|----------------------------------------------|
| `/ws`                        | first WS channel         | `{ "topic": "<name>", "text": "..." }`     |
| `/ws/<channel>`              | channel's WS adapter     | `{ "topic": "<name>", "text": "..." }`     |
| `/ws/<channel>/<topic>` (WS channel)   | `ScopedWsHandler` → adapter   | `{ "topic"?: "<name>", "text": "..." }` (payload `topic` overrides the URL) |
| `/ws/<channel>/<topic>` (other channel) | `TopicProxyHandler`          | `{ "text": "..." }` (payload `topic` is ignored; URL is the only source) |

Handlers on a WebSocket-type channel (`/ws`, `/ws/<channel>`, and
`/ws/<channel>/<topic>` when `<channel>` is a WS channel) also accept
these optional `message` fields. The proxy route for non-WS channels
ignores them — it hardcodes `sender: "dashboard"` and loads metadata
from the persisted `topic-meta.json` instead.

| Field           | Type                       | Meaning                                             |
|-----------------|----------------------------|-----------------------------------------------------|
| `sender`        | `string?`                  | Display name (e.g. a feishu user name).             |
| `sender_address`| `string?`                  | Canonical address (e.g. an open_id).                |
| `metadata`      | `object?` (string → JSON)  | Pipe hints (`pipe_pattern`), platform ids, ...      |
| `attachments`   | `array?`                   | Attachments staged beforehand (§2.4.10), each `{ "filename", "path", "content_type" }` — same shape as a reply's `attachments` minus `size` (the worker stats the file), with `path` naming a file in the hub's staging directory instead of a URL. |

`close_topic` (`{ "topic": "<name>" }`) asks a websocket-channel hub to
close a topic (`TopicManager::auto_close_topic`). Sent by external pipe
processes (`jyc-pipe`) when the platform-side conversation ends (feishu
chat disband, GitHub issue/PR closed). Ignored with a warning when the
adapter has no `TopicManager`.

`disconnect` (`{}`) and `ping` (`{}`) are accepted by both handlers
with identical semantics.

> **Removed.** Earlier protocol versions carried `list_patterns`,
> `subscribe`, and `create_topic` as client messages. These were
> **moved to REST** (§2.4.5, §2.4.6).

### 3.4 Server → Client events

Server-pushed events are JSON text frames from a shared
`tokio::sync::broadcast::Sender` filtered per-connection by
`(channel, topic)`.

| `type`         | Payload                                                            | Emitted when                                                                  |
|----------------|--------------------------------------------------------------------|-------------------------------------------------------------------------------|
| `topic_event`  | `{ "channel", "topic", "event": TopicEvent }`                     | Any typed `TopicEvent` fires on a topic — the raw event stream (`ProcessingStarted`, `Thinking`, `ToolStarted`, `ProcessingCompleted`, ...). The 1 Hz `LoopTick` heartbeat is excluded. Consumed by external pipe processes (`jyc-pipe`); dashboard clients can ignore it. |
| `activity`     | `{ "channel", "topic", "id", "entry": ActivityEntry }`            | A new entry is appended to `.jyc/activity.jsonl`.                             |
| `chat_message` | `{ "channel", "topic", "id", "entry": ChatMessageEntry }`         | An incoming message or a sent reply arrives. The row's `id` is its chat sequence number, from a different number space than `activity`'s. |
| `thinking`     | `{ "channel", "topic", "text" }`                                  | The agent publishes a thinking chunk.                                         |
| `processing`   | `{ "channel", "topic", "is_processing": bool, "has_error": bool }` | A topic enters / leaves the Processing state. `has_error` is the latest turn's outcome, not a running report on the tools: `true` when that turn failed, cleared when the next turn starts. A tool call that failed along the way does not set it — the turn may have reached the answer another way, and the failure stays in the `activity` stream as a `tool_completed` entry with `success: false`. |
| `reply`        | `{ "topic": "<name>", "text": "...", "id": u64 }`                 | A websocket-channel `WebsocketOutboundAdapter` broadcasts an AI reply; `id` is the chat sequence number of its log row (`0` for a proactive send, which logs none). |
| `loop_tick`    | `{ "channel", "topic", "elapsed_ms": u64 }`                       | 1 Hz wall-clock tick during a processing cycle (first tick at `t=0`). Drives the dashboard's live-duration ticker. Not persisted. |
| `resync`       | `{ "channel", "topic", "dropped": <n> }`                          | The client's broadcast receiver lagged and missed messages.                   |

#### 3.4.1 `activity`

```json
{
  "type": "activity",
  "channel": "feishu_bot",
  "topic":  "issue-42",
  "id": 142,
  "entry": { "event": { "type": "tool_completed", "tool_name": "bash",
                        "success": true, "duration_secs": 12, "output": "…" },
             "timestamp": "2026-07-31T08:12:34Z", "id": 142 }
}
```

> Heartbeat events (`ProcessingProgress`, `LoopTick`) are dropped at write
> time: they are not persisted to `activity.jsonl`, not returned by
> `GET /api/topics/.../activity`, and not forwarded as an `activity` frame.
> There is no server-side visibility filter — the entry is the raw event,
> and rendering is the client's job. Rows written before this shape carry
> `text` instead of `event`.

#### 3.4.2 `chat_message`

```json
{
  "type": "chat_message",
  "channel": "feishu_bot",
  "topic":  "issue-42",
  "id": 9,
  "entry": { "sender": "ai", "text": "Here is the fix ...",
             "timestamp": "2026-07-31T08:13:02Z", "id": 9 }
}
```

`sender` is `"user"`, `"ai"`, or a display name supplied by the channel.

#### 3.4.3 `thinking`

```json
{ "type": "thinking", "channel": "feishu_bot", "topic": "issue-42",
  "text": "The user wants me to check the lint config ..." }
```

Sent **incrementally** while the LLM streams reasoning text. The
`text` field is the **full** accumulated text, not a delta — clients
should replace their local copy on each event.

#### 3.4.4 `processing`

```json
{ "type": "processing", "channel": "feishu_bot", "topic": "issue-42",
  "is_processing": true, "has_error": false }
```

#### 3.4.5 `reply`

```json
{ "type": "reply", "topic": "issue-42", "text": "Here is the fix ..." }
```

> **Legacy.** Only emitted on websocket-type channels via
> `WebsocketOutboundAdapter::broadcast_reply`. The dashboard-side
> `TopicProxyHandler` does **not** emit `reply` — use `chat_message`
> instead.

#### 3.4.6 `resync`

```json
{ "type": "resync", "channel": "feishu_bot", "topic": "issue-42",
  "dropped": 7 }
```

Emitted by `TopicProxyHandler` after
`tokio::sync::broadcast::error::RecvError::Lagged(n)`. The client
should drop in-memory state for `(channel, topic)` and re-fetch
via REST.

---

## §4 Type Catalog

All shared types live in `crates/jyc-types/src/inspect.rs`.

### 4.1 `InspectState` (`GET /api/state`)

| Field         | Type                | Description                                              |
|---------------|---------------------|----------------------------------------------------------|
| `uptime_secs` | u64                 | Seconds since the monitor process started.               |
| `version`     | string              | Server `CARGO_PKG_VERSION`.                              |
| `channels`    | `Vec<ChannelInfo>`  | All configured channels.                                 |
| `topics`     | `Vec<TopicInfo>`   | All known topics (incl. activity, messages, thinking).  |
| `stats`       | `GlobalStats`       | Aggregate counters.                                      |
| `commands`    | `Vec<CommandInfo>`  | Available slash commands.                                |
| `models`      | `Vec<ModelInfo>`    | Available model identifiers.                             |

### 4.2 `InspectOverview` (`GET /api/state/overview`)

Identical to `InspectState` except `topics: Vec<TopicSummary>` — the list
form (see §2.4.2): the topic's identity, its status, its context usage, and
when it was last active. The detail figures are not in it at all; fetch
§2.4.11 for the one topic you show.

### 4.3 `TopicInfo` vs `TopicSummary`

The two types are the two views of a topic, and their *figures* do not
overlap: the list has what the table needs, the detail has what the topic's
own figures are, so a client can never read the same figure from two places
that could disagree. The fields both carry — name, channel, pattern, the
context bounds, `last_active_at`, `topic_path`, `parent`, `commands` —
identify the topic rather than measure it, and both carry the same value
for each.

| Field                        | Type                  | In `TopicInfo` | In `TopicSummary` |
|------------------------------|-----------------------|:---------------:|:------------------:|
| `name`                       | string                | ✓               | ✓                  |
| `channel`                    | string                | ✓               | ✓                  |
| `agent`                      | string?               | —               | ✓                  |
| `pattern`                    | string?               | ✓               | ✓                  |
| `status`                     | `TopicStatus`        | ✓               | ✓                  |
| `context_input_tokens`       | u64?                  | ✓               | ✓                  |
| `context_output_tokens`      | u64?                  | ✓               | ✓                  |
| `max_tokens`                 | u64?                  | ✓               | ✓                  |
| `last_active_at`             | string? (RFC 3339)    | ✓               | ✓                  |
| `topic_path`                | `PathBuf?`            | ✓               | ✓                  |
| `parent`                     | string?               | ✓               | ✓                  |
| `commands`                   | `Vec<CommandInfo>`    | ✓               | ✓                  |
| `model`                      | string?               | ✓               | —                  |
| `mode`                       | string?               | ✓               | —                  |
| `output_tokens`              | u64?                  | ✓               | —                  |
| `turn_output_tokens`        | u64?                  | ✓               | —                  |
| `turn_reasoning_tokens`     | u64?                  | ✓               | —                  |
| `total_input_tokens`         | u64?                  | ✓               | —                  |
| `total_cache_hit_tokens`     | u64?                  | ✓               | —                  |
| `total_cache_creation_tokens`| u64?                  | ✓               | —                  |
| `total_reasoning_tokens`     | u64?                  | ✓               | —                  |
| `turn_cache_hit_tokens`      | u64?                  | ✓               | —                  |
| `turn_input_tokens`          | u64?                  | ✓               | —                  |
| `avg_call_ms`                | u64?                  | ✓               | —                  |
| `call_count`                 | u64?                  | ✓               | —                  |
| `avg_turn_call_ms`           | u64?                  | ✓               | —                  |
| `turn_call_count`            | u64?                  | ✓               | —                  |
| `skills`                     | `Vec<string>`         | ✓               | —                  |
| `available_skills`           | `Vec<SkillMeta>`      | ✓               | —                  |
| `branch`                     | string?               | ✓               | —                  |
| `changed_files`              | `Vec<{path, uncommitted, change, lines?}?>` | ✓          | —                  |
| `tasks`                      | `TaskList`            | ✓               | —                  |
| `cost`                       | `TopicCost?`         | ✓               | —                  |
| `today_all`                  | `Vec<(string, f64)>` | ✓               | —                  |
| `activity`                   | `Vec<ActivityEntry>`  | ✓               | —                  |
| `recent_messages`            | `Vec<ChatMessageEntry>`| ✓              | —                  |
| `thinking_text`              | string?               | ✓               | —                  |
| `subagents`                  | `Vec<TopicNode>?`     | ✓               | —                  |

`status` is the one figure both carry, and both carry the same one: the
runtime's live status, written as turns start and end (see §2.4.11).
`last_active_at` is the other field both carry, and both carry the same one:
the event stream's stamp for the topic, with the state dir's mtime as the
fallback for a topic the tracker has not seen yet.

The lineage is split the same way: the list carries only the one-hop
`parent` (a topic's own parent, read from the `spawned-from` breadcrumb the
subagent wrote into its state dir), while the detail carries the whole tree
in `subagents` — the topic's descendants, each node holding the subagents it
spawned in turn. Both come from one resolution: the topic manager reads the
breadcrumbs once and answers with the parents and the tree alike, so the
list's `parent` and the detail's `subagents` cannot disagree. `TopicSummary`
has no `subagents` field to carry the tree; in the detail, `subagents` is
`Some(vec![])` when the topic has no subagents.

The cache figures come in two scopes: `total_cache_hit_tokens` /
`total_input_tokens` are the session's (every LLM call since the last
session reset), while `turn_cache_hit_tokens` / `turn_input_tokens` are the
latest turn's — the pair is zeroed at every turn start, so it is `None` until
the turn makes its first call. Both scopes feed the info surfaces' cache
rows: `Turn` and `Session` under the info surfaces' `Cache:` header, each
showing the hits, the input they are a share of, and that share in percent.

The output figures come in the same two scopes, and print on one row:
`output_tokens` is the session's and `turn_output_tokens` the latest turn's
(also zeroed at every turn start, so `None` until the turn makes its first
call). Both surfaces print them as `12.00K turn · 1.18M session` from one
shared formatter.

`total_reasoning_tokens` (the hidden chain-of-thought share of the output,
already included in `output_tokens`) and `turn_reasoning_tokens` follow the
same pattern and print on the `Reasoning:` row through that same formatter —
turn first, session second.

`today_all` is the day's spend across *every* topic, per currency — the
ledger's own day total, the same figure `/bill` reports. It is kept apart
from `cost`, which is this topic's own spend (`cost.today`).

### 4.4 Other types

- `TopicStatus` — `queued` / `processing` / `idle` / `error`
- `Severity` — `info` / `warning` / `error` (default `info`)
- `ActivityEntry` — `text`, `timestamp?`, `severity`, `id`, `is_internal`
- `ChatMessageEntry` — `sender`, `text`, `timestamp?`, `id`, `client_msg_id?`
- `ChannelInfo` — `name`, `channel_type`, `active_workers`, `max_concurrent`
- `ChangedFileEntry` — `{path: string, uncommitted: bool, change: ChangeKind, lines: LineStats?}`. `uncommitted` is `true` when the working tree has changes vs HEAD (i.e. the file is currently dirty — staged or unstaged). A path present in both `main...HEAD` and the working-tree diff appears once with `uncommitted: true` (the more-noisy state wins) and the branch-side `change` / `lines`. `change` carries the status (`added` / `modified` / `deleted`); the chat info pane renders a one-column prefix glyph per row (`+`, `-`, two spaces). `lines` is `{insertions: u32, deletions: u32}` for that same diff, absent for a binary file. Resolved server-side from `git diff` invocations on the topic's working directory (`--name-status main...HEAD` ∪ `--name-status HEAD`, each joined with its `--numstat`); `None` when the path isn't a git repo or both invocations fail.
- `ChangeKind` — `added` / `modified` / `deleted`. `Modified` is the default — old payloads missing the field deserialize as `Modified`. Renames, copies, type changes from `git diff --name-status` are normalized to `Modified` server-side (no separate variant for those).
- `TaskList` — `{items: Vec<TaskItem>}`: the agent's own plan for the topic, persisted in `.jyc/tasks.json` and replaced wholesale by the next `task_create`. `TaskItem` is `{id: u32, text: string, status: TaskStatus}`; `id` is assigned by `task_create` and stable until a new list replaces it. `TaskStatus` — `pending` / `in_progress` / `completed`.
- `TopicNode` — `{name: string, channel: string, status: TopicStatus, children: Vec<TopicNode>}`: one topic in the lineage tree, with the subagents it spawned nested under it. `children` is the same shape, recursively, and is `[]` for a leaf. A cycle in the breadcrumbs (hand-edited state) is cut, not followed: every topic appears once, and the edge that would close the cycle is the one dropped.
- `TopicCost` — `session: f64` (current agent session, zeroes on reset), `today: f64` (UTC day total from billing ledger), `currency: string` (`"USD"` or `"mixed"` when today's entries span multiple currencies)
- `GlobalStats` — `active_workers`, `total_topics`, `max_concurrent`, `available_workers`, `messages_received`, `messages_processed`, `errors`
- `CommandInfo` — `name` (e.g. `"/model"`), `description`, `continues_to_agent`,
  `args: Vec<CommandArg>` — enumerable argument values for the command, `[]`
  when its arguments are free text. Filled per topic by the inspect server
  from `jyc_core::command::command_args` (models, skills, MCP servers, and the
  literals each handler accepts). `serde(default)`, so payloads from older
  servers simply carry no values.
- `CommandArg` — `{value: string, description: string, args: Vec<CommandArg>}`.
  Nests to arbitrary depth: `args` holds the values of the *next* argument
  position, so `/skill on <name>` is an `on` entry whose `args` are the skill
  names. A leaf entry has `args: []`.
- `ModelInfo` — `name` (e.g. `"deepseek/deepseek-chat"`)

---

## §5 Compatibility Notes

- The HTTP REST API replaces the old line-delimited JSON protocol
  (removed). Old clients must migrate.
- The WebSocket protocol is unchanged. The new auth check runs as
  `require_bearer` middleware (instead of inline in the WS handler);
  observable behavior is identical.
- The `Authorization: Bearer <token>` scheme is case-insensitive per
  RFC 7235 §2.1.
- Token comparison is constant-time. A trivial timing attack against
  the WS path's old `!=` comparison is fixed.
- `ActivityEntry` / `ChatMessageEntry` missing `id` default to `0`;
  missing `is_internal` defaults to `false`; a `ChatMessageEntry` without
  `client_msg_id` simply has none.

---

## §6 References

- Source files:
  - `crates/jyc-types/src/inspect.rs` — protocol data types
  - `crates/jyc-inspect/src/server.rs` — TCP / axum server, WS routes
  - `crates/jyc-inspect/src/auth.rs` — `require_bearer` middleware
  - `crates/jyc-inspect/src/api.rs` — REST handlers
  - `crates/jyc-inspect/src/client.rs` — `reqwest` client
  - `crates/jyc-inspect/src/scoped_ws.rs` — `ScopedWsHandler`
  - `crates/jyc-inspect/src/topic_proxy.rs` — `TopicProxyHandler`
  - `crates/jyc-inspect/src/server/websocket/inbound.rs` — `WebsocketInboundAdapter`
  - `crates/jyc-inspect/src/server/websocket/outbound.rs` — `WebsocketOutboundAdapter` (legacy `reply` event)
- Related docs:
  - `docs/channels/websocket.md` — websocket channel (user-facing)
  - `docs/tools.md` — agent tools
  - `DESIGN.md` — architecture (inspect server section)
  - `config.example.toml` — `[inspect]` config block
