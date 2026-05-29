# WeCom (企业微信) Channel

WeCom (WeChat Work / 企业微信) Bot channel implementation for JYC.

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

┌─────────────────┐     POST webhook URL           ┌──────────────────┐
│  WeCom Bot API  │ ◀──────────────────────────   │  Outbound        │
│  (qyapi.weixin) │     {"msgtype":"text",...}    │  Adapter         │
└─────────────────┘                                └──────────────────┘
```

### Key Design Decisions

- **Shared HTTP Server**: All WeCom channels share a single axum HTTP server (方案B).
  Configured via `[wecom].bind_addr` (default: `127.0.0.1:10001`).
- **Path-based Routing**: Each channel registers at `/webhook/{channel_name}`.
- **One Bot = One Thread**: Similar to WeChat, each WeCom Bot maps to a single thread.
- **Stateless Outbound**: Outbound messages are standalone HTTP POST requests to the Bot webhook URL.

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
webhook_url = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=xxx-xxx"

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
| `webhook_url` | Yes | Bot webhook URL for sending messages |
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
3. Routes to the channel's message handler
4. Returns 200 OK

## Message Types

### Outbound: Text

```json
{
  "msgtype": "text",
  "text": {
    "content": "Hello World"
  }
}
```

### Outbound: Markdown

```json
{
  "msgtype": "markdown",
  "markdown": {
    "content": "# Title\n\n**bold** text"
  }
}
```

The adapter auto-detects markdown content by checking for code blocks (` ``` `), bold (`**`), headings (`##`), tables (`|`), task lists (`- [`), and images (`![`).

## Testing

```bash
# Test WeCom crypto module
cargo test -p jyc-channels wecom::crypto

# Test WeCom server module
cargo test -p jyc-channels wecom::server

# Test WeCom inbound adapter
cargo test -p jyc-channels wecom::inbound

# Test WeCom outbound adapter
cargo test -p jyc-channels wecom::outbound

# Test configuration validation
cargo test -p jyc-types wecom

# Run all wecom tests
cargo test -p jyc-channels wecom
```

## References

- [WeCom Bot Message Types](https://developer.work.weixin.qq.com/document/path/91770)
- [WeCom Callback Protocol](https://developer.work.weixin.qq.com/document/path/90968)
