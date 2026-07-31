# JYC API Reference

This document describes the two real-time protocols exposed by the JYC
monitor process:

- **§2 Inspect TCP/JSON API** — line-delimited JSON for state queries and
  REST-style mutations (create thread, reload config, list patterns).
- **§3 WebSocket API** — upgrade on the same TCP port for live chat and
  the activity / thinking / processing event stream.

Both protocols ride on **the same TCP listener** (default
`127.0.0.1:9876`). The server inspects the first byte of each new
connection to dispatch: lines starting with `G` (i.e. `GET … HTTP/1.x`)
are routed as WebSocket upgrade requests; everything else is handled as
**a custom line-delimited JSON protocol** (see §2 for why this is not
HTTP and not REST).

> **Source of truth.** The protocol types live in
> `crates/jyc-types/src/inspect.rs`, the server in
> `crates/jyc-inspect/src/server.rs`, and the Rust client in
> `crates/jyc-inspect/src/client.rs`. This document is a derivative —
> if the two disagree, the code wins. Please file an issue or PR to fix
> the doc.

---

## §1 Overview

### 1.1 Process model

```
┌─────────────────────────────────────────────────────────────┐
│  jyc serve                                                  │
│  ┌──────────────────────────────────────────────────────┐  │
│  │  InspectServer  (TCP, default 127.0.0.1:9876)        │  │
│  │  • Inspect first byte of each connection              │  │
│  │    → 'G' (GET)  : WebSocket upgrade on /ws            │  │
│  │    → else        : line-delimited JSON request/resp   │  │
│  └──────┬───────────────────────────────┬──────────────┘  │
│         │                               │                 │
│  ┌──────▼────────────────────┐  ┌──────▼────────────────┐ │
│  │  WebsocketHandler         │  │  handle_request       │ │
│  │  (per channel)            │  │  (7 methods)          │ │
│  │  • WebsocketInboundAdapter│  │  Routes JSON request  │ │
│  │  • ThreadProxyHandler     │  │  to a typed handler   │ │
│  │  • ScopedWsHandler        │  │  and serializes the   │ │
│  │    (URL-scoped wrapper)   │  │  InspectResponse      │ │
│  └───────────────────────────┘  └───────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
                ▲                          ▲
                │ JSON line                │ JSON line
                │                          │
        ┌───────┴────────┐         ┌───────┴────────┐
        │  jyc dashboard │         │  Third-party   │
        │  (TUI client)  │         │  monitoring    │
        └────────────────┘         └────────────────┘
                ▲
                │ WebSocket (text frames)
                │
        ┌───────┴────────┐
        │  Browser / CLI │
        │  chat client   │
        └────────────────┘
```

### 1.2 Configuration

```toml
[inspect]
enabled = true
bind = "127.0.0.1:9876"  # Default; localhost only for security
# auth_token = "..."      # Optional; when set, every JSON request must
                          # carry {"auth_token": "..."} and every WS
                          # upgrade must carry `Authorization: Bearer ...`
```

| Field        | Required | Default              | Notes                                                                              |
|--------------|----------|----------------------|------------------------------------------------------------------------------------|
| `enabled`    | yes      | `false`              | When `false`, the inspect server is not started and the websocket channel refuses to register. |
| `bind`       | no       | `127.0.0.1:9876`     | TCP bind address. Loopback by default; expose externally only with `auth_token`.   |
| `auth_token` | no       | `None`               | When set, both protocols enforce the matching token (see §2.2 and §3.1).            |

### 1.3 Versioning

The protocol is **not yet formally versioned**. The server embeds its
own version in every state payload via `InspectState.version` (compiled
from `CARGO_PKG_VERSION`), so a client can at least detect a server
upgrade after reconnect.

Backward-compatible additive changes are expected (new optional fields
on `InspectRequest`, new `InspectResponse` variants, new optional
fields on `ThreadInfo` / `ThreadSummary`). Backward-incompatible changes
should bump the server version and be called out in `CHANGELOG.md`.

