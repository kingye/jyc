# WeCom KF (Customer Service) Channel

WeCom KF (企业微信客服) channel implementation for JYC, using the **WeCom
Customer Service API** (`kf/sync_msg`, `kf/send_msg`) for both inbound and
outbound messaging.

Unlike the regular [WeCom channel](./wecom.md) (which receives direct XML push
messages via webhook and sends through the external contact API), the KF channel:

- Receives `kf_msg_or_event` event **notifications** via the shared webhook server
- Pulls actual message content via `kf/sync_msg` API (cursor-based incremental sync)
- Sends replies via `kf/send_msg` API
- One thread per customer per KF account

## Architecture

```
┌─────────────────┐     POST /webhook/{name}      ┌──────────────────────────┐
│   WeCom Server  │  (kf_msg_or_event event       │   Shared HTTP Server     │
│   (第三方平台) ──────────────────────────────▶   │   (axum, 127.0.0.1:10001)│
│                 │      notification with         │                          │
│                 │      Token + OpenKfId)         └────────┬─────────────────┘
└─────────────────┘                                        │
                                                            │ event notification
                                                            ▼
                                          ┌────────────────────────────────┐
                                          │  WecomKfInboundAdapter         │
                                          │  1. Extract token & open_kfid  │
                                          │  2. Call sync_msg API          │
                                          │  3. Dedup by msgid             │
                                          │  4. Convert to InboundMessage  │
                                          │  5. Route via MessageRouter    │
                                          └───────┬────────────┬───────────┘
                                                   │            │
                                          ┌────────▼──┐  ┌─────▼──────┐
                                          │ KfCursor  │  │ KfDedup   │
                                          │ Store     │  │ Store     │
                                          └───────────┘  └────────────┘

┌────────────────────────────┐   POST /cgi-bin/kf/send_msg    ┌──────────────────┐
│  WeCom KF API              │ ◀──── ?access_token=...       │  WecomKfOutbound │
│  (qyapi.weixin.cn)         │     {"touser": "...", ...}    │  Adapter         │
└────────────────────────────┘                                └──────────────────┘

┌────────────────────────────┐   POST /cgi-bin/kf/sync_msg    ┌──────────────────┐
│  WeCom KF API              │ ◀──── ?access_token=...       │  KfApiClient     │
│  (qyapi.weixin.cn)         │     {"token": "...", ...}     │  (via Inbound)   │
└────────────────────────────┘                                └──────────────────┘

┌────────────────────────────┐   GET /cgi-bin/gettoken        ┌──────────────────┐
│  WeCom Token API           │ ◀─── ?corpid=...              │  Token Cache     │
│  (qyapi.weixin.cn)         │       &corpsecret=...          │  (shared)        │
└────────────────────────────┘                                └──────────────────┘
```

### Key Design Decisions

- **Shared HTTP Server**: KF channels share the same `WecomWebhookServer` as
  regular WeCom channels (configured via `[wecom].bind_addr`). Differentiated
  by URL path `/webhook/{channel_name}`.
- **Event-Driven Inbound**: The webhook only delivers event notifications
  (`kf_msg_or_event`). Actual message content is pulled via the `kf/sync_msg` API.
- **Cursor-Based Sync**: Uses incremental cursor pagination to avoid re-syncing
  all historical messages on every notification.
- **Cursor Persistence**: Cursors are persisted to a JSON file (configurable via
  `cursor_store_path`). If not configured, cursors are memory-only (lost on
  restart, but dedup prevents double-processing).
- **Message Dedup**: In-memory `HashSet` capped at 10,000 entries prevents
  duplicate processing of overlapping sync results.
- **One Customer Per Thread**: Thread name is `{channel_name}_{open_kfid}_{external_userid}`,
  ensuring each customer conversation maps to a dedicated agent thread.
- **Shared Token Cache**: Uses the same `AccessTokenCache` as the regular WeCom
  channel, shared between inbound and outbound adapters.
- **Pattern Reuse**: Pattern matching delegates to `wecom_match_message` —
  no KF-specific matching rules needed initially.

## Configuration

### Global Server Config

The KF channel reuses the global `[wecom]` HTTP server:

```toml
[wecom]
bind_addr = "127.0.0.1:10001"
```

### Channel Config

