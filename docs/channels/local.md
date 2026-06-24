# Local TUI Channel

The `local` channel enables direct terminal-based AI interaction with JYC via an interactive TUI (Terminal User Interface) built with ratatui + crossterm.

## Overview

Unlike server-based channels (email, WeCom, Feishu) that require external services, the local channel runs entirely within your terminal. Each `jyc local` instance is an independent process with its own workspace directory and agent context.

**Key characteristics:**
- **No external services required** — runs entirely offline (except for AI API calls)
- **Independent process per instance** — each `jyc local` has its own workspace and conversation history
- **Real-time bidirectional chat** — type messages and see AI replies stream in the TUI
- **Single-thread per instance** — one conversation per channel
- **Supports all agent features** — skills, MCP tools, pattern routing, model overrides work normally

## Configuration

```toml
[channels.my_local]
type = "local"

# Optional: override model for this channel
# model = "anthropic/claude-opus-4-6"
# small_model = "deepseek/deepseek-v4-flash"

# Define a catch-all pattern (local input always matches the first enabled pattern)
[[channels.my_local.patterns]]
name = "all"
enabled = true
```

### Required Fields

| Field | Description |
|-------|-------------|
| `type` | Must be `"local"` |

### Optional Fields

| Field | Description |
|-------|-------------|
| `model` | Per-channel model override |
| `small_model` | Per-channel small model override |
| `patterns` | Pattern rules (only the first enabled pattern is used) |

## Usage

Start an interactive terminal session:

```bash
# Default channel name: "local"
jyc local

# Use a specific channel from your config
jyc local --name my_local
```

### TUI Controls

| Key | Action |
|-----|--------|
| `Ctrl+D` | Send the current input |
| `Ctrl+C` | Quit the application |
| `Enter` | Insert a newline in the input |
| `Backspace` | Delete the last character |
| `↑` / `↓` | Scroll through conversation history |

### Interface Layout

The TUI is split into two areas:

```
┌─────────────────────────────────────┐
│  Conversation                       │
│  [You] Hello!                       │
│  [Agent] Hello! How can I help you? │
│                                     │
│                                     │
└─────────────────────────────────────┘
│  Input (Ctrl+D=send, Ctrl+C=quit)   │
│  >                                  │
└─────────────────────────────────────┘
```

- **Conversation area** (top 80%): Displays the chat history with color-coded roles (cyan for You, green for Agent)
- **Input area** (bottom 20%): Where you type your message

## Architecture

### Process Model

```
┌─────────────────────────────────────────────┐
│  Terminal (stdin/stdout)                    │
│  ┌───────────────────────────────────────┐  │
│  │  TUI Task (spawn_blocking)            │  │
│  │  • Handles keyboard input             │  │
│  │  • Renders conversation UI            │  │
│  └──────────┬────────────────────────────┘  │
│             │ mpsc channels                 │
│  ┌──────────▼────────────────────────────┐  │
│  │  Async Inbound Adapter                │  │
│  │  • Converts TUI input → InboundMessage│  │
│  │  • Routes to MessageRouter            │  │
│  └──────────┬────────────────────────────┘  │
│             │                               │
│  ┌──────────▼────────────────────────────┐  │
│  │  Agent Service                        │  │
│  │  • Processes messages                 │  │
│  │  • Generates replies                  │  │
│  └──────────┬────────────────────────────┘  │
│             │                               │
│  ┌──────────▼────────────────────────────┐  │
│  │  Async Outbound Adapter               │  │
│  │  • Sends replies → TUI via mpsc       │  │
│  └───────────────────────────────────────┘  │
└─────────────────────────────────────────────┘
```

### Communication Flow

```
User ──► TUI ──► input_tx ──► InboundAdapter ──► MessageRouter ──► Agent
                                                            │
User ◄── TUI ◄── output_rx ◄── OutboundAdapter ◄────────────┘
```

The TUI runs in a blocking task (`tokio::task::spawn_blocking`) while the rest of the system operates asynchronously. Two unbounded mpsc channels bridge the sync/async boundary:

- `input_tx` — carries user input from TUI to the inbound adapter
- `output_rx` — carries AI replies from the outbound adapter to the TUI

## Thread Naming

Each local channel has exactly one thread, named after the channel:

```
thread_name = "{channel_name}"   # e.g., "my_local"
```

Workspace files are stored under:

```
{workdir}/workspace/{channel_name}/
```

## Pattern Matching

Local input bypasses complex pattern rules. The matcher always selects the **first enabled pattern** from the channel's pattern list. This is because local input is inherently for this channel — there is no sender, subject, or keyword to match against.

```toml
# Only the first enabled pattern matters for local channels
[[channels.my_local.patterns]]
name = "all"
enabled = true
# This pattern will always be used
```

## Model Override Resolution

The full resolution chain for local channels (highest to lowest priority):

1. Runtime `.jyc/model-override` file
2. Pattern-level `model` / `small_model`
3. Channel-level `model` / `small_model`
4. Global `[agent].model` / `[agent].small_model`

## Limitations

1. **Single conversation** — each `jyc local` instance supports only one concurrent conversation. To chat about a different topic, start a new instance or clear the workspace.
2. **No attachments** — the TUI does not support file upload/download. The agent can still use `read_image` and other tools to access files on disk.
3. **No proactive messaging** — `jyc_send_message` is not applicable since there is no external recipient.
4. **No history persistence across restarts** — conversation context is maintained within a single session. Restarting `jyc local` starts fresh (unless you keep the workspace directory).
5. **Terminal size** — the TUI requires a minimum terminal size. Very small terminals may render poorly.

## Comparison with Other Channels

| Feature | `email` | `wecom` | `feishu` | `local` |
|---------|---------|---------|----------|---------|
| External service | IMAP/SMTP server | WeCom server | Feishu server | None |
| Setup complexity | High | High | Medium | Low |
| Real-time | Polling | Webhook/WebSocket | WebSocket | Instant |
| Attachments | Yes | Yes | Yes | No |
| Multi-thread | Yes | Yes | Yes | No (1 thread) |
| Proactive messaging | Yes | Yes | Yes | No |
| Workspace isolation | Per-thread | Per-thread | Per-thread | Per-instance |

## Workspace Layout

Each local instance creates a workspace directory:

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

The workspace is reused across restarts, so conversation history from previous sessions is available to the agent if the workspace is not cleared.

## References

- See `crates/jyc-channels/src/local/` for implementation
- See `crates/jyc-cli/src/cli/local.rs` for TUI implementation
- See `config.example.toml` for configuration example