---

## §2 Inspect TCP/JSON API

> **Not HTTP.** Despite living on the same port as the WebSocket
> upgrade endpoint, the JSON inspect API is a **custom
> line-delimited JSON protocol over a raw TCP socket** — it does not
> speak HTTP, has no `Content-Type`, no status codes, no methods like
> `GET`/`POST`, and no URL routing. There is also no "REST" surface
> even though some methods are RPC-style; each request is a single
> JSON object on its own line and is identified by the `method` field
> inside the JSON envelope (not by the URL path). The only path the
> server inspects is on the WebSocket upgrade branch (first byte
> `G`); the JSON branch never parses an HTTP request line.

### 2.1 Wire format

- One TCP connection per client; the client may pipeline multiple
  requests on the same socket.
- The client writes **one JSON object per line**, terminated by `\n`.
- The server responds with **one JSON object per line** in the same
  order.
- There is no keep-alive framing — if the client idles, the socket
  remains open until the server shuts down or the OS drops it.
- All messages are UTF-8. Numbers fit in `u64` / `i64`. Strings are not
  size-limited at the protocol level; field-level limits are documented
  per method.

The Rust client `InspectClient` (see `crates/jyc-inspect/src/client.rs`)
reuses the connection across calls and auto-reconnects on EOF.

### 2.2 Auth

When the server is configured with `auth_token`, every request must
include the token in the `auth_token` field of `InspectRequest`:

```json
{ "method": "get_state", "auth_token": "<the configured token>" }
```

If the token is missing or wrong, the server responds:

```json
{ "type": "error", "error": "auth_failed" }
```

No auth is enforced when the server has no `auth_token` configured.

### 2.3 Request envelope

```rust
pub struct InspectRequest {
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_token: Option<String>,
}
```

| Field        | Type   | Required | Description                                  |
|--------------|--------|----------|----------------------------------------------|
| `method`     | string | yes      | One of the method names listed in §2.5.      |
| `params`     | object | no       | Method-specific parameters (see each method).|
| `auth_token` | string | no       | Required only when the server has `auth_token` configured. |

### 2.4 Response envelope

The response is a tagged union with the discriminator on `type` (snake_case):

```rust
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InspectResponse {
    State(InspectState),
    Overview(InspectOverview),
    Error { error: String },
    ReloadResult { success: bool, message: String },
    ActivityHistory { entries: Vec<ActivityEntry> },
    ChatHistory { entries: Vec<ChatMessageEntry> },
    Patterns { patterns: Vec<String> },
    CreateThreadResult { success: bool, message: String },
}
```

| `type`                  | Returned by                          |
|-------------------------|--------------------------------------|
| `state`                 | `get_state`                          |
| `overview`              | `get_state_overview`                 |
| `error`                 | any method, on failure               |
| `reload_result`         | `reload_config`                      |
| `activity_history`      | `get_thread_activity`                |
| `chat_history`          | `get_thread_chat`                    |
| `patterns`              | `list_patterns`                      |
| `create_thread_result`  | `create_thread`                      |

### 2.5 Methods

#### 2.5.1 `get_state`

Returns the **full** runtime state snapshot, including per-thread
`activity`, `recent_messages`, and `thinking_text`. Used for initial
hydration; the polling loop should prefer `get_state_overview` (smaller
payload).

**Request:**

```json
{ "method": "get_state" }
```

**Response:**