```toml
[channels.my_kf_bot]
type = "wecomkf"

[channels.my_kf_bot.wecom_kf]
token = "your_kf_token"
encoding_aes_key = "your_encoding_aes_key_43bytes"
corp_id = "ww1234567890abcdef"
corp_secret = "${WECOM_CORP_SECRET}"
# Optional: filter by KF account IDs (empty = accept all)
open_kf_ids = ["kf1234567890"]
# Optional: cursor persistence file path (JSON)
# If not set, cursors are memory-only (lost on restart)
cursor_store_path = "./data/kf_cursors.json"

[[channels.my_kf_bot.patterns]]
name = "catch_all"

[channels.my_kf_bot.patterns.rules]
keywords = ["help", "问题"]
```

### Configuration Fields

| Field | Required | Description |
|-------|----------|-------------|
| `token` | Yes | Token from WeCom KF callback settings |
| `encoding_aes_key` | Yes | Base64-encoded AES key (43 chars with `=`) |
| `corp_id` | Yes | Enterprise ID / Corp ID |
| `corp_secret` | Yes | Corp secret for access_token (use `${ENV_VAR}` syntax) |
| `open_kf_ids` | No | List of KF account IDs to process (empty = accept all — planned for future use) |
| `cursor_store_path` | No | File path for cursor persistence JSON file |

## Webhook Protocol

The KF channel reuses the same webhook infrastructure as the regular WeCom channel.

### URL Verification (GET)

Same as regular WeCom — WeCom sends a GET with `msg_signature`, `timestamp`,
`nonce`, and `echostr` to verify the URL.

### Event Notification (POST)

WeCom sends a POST request with XML body containing a `kf_msg_or_event` event:

```xml
<xml>
  <ToUserName><![CDATA[ww123456]]></ToUserName>
  <FromUserName><![CDATA[KF_EVENT]]></FromUserName>
  <CreateTime>1700000000</CreateTime>
  <MsgType><![CDATA[event]]></MsgType>
  <Event><![CDATA[kf_msg_or_event]]></Event>
  <Token><![CDATA[xxxxxx]]></Token>
  <OpenKfId><![CDATA[kf1234567]]></OpenKfId>
</xml>
```

The inbound adapter:
1. Verifies `MsgType == "event"` (non-event messages are skipped)
2. Extracts `Token` and `OpenKfId` from the parsed XML
3. Calls `kf/sync_msg` API with the current cursor to pull new messages
4. Deduplicates by `msgid`
5. Converts each message to `InboundMessage` and routes via `MessageRouter`

### Parsed Event Fields

| Field | Source | Description |
|-------|--------|-------------|
| `msg_type` | `<MsgType>` | Always `"event"` for KF notifications |
| `token` | `<Token>` | Token from the event (used for sync_msg API call) |
| `open_kfid` | `<OpenKfId>` | KF account ID that received the message |
| `from_user` | `<FromUserName>` | Usually `"KF_EVENT"` for event notifications |
| `content` | `<Content>` | Empty for event notifications |
| `chat_id` | `<ChatId>` | Empty for KF events |
| `msg_id` | `<MsgId>` | Empty for KF events |
| `create_time` | `<CreateTime>` | Event creation timestamp |

## Inbound: KF Sync Message API

After receiving a `kf_msg_or_event` notification, the inbound adapter calls:

```
POST /cgi-bin/kf/sync_msg?access_token={token}
```

### Request Body

```json
{
  "token": "xxxxxx",
  "cursor": "next_cursor_from_previous_sync",
  "open_kfid": "kf1234567",
  "limit": 100
}
```

### Response

```json
{
  "errcode": 0,
  "errmsg": "ok",
  "next_cursor": "next_cursor_xyz",
  "has_more": 1,
  "msg_list": [
    {
      "msgid": "msg_001",
      "open_kfid": "kf1234567",
      "external_userid": "user123",
      "send_time": 1700000000,
      "msgtype": "text",
      "text": {
        "content": "Hello, support!"
      }
    }
  ]
}
```

The adapter loops until `has_more` is `0`, saving the cursor after each page.

## Outbound: KF Send Message API

Outbound messages are sent via:

```
POST /cgi-bin/kf/send_msg?access_token={token}
```

### Request Body

```json
{
  "touser": "user123",
  "open_kfid": "kf1234567",
  "msgtype": "text",
  "text": {
    "content": "Hello, how can I help you?"
  }
}
```

