# Feishu Bot Onboarding Guide

This guide covers setting up a Feishu (飞书/Lark) bot for use with JYC.

## Prerequisites

- A Feishu account with developer access
- Access to the [Feishu Open Platform](https://open.feishu.cn/app) (or [Lark Developer Console](https://open.larksuite.com/app) for international)
- JYC built with the `websocket` feature (default in the current build)
- `protobuf-compiler` installed on the build machine (`apt-get install protobuf-compiler`)

## Step 1: Create a Feishu App

1. Go to [Feishu Open Platform](https://open.feishu.cn/app)
2. Click **Create Custom App** (创建自建应用)
3. Fill in:
   - **App Name**: e.g., "jyc" (this is the display name users see when @-mentioning the bot)
   - **Description**: e.g., "AI Assistant powered by JYC"
4. After creation, note your **App ID** (`cli_xxxxx`) and **App Secret** from the app's **Credentials & Basic Info** page

## Step 2: Configure Permissions (Scopes)

Navigate to **Permissions & Scopes** (权限管理) in your app settings and add the following scopes:

### Required Scopes

| Scope | Purpose | Description |
|-------|---------|-------------|
| `im:message` | Send/receive messages | Base messaging permission |
| `im:message:send_as_bot` | Send messages as bot | Required for the bot to reply in chats |
| `im:message.group_msg` | Read group messages | Receive messages in group chats |
| `im:message.p2p_msg:readonly` | Read DM messages | Receive direct messages to the bot |
| `im:message.group_at_msg:readonly` | Read @-mention messages | Receive messages where the bot is @-mentioned |
| `im:chat:readonly` | Read chat info | Get group chat names (for readable thread directory names) |
| `contact:user.base:readonly` | Read user info | Get user display names (for sender names in prompts) |

After adding the scopes, click **Apply for Permissions** and wait for approval (self-built apps in your own tenant are usually auto-approved).

## Step 3: Enable Bot Capability

1. Go to **App Features** > **Bot** (应用功能 > 机器人)
2. Enable the bot capability
3. The bot will appear in the Feishu contact list after publishing

## Step 4: Subscribe to Events

1. Go to **Event Subscriptions** (事件订阅)
2. **Choose the connection method**: Select **WebSocket** (长连接) — NOT HTTP callback
3. Add the following event:

| Event | Event Type | Description |
|-------|-----------|-------------|
| Receive messages | `im.message.receive_v1` | Triggered when the bot receives a message (group @-mention or DM) |

> **Important**: JYC uses WebSocket (long connection) mode, not HTTP callback mode. This means you do NOT need a public-facing server or webhook URL. JYC connects outbound to Feishu's WebSocket endpoint.

## Step 5: Publish the App

1. Go to **Version Management** (版本管理)
2. Create a new version and submit for review
3. For self-built enterprise apps, approval is typically instant
4. After approval, the bot is available in your Feishu workspace

## Step 6: Add Bot to a Group Chat

1. Open a group chat in Feishu
2. Click the group settings (⚙️) > **Bots** (群机器人)
3. Add your bot (search by the app name, e.g., "jyc")
4. The bot is now a member of the group and will receive @-mention events

## Step 7: Configure JYC

Add the feishu channel to your `config.toml`:

```toml
[channels.feishu_bot]
type = "feishu"
heartbeat_template = "正在处理中，请稍候... (已用时 {elapsed})"

[channels.feishu_bot.feishu]
app_id = "cli_xxxxxxxxxxxxx"       # Your App ID from Step 1
app_secret = "xxxxxxxxxxxxxxxxxxxxxx"  # Your App Secret from Step 1
base_url = "https://open.feishu.cn"    # or "https://open.larksuite.com" for Lark

[channels.feishu_bot.feishu.websocket]
enabled = true
reconnect_delay_secs = 5
max_reconnect_attempts = 10
heartbeat_interval_secs = 30

# Pattern: only process messages where the bot is @-mentioned
[[channels.feishu_bot.patterns]]
name = "mention_bot"
enabled = true

[channels.feishu_bot.patterns.rules]
mentions = ["jyc"]                     # Your bot's display name
```

### Configuration Notes

- **`mentions = ["jyc"]`**: Only messages that @-mention the bot are processed. Without this, the bot would respond to every message in the group.
- **`heartbeat_template`**: Customizes the progress message sent during long AI processing. Use the language your users prefer.
- **`base_url`**: Use `https://open.feishu.cn` for Feishu (China), `https://open.larksuite.com` for Lark (international).
- **Environment variables**: Use `${ENV_VAR}` syntax for secrets:
  ```toml
  app_id = "${FEISHU_APP_ID}"
  app_secret = "${FEISHU_APP_SECRET}"
  ```

## Step 8: Start JYC

```bash
./target/release/jyc monitor --workdir /path/to/data
```

You should see in the logs:

```
INFO  Feishu outbound adapter connected (client + token ready)
INFO  Starting Feishu WebSocket connection...
INFO  Connecting to Feishu WebSocket...
INFO  connected to wss://open.feishu.cn/...
INFO  Feishu WebSocket connected, listening for events
```

## Step 9: Test

1. Open the group chat where you added the bot
2. Type `@jyc Hello, what's the weather today?`
3. The bot should:
   - Receive the message (log: `Feishu message received`)
   - Match the pattern (log: `Pattern matched`)
   - Process via AI
   - Reply in the chat

## Troubleshooting

### Bot doesn't receive messages

- **Check permissions**: Ensure all required scopes are approved in the Feishu developer console
- **Check event subscription**: Ensure `im.message.receive_v1` is subscribed with WebSocket mode
- **Check the app is published**: Unpublished apps cannot receive events
- **Check bot is in the group**: The bot must be added to the group chat

### "No pattern matched" in logs

- **Check mentions config**: If `mentions = ["jyc"]`, the message must contain `@jyc`. The name must match your bot's display name exactly (case-insensitive).
- **Check if bot is @-mentioned**: Simply typing "jyc" without the @ doesn't trigger a mention event

### WebSocket connection fails

- **Check credentials**: Verify `app_id` and `app_secret` are correct
- **Check network**: The server must be able to reach `open.feishu.cn` (or `open.larksuite.com`) on port 443
- **Check logs**: Look for `Feishu WebSocket error` messages

### Reply not delivered (timeout)

- **Check recent JYC version**: The MCP reply tool now writes to disk and the monitor process sends via pre-warmed client. Old versions had timeout issues with cold-start API calls.
- **Check Feishu API access**: The server must be able to reach `open.feishu.cn` for sending messages
- **Delete stale sessions**: Remove `opencode.json` and `.jyc/opencode-session.json` in the thread directory to force a fresh session

### Thread directory names

Thread directories are named using readable chat/user names:
- **Group chat**: `feishu_<chat_name>` (e.g., `feishu_Project Alpha`)
- **Direct message**: `feishu_dm_<sender_name>` (e.g., `feishu_dm_Zhang San`)
- **Fallback** (if name lookup fails): `feishu_chat_<chat_id>`

If you rename the bot's display name in Feishu or change group names, new messages will create new thread directories. Rename old directories manually if you want to preserve conversation history.

## Architecture Overview

```
Feishu Server
     │ WebSocket (protobuf frames, persistent connection)
     ▼
LarkWsClient::open()          ← openlark SDK handles connection, ping/pong, reconnect
     │ mpsc channel (raw JSON payloads)
     ▼
websocket.rs event loop        ← parse JSON → enrich with names → InboundMessage
     │ on_message callback
     ▼
FeishuMatcher → MessageRouter → ThreadManager → OpenCode AI
     │ reply text (stored to reply.md)
     ▼
FeishuOutboundAdapter → FeishuClient.send_text_message()
     │ CreateMessageRequest (IM API, pre-warmed client + cached token)
     ▼
Feishu Server → User sees reply in chat
```
