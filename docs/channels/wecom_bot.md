# WeCom Smart Robot (wecom_bot) Channel

The `wecom_bot` channel connects JYC to **WeCom Smart Robot** (企业微信智能机器人)
via WebSocket long connection.

## Overview

Unlike the existing `wecom` channel (HTTP callback with AES encryption) or `wecomkf`
(WeCom Customer Service sync API), `wecom_bot` uses a persistent WebSocket connection
to `wss://openws.work.weixin.qq.com` for real-time bidirectional messaging.

**Key characteristics:**
- **No AES encryption/decryption** required (unlike `wecom` HTTP callbacks)
- **WebSocket long connection** with auto-reconnect and heartbeat
- **Streaming replies** supported (`msgtype: "stream"`)
- **Proactive messages** can be sent after initial user interaction
- **Outbound attachments** supported: file, image, voice, and video upload via WebSocket media protocol

## Configuration

```toml
[channels.my_wecom_bot]
type = "wecom_bot"

[channels.my_wecom_bot.wecom_bot]
bot_id = "your_bot_id"
secret = "${WECOM_BOT_SECRET}"

# Optional settings (defaults shown):
# ws_url = "wss://openws.work.weixin.qq.com"
# heartbeat_interval_secs = 30
# reconnect_delay_secs = 5
# max_reconnect_attempts = 10
# auto_reconnect = true
```

### Getting Credentials

1. Open **WeCom Admin Console** → **Customer Contact** → **Smart Robot**
2. Create or select your smart robot
3. Copy the **Bot ID** (e.g., `aibot_xxxxx`)
4. Go to **"My smart robot"** → **"Long connection secret"**
5. Copy the **secret** (NOT corp_secret — this is a separate secret for WebSocket)

## Protocol Reference

### Connection Flow

```
Client                                  Server
  | ── TCP + TLS ──► wss://openws.work.weixin.qq.com
  | ── {cmd:"aibot_subscribe", bot_id, secret} ──►
  | ◄──────────────── {cmd:"pong"} ─────────────── (heartbeat)
  | ◄── {cmd:"aibot_msg_callback", msgid, ...} ─── (message)
  | ◄── {cmd:"aibot_event_callback", event, ...} ─ (event)
  | ── {cmd:"aibot_respond_msg", msgtype, ...} ──► (reply)
```

### Heartbeat

Client sends `ping` every `heartbeat_interval_secs` (default: 30s).
Server responds with `pong`. If no response after 2 missed pings,
the client reconnects.

### Receiving Messages

Messages arrive as `aibot_msg_callback` with the following structure:

```json
{
  "cmd": "aibot_msg_callback",
  "msgid": "msg_xxx",
  "aibotid": "bot_xxx",
  "chatid": "chat_xxx",
  "chattype": "single|groupchat",
  "from": {"userid": "user_xxx"},
  "msgtime": 1704067200000,
  "msgtype": "text|image|mixed|voice|file|video",
  "text": {"content": "Hello bot"},
  "req_id": "req_xxx"
}
```

### Sending Replies

Replies use `aibot_respond_msg` and **must echo the `req_id`** from the callback:

```json
{
  "cmd": "aibot_respond_msg",
  "msgtype": "text|markdown|stream",
  "text": {"content": "Hello back"},
  "req_id": "req_xxx"
}
```

### Streaming Replies

For streaming, use `msgtype: "stream"`:

```json
{
  "cmd": "aibot_respond_msg",
  "msgtype": "stream",
  "stream": {
    "id": "stream_123",
    "content": "Partial content...",
    "finish": false
  },
  "req_id": "req_xxx"
}
```

- First chunk: set `stream.id` and `finish: false`
- Subsequent chunks: reuse same `stream.id`, update `content`
- Final chunk: set `finish: true`
- **10-minute timeout** from first chunk

### Proactive Messages

After a user has messaged the bot, the bot can proactively send messages using
`aibot_send_msg`:

```json
{
  "cmd": "aibot_send_msg",
  "chatid": "chat_xxx",
  "msgtype": "markdown",
  "markdown": {"content": "**Alert:** ..."},
  "req_id": "req_new"
}
```

### Sending Attachments in Replies

When the AI reply includes attachments, each attachment is uploaded over the
same WebSocket and sent as a separate message after the text reply:

