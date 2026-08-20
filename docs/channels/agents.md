# Agents — `[agents.<name>]`

Each `[agents.<name>]` entry is a **websocket-based endpoint** with
behavior but no matching rules. The WebSocket transport (broadcast,
dashboard chat pane, `/ws/agents/<topic>` URL) is unchanged from the
historical websocket channel; only the **config surface** moves.

## Overview

- **One channel** (synthesized): `channel_type = "websocket"`,
  `channel_name = "agents"`. Holds one pattern per `[agents.<name>]`.
- **One pattern per agent**, name = agent name. Pattern rules are
  empty; routing is by topic (URL segment or WS payload `topic`).
- **Multiple topics** under each agent — each WS message's `topic`
  selects a sub-topic; every topic gets **its own directory**.
- **Default topic directory**: `<data_home>/agents/<topic_name>/`
  - Linux/macOS: `~/.local/share/jyc/agents/<topic_name>/`
  - Windows: `%LOCALAPPDATA%\jyc\agents\<topic_name>\`
  - For the 1:1 case (topic name == agent name) this is
    `<data_home>/agents/<agent_name>/`.
  - Override per-agent with `topic_path` (relative paths resolve
    against the jyc workdir; absolute paths used as-is; `~` expands).
    Note: `topic_path` **pins** one fixed directory — every topic of
    that agent then shares it (one chat history, one checkout). Use it
    for single-topic agents; leave it unset for agents that receive
    pipe-routed dynamic topics.

## Configuration

```toml
[agents.jyc]
template = "jyc"
# topic_path = "~/projects/jyc"      # override the default workspace root
skills = ["coding-principles", "internal-comms"]
access = { read = ["~/.cargo/registry/src"], write = ["/tmp/jyc-builds"] }
model = "anthropic/claude-opus-4-6"
small_model = "deepseek/deepseek-v4-flash"
mcps = []
disabled_tools = []
live_injection = true
inject_inbound_images = false
mode = "build"
reset_compression = { mode = "llm", keep_pairs = 3 }
auto_reset_threshold = 0.95
role = "Developer"
attachments = { enabled = true, allowed_extensions = [".pdf", ".md"] }
```

### Fields

| Field | Description |
|-------|-------------|
| `template` | Topic template name (from `templates/`) |
| `topic_path` | Pins **one fixed** directory for every topic of this agent (overrides the per-topic `<data_home>/agents/<topic_name>/` default). Leave unset for agents receiving pipe-routed dynamic topics. |
| `skills` | Whitelist; when set, only these skills are loaded for topics of this agent |
| `disabled_skills` | Skills to disable for this agent |
| `access` | Filesystem read/write whitelist (`{ read = [...], write = [...] }`) |
| `role` | Agent role name (e.g., "Planner", "Developer", "Reviewer") |
| `live_injection` | Inject follow-ups into the active session (default: `true`) |
| `inject_inbound_images` | Auto-inject inbound image attachments (default: `false`) |
| `model` / `plan_model` / `build_model` / `small_model` | Model overrides |
| `mode` | Initial mode for topics: `"plan"` or `"build"` |
| `mcps` | MCP servers (overrides global `[[mcps]]` for topics of this agent) |
| `disabled_tools` / `disabled_builtin_tools` / `disabled_mcp_servers` | Tool/MCP gating |
| `reset_compression` | Session-reset compression (`{ mode, keep_pairs }`) |
| `auto_reset_threshold` | Auto-reset threshold as fraction of context window |
| `attachments` | Inbound attachment config (allowed extensions, max size, etc.) |

### Prerequisites

The agents system requires the inspect server to be enabled (same as
the legacy websocket channel — the WebSocket handler rides on the
inspect server's port):

```toml
[inspect]
enabled = true
bind = "127.0.0.1:9876"
```

The WebSocket endpoint sits at `/ws/agents/<topic>` — same URL shape
as the legacy `/ws/<channel>/<topic>`, just with a fixed channel name
(`agents`) regardless of agent name. The agent name lives inside the
matcher, selected by `message.topic` against the synthesized pattern
list.

## Usage

```bash
jyc serve --workdir /path/to/data
jyc dashboard --workdir /path/to/data
```

In the dashboard:

1. Press `c` to open the chat pane.
2. Pattern select lists every `[agents.<name>]`. Pick one (or send
   directly to the auto-default).
3. Chat as usual.

URL: `ws://<addr>/ws/agents/<topic>` (URL-scoped topic, or
`<topic>` from the WS payload's `topic` field).