```json
{
  "type": "state",
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

#### 2.5.2 `get_state_overview`

Same shape as `get_state` but threads are returned as `ThreadSummary`
(no `activity`, no `recent_messages`, no `thinking_text`). Designed for
the dashboard's polling loop.

**Request:**

```json
{ "method": "get_state_overview" }
```

**Response:** identical to `get_state` except `"threads": [ ThreadSummary, … ]`.

> The two payloads have the same JSON shape from a client's point of
> view; only the per-thread fields differ.

#### 2.5.3 `get_thread_activity`

Returns recent activity entries (from `.jyc/activity.jsonl`) for one
thread. The server filters out internal-only entries (`is_internal: true`
— e.g. progress heartbeats) before returning.

**Request:**

```json
{
  "method": "get_thread_activity",
  "params": {
    "channel": "feishu_bot",
    "thread":  "issue-42",
    "since":   "2026-07-31T08:00:00Z",   // optional, RFC 3339
    "limit":   180                        // optional, default 180
  }
}
```

| Param     | Type    | Required | Default | Description                                        |
|-----------|---------|----------|---------|----------------------------------------------------|
| `channel` | string  | yes      | —       | Channel name (must match a registered channel).    |
| `thread`  | string  | yes      | —       | Thread name within that channel.                   |
| `since`   | string  | no       | none    | RFC 3339 timestamp; only entries `>= since` are returned. |
| `limit`   | integer | no       | `180`   | Max entries to return.                             |

**Response:**

```json
{
  "type": "activity_history",
  "entries": [
    { "text": "tool execution (12s, 348 chars)", "timestamp": "2026-07-31T08:12:34Z",
      "severity": "info", "id": 142, "is_internal": false }
  ]
}
```

**Errors:** `missing or invalid 'channel' param`, `missing or invalid 'thread' param`, `no thread manager found for channel '<x>'`, `thread '<x>' not found in channel '<y>'`, `failed to load activity: <reason>`.

#### 2.5.4 `get_thread_chat`

Returns recent chat messages (from `chat_history_*.jsonl`) for one
thread. Unlike `get_thread_activity`, this includes **both** user
incoming messages and AI replies — i.e. the conversation transcript.

**Request:**

```json
{
  "method": "get_thread_chat",
  "params": {
    "channel": "feishu_bot",
    "thread":  "issue-42",
    "since":   "2026-07-31T08:00:00Z",
    "limit":   100
  }
}
```

| Param     | Type    | Required | Default | Description                              |
|-----------|---------|----------|---------|------------------------------------------|
| `channel` | string  | yes      | —       | Channel name.                            |
| `thread`  | string  | yes      | —       | Thread name.                             |
| `since`   | string  | no       | none    | RFC 3339 timestamp; only entries `>= since`. |
| `limit`   | integer | no       | `100`   | Max entries to return.                   |

**Response:**

```json
{
  "type": "chat_history",
  "entries": [
    { "sender": "user", "text": "explain this diff",
      "timestamp": "2026-07-31T08:00:11Z", "id": 3 },
    { "sender": "ai",   "text": "This PR fixes ...",
      "timestamp": "2026-07-31T08:00:14Z", "id": 4 }
  ]
}
```

#### 2.5.5 `list_patterns`

Returns the **enabled** pattern names configured for a channel. Used
by the dashboard's `c` key to populate the pattern-select UI.

**Request:**

```json
{
  "method": "list_patterns",
  "params": { "channel": "feishu_bot" }
}
```

**Response:**

```json
{ "type": "patterns", "patterns": ["general", "coding-help"] }
```

> The response is the **list of pattern names only**. To fetch a pattern's
> configuration (model override, skills, etc.) use `get_state_overview`
> and read `ThreadInfo.pattern` for threads that already exist, or read
> the config file directly.

#### 2.5.6 `create_thread`

Registers a new ad-hoc thread for a channel at a custom workspace
path. Replaces the legacy WebSocket `create_thread` command (moved to
REST in this version).

The thread name is validated against path traversal: names containing
`..`, `/`, or `\` are rejected.

**Request:**

```json
{
  "method": "create_thread",
  "params": {
    "channel": "feishu_bot",
    "thread":  "my-adhoc",
    "path":    "/home/me/projects/my-adhoc"
  }
}
```

| Param     | Type   | Required | Description                                  |
|-----------|--------|----------|----------------------------------------------|
| `channel` | string | yes      | Channel name.                                |
| `thread`  | string | yes      | Thread name (no `..`, `/`, or `\` allowed).  |
| `path`    | string | yes      | Absolute filesystem path to use as workspace.|

**Response (success):**

```json
{ "type": "create_thread_result",
  "success": true,
  "message": "thread 'my-adhoc' registered at /home/me/projects/my-adhoc" }
