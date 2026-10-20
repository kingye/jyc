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
- **Default topic directory**: `<data dir>/agents/<agent_name>/<topic_name>/.data/`
  - Linux/macOS: `~/.local/share/jyc/agents/<agent_name>/<topic_name>/.data/`
  - Windows: `%LOCALAPPDATA%\jyc\agents\<agent_name>\<topic_name>\.data\`
  - Every topic of an agent is a sibling under that agent's root, so
    `plan-197` and `plan-198` never share a directory.
  - That directory is the topic's **home**: the topic's `.jyc` state sits
    beside the working directory at `<home>/.jyc`, never inside it.
  - For the 1:1 case (topic name == agent name) the home is the agent's own
    root, `<data dir>/agents/<agent_name>/`, so the topic works in
    `<data dir>/agents/<agent_name>/.data/`.
  - `topic_path` (relative paths resolve against the jyc data dir;
    absolute paths used as-is; `~` expands) **pins the agent's 1:1
    topic** — the topic whose name equals the agent name — to that
    directory. It is the agent's own home (usually a code checkout), so
    other topics of the agent are never placed inside it; they stay
    under `<data dir>/agents/<agent_name>/<topic_name>/`.

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
role = "Developer"
attachments = { enabled = true, allowed_extensions = [".pdf", ".md"] }
```

### Fields

| Field | Description |
|-------|-------------|
| `template` | Topic template name (from `templates/`) |
| `topic_path` | Pins this agent's **1:1 topic** (topic name == agent name) to a fixed directory — the agent's own home, typically a code checkout. Other topics of the agent are never placed inside it; they stay at `<data dir>/agents/<agent_name>/<topic_name>/`. |
| `skills` | Whitelist; when set, only these skills are loaded for topics of this agent |
| `disabled_skills` | Skills to disable for this agent |
| `access` | Filesystem read/write whitelist (`{ read = [...], write = [...] }`) |
| `role` | Agent role name (e.g., "Planner", "Developer", "Reviewer") |
| `live_injection` | Inject follow-ups into the active session (default: `true`) |
| `inject_inbound_images` | Auto-inject inbound image attachments (default: `false`) |
| `model` / `plan_model` / `build_model` / `small_model` | Model overrides |
| `mode` | Initial mode for topics: `"plan"` or `"build"` |
| `mcps` | MCP servers (overrides global `[[mcps]]` for topics of this agent) |
| `disabled_tools` / `disabled_builtin_tools` / `disabled_mcps` | Tool/MCP gating |
| `reset_compression` | **Deprecated — ignored.** The auto-reset mechanism was removed |
| `auto_reset_threshold` | **Deprecated — ignored.** The auto-reset mechanism was removed |
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
jyc serve --data-dir /path/to/data
jyc dashboard --data-dir /path/to/data
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

Each agent owns a subtree; its topics are siblings inside it, one **home**
per topic name — and the agent root is its own 1:1 topic's home:

```
<data dir>/agents/
├── jyc_git_planner/          # agent root — the home of its 1:1 topic
│   ├── .jyc/                 # that topic's state dir (transcript, session, tasks)
│   ├── .data/                # its working directory (`topic_path`)
│   ├── plan-197/             # pipe-routed topic: a home of its own
│   │   ├── .jyc/             # state dir
│   │   └── .data/            # working directory
│   │       └── .attachments/ # delivered files, when a message carries any
│   ├── plan-198/             # sibling topic, fully isolated
│   │   └── ...
├── group_chat/
│   ├── 英国旅行2026/
│   └── ...
```

Inside a home, `.jyc` is the topic's state dir and `.data` is the working
directory its agent runs in (`topic_path`). Every framework-owned directory is
dot-prefixed, so a name a user's project owns — `attachments/`, a checkout's
own files — always stays free. A topic keeps the directory it was created with:
one that predates the `.data` split keeps its older layout, with its state dir
beside it as ever.

Each topic is isolated: its own chat history, session state, and (for
GitHub topics) its own repository checkout. Topics of one agent share
the agent's `template`, `skills`, `access`, model overrides, etc. —
those come from the agent's pattern, not from the directory.

Topic names are a single namespace inside the synthesized `agents`
channel, so two agents cannot hold same-named topics. Prefix your pipe
topic templates (`plan-`, `review-`, `dev-`) to keep them distinct.

An agent with an explicit `topic_path` places only its 1:1 topic there
(e.g. agent `jyc` with `topic_path = "~/projects/jyc"` → topic `jyc`
works in the checkout). Its pipe-routed topics still get their home under
`<data dir>/agents/jyc/<topic>/` (working in `<topic>/.data/`), so dynamic
topics never litter the pinned directory.

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

- See `crates/jyc-inspect/src/server/websocket/` for transport implementation
- See `crates/jyc-cli/src/cli/serve/agents_synth.rs` for the synthesis logic
- See `crates/jyc-types/src/config/agent.rs` for the `AgentConfig` type