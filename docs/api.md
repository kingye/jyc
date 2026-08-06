# JYC API Reference

Real-time protocols exposed by the JYC monitor on the inspect TCP port
(default `127.0.0.1:9876`):

- **§2 HTTP REST API** — JSON requests/responses over HTTP/1.1. State
  queries, thread / pattern management, config reload.
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
at startup, persists it to `<workdir>/auth.token` (mode `0600`), and uses it
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
- Success: HTTP `200` (or `201 Created` for `POST /api/threads`) + JSON body.
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
Access control there is a per-thread `?token=` query parameter generated
by the `jyc_publish_file` tool (see §2.4), so share links work for end
users who have no dashboard token.

### 2.3 Route table

| Method | Path                                          | Purpose                                            |
|--------|-----------------------------------------------|----------------------------------------------------|
| GET    | `/api/state`                                  | Full runtime state snapshot.                       |
| GET    | `/api/state/overview`                         | Slim state (no per-thread activity/messages).      |
| GET    | `/api/threads/{channel}/{thread}/activity`    | Recent activity entries for a thread.              |
| GET    | `/api/threads/{channel}/{thread}/chat`        | Recent chat messages for a thread.                 |
| GET    | `/api/channels/{channel}/patterns`            | Pattern names configured for a channel.            |
| POST   | `/api/threads`                                | Register a new ad-hoc thread.                      |
| POST   | `/api/config/reload`                          | Reload the layered config (global + workdir).      |
| GET    | `/exchange/{channel}/{thread}/{file...}?token=` | Agent-published file (no bearer auth; see §2.2).   |

WebSocket routes are documented in §3.

### 2.4 REST endpoints

#### 2.4.1 `GET /api/state`

Full runtime state including per-thread `activity`, `recent_messages`,
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
  "threads": [ /* ThreadInfo — see §4 */ ],
  "stats": {
    "active_workers": 1, "total_threads": 12, "max_concurrent": 4,
    "available_workers": 3,
    "messages_received": 567, "messages_processed": 560, "errors": 2
  },
  "commands": [{ "name": "/model", "description": "..." }],
  "models":   [{ "name": "deepseek/deepseek-chat" }]
}
```

#### 2.4.2 `GET /api/state/overview`

Same shape as `GET /api/state` but `threads` is `Vec<ThreadSummary>`
(no `activity`, `recent_messages`, or `thinking_text` per thread). The
dashboard's polling loop uses this endpoint.

#### 2.4.3 `GET /api/threads/{channel}/{thread}/activity`

Returns recent activity entries from `.jyc/activity.jsonl`. Internal
heartbeats (`is_internal: true`) are filtered server-side.

**Query parameters:**

| Param   | Type    | Required | Default | Description                                       |
|---------|---------|----------|---------|---------------------------------------------------|
| `since` | string  | no       | none    | RFC 3339 timestamp; only entries `>= since`.      |
| `limit` | integer | no       | `180`   | Max entries to return.                            |

**Request:**

```bash
curl -H 'Authorization: Bearer <token>' \
  "http://127.0.0.1:9876/api/threads/feishu_bot/issue-42/activity?limit=50"