```

**Response (failure):**

```json
{ "type": "create_thread_result",
  "success": false,
  "message": "invalid thread_name: path traversal not allowed" }
```

#### 2.5.7 `reload_config`

Reloads the layered config (global L1 + workdir overlay) from disk,
validates it, atomically swaps the in-memory `AppConfig`, and (if a
reload callback is registered) re-creates channel state.

**Request:**

```json
{ "method": "reload_config" }
```

**Response (success):**

```json
{ "type": "reload_result",
  "success": true,
  "message": "configuration reloaded" }
```

**Response (failure):**

```json
{ "type": "reload_result",
  "success": false,
  "message": "validation failed: <details>" }
```

### 2.6 Error responses

All errors share the same shape:

```json
{ "type": "error", "error": "<human-readable reason>" }
```

Common error messages (non-exhaustive):

| Message                                              | Trigger                                                  |
|------------------------------------------------------|----------------------------------------------------------|
| `auth_failed`                                        | `auth_token` configured on server but missing/wrong.     |
| `missing params`                                     | A method that requires `params` was called without one.  |
| `missing or invalid 'channel' param`                 | `channel` missing or not a string.                       |
| `missing or invalid 'thread' param`                  | `thread` missing or not a string.                        |
| `no thread manager found for channel '<x>'`          | Channel name unknown.                                    |
| `thread '<x>' not found in channel '<y>'`            | Thread name not registered in that channel.              |
| `failed to load activity: <reason>`                  | I/O or parse error reading `.jyc/activity.jsonl`.        |
| `unknown method: <name>`                             | Typo or unsupported method.                              |
| `invalid request: <serde error>`                     | Request body is not valid JSON for `InspectRequest`.     |

### 2.7 Example session (Python)

```python
import json, socket

def call(host: str, port: int, method: str, params=None, token=None):
    req = {"method": method}
    if params is not None: req["params"] = params
    if token is not None:  req["auth_token"] = token
    with socket.create_connection((host, port)) as s:
        s.sendall((json.dumps(req) + "\n").encode())
        buf = b""
        while not buf.endswith(b"\n"):
            buf += s.recv(65536)
        return json.loads(buf)

# Poll the dashboard
state = call("127.0.0.1", 9876, "get_state_overview")
for t in state["threads"]:
    print(t["channel"], t["name"], t["status"])