1. Text reply is sent as a streaming message (`msgtype: "stream"`, `finish: true`).
2. Each attachment is uploaded via:
   - `aibot_upload_media_init` → returns `upload_id`
   - `aibot_upload_media_chunk` → base64-encoded chunks (≤ 512 KiB each)
   - `aibot_upload_media_finish` → returns `media_id`
3. The attachment is sent with `aibot_respond_msg` using the original `req_id`:

```json
{
  "cmd": "aibot_respond_msg",
  "headers": {"req_id": "req_xxx"},
  "body": {
    "msgtype": "file",
    "file": {"media_id": "MEDIA_ID"}
  }
}
```

Supported mappings:

| File extension | WeCom msgtype | Size limit |
|----------------|---------------|------------|
| png, jpg, jpeg, gif | `image` | 10 MB |
| amr | `voice` | 2 MB |
| mp4 | `video` | 10 MB |
| pdf, doc, xlsx, ppt, csv, etc. | `file` | 20 MB |

Configuration uses the generic `[attachments.outbound]` settings (same as Feishu
and email): `enabled`, `allowed_extensions`, `max_file_size`, `max_per_message`.

## Thread Naming

- **Single chat**: `bot-{userid}`
- **Group chat**: `bot-{chatid}`

## Limitations

- **24-hour reply window**: messages older than 24h cannot be replied to
- **Rate limits**: 30 messages/minute, 1000 messages/hour
- **Proactive push**: user must message the bot first before proactive `aibot_send_msg` works
- **Media upload**: outbound attachments (files, images, etc.) are uploaded over the
  WebSocket and sent as separate messages.
- **Media download**: image/file/video URLs returned in messages are downloaded and
  decrypted using AES-256-CBC with the per-URL `aeskey`.

## Comparison with Other WeCom Channels

| Feature | `wecom` | `wecomkf` | `wecom_bot` |
|---------|---------|-----------|-------------|
| Transport | HTTP callback | HTTP API | WebSocket |
| Encryption | AES-CBC required | Token-based | None |
| Real-time | Yes (webhook) | Polling | Yes (WebSocket) |
| Streaming | No | No | Yes |
| Outbound attachments | No | No | Yes |
| Config | token + aes_key | corp_id + secret | bot_id + secret |

## TODO: Per-Group Pattern Routing

To route different group chats to different agents/templates, we need a way to
match messages by their group identity. Since WeCom Bot messages only carry an
opaque `chatid` (not a human-readable `chat_name`), several approaches were
considered:

### Option 1: `chat_id` Pattern Rule (Recommended)

Add a new `chat_id` rule to `PatternRules` that matches `message.metadata["chatid"]`.

```toml
[[channels.bot.patterns]]
name = "ops_group"
template = "ops_agent"

[channels.bot.patterns.rules]
chat_id = ["wrj7DwDgAA-xxxxxxxxxx"]
```

- **Pros**: Simple, no external API calls, deterministic
- **Cons**: `chatid` is opaque; users must extract it from logs first

### Option 2: `chat_name` Pattern Rule

Query WeCom API (`externalcontact/group_chat/get`) to resolve `chatid → name`,
then match on `chat_name` like Feishu does.

- **Pros**: Human-readable configuration
- **Cons**: Requires `access_token` (WeCom Bot's long-connection `secret` cannot
  be used for general API calls); adds latency and caching complexity

### Option 3: Keyword-Based Routing

Users include a keyword trigger in their messages:

```toml
[channels.bot.patterns.rules]
keywords = ["#ops"]
```

- **Pros**: No code changes needed
- **Cons**: Requires users to remember keywords; cannot enforce per-group routing

### Option 4: First-Message Auto-Registration

On first message from a new `chatid`, log the mapping and optionally prompt the
user to assign a friendly name in a local mapping file.

- **Pros**: Bridges the gap between opaque IDs and human names
- **Cons**: More complex; needs persistent mapping storage

### Decision

**Option 1 (`chat_id` rule) is the most pragmatic for now.** Users can obtain
their `chatid` from JYC logs and configure patterns directly. Options 2–4 can be
revisited if demand grows.

## References

- Doc 101463: Smart Robot WebSocket Long Connection
- Doc 100719: Receiving Messages (JSON format)
- Doc 101031: Passive Reply Messages (including streaming)