The adapter always sends as `"text"` since the KF `send_msg` API does
not support the `"markdown"` type.

### Alert Format

Alerts use the recipient format `wecomkf:{open_kfid}:{external_userid}` and
are sent as text with the subject prefixed by `## `:

```json
{
  "touser": "user123",
  "open_kfid": "kf1234567",
  "msgtype": "text",
  "text": {
    "content": "## Alert Title\n\nAlert body content"
  }
}
```

### Proactive Messaging (`send_message`)

The `send_message` method (renamed from `send_alert`) sends proactive messages
to any WeCom KF customer. It is used by the `jyc_send_message` MCP tool for
out-of-thread notifications.

**Recipient format**: `wecomkf:{open_kfid}:{external_userid}`

Example:
```
wecomkf:kf001:wmE8OcHAAA...
```

The adapter parses this format, extracts `open_kfid` and `external_userid`,
and sends via the `kf/send_msg` API. Subject is optional for WeCom KF (ignored
since KF messages have no subject line).

## Thread Naming

Threads are named using the `open_kfid` and `external_userid` fields from
the synced message metadata:

```
{channel_name}_{sanitized_open_kfid}_{sanitized_external_userid}
```

For example, a channel named `my_kf_bot` receiving a message from customer
`user123` through KF account `kf001` will produce thread name
`my_kf_bot_kf001_user123`.

This ensures:
- One thread per customer per KF account
- Clean isolation between different customers
- Consistent naming with the rest of the JYC channel ecosystem

## Thread Persistence (`thread.json`)

WeCom KF threads persist customer metadata in `.jyc/thread.json` within each
thread directory:

```json
{
  "channel_type": "wecomkf",
  "version": 1,
  "data": {
    "external_userid": "wmE8OcHAAA...",
    "user_name": "Alice",
    "open_kfid": "kf001",
    "first_message_at": "2026-05-31T12:34:56Z"
  }
}
```

**Fields:**

| Field | Description |
|-------|-------------|
| `external_userid` | WeCom external user ID (unique per customer) |
| `user_name` | Display name fetched from `externalcontact/get` API (may be empty if 48002 permission error) |
| `open_kfid` | KF account ID that received the message |
| `first_message_at` | ISO 8601 timestamp of the first message in this thread |

**Usage:**
- Written on first message to a new thread
- Read by `chat_log_store.rs` for `user_name` fallback when building chat history
- Enables human-readable thread names even when `externalcontact/get` fails (48002)

## Cursor and Dedup

### Cursor Store

The `KfCursorStore` persists the last sync cursor for each `open_kfid` to
a JSON file. On startup, cursors are loaded from disk, allowing the adapter
to resume syncing from where it left off.

When `cursor_store_path` is not configured, cursors are kept in memory only.
The dedup store prevents double-processing of messages that are re-synced
after a restart.

### Dedup Store

The `KfDedupStore` maintains an in-memory set of seen `msgid` values,
capped at 10,000 entries. Older entries are evicted (FIFO) when the
limit is exceeded.

## Testing

```bash
# Test KF API client (payload building, response deserialization)
cargo test -p jyc-channels wecom::kf_client

# Test KF cursor store (get/set/persist/load)
cargo test -p jyc-channels wecom::kf_cursor

# Test KF dedup store (dedup, eviction)
cargo test -p jyc-channels wecom::kf_dedup

# Test KF inbound adapter (thread name derivation)
cargo test -p jyc-channels wecom::kf_inbound

# Test KF outbound adapter (payload building, channel type)
cargo test -p jyc-channels wecom::kf_outbound

# Test KF config deserialization
cargo test -p jyc-types wecom_kf_config

# Test extended XML parsing (KF event XML)
cargo test -p jyc-channels wecom::server

# Run all wecom tests (includes KF)
cargo test -p jyc-channels wecom
```

## References

- [WeCom KF Development Guide](https://developer.work.weixin.qq.com/document/path/94677)
- [WeCom KF sync_msg API](https://developer.work.weixin.qq.com/document/path/94681)
- [WeCom KF send_msg API](https://developer.work.weixin.qq.com/document/path/94682)
- [WeCom Callback Protocol](https://developer.work.weixin.qq.com/document/path/90968)
- [WeCom gettoken API](https://developer.work.weixin.qq.com/document/path/91039)