```

To reuse one connection across calls, keep the socket open and write
multiple JSON lines.

---

## §3 WebSocket API

### 3.1 Connection

Connect to the inspect TCP port with an HTTP/1.1 GET that requests a
WebSocket upgrade. When `auth_token` is configured on the server, the
`Authorization: Bearer …` header (case-insensitive scheme per RFC 7235
§2.1) is required — a failed match returns `HTTP/1.1 401 Unauthorized`
**before** the upgrade is performed:

```
HTTP/1.1 401 Unauthorized
Content-Length: 0
Connection: close
```

### 3.2 URL routes

| Path                          | Handler                                | Use case                                                              |
|-------------------------------|----------------------------------------|-----------------------------------------------------------------------|
| `GET /ws`                     | First registered WS-type channel       | Bare open; the client picks the thread per `message` payload.         |
| `GET /ws/<channel>`           | That channel's `WebsocketInboundAdapter` (must be `type = "websocket"`) | Ad-hoc thread on a websocket channel. |
| `GET /ws/<channel>/<thread>`  | If channel is WS-type → `ScopedWsHandler` (auto-propagates URL thread); else → `ThreadProxyHandler` (works for any channel type). | Thread-scoped chat, the canonical dashboard path. |

> **Naming.** `<channel>` is the configured channel name (e.g.
> `feishu_bot`, `local_dev`), `<thread>` is the thread name (e.g.
> `issue-42`).

### 3.3 Routing rules

1. If the path does not start with `/ws`, the connection is closed (the
   inspect server is not a general-purpose HTTP server).
2. `GET /ws` with **no** registered websocket channel → server closes
   the upgraded socket with a `Close` frame and logs an error.
3. `GET /ws/<channel>` requires that `<channel>` be a websocket-type
   channel; otherwise the connection is closed.
4. `GET /ws/<channel>/<thread>` works for **any** channel type via
   `ThreadProxyHandler`. When the channel is also a websocket channel,
   `ScopedWsHandler` wraps the inbound adapter and the URL thread name
   is propagated as `scoped_thread` so the client can omit `thread`
   from `message` payloads.

### 3.4 Client → Server messages

All client messages are JSON text frames with a `type` discriminator:

| `type`        | Payload                              | Effect                                                                                  |
|---------------|--------------------------------------|-----------------------------------------------------------------------------------------|
| `message`     | `{ "thread"?: "<name>", "text": "..." }` | Send a chat message to the bound thread. `thread` is required on `/ws` and `/ws/<channel>`; optional on `/ws/<channel>/<thread>` (the URL wins). |
| `disconnect`  | `{}`                                 | Close the WebSocket cleanly. The handler breaks the read loop and the runtime sends a WS `Close` frame. |
| `ping`        | `{}`                                 | Application-level keep-alive (no-op). The library also auto-replies to protocol-level `Ping` frames. |

> **Removed.** Earlier protocol versions carried `list_patterns`,
> `subscribe`, and `create_thread` as client messages. These have been
> **replaced by REST endpoints on the inspect server** (§2.5.5,
> §2.5.6). The websocket protocol now carries only the live chat and
> event stream.

#### Example: `WebsocketInboundAdapter` (`/ws/<channel>`)

```json
{ "type": "message", "thread": "general", "text": "hello" }
```

#### Example: `ThreadProxyHandler` (`/ws/<channel>/<thread>`)

```json
{ "type": "message", "text": "hello" }
```

No `thread` field is needed — it's bound from the URL.

### 3.5 Server → Client events

Server → client events are JSON text frames published on a shared
`tokio::sync::broadcast::Sender` and filtered per-connection by
`(channel, thread)` (for `ThreadProxyHandler`) or by `channel` (for
`WebsocketInboundAdapter`).

All events that carry a `(channel, thread)` carry it on the top-level
`channel` / `thread` fields.

| `type`         | Payload                                                            | Emitted when                                                                  | Filter             |
|----------------|--------------------------------------------------------------------|-------------------------------------------------------------------------------|--------------------|
| `activity`     | `{ "channel", "thread", "id", "entry": ActivityEntry }`            | A new entry is appended to `.jyc/activity.jsonl`.                             | by `(channel, thread)` |
| `chat_message` | `{ "channel", "thread", "id", "entry": ChatMessageEntry }`         | An incoming message or a sent reply arrives via the ActivityTracker.          | by `(channel, thread)` |
| `thinking`     | `{ "channel", "thread", "text" }`                                  | The agent publishes a thinking chunk (while the LLM streams reasoning).       | by `(channel, thread)` |
| `processing`   | `{ "channel", "thread", "is_processing": bool, "has_error": bool }` | A thread enters / leaves the Processing state (worker starts, finishes, errors). | by `(channel, thread)` |
| `reply`        | `{ "thread": "<name>", "text": "..." }`                            | A websocket-channel `WebsocketOutboundAdapter` broadcasts an AI reply.         | by `channel` only (no `channel` field on payload) |
| `resync`       | `{ "channel", "thread", "dropped": <n> }`                          | The client's broadcast receiver lagged and missed messages.                   | n/a (only the lagging client) |

#### 3.5.1 `activity`

Mirrors `ActivityEntry` from `InspectState.threads[].activity`.

```json
{
  "type": "activity",
  "channel": "feishu_bot",
  "thread":  "issue-42",
  "id": 142,
  "entry": {
    "text": "tool execution (12s, 348 chars)",
    "timestamp": "2026-07-31T08:12:34Z",
    "severity": "info",
    "id": 142,
    "is_internal": false
  }
}
```

> `is_internal: true` entries (e.g. progress heartbeats) **are
> forwarded on the WebSocket**; they are filtered only on the REST
> `get_thread_activity` path.

#### 3.5.2 `chat_message`

Mirrors `ChatMessageEntry`:

```json
{
  "type": "chat_message",
  "channel": "feishu_bot",
  "thread":  "issue-42",
  "id": 9,
  "entry": {
    "sender": "ai",
    "text": "Here is the fix ...",
    "timestamp": "2026-07-31T08:13:02Z",
    "id": 9
  }
}
```

`sender` is `"user"`, `"ai"`, or a display name supplied by the
channel (e.g. github username).

#### 3.5.3 `thinking`

```json
{ "type": "thinking", "channel": "feishu_bot", "thread": "issue-42",
  "text": "The user wants me to check the lint config ..." }