```

**Response (200):**

```json
[
  { "text": "tool execution (12s, 348 chars)",
    "timestamp": "2026-07-31T08:12:34Z",
    "severity": "info", "id": 142, "is_internal": false }
]
```

**Errors:**

| Status | Body                              | Trigger                                |
|--------|-----------------------------------|----------------------------------------|
| `404`  | `{"error":"no thread manager…"}`  | `channel` not registered.              |
| `404`  | `{"error":"thread '…' not found"}`| Thread unknown.                        |
| `500`  | `{"error":"failed to load: …"}`   | I/O or parse error on `.jyc/activity.jsonl`. |

#### 2.4.4 `GET /api/threads/{channel}/{thread}/chat`

Returns recent chat messages from `chat_history_*.jsonl`. Includes
**both** user incoming messages and AI replies (the conversation
transcript).

**Query parameters:** same as activity (`since`, `limit`; default `100`).

**Response (200):**

```json
[
  { "sender": "user", "text": "explain this diff",
    "timestamp": "2026-07-31T08:00:11Z", "id": 3 },
  { "sender": "ai",   "text": "This PR fixes ...",
    "timestamp": "2026-07-31T08:00:14Z", "id": 4 }
]
```

#### 2.4.5 `GET /api/channels/{channel}/patterns`

Returns the **enabled** pattern names for a channel. Used by the
dashboard's `c` key to populate the pattern-select UI.

**Response (200):**

```json
{ "patterns": ["general", "coding-help"] }
```

**Errors:** `404` for unknown channel.

#### 2.4.6 `POST /api/threads`

Registers a new ad-hoc thread at a custom workspace path. The thread
name is validated against path traversal: names containing `..`, `/`,
or `\` are rejected with `400`.

**Request body:**

```json
{
  "channel": "feishu_bot",
  "thread":  "my-adhoc",
  "path":    "/home/me/projects/my-adhoc"
}
```

**Response (201):**

```json
{ "message": "thread 'my-adhoc' registered at /home/me/projects/my-adhoc" }
```

**Errors:**

| Status | Body | Trigger |
|--------|------|---------|
| `400`  | `{"error":"invalid thread_name: path traversal not allowed"}` | `thread` contains `..`, `/`, or `\`. |
| `404`  | `{"error":"no thread manager found for channel '…'"}`         | Unknown channel. |
| `500`  | `{"error":"failed to create thread: …"}`                       | Backend error. |

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

#### 2.4.8 `GET /exchange/{channel}/{thread}/{file...}?token=`

Serves a file previously published by the `jyc_publish_file` agent tool
(stored under `<thread>/.jyc/exchange/`). NOT gated by the bearer
middleware (§2.2) — the per-thread `token` query parameter is the access
control. The token lives in `<thread>/.jyc/exchange-token`, is created on
first publish, and is deleted by `/reset` (which also removes the
published files), invalidating previously shared links.

**Request:**

```bash
curl 'http://127.0.0.1:9876/exchange/email/weather/report.pdf?token=<64-hex>'
```

**Response (200):** raw file bytes with an extension-based `Content-Type`
(`text/html`, `application/pdf`, `image/png`, …, default
`application/octet-stream`).

**Errors:**

| Status | Trigger |
|--------|---------|
| `403`  | Missing/wrong `token`, or no `exchange-token` for the thread. |
| `400`  | Path contains non-normal components (`..`, `.`, absolute). |
| `404`  | Unknown channel/thread, missing file, or path is a directory (no listing). |

### 2.5 Example session (curl + Python)

```bash
# 1. Poll overview
curl -s -H 'Authorization: Bearer <token>' \
  http://127.0.0.1:9876/api/state/overview | jq '.threads | length'

# 2. Fetch activity for a thread
curl -s -H 'Authorization: Bearer <token>' \
  "http://127.0.0.1:9876/api/threads/feishu_bot/issue-42/activity?limit=20"

# 3. Create an ad-hoc thread
curl -s -H 'Authorization: Bearer <token>' -X POST \
  -H 'Content-Type: application/json' \
  -d '{"channel":"feishu_bot","thread":"my-adhoc","path":"/tmp/x"}' \
  http://127.0.0.1:9876/api/threads

# 4. Reload config
curl -s -H 'Authorization: Bearer <token>' -X POST \
  http://127.0.0.1:9876/api/config/reload
```

Python with `requests`:

```python
import requests
BASE = "http://127.0.0.1:9876"
H = {"Authorization": "Bearer <token>"}

overview = requests.get(f"{BASE}/api/state/overview", headers=H).json()
for t in overview["threads"]:
    print(t["channel"], t["name"], t["status"])
