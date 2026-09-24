# WebSocket Channel

The `websocket` channel runs a WebSocket server inside `jyc serve` for interactive terminal-based AI interaction via `jyc dashboard`.

## Overview

Unlike the old standalone `jyc local` command, the websocket channel is a first-class channel type that runs inside the serve process alongside other channels (email, GitHub, etc.). Multiple dashboard clients can connect simultaneously and chat via the interactive chat pane.

**Key characteristics:**
- **Runs inside `jyc serve`** — no separate process needed
- **Multi-client support** — multiple dashboard clients via `tokio::sync::broadcast`
- **Real-time bidirectional chat** — type messages and see AI replies stream in the dashboard
- **Pattern-based topic selection** — patterns serve as entry points for conversations
- **Supports all agent features** — skills, MCP tools, model overrides work normally

## Configuration

```toml
[channels.my_ws]
type = "websocket"

# Optional: override model for this channel
# model = "anthropic/claude-opus-4-6"
# small_model = "deepseek/deepseek-v4-flash"

# Define patterns for the chat pane (first enabled is the default)
[[channels.my_ws.patterns]]
name = "general"
enabled = true
```

### Required Fields

| Field | Description |
|-------|-------------|
| `type` | Must be `"websocket"` |

### Optional Fields

| Field | Description |
|-------|-------------|
| `model` | Per-channel model override |
| `small_model` | Per-channel small model override |
| `patterns` | Pattern rules (first enabled pattern is the default) |

### Prerequisites

The websocket channel requires the inspect server to be enabled:

```toml
[inspect]
enabled = true
bind = "127.0.0.1:9876"
```

The WebSocket handler rides on the same port as the inspect server. Dashboard clients connect to `ws://<inspect_addr>/ws`.

## Usage

1. Start the server with a websocket channel configured:

```bash
jyc serve --workdir /path/to/data
```

2. Open the dashboard in another terminal:

```bash
jyc dashboard --workdir /path/to/data
```

Or create an ad-hoc topic directly from the CLI:

```bash
cd /path/to/project
jyc open --workdir /path/to/data
```

The `open` command creates a websocket topic named after the current
folder and opens it in chat mode. Use `-t/--topic`, `-p/--path`, and
`-c/--channel` to override the defaults.

3. Press `c` to open the chat pane:
   - Select a pattern with `↑/↓` + `Enter`
   - Type a message and press `Enter` to send (`Shift+Enter` / `Alt+Enter` inserts a newline)
   - Press `Ctrl+P` to open the command palette and choose `open dashboard` to close the chat (this also works at pattern selection)

### Chat Pane Controls