```

Sent **incrementally** while the LLM streams reasoning text. The field
holds the **full** accumulated thinking text, not a delta — clients
should replace their local copy on each event.

#### 3.5.4 `processing`

```json
{ "type": "processing", "channel": "feishu_bot", "thread": "issue-42",
  "is_processing": true, "has_error": false }
```

Use this to drive a spinner / status indicator in the UI.

#### 3.5.5 `reply`

```json
{ "type": "reply", "thread": "issue-42", "text": "Here is the fix ..." }
```

> **Legacy.** Only emitted on websocket-type channels via
> `WebsocketOutboundAdapter::broadcast_reply`. The dashboard-side
> `ThreadProxyHandler` does **not** emit `reply` events — use
> `chat_message` instead, which has the same data and works on every
> channel type.

#### 3.5.6 `resync`

```json
{ "type": "resync", "channel": "feishu_bot", "thread": "issue-42",
  "dropped": 7 }
```

Emitted by `ThreadProxyHandler` after
`tokio::sync::broadcast::error::RecvError::Lagged(n)`. The client
should drop its in-memory state for `(channel, thread)` and re-fetch
via REST (`get_thread_activity`, `get_thread_chat`).

### 3.6 Sequence diagram

```
client                inspect server            ThreadManager / ActivityTracker
  │                        │                              │
  │ GET /ws/c/t  ────────►│                              │
  │                        │ (upgrade, route to ThreadProxy)│
  │ ◄──── 101 Switching ──│                              │
  │                        │                              │
  │ {type:"message", ...} ─►                              │
  │                        │ enqueue inbound message ────►│
  │                        │                              │
  │                        │ ◄──── ThreadEvent::Started ──┤
  │ {type:"processing",  ─►│                              │
  │   is_processing:true}  │                              │
  │                        │                              │
  │                        │ ◄──── ThreadEvent::Thinking ─┤
  │ {type:"thinking", ...} ►                              │
  │                        │                              │
  │                        │ ◄──── ThreadEvent::Activity ─┤
  │ {type:"activity", ...} ►                              │
  │                        │                              │
  │                        │ ◄──── ThreadEvent::ReplySent ┤
  │ {type:"chat_message", ►                              │
  │   entry.sender:"ai"}   │                              │
  │                        │                              │
  │                        │ ◄──── ThreadEvent::Done ─────┤
  │ {type:"processing",  ─►│                              │
  │   is_processing:false} │                              │
  │                        │                              │
  │ {type:"disconnect"} ──►│                              │
  │ ◄──── Close frame ────│                              │