## Mapping from legacy `[channels.<name>] type="websocket"`

OLD (still accepted, deprecation warning at startup):

```toml
[channels.local_dev]
type = "websocket"

[[channels.local_dev.patterns]]
name = "jyc"
template = "jyc"
topic_path = "~/projects/jyc"
skills = ["coding-principles"]
access = { read = [...], write = [...] }
```

NEW (preferred):

```toml
[agents.jyc]
template = "jyc"
topic_path = "~/projects/jyc"
skills = ["coding-principles"]
access = { read = [...], write = [...] }
```

Each pattern becomes its own agent (channel). The legacy `channel`
name (`local_dev` in the example) is no longer configurable — there is
exactly one synthetic channel named `agents` for the whole agent set.

### Migration steps

1. Pick one pattern per `[channels.<name>.patterns]` to promote.
2. Rename the pattern's `name` to the agent name.
3. Move the pattern's fields up to a top-level `[agents.<name>]` table
   (drop `name`, `enabled`, `rules`).
4. If multiple patterns were per-channel before, each becomes its
   own agent. Distinct topics under one agent are still created via
   the WS `topic` field — they share the agent's behavior.
5. Remove the `[channels.<name>] type="websocket"` block.
6. Restart `jyc serve` (new agents require restart, like any new
   channel).

## Workspace layout

Topics of every agent are siblings under the shared agents workspace
root, one directory per topic name:

```
<data_home>/agents/
├── jyc/                  # 1:1 topic of agent "jyc" (topic == agent name)
│   ├── .jyc/
│   ├── chat_history_<date>.jsonl
│   └── attachments/
├── plan-197/             # pipe-routed topic of agent "jyc_git_planner"
│   ├── .jyc/
│   └── ...
├── review-42/            # another pipe-routed topic
│   └── ...
```

Each topic is isolated: its own chat history, session state, and (for
GitHub topics) its own repository checkout. Topics of one agent share
the agent's `template`, `skills`, `access`, model overrides, etc. —
those come from the agent's pattern, not from the directory.

Topic names are a single namespace inside the synthesized `agents`
channel, so two agents cannot hold same-named topics. Prefix your pipe
topic templates (`plan-`, `review-`, `dev-`) to keep them distinct.

An agent with an explicit `topic_path` is the exception: all of its
topics collapse into that one pinned directory.

## Cross-channel piping (feishu / pipe)

`pipe = { agent = "<name>", topic = "..." }` is the new form. The pipe
target is an agent (resolved against `[agents.<name>]`):

```toml
[[channels.feishu_bot.patterns]]
name = "mentions"
pipe = { agent = "jyc", topic = "${msg.chat_name}" }
```

The legacy form `pipe = { channel = "...", pattern = "...", topic = "..." }`
still works; it emits a deprecation warning.

## Architecture

The agent system reuses the existing WebSocket transport unchanged:

```
User sends message (any channel) → Pattern Match → Topic Queue → Worker (AI) → Reply
                       ↑
                  matcher picks agent by
                  message.topic against the
                  synthesized pattern list
```

Pattern matching and routing logic for the synthesized "agents"
channel is identical to the legacy `[channels.local_dev]` flow —
`WebsocketInboundAdapter` / `WebsocketOutboundAdapter` /
`TopicManager` / `MessageRouter` all see a normal websocket channel.
Only the configuration syntax is different.

## References

- See `crates/jyc-channels/src/websocket/` for transport implementation
- See `crates/jyc-cli/src/cli/serve/agents_synth.rs` for the synthesis logic
- See `crates/jyc-types/src/config/agent.rs` for the `AgentConfig` type