```

---

## §3 WebSocket API

### 3.1 Connection

WebSocket upgrades are accepted on the same TCP port as REST.
Auth: same `Authorization: Bearer <token>` header (sent on the HTTP
upgrade request). On a 401 the upgrade is denied before the `101
Switching Protocols` response.

Browsers cannot set custom headers on `new WebSocket(url)` from JS.
Browser-based clients must use a reverse proxy that injects the header,
or use a cookie/query-param auth (not currently supported).

### 3.2 URL routes

| Path                          | Handler                                              | Use case                                                              |
|-------------------------------|------------------------------------------------------|-----------------------------------------------------------------------|
| `GET /ws`                     | First registered WS-type channel                     | Bare open; thread chosen per `message` payload.                       |
| `GET /ws/<channel>`           | That channel's `WebsocketInboundAdapter`             | Ad-hoc thread on a websocket channel.                                 |
| `GET /ws/<channel>/<thread>`  | If WS channel → `ScopedWsHandler`; else → `ThreadProxyHandler` | Thread-scoped chat, the canonical dashboard path.                    |

> **Naming.** `<channel>` is the configured channel name (e.g.
> `feishu_bot`); `<thread>` is the thread name (e.g. `issue-42`).

### 3.3 Client → Server messages

All client messages are JSON text frames with a `type` discriminator.
There are **two** `ClientMessage` enums, depending on which handler
serves the route (see §3.2):

| Route                        | Handler                  | `message` payload                            |
|------------------------------|--------------------------|----------------------------------------------|
| `/ws`                        | first WS channel         | `{ "thread": "<name>", "text": "..." }`     |
| `/ws/<channel>`              | channel's WS adapter     | `{ "thread": "<name>", "text": "..." }`     |
| `/ws/<channel>/<thread>` (WS channel)   | `ScopedWsHandler` → adapter   | `{ "thread"?: "<name>", "text": "..." }` (payload `thread` overrides the URL) |
| `/ws/<channel>/<thread>` (other channel) | `ThreadProxyHandler`          | `{ "text": "..." }` (payload `thread` is ignored; URL is the only source) |

`disconnect` (`{}`) and `ping` (`{}`) are accepted by both handlers
with identical semantics.

> **Removed.** Earlier protocol versions carried `list_patterns`,
> `subscribe`, and `create_thread` as client messages. These were
> **moved to REST** (§2.4.5, §2.4.6).

### 3.4 Server → Client events

Server-pushed events are JSON text frames from a shared
`tokio::sync::broadcast::Sender` filtered per-connection by
`(channel, thread)`.

| `type`         | Payload                                                            | Emitted when                                                                  |
|----------------|--------------------------------------------------------------------|-------------------------------------------------------------------------------|
| `activity`     | `{ "channel", "thread", "id", "entry": ActivityEntry }`            | A new entry is appended to `.jyc/activity.jsonl`.                             |
| `chat_message` | `{ "channel", "thread", "id", "entry": ChatMessageEntry }`         | An incoming message or a sent reply arrives.                                  |
| `thinking`     | `{ "channel", "thread", "text" }`                                  | The agent publishes a thinking chunk.                                         |
| `processing`   | `{ "channel", "thread", "is_processing": bool, "has_error": bool }` | A thread enters / leaves the Processing state.                                |
| `reply`        | `{ "thread": "<name>", "text": "..." }`                            | A websocket-channel `WebsocketOutboundAdapter` broadcasts an AI reply.        |
| `loop_tick`    | `{ "channel", "thread", "elapsed_ms": u64 }`                       | 1 Hz wall-clock tick during a processing cycle (first tick at `t=0`). Drives the dashboard's live-duration ticker. Not persisted. |
| `resync`       | `{ "channel", "thread", "dropped": <n> }`                          | The client's broadcast receiver lagged and missed messages.                   |

#### 3.4.1 `activity`

```json
{
  "type": "activity",
  "channel": "feishu_bot",
  "thread":  "issue-42",
  "id": 142,
  "entry": { "text": "tool execution (12s, 348 chars)",
             "timestamp": "2026-07-31T08:12:34Z",
             "severity": "info", "id": 142, "is_internal": false }
}
```

> `is_internal: true` entries (e.g. `ProcessingProgress` heartbeats) are
> filtered out on **both** surfaces: they are not persisted to
> `activity.jsonl`, not returned by `GET /api/threads/.../activity`, and
> not forwarded as an `activity` event on the WebSocket. Only the
> in-memory `ThreadActivityState` buffer keeps them, for debug purposes.

#### 3.4.2 `chat_message`

```json
{
  "type": "chat_message",
  "channel": "feishu_bot",
  "thread":  "issue-42",
  "id": 9,
  "entry": { "sender": "ai", "text": "Here is the fix ...",
             "timestamp": "2026-07-31T08:13:02Z", "id": 9 }
}
```

`sender` is `"user"`, `"ai"`, or a display name supplied by the channel.

#### 3.4.3 `thinking`

```json
{ "type": "thinking", "channel": "feishu_bot", "thread": "issue-42",
  "text": "The user wants me to check the lint config ..." }
