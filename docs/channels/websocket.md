# WebSocket Channel

The `websocket` channel runs a WebSocket server inside `jyc serve` for interactive terminal-based AI interaction via `jyc dashboard`.

## Overview

Unlike the old standalone `jyc local` command, the websocket channel is a first-class channel type that runs inside the serve process alongside other channels (email, GitHub, etc.). Multiple dashboard clients can connect simultaneously and chat via the interactive chat pane.

**Key characteristics:**
- **Runs inside `jyc serve`** — no separate process needed
- **Multi-client support** — multiple dashboard clients via `tokio::sync::broadcast`
- **Real-time bidirectional chat** — type messages and see AI replies stream in the dashboard
- **Pattern-based thread selection** — patterns serve as entry points for conversations
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

Or create an ad-hoc thread directly from the CLI:

```bash
cd /path/to/project
jyc open --workdir /path/to/data
```

The `open` command creates a websocket thread named after the current
folder and opens it in chat mode. Use `-t/--thread`, `-p/--path`, and
`-c/--channel` to override the defaults.

3. Press `c` to open the chat pane:
   - Select a pattern with `↑/↓` + `Enter`
   - Type a message and press `Shift+Enter` / `Alt+Enter` to send (`Enter` inserts a newline); in Normal (vi) mode `Enter` sends
   - Press `Esc` to enter Normal (vi) mode
   - Press `Ctrl+P` to open the command palette and choose `open dashboard` to close the chat (this also works at pattern selection)

### Chat Pane Controls

The chat input is a vi-style modal editor (via [edtui](https://docs.rs/edtui))
with Insert/Normal/Visual modes and a mode indicator in its status line. It
starts in Insert mode.

| Key | Action |
|-----|--------|
| `c` | Open chat pane (from thread list) |
| `↑` / `↓` or `j` / `k` | Select pattern (pattern select); scroll messages (message-area focus); move cursor (Insert/Visual mode) |
| `gg` / `G` | Jump to top / bottom of the focused pane (message-area focus) |
| `Enter` | Select pattern / insert newline (Insert mode) / send message (Normal mode) |
| `Shift+Enter` / `Alt+Enter` | Send message (Insert mode) |
| `Esc` | Insert → Normal mode; message-area/explorer focus → back to input. Does not close the chat — use the palette (`open dashboard`) |
| `Ctrl+P` / `:` (Normal mode) | Open the command palette: navigation (`open dashboard`, `new chat`, `reload config`, `quit`) and pane actions (zen, explorer, activity, editor, scroll) |
| `Ctrl+O` | Open `$VISUAL` / `$EDITOR` (fallback: `vi`) to edit the chat input |
| `Tab` | Cycle focus: Input → Message area → Activity pane (skipped when hidden) |
| `PgUp` / `PgDn` (or `Ctrl+B` / `Ctrl+F`) | Scroll focused pane |
| `Ctrl+A` | Cycle activity pane size: hidden → bottom 20% → bottom 80% → activity-only → hidden |
| `Ctrl+E` | Toggle the thread explorer pane (left side); `Enter` in it switches the chat to the selected thread |
| `Ctrl+Z` | Toggle zen mode: hide (or show) thread info pane and status bar together. If the activity pane is visible, `Ctrl+Z` also hides it. Exiting zen mode restores only info + status, not activity. |
| `Ctrl+C` | Cancel current AI processing |
| `Shift+Tab` | Toggle plan / build mode |
| `Ctrl+Q` | Quit the dashboard |

In Normal mode the usual vi keys are available: motions (`h j k l w e b 0 $ gg
G % { } f`/`t`), edits (`x dd dw D cw J o O`), text objects (`diw ciw vi" di(` …),
yank/paste (`y yy p P`), undo/redo (`u` / `Ctrl+r`), repeat (`.`), half-page
jumps (`Ctrl+d` / `Ctrl+u`), and Visual mode (`v`). See the
[edtui keybinding list](https://docs.rs/edtui) for details.

Note: with the input field focused, Up/Down recall sent-message history when
the input is empty (Insert mode); to scroll the conversation, press `Tab` to
focus the message area, then use `↑/↓`/`j/k`, `PgUp/PgDn`, or `gg/G`. Typing
while the message area is focused refocuses the input automatically. The input
area grows with content from 1 up to 10 text lines. The input prompt shows an
always-visible agent-mode letter chip before `❯ `: `B` (green) for build mode,
`P` (yellow) for plan mode.

### Interface Layout

Normal mode (default):

```
┌────────────────────────┐
│ Channels bar           │
├────────────────────────┤
│ Threads table          │
├────────────────────────┤
│ Detail panel (8 lines) │
├────────────────────────┤
│ Activity log           │
├────────────────────────┤
│ Help bar               │
└────────────────────────┘
```

Chat mode (`c` toggled on). The channel bar is hidden; chat, thread info
pane, activity pane, and status bar are individually togglable. By
default all auxiliary UI is hidden (zen mode). Each chat round has only
a top time rule and a bottom right-aligned duration rule — there are no
side borders or middle dividers.

```
Default (zen mode):
┌────────────────────────┐
│ Chat conversation      │
│ (borderless, full size)│
└────────────────────────┘

After Ctrl+Z (info + status shown):
┌────────────────────────┐
│ Chat conversation      │┌─────────────┐
│ (borderless)           ││ Thread Info │
│                        ││  (20% wide) │
│                        ││             │
└────────────────────────┘└─────────────┘
 Help bar (1 line)

After Ctrl+A twice (activity 80% bottom):
┌────────────────────────┐
│ Chat conversation      │
├────────────────────────┤
│ Activity log (80%)     │
└────────────────────────┘

After Ctrl+A three times (activity only):
┌────────────────────────┐
│ Activity log           │
└────────────────────────┘
```

## WebSocket Protocol

JSON envelope over WebSocket:

| Direction | Message | Purpose |
|-----------|---------|---------|
| Client→Server | `{"type":"list_patterns"}` | Get available patterns |
| Server→Client | `{"type":"patterns","patterns":["general","coding-help"]}` | Pattern list response |
| Client→Server | `{"type":"subscribe","thread":"general"}` | Subscribe to thread replies |
| Client→Server | `{"type":"message","thread":"general","text":"hello"}` | Send message |
| Server→Client | `{"type":"reply","thread":"general","text":"AI reply..."}` | Broadcast reply |

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
│  │  MessageRouter / ThreadManager        │  │
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

## Thread Naming

WebSocket thread names are derived from the `thread` field in client messages:

```json
{"type":"message","thread":"general","text":"hello"}
{"type":"subscribe","thread":"general"}
```

When a message's `thread` field is non-empty, it is used as the thread name. When empty, the thread name falls back to the channel name (e.g., `my_ws`).

Workspace files are stored under:

```
{workdir}/workspace/{channel_name}/
```

### Multi-Thread Workspace Isolation

Each unique `thread` value creates a separate workspace directory:

```
{workdir}/workspace/{channel_name}/
├── general/                 # thread="general"
│   ├── message_001/
│   └── .jyc/
├── coding/                  # thread="coding"
│   ├── message_001/
│   └── .jyc/
└── review/                  # thread="review"
    └── ...
```

This enables completely isolated conversation contexts, skills, and file systems per thread — different threads within the same websocket channel behave like independent channels. When no `thread` is specified, the channel name is used as fallback (backward compatible).

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
| Workspace isolation | Per-thread | Per-thread | Per-thread | Per-thread |

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