The chat input is a multi-line text editor (via
[ratatui-textarea](https://docs.rs/ratatui-textarea)) with soft wrapping,
undo/redo, and standard readline-style editing keys.

| Key | Action |
|-----|--------|
| `c` | Open chat pane (from topic list) |
| `↑` / `↓` or `j` / `k` | Select pattern (pattern select); move the message cursor (message-area focus — the view scrolls only once the cursor hits its top or bottom row); move the text cursor / recall history (input, when empty) |
| `J` / `K` or `Shift+↑` / `Shift+↓` | With message-area focus: open a selection at the cursor and extend it by that many rows; every movement afterwards (with or without Shift) keeps growing it |
| `gg` / `G` | Jump to top / bottom of the focused pane; in the message pane the cursor goes to the first / last row with the view, taking an open selection with it |
| `Enter` | Select pattern / send message (chat input) |
| `Shift+Enter` / `Alt+Enter` | Insert a newline in the chat input |
| `/` popup: `Tab` | Complete the highlighted row — writes it into the input followed by one space, which opens the next level (or closes the popup when nothing is left to complete) |
| `/` popup: `Enter` | Sends the command at the first (command) level. Below it `Enter` behaves like `Tab`, so sending a command with arguments takes a second `Enter`; with nothing to select it sends what was typed |
| `Esc` | Message-area focus: drops an open selection first, the next `Esc` goes back to the input. Explorer focus → back to input. Does not close the chat — use the palette (`open dashboard`) |
| `Ctrl+P` | Open the leader-key popup: navigation (`open dashboard`, `new chat`, `reload config`, `quit`), pane actions (`z` zen, `e` explorer, `a` activity, `s` status bar, `i` topic info, `o` editor, scroll), and `toggle mouse` (flips terminal mouse capture for the chat message area; off restores tmux/terminal-native text selection) |
| `Ctrl+P` → `o` | Open `$VISUAL` / `$EDITOR` (fallback: `vi`) to edit the chat input |
| `Ctrl+P` → `/` | Open the `/` command popup (same as typing `/` in an empty input, but works from any focus) |
| `Tab` | Cycle focus: Input → Message area → Info pane → Activity pane → Explorer pane (each skipped when hidden) |
| `PgUp` / `PgDn` (or `Ctrl+B` / `Ctrl+F`) | Scroll focused pane. In the message pane the cursor travels with the view, so it keeps the screen row it was on |
| Digits before a movement or yank key | Count rows: `5j` moves five, `y3y` copies three, `20k` moves twenty up (capped at 999) |
| `yy` / `y3y` / `3yy` | Copy that many transcript rows, starting at the message cursor, to the clipboard |
| `y` with a selection | Copy every selected row (both ends included), then drop the selection and move the cursor to its first row |
| `Ctrl+P` → `a` | Toggle activity pane: hidden ↔ bottom 20% |
| `Ctrl+P` → `e` | Toggle the topic explorer pane (left side); `Enter` in it switches the chat to the selected topic |
| `Ctrl+P` → `s` | Toggle the bottom status bar |
| `Ctrl+P` → `i` | Toggle the topic info pane (right side) |
| `Ctrl+P` → `c` | Focus the chat message area, which shows the cursor bar (j/k/arrows move it, Shift selects, `y` copies); pressing any other key returns focus to the input (the key itself is consumed) |
| `Ctrl+P` → `z` | Toggle zen mode: snapshot and hide all aux panes (activity, topic info, status bar, explorer); pressing again restores the exact pre-zen state |
| `Ctrl+C` | Cancel current AI processing |
| `Shift+Tab` | Toggle plan / build mode |
| `Ctrl+Q` | Quit the dashboard |

Note: with the input field focused, Up/Down recall sent-message history when
the input is empty; to scroll the conversation, press `Tab` to
focus the message area, then use `↑/↓`/`j/k`, `PgUp/PgDn`, or `gg/G`. Typing
while the message area is focused refocuses the input automatically (the
keypress is consumed). The input
area grows with content from 1 up to 10 text lines. The input prompt shows an
always-visible agent-mode letter chip before `❯ `: `B` (green) for build mode,
`P` (yellow) for plan mode.

The cursor row and the selected rows share one highlight colour — the status line
says how many rows are selected, so a second shade would only be noise. Only a
*selected* row also takes a light foreground (the navy needs it on a light theme);
the cursor row keeps the transcript's own colours, because it is lit while you are
merely reading. Scrolling with a selection open leaves it alone: the view moves
under the selected rows instead of growing them, so looking around cannot change
what `y` will copy. `Esc`, leaving the message area, and switching topics drop the
selection. Exiting differs between the two: `y` puts the cursor back on the first
row of what it copied (linewise vim), while `Esc` leaves it on the row it moved to
— dropping a selection is not a copy, so it does not move you. A topic opens with
the cursor on the last written row of its newest message — for an answered round
that is the `──── 1m ──` rule closing it, one `k` above the reply's text — skipping
the blank spacer and the pending rows below. No bar shows at all while a topic's
messages are still loading.

Many terminals send the same bytes for `Shift+↑` as for `↑`, so `J`/`K` are the
reliable way to start a selection — where the terminal does report Shift on the
arrows, both work.

The `y` keys copy through OSC 52, i.e. the *terminal's* clipboard — so a yank
made inside an SSH session lands where you paste. tmux has to be told to forward
the escape (`set -g set-clipboard on` in `~/.tmux.conf`); a terminal that
implements no OSC 52 accepts the sequence and drops it, and the status line
reports what was taken either way. Drag-selected text is unaffected: `Ctrl+P` →
`toggle mouse` off restores native selection.

The rows are the *rendered* ones, so a long message copies as the several lines
it read as, with its indentation. The live tail — activity progress and the
reply currently streaming — is not part of the transcript and is not copyable;
yanking there says `Nothing to copy`.

### Interface Layout

Normal mode (default):

```
┌────────────────────────┐
│ Channels bar           │
├────────────────────────┤
│ Topics table          │
├────────────────────────┤
│ Detail panel (8 lines) │
├────────────────────────┤
│ Activity log           │
├────────────────────────┤
│ Help bar               │
└────────────────────────┘
```

Chat mode (`c` toggled on). The channel bar is hidden; chat, topic info
pane, activity pane, explorer pane, and status bar are individually
togglable (`Ctrl+P` then `i` / `a` / `e` / `s`). The topic info pane
and status bar are visible by default; zen mode (`Ctrl+P` then `z`)
hides all auxiliary UI and restores the previous state when pressed
again. `Ctrl+P` then `c` focuses the message area for keyboard
scrolling; from any focused pane (message area, topic info, activity,
explorer) pressing any key refocuses the input; the first keypress is
consumed, so no stray characters land in the input. Each chat round has
only a top time rule and a bottom right-aligned duration rule — there
are no side borders or middle dividers.

```
Default (info + status shown):
┌────────────────────────┐
│ Chat conversation      │┌─────────────┐
│ (borderless)           ││ Topic Info │
│                        ││  (20% wide) │
│                        ││             │
└────────────────────────┘└─────────────┘
 Help bar (1 line)

Zen mode (`Ctrl+P` `z`):
┌────────────────────────┐
│ Chat conversation      │
│ (borderless, full size)│
└────────────────────────┘

After `Ctrl+P` `a` (activity 20% bottom):
┌────────────────────────┐
│ Chat conversation      │
├────────────────────────┤
│ Activity log (20%)     │
└────────────────────────┘

After `Ctrl+P` `a` twice (back to hidden):
┌────────────────────────┐
│ Chat conversation      │
│ (borderless, full size)│
└────────────────────────┘
```

## WebSocket Protocol

JSON envelope over WebSocket:

| Direction | Message | Purpose |
|-----------|---------|---------|
| Client→Server | `{"type":"list_patterns"}` | Get available patterns |
| Server→Client | `{"type":"patterns","patterns":["general","coding-help"]}` | Pattern list response |
| Client→Server | `{"type":"subscribe","topic":"general"}` | Subscribe to topic replies |
| Client→Server | `{"type":"message","topic":"general","text":"hello"}` | Send message |

`message` frames accept two optional fields for channel-native identity:
`sender` (display name, default `"user"`) and `sender_address` (canonical
address, default the connection address). Bridge processes use them to
carry the remote user's identity (e.g. a feishu user name / open_id).
| Server→Client | `{"type":"reply","topic":"general","text":"AI reply..."}` | Broadcast reply |
| Server→Client | `{"type":"question","id":"…","topic":"general","question":"Deploy?","options":["yes","no"],"allow_multiple":false,"position":[2,3],"timeout_seconds":300}` | Interactive `ask_user` question (broadcast channel-wide; filter by `topic`). `allow_multiple` means several options may be picked; `position` is the question's 1-based place inside a multi-question call, `null` when it was asked alone — one `ask_user` call pushes one frame per question, oldest first, and the client answers each under its own `id` |
| Client→Server | `{"type":"question_response","id":"…","choices":["yes"]}` / `{"type":"question_response","id":"…","cancelled":true}` | Answer / dismiss a question (routed to the agent's pending tool call, never enqueued as a topic message). `choices` carries every picked option; the older singular `choice` is still accepted |

`reply` frames may carry an optional `attachments` array when the agent
replied with files. Entries contain `filename`, `content_type`, and
`path` — a percent-encoded relative URL served by the inspect server's
topic-file endpoint. Prepend the inspect base URL to download:

```json
{"type":"reply","topic":"general","text":"done",
 "attachments":[{"filename":"report.pdf","content_type":"application/pdf",
                 "path":"/api/topics/local_dev/general/files/report.pdf"}]}
```

```bash
curl -H 'Authorization: Bearer <token>' \
  'http://127.0.0.1:9876/api/topics/local_dev/general/files/report.pdf'
```

Files are served in place from the topic directory (bearer-gated; paths
under `.jyc/` are rejected). This is separate from the `/exchange/...`
token links, which remain reserved for files the agent explicitly
publishes via `jyc_publish_file`.

Pipe reply forwarders (e.g. feishu patterns with
`pipe = { channel = "local_dev", topic = "..." }`) consume the same
`attachments` array: each file is fetched from the files endpoint,
checked against `[attachments.outbound]`, and re-uploaded to the source
chat (images as image messages, everything else as file messages).

A pipe target has two independent knobs:

- `pattern` — names a pattern on this channel whose config (`topic_path`,
  template, skills, model) applies to the piped message;
- `topic` — the topic name, which may embed the runtime placeholder
  `${msg.chat_name}`, resolved per message from the source channel's
  chat-name metadata (sanitized for filesystem use).

`topic` defaults to `pattern` when omitted, so
`pipe = { channel = "local_dev", pattern = "jyc" }` is shorthand for
`pattern = "jyc", topic = "jyc"`. The full dynamic form
`pipe = { channel = "local_dev", pattern = "group_chat", topic = "${msg.chat_name}" }`
lets one feishu `mentions` pattern route each group chat to its own topic
here while sharing the `group_chat` pattern's config. Unlike load-time
`${ENV_VAR}` expansion, the `msg.` namespace is resolved at runtime per
message. Messages lacking a chat name (e.g. P2P chats) are dropped with a
warning when the placeholder is used; a `pattern` naming no enabled pattern
on this channel falls back to topic-name matching with a warning.

## Architecture

### Process Model

```
┌─────────────────────────────────────────────┐
│  jyc serve                                  │
│  ┌───────────────────────────────────────┐  │
│  │  Inspect Server (dual-protocol)       │  │
│  │  • TCP JSON for state queries         │  │
│  │  • WebSocket upgrade on /ws           │  │
│  │  • Hands WebSocket to handler         │  │
│  └──────────┬────────────────────────────┘  │
│             │                               │
│  ┌──────────▼────────────────────────────┐  │
│  │  WebSocket Inbound Adapter            │  │
│  │  • Handles JSON protocol              │  │
│  │  • Dispatches to MessageRouter        │  │
│  └──────────┬────────────────────────────┘  │
│             │                               │
│  ┌──────────▼────────────────────────────┐  │
│  │  MessageRouter / TopicManager        │  │
│  │  (same as other channels)             │  │
│  └──────────┬────────────────────────────┘  │
│             │                               │
│  ┌──────────▼────────────────────────────┐  │
│  │  WebSocket Outbound Adapter           │  │
│  │  • Broadcasts replies via broadcast   │  │
│  └───────────────────────────────────────┘  │
└─────────────────────────────────────────────┘
         ▲                               │
         │ ws://127.0.0.1:9876/ws      │ broadcast
         │                               ▼
┌─────────────────────────────────────────────┐
│  jyc dashboard                              │
│  • TCP JSON for state queries               │
│  • WebSocket client for chat                │
│  • Receives broadcast replies               │
└─────────────────────────────────────────────┘
```

### Communication Flow

```
Dashboard ──► Inspect Server ──► WebSocket Handler ──► MessageRouter ──► Agent
                                                                   │
Dashboard ◄── WebSocket ◄────── OutboundAdapter ◄──────────────────┘
```

All connected dashboard clients receive broadcast replies via `tokio::sync::broadcast`.

## Topic Naming

WebSocket topic names are derived from the `topic` field in client messages:

```json
{"type":"message","topic":"general","text":"hello"}
{"type":"subscribe","topic":"general"}
```

When a message's `topic` field is non-empty, it is used as the topic name. When empty, the topic name falls back to the channel name (e.g., `my_ws`).

Workspace files are stored under:

```
{workdir}/workspace/{channel_name}/
```

### Multi-Topic Workspace Isolation

Each unique `topic` value creates a separate workspace directory:

```
{workdir}/workspace/{channel_name}/
├── general/                 # topic="general"
│   ├── message_001/
│   └── .jyc/
├── coding/                  # topic="coding"
│   ├── message_001/
│   └── .jyc/
└── review/                  # topic="review"
    └── ...
```

This enables completely isolated conversation contexts, skills, and file systems per topic — different topics within the same websocket channel behave like independent channels. When no `topic` is specified, the channel name is used as fallback (backward compatible).

## Pattern Matching

WebSocket input bypasses complex pattern rules. The matcher always selects the **first enabled pattern** from the channel's pattern list. Patterns serve as entry points for the chat pane — the user explicitly selects which pattern (and thus which configuration) to use.

```toml
# First enabled pattern is the default in the chat pane
[[channels.my_ws.patterns]]
name = "general"
enabled = true
```

## Model Override Resolution

The full resolution chain for websocket channels (highest to lowest priority):

1. Runtime `.jyc/model-override` file
2. Pattern-level `model` / `small_model`
3. Channel-level `model` / `small_model`
4. Global `[agent].model` / `[agent].small_model`

## Comparison with Other Channels

| Feature | `email` | `wecom` | `feishu` | `websocket` |
|---------|---------|---------|----------|-------------|
| External service | IMAP/SMTP server | WeCom server | Feishu server | None |
| Setup complexity | High | High | Medium | Low |
| Real-time | Polling | Webhook/WebSocket | WebSocket | Instant |
| Multi-client | No | No | No | Yes |
| Workspace isolation | Per-topic | Per-topic | Per-topic | Per-topic |

## Workspace Layout

The websocket channel creates a workspace directory:

```
{workdir}/workspace/{channel_name}/
├── message_001/
│   ├── message.json
│   └── reply.json
├── message_002/
│   └── ...
└── .jyc/
    └── ...
```

The workspace is reused across restarts, so conversation history is available to the agent.

## References

- See `crates/jyc-channels/src/websocket/` for implementation
- See `crates/jyc-cli/src/cli/dashboard.rs` for dashboard chat pane
- See `config.example.toml` for configuration example