```

Sent **incrementally** while the LLM streams reasoning text. The
`text` field is the **full** accumulated text, not a delta — clients
should replace their local copy on each event.

#### 3.4.4 `processing`

```json
{ "type": "processing", "channel": "feishu_bot", "thread": "issue-42",
  "is_processing": true, "has_error": false }
```

#### 3.4.5 `reply`

```json
{ "type": "reply", "thread": "issue-42", "text": "Here is the fix ..." }
```

> **Legacy.** Only emitted on websocket-type channels via
> `WebsocketOutboundAdapter::broadcast_reply`. The dashboard-side
> `ThreadProxyHandler` does **not** emit `reply` — use `chat_message`
> instead.

#### 3.4.6 `resync`

```json
{ "type": "resync", "channel": "feishu_bot", "thread": "issue-42",
  "dropped": 7 }
```

Emitted by `ThreadProxyHandler` after
`tokio::sync::broadcast::error::RecvError::Lagged(n)`. The client
should drop in-memory state for `(channel, thread)` and re-fetch
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
| `threads`     | `Vec<ThreadInfo>`   | All known threads (incl. activity, messages, thinking).  |
| `stats`       | `GlobalStats`       | Aggregate counters.                                      |
| `commands`    | `Vec<CommandInfo>`  | Available slash commands.                                |
| `models`      | `Vec<ModelInfo>`    | Available model identifiers.                             |

### 4.2 `InspectOverview` (`GET /api/state/overview`)

Identical to `InspectState` except `threads: Vec<ThreadSummary>` — i.e.
no `activity`, no `recent_messages`, no `thinking_text` per thread.

### 4.3 `ThreadInfo` vs `ThreadSummary`

Both have the fields below, except `ThreadSummary` **omits** `activity`,
`recent_messages`, and `thinking_text`.

| Field                        | Type                  | In `ThreadInfo` | In `ThreadSummary` |
|------------------------------|-----------------------|:---------------:|:------------------:|
| `name`                       | string                | ✓               | ✓                  |
| `channel`                    | string                | ✓               | ✓                  |
| `pattern`                    | string?               | ✓               | ✓                  |
| `status`                     | `ThreadStatus`        | ✓               | ✓                  |
| `model`                      | string?               | ✓               | ✓                  |
| `mode`                       | string?               | ✓               | ✓                  |
| `context_input_tokens`       | u64?                  | ✓               | ✓                  |
| `max_tokens`                 | u64?                  | ✓               | ✓                  |
| `output_tokens`              | u64?                  | ✓               | ✓                  |
| `total_input_tokens`         | u64?                  | ✓               | ✓                  |
| `total_cache_hit_tokens`     | u64?                  | ✓               | ✓                  |
| `total_cache_creation_tokens`| u64?                  | ✓               | ✓                  |
| `last_active_at`             | string? (RFC 3339)    | ✓               | ✓                  |
| `skills`                     | `Vec<string>`         | ✓               | ✓                  |
| `thread_path`                | `PathBuf?`            | ✓               | ✓                  |
| `branch`                     | string?               | ✓               | ✓                  |
| `changed_files`              | `Vec<{path, uncommitted, change}?>` | ✓          | ✓                  |
| `cost`                       | `ThreadCost?`         | ✓               | ✓                  |
| `activity`                   | `Vec<ActivityEntry>`  | ✓               | —                  |
| `recent_messages`            | `Vec<ChatMessageEntry>`| ✓              | —                  |
| `thinking_text`              | string?               | ✓               | —                  |

### 4.4 Other types

- `ThreadStatus` — `queued` / `processing` / `idle` / `waiting_for_answer` / `error`
- `Severity` — `info` / `warning` / `error` (default `info`)
- `ActivityEntry` — `text`, `timestamp?`, `severity`, `id`, `is_internal`
- `ChatMessageEntry` — `sender`, `text`, `timestamp?`, `id`
- `ChannelInfo` — `name`, `channel_type`, `active_workers`, `max_concurrent`
- `ChangedFileEntry` — `{path: string, uncommitted: bool, change: ChangeKind}`. `uncommitted` is `true` when the working tree has changes vs HEAD (i.e. the file is currently dirty — staged or unstaged). A path present in both `main...HEAD` and the working-tree diff appears once with `uncommitted: true` (the more-noisy state wins). `change` carries the branch-side status (`added` / `modified` / `deleted`); the chat info pane renders a one-column prefix glyph per row (`+`, `-`, two spaces). Resolved server-side from two `git diff` invocations (`--name-status main...HEAD` ∪ `--name-only HEAD`) on the thread's working directory; `None` when the path isn't a git repo or both invocations fail.
- `ChangeKind` — `added` / `modified` / `deleted`. `Modified` is the default — old payloads missing the field deserialize as `Modified`. Renames, copies, type changes from `git diff --name-status` are normalized to `Modified` server-side (no separate variant for those).
- `ThreadCost` — `session: f64` (current agent session, zeroes on reset), `today: f64` (UTC day total from billing ledger), `currency: string` (`"USD"` or `"mixed"` when today's entries span multiple currencies)
- `GlobalStats` — `active_workers`, `total_threads`, `max_concurrent`, `available_workers`, `messages_received`, `messages_processed`, `errors`
- `CommandInfo` — `name` (e.g. `"/model"`), `description`
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
  missing `is_internal` defaults to `false`.

---

## §6 References

- Source files:
  - `crates/jyc-types/src/inspect.rs` — protocol data types
  - `crates/jyc-inspect/src/server.rs` — TCP / axum server, WS routes
  - `crates/jyc-inspect/src/auth.rs` — `require_bearer` middleware
  - `crates/jyc-inspect/src/api.rs` — REST handlers
  - `crates/jyc-inspect/src/client.rs` — `reqwest` client
  - `crates/jyc-inspect/src/scoped_ws.rs` — `ScopedWsHandler`
  - `crates/jyc-inspect/src/thread_proxy.rs` — `ThreadProxyHandler`
  - `crates/jyc-channels/src/websocket/inbound.rs` — `WebsocketInboundAdapter`
  - `crates/jyc-channels/src/websocket/outbound.rs` — `WebsocketOutboundAdapter` (legacy `reply` event)
- Related docs:
  - `docs/channels/websocket.md` — websocket channel (user-facing)
  - `docs/tools.md` — agent tools
  - `DESIGN.md` — architecture (inspect server section)
  - `config.example.toml` — `[inspect]` config block
