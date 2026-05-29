# WeCom (企业微信) Channel

WeCom (WeChat Work / 企业微信) channel implementation for JYC, using the
**external contact (客户群) API** for outbound messaging.

## Architecture

```
┌─────────────────┐     POST /webhook/{name}      ┌──────────────────┐
│   WeCom Server  │ ──────────────────────────▶   │  Shared HTTP     │
│  (第三方平台)    │                               │  Server (axum)   │
│                 │ ◀──────────────────────────   │  127.0.0.1:10001 │
│                 │    200 OK (decrypted echo)    └────────┬─────────┘
└─────────────────┘                                       │
                                                    ┌──────┴──────┐
                                                    │  Inbound    │
                                                    │  Adapter    │
                                                    └──────┬──────┘
                                                           │
                                                    ┌──────┴──────┐
                                                    │  Message    │
                                                    │  Router     │
                                                    └─────────────┘

┌──────────────────────┐   POST /cgi-bin/externalcontact/ ┌──────────────────┐
│  WeCom External API  │ ◀─── /message/send?access_token= │  Outbound        │
│  (qyapi.weixin.cn)   │     {"chat_id":"...",...}       │  Adapter         │
└──────────────────────┘                                  └──────────────────┘

┌──────────────────────┐   GET /cgi-bin/gettoken          ┌──────────────────┐
│  WeCom Token API     │ ◀─── ?corpid=...&corpsecret=...  │  Token Cache     │
└──────────────────────┘   (refreshes before expiry)      └──────────────────┘
```

### Key Design Decisions

- **Shared HTTP Server**: All WeCom channels share a single axum HTTP server (方案B).
  Configured via `[wecom].bind_addr` (default: `127.0.0.1:10001`).
- **Path-based Routing**: Each channel registers at `/webhook/{channel_name}`.
- **One Group = One Thread**: Thread name is derived from `chat_id`
  (`{channel_name}_{sanitized_chat_id}`), ensuring each WeCom chat group maps to a dedicated
  agent thread.
- **Token-based Outbound**: Uses `corp_id` + `corp_secret` to obtain an access_token
  from the WeCom API, then sends messages via the external contact API.
- **Token Caching**: Access tokens are cached in memory with automatic refresh
  5 minutes before expiry.
- **XML Parsing**: Uses simple string extraction (matching `extract_encrypt_from_xml`
  style) — no XML parsing library dependency.

## Configuration

### Global Server Config

```toml
[wecom]
bind_addr = "127.0.0.1:10001"
```

### Channel Config

```toml
[channels.my_bot]
type = "wecom"

[channels.my_bot.wecom]
token = "your_token_from_wecom"
encoding_aes_key = "your_aes_key_43_chars"
corp_id = "ww1234567890abcdef"
corp_secret = "your_corp_secret_value"

[[channels.my_bot.patterns]]
name = "catch_all"

[channels.my_bot.patterns.rules]
keywords = ["help", "问题"]
```

### Configuration Fields

| Field | Required | Description |
|-------|----------|-------------|
| `token` | Yes | Token from WeCom callback settings |
| `encoding_aes_key` | Yes | Base64-encoded AES key (43 chars with `=`) |
| `corp_id` | Yes | Enterprise ID / Corp ID |
| `corp_secret` | Yes | Corp secret for access_token acquisition (use `${ENV_VAR}` syntax) |
| `bind_addr` | No | Global server bind address (default: `127.0.0.1:10001`) |

## Webhook Protocol

### URL Verification (GET)

WeCom sends a GET request with:
- `msg_signature` — SHA1(token + timestamp + nonce + echostr)
- `timestamp` — current timestamp
- `nonce` — random nonce
- `echostr` — encrypted echo string

Response: decrypted `echostr` as plain text (status 200).

### Message Callback (POST)

WeCom sends a POST request with XML body:
```xml
<xml>
  <ToUserName><![CDATA[ww123456]]></ToUserName>
  <Encrypt><![CDATA[base64_encrypted_content]]></Encrypt>
  <AgentID><![CDATA[1000002]]></AgentID>
</xml>
```

The server:
1. Verifies SHA1 signature
2. Decrypts the AES-256-CBC encrypted content
3. Parses the decrypted XML into structured fields (Content, FromUserName, ChatId, MsgType, etc.)
4. Routes to the channel's message handler
5. Returns 200 OK

### Parsed Message Fields

After decryption, the inner XML is parsed into the following fields:

| Field | Source | Description |
|-------|--------|-------------|
| `content` | `<Content>` | Message text content |
| `from_user` | `<FromUserName>` | Sender's UserName |
| `chat_id` | `<ChatId>` | Group chat room ID (used for outbound routing) |
| `msg_type` | `<MsgType>` | Message type (e.g., "text", "image") |
| `msg_id` | `<MsgId>` | Unique message ID |
| `create_time` | `<CreateTime>` | Message creation timestamp |

## Outbound: External Contact API

Outbound messages are sent via the WeCom External Contact API:
`POST /cgi-bin/externalcontact/message/send?access_token={token}`

### Authentication

1. On startup, the adapter calls `GET /cgi-bin/gettoken?corpid={corp_id}&corpsecret={corp_secret}`
2. The access_token is cached in memory
3. Tokens are automatically refreshed 5 minutes before expiry (default: 7200 seconds)

### Outbound: Text

```json
{
  "chat_id": "wr9876543210",
  "msgtype": "text",
  "text": {
    "content": "Hello World"
  }
}
```

### Outbound: Markdown

```json
{
  "chat_id": "wr9876543210",
  "msgtype": "markdown",
  "markdown": {
    "content": "# Title\n\n**bold** text"
  }
}
```

The adapter auto-detects markdown content by checking for code blocks (` ``` `),
bold (`**`), headings (`##`), tables (`|`), task lists (`- [`), and images (`![`).

## Thread Naming

Threads are named using the `chat_id` field combined with the `channel_name` from the inbound message metadata:

```
{channel_name}_{sanitized_chat_id}
```

For example, a channel named `my_bot` receiving a message from chat group `wrOgQhDgA...` will produce thread name `my_bot_wrOgQhDgA...`.

This ensures:
- One thread per channel+group pair (consistent with the "通道 + 群" design)
- Consistency with Feishu's `{channel_name}_{chat_id}` naming pattern
- Proper isolation between different channels and group conversations

## Testing

```bash
# Test WeCom crypto module
cargo test -p jyc-channels wecom::crypto

# Test WeCom server module (including XML parsing)
cargo test -p jyc-channels wecom::server

# Test WeCom inbound adapter
cargo test -p jyc-channels wecom::inbound

# Test WeCom outbound adapter (including token cache)
cargo test -p jyc-channels wecom::outbound

# Test configuration validation
cargo test -p jyc-types wecom

# Run all wecom tests
cargo test -p jyc-channels wecom
```

## References

- [WeCom External Contact Message API](https://developer.work.weixin.qq.com/document/path/92135)
- [WeCom Callback Protocol](https://developer.work.weixin.qq.com/document/path/90968)
- [WeCom gettoken API](https://developer.work.weixin.qq.com/document/path/91039)