```

---

## §4 Type Catalog

All shared types live in `crates/jyc-types/src/inspect.rs`. Field
defaults are documented inline; old payloads missing optional fields
still parse.

### 4.1 `InspectState` (response of `get_state`)

| Field            | Type                | Description                                              |
|------------------|---------------------|----------------------------------------------------------|
| `uptime_secs`    | u64                 | Seconds since the monitor process started.               |
| `version`        | string              | Server `CARGO_PKG_VERSION`.                              |
| `channels`       | `Vec<ChannelInfo>`  | All configured channels.                                 |
| `threads`        | `Vec<ThreadInfo>`   | All known threads (incl. activity, messages, thinking).  |
| `stats`          | `GlobalStats`       | Aggregate counters.                                      |
| `commands`       | `Vec<CommandInfo>`  | Available slash commands (server-side registry).         |
| `models`         | `Vec<ModelInfo>`    | Available model identifiers from `[agent].providers`.    |

### 4.2 `InspectOverview` (response of `get_state_overview`)

Identical to `InspectState` except `threads: Vec<ThreadSummary>` — i.e.
no `activity`, no `recent_messages`, no `thinking_text` per thread.

### 4.3 `ChannelInfo`

| Field            | Type   | Description                                       |
|------------------|--------|---------------------------------------------------|
| `name`           | string | Channel name from config (e.g. `"feishu_bot"`).   |
| `channel_type`   | string | One of `"email"`, `"feishu"`, `"github"`, `"websocket"`, … |
| `active_workers` | usize  | Workers holding semaphore permits right now.     |
| `max_concurrent` | usize  | Configured max concurrent workers.                |

### 4.4 `ThreadInfo` vs `ThreadSummary`

Both have the same fields listed below, except `ThreadSummary`
**omits** `activity`, `recent_messages`, and `thinking_text`.

| Field             | Type                | In `ThreadInfo` | In `ThreadSummary` | Description                                   |
|-------------------|---------------------|:---------------:|:------------------:|-----------------------------------------------|
| `name`            | string              | ✓               | ✓                  | Thread name (e.g. `"issue-42"`).              |
| `channel`         | string              | ✓               | ✓                  | Owning channel.                               |
| `pattern`         | string?             | ✓               | ✓                  | Pattern that created the thread.              |
| `status`          | `ThreadStatus`      | ✓               | ✓                  | Current processing status.                    |
| `model`           | string?             | ✓               | ✓                  | AI model in use.                              |
| `mode`            | string?             | ✓               | ✓                  | `"plan"` or `"build"`.                        |
| `input_tokens`    | u64?                | ✓               | ✓                  | Current input tokens in this session.         |
| `max_tokens`      | u64?                | ✓               | ✓                  | Model's `max_input_tokens`.                   |
| `output_tokens`   | u64?                | ✓               | ✓                  | Running total output tokens for this session. |
| `last_active_at`  | string? (RFC 3339)  | ✓               | ✓                  | Last activity timestamp.                      |
| `skills`          | `Vec<string>`       | ✓               | ✓                  | Skills loaded for this thread.                |
| `thread_path`     | `PathBuf?`          | ✓               | ✓                  | Filesystem path of the thread.                |
| `activity`        | `Vec<ActivityEntry>` | ✓              | —                  | Recent activity log (newest first, ≤ 20).     |
| `recent_messages` | `Vec<ChatMessageEntry>` | ✓           | —                  | Recent chat transcript (≤ 50).                |
| `thinking_text`   | string?             | ✓               | —                  | Latest AI thinking text (full, untruncated).  |

### 4.5 `ThreadStatus`

```rust
enum ThreadStatus {
    Queued,            // waiting for semaphore permit
    Processing,        // AI processing active
    Idle,              // worker running, waiting for messages
    WaitingForAnswer,  // question tool waiting for user reply
    Error,             // thread encountered an error
}
```

### 4.6 `ActivityEntry`

| Field         | Type                | Description                                              |
|---------------|---------------------|----------------------------------------------------------|
| `text`        | string              | Human-readable description.                              |
| `timestamp`   | string? (RFC 3339)  | Ordering timestamp.                                      |
| `severity`    | `Severity`          | `info` / `warning` / `error` (default `info`).           |
| `id`          | u64                 | Monotonic per-thread sequence; clients use it to drop dupes after reconnect / Lagged. |
| `is_internal` | bool                | If `true`, do not show in user-facing UIs (progress heartbeats). Default `false`. |

### 4.7 `ChatMessageEntry`

| Field       | Type                | Description                                  |
|-------------|---------------------|----------------------------------------------|
| `sender`    | string              | `"user"`, `"ai"`, or a channel-specific display name. |
| `text`      | string              | Message or reply text.                       |
| `timestamp` | string? (RFC 3339)  | Event timestamp.                             |
| `id`        | u64                 | Monotonic per-thread sequence (same as `ActivityEntry::id`). |

### 4.8 `Severity`

```rust
enum Severity { Info, Warning, Error }   // default: Info
```

### 4.9 `GlobalStats`

| Field               | Type | Description                                       |
|---------------------|------|---------------------------------------------------|
| `active_workers`    | usize | Workers holding semaphore permits.              |
| `total_threads`     | usize | Open threads.                                   |
| `max_concurrent`    | usize | Sum of `max_concurrent` across channels.        |
| `available_workers` | usize | `max_concurrent - active_workers`.              |
| `messages_received` | u64  | Lifetime counter.                                |
| `messages_processed`| u64  | Lifetime counter.                                |
| `errors`            | u64  | Lifetime counter.                                |

### 4.10 `CommandInfo`

| Field         | Type   | Description                          |
|---------------|--------|--------------------------------------|
| `name`        | string | Slash command including the `/` (e.g. `"/model"`). |
| `description` | string | Short description shown in the palette. |

### 4.11 `ModelInfo`

| Field  | Type   | Description                                          |
|--------|--------|------------------------------------------------------|
| `name` | string | Full model identifier (e.g. `"deepseek/deepseek-chat"`). |

---

## §5 Compatibility Notes

- `InspectRequest` accepts missing `params` (back-compat for old
  clients that send `{ "method": "get_state" }` only).
- `ActivityEntry` and `ChatMessageEntry` missing `id` default to `0`;
  missing `is_internal` defaults to `false`.
- `ThreadInfo` / `InspectState` fields added after the initial release
  use `#[serde(default)]` so old log entries still parse.
- The `InspectClient` Rust client reuses a single TCP connection and
  reconnects on EOF (see `client.rs:170-184`).

---

## §6 References

- Source files:
  - `crates/jyc-types/src/inspect.rs` — protocol types
  - `crates/jyc-inspect/src/server.rs` — TCP / WS server
  - `crates/jyc-inspect/src/client.rs` — Rust client
  - `crates/jyc-inspect/src/scoped_ws.rs` — `ScopedWsHandler`
  - `crates/jyc-inspect/src/thread_proxy.rs` — `ThreadProxyHandler`
  - `crates/jyc-channels/src/websocket/inbound.rs` — `WebsocketInboundAdapter`
  - `crates/jyc-channels/src/websocket/outbound.rs` — `WebsocketOutboundAdapter` (legacy `reply` event)
- Related docs:
  - `docs/channels/websocket.md` — websocket channel (user-facing)
  - `docs/tools.md` — agent tools
  - `DESIGN.md` — architecture (inspect server section)
  - `config.example.toml` — `[inspect]` config block
