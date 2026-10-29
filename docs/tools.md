# JYC Tools Reference

This document lists all tools available to the AI agent in JYC, including built-in tools, MCP bridge tools, and externally configurable MCP tools.

**Audience:** AI agents (for understanding available capabilities) and developers/operators (for configuration and debugging).

---

## Tool Categories

| Category | Description | Configuration |
|----------|-------------|---------------|
| **Built-in** | Core file system and execution tools. Always available unless explicitly disabled. | Hardcoded in `jyc-agent` |
| **MCP Bridge** | In-process JYC-specific tools (send message). | Always registered |
| **Cross-Topic** | Cross-channel topic communication tool. | Registered when cross-channel `topic_managers` available |
| **External MCP** | Optional tools provided by external MCP servers configured in `config.toml`. Registered but not advertised until the model expands the server through `load_mcp_schema`. | `[[mcps]]` config |

---

## Built-in Tools

### `bash`

Execute shell commands in the working directory.

**Parameters:**
- `command` (string, required): The bash command to execute
- `timeout` (integer, optional): Timeout in seconds (default: 120)

**Security:** Best-effort path boundary check — scans for absolute-path tokens (outside quoted strings) and verifies they are within `working_dir`, the system temp dir, or configured read/write roots (`access.read` / `access.write`). This is a heuristic, not a sandbox. Full isolation requires OS-level containment.

**Limits:** Output truncated at 128 KB.

**Example:**
```json
{"command": "cargo test --workspace", "timeout": 300}
```

---

### `read`

Read a file or directory listing.

**Parameters:**
- `file_path` (string, required): Path to file or directory (relative to working dir or absolute)
- `offset` (integer, optional): Line number to start from, 1-indexed (default: 1)
- `limit` (integer, optional): Maximum lines to read (default: 2000)

**Security:** Path must be within `working_dir`, the system temp dir, `additional_read_roots`, or `additional_write_roots`. Symlink exemption supported for symlinked repos (e.g. `topic/repo -> /other/path`).

**Example:**
```json
{"file_path": "src/main.rs", "offset": 1, "limit": 50}
```

---

### `write`

Create or overwrite a file. Creates parent directories as needed.

**Parameters:**
- `file_path` (string, required): Path to the file
- `content` (string, required): Content to write

**Security:** Path must be within `working_dir` or configured write roots (`access.write`).
```

---

### `edit`

Perform exact string replacement in a file.

**Parameters:**
- `file_path` (string, required): Path to the file
- `old_string` (string, required): Exact text to replace (including whitespace/indentation)
- `new_string` (string, required): Replacement text
- `replace_all` (boolean, optional): Replace all occurrences (default: false)

**Behavior:**
- Fails if `old_string` is not found
- Fails if `old_string` matches multiple times and `replace_all` is false
- Fails if `old_string == new_string`

**Security:** Path must be within `working_dir` or configured write roots (`access.write`).

**Example:**
```json
{"file_path": "src/main.rs", "old_string": "fn main() {", "new_string": "fn main() {\n    println!(\"hello\");"}
```

---

### `grep`

Search file contents using regular expressions.

**Parameters:**
- `pattern` (string, required): Regex pattern to search for
- `path` (string, optional): Directory to search in (default: working directory)
- `include` (string, optional): File pattern filter (e.g., `*.rs`, `*.{ts,tsx}`)

**Limits:** Maximum 100 results returned; truncates with count if more.

**Security:** If `path` is explicitly provided, it must be within `working_dir`.

**Example:**
```json
{"pattern": "async fn", "path": "src", "include": "*.rs"}
```

---

### `webfetch`

Fetch content from a URL.

**Parameters:**
- `url` (string, required): URL to fetch
- `timeout` (integer, optional): Timeout in seconds (default: 30)

**Limits:** Response truncated at 512 KB. User-agent: `Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36`.

**Example:**
```json
{"url": "https://api.github.com/repos/kingye/jyc", "timeout": 10}
```

---

### `read_image`

Load an image for analysis. Dual-mode operation:

1. **Image injection mode** (when the active model supports images): Queues the image for injection into the next user turn as a content block
2. **Vision fallback mode** (when model does not support images but `[agent.vision]` is configured): Sends image to an external vision model and returns textual analysis

**Parameters:**
- `path` (string, optional): Absolute path to a local image file (must be within working dir or configured attachments directory)
- `url` (string, optional): HTTP(S) URL to fetch image from

**Constraints:**
- Provide exactly one of `path` or `url`
- Supported formats: `image/png`, `image/jpeg`, `image/gif`, `image/webp`
- Maximum size: 10 MB
- Requires `inject_inbound_images = true` on the active pattern for fallback mode

**Example:**
```json
{"path": "/workspace/screenshot.png"}
```

---

### `context_browse`

Page through the agent's in-memory conversation transcript (user/assistant text pairs), including turns that fell out of the model's sliding context window.

**Parameters:**
- `offset` (integer, optional, default 0): Pairs to skip from the newest end. 0 = most recent pairs; larger values page toward older pairs.
- `limit` (integer, optional, default 10): Max pairs to return (capped at 50).

**Behavior:**
- Pair numbers count from the oldest pair (1 = first); the returned page is always oldest→newest.
- The transcript snapshot is taken once per tool batch, before tool results are appended — a browse never sees results from tools called in the same batch.
- Returns a header (`=== Transcript pairs X-Y of N ===`) followed by numbered `USER:` / `ASSISTANT:` lines.

**Security:** Read-only, in-memory. No filesystem access; no boundary check needed.

**Example:**
```json
{"offset": 20, "limit": 10}
```

---

### `task_list` / `task_create` / `task_update`

The agent's plan for the topic's current piece of work, persisted at
`.jyc/tasks.json` — one list per topic. `task_create` replaces whatever list was
there, and `/reset` / `/new` delete it.

| Tool | Input | Effect |
|------|-------|--------|
| `task_create` | `items: string[]` (1–20 one-line steps) | Writes a fresh list, numbered 1..N, all `pending` |
| `task_update` | `id: integer`, `status: "pending" \| "in_progress" \| "completed"` | Sets one item's status |
| `task_list` | — | Shows the list, including the ids |

Three tools rather than one whole-list call so that finishing a step stays a
small call, and `task_list` exists because the ids must be recoverable after the
context is compressed. All three return the rendered list — `Tasks (2/5):` plus
one `[ ]` / `[~]` / `[✓]` line per item — which is also what the TUI topic-info
pane and `/info` show, so the ids the user reads are the ids `task_update` takes.

Use it for anything multi-step: write the list before starting, mark an item
`in_progress` as you pick it up and `completed` once it is genuinely done.
Re-planning means a new `task_create` (which renumbers), not patching a stale list.

### `lesson_add` / `lesson_read`

The agent's own record of what it learned, kept at `lessons.md` in the topic's
state dir (in the unpinned layout that is `<topic>/.jyc/lessons.md`; a pinned
topic's state dir lives in the data dir) — one file per topic, beside the
user's `notes.md`. A lesson is one distilled, reusable rule: a trap the agent
fell into, a fix that worked, a convention of this repo or this user.

| Tool | Input | Effect |
|------|-------|--------|
| `lesson_add` | `lesson: string` (one rule, ≤1000 chars) | Appends `- YYYY-MM-DD — <rule>`; the same rule twice is not appended twice |
| `lesson_read` | — | Returns the earlier lessons — the ones the system prompt could not carry |

The **newest** lessons ride along in the system prompt — 2 KiB of them, and up
to 4 KiB when the cut backs up to a whole lesson rather than show half of one
— so a recent lesson is in view on every later turn without a tool call;
`lesson_read` reaches the earlier ones. The cut lands on a line boundary, so
a lesson is whole in one end or the other; when the remainder is too long to
carry whole, the fragment is marked as cut rather than passed off as a whole
rule. The tool only appends and reads — it never rewrites the file — so
pruning it is the user's call. (That is a property of the tool, not a security
boundary: in the unpinned layout the state dir sits inside the topic's
working dir, where `write` and `edit` can already reach it.)

The file is meant to stay small. Past roughly 20 KB (at most 4 KiB in the
prompt + 16 KiB per read) the middle of the archive is out of reach of both
tools, and the file wants pruning — by the user, since the tools never
rewrite it. The lessons live in the topic's state dir, so closing the topic
takes them with it.

---

## Extended Tools (loaded on demand)

Some tools are worth having but not worth paying for on every call: a topic
that never delegates, never publishes, never fetches a URL, never browses its
transcript, never records a lesson and never needs a multi-step plan should
not carry their schemas in every request. They are **registered but not
advertised** — the model gets them by calling one tool.

### `load_extended_tools`

Brings the extended group into the tool list.

**Parameters:** none.

**Effect:** the group's schemas are advertised from the next model call on, and
they stay advertised for the rest of the topic's registry lifetime (a config
reload rebuilds the registry and starts over). The tool is idempotent: calling
it again reports that the group is already loaded.

**The group** (the tools that are in it depend on what the instance has — a
tool disabled through `disabled_tools`, or one that does not exist on this
channel, is not part of it):

| Tool | What it is for |
|------|----------------|
| `jyc_spawn_subagent` | Delegate a task to a subagent topic |
| `jyc_send_to_topic` | Message another topic |
| `list_topics` | List the channels and their active topics |
| `jyc_publish_file` | Publish a file as a download link |
| `read_image` | View an image |
| `webfetch` | Fetch a URL as text |
| `context_browse` | Page the transcript for earlier turns |
| `lesson_add` | Record a reusable lesson |
| `lesson_read` | Read earlier recorded lessons |
| `task_list` | Read the plan and its item statuses |
| `task_create` | Write a new plan |
| `task_update` | Mark one item's status |

`jyc_send_message` is not in this group: the send tool must be discoverable
for attachment-bearing replies without a loader call, so it stays advertised
with the core tools. The task tools are in the group because whether work
needs a plan is a project convention, not a framework-wide cost. The loader
points multi-step work at them, and omits that sentence when `disabled_tools`
removes every task tool.

The loader's own description is the index of the group and names the core
tools already available, so the model learns what it holds without the group's
schemas being in the request.

**Only advertisement is gated.** The tools stay registered and executable, so
a model that names one before loading it gets a working call rather than a
"not found" error. `disabled_tools` is the boundary that removes a tool for
good — see [Tool Exclusion](#tool-exclusion). That includes the loader
itself: `disabled_tools = ["load_extended_tools"]` removes it, and the group
then stays out of every request (its tools remain callable by name, as always).

### `list_topics`

Lists the channels and their active topics — the addressing table for
`jyc_send_to_topic` and `jyc_send_message`.

**Parameters:** none.

**Output:** the same shape the per-turn runtime-state block used to carry —
one `- Channel "name" (type)` line per channel, and the topic names under it.

---

## MCP Bridge Tools

These are JYC-specific tools implemented as in-process bridges (not external MCP subprocesses). They are always registered unless excluded via `disabled_tools`.

### Reply Delivery (no tool)

There is no reply tool. The agent's final message at the end of a turn IS the in-topic reply:

- Delivered synchronously through the pre-warmed outbound adapter when a reply target is live; otherwise queued via the `.jyc/reply.md` + `reply-sent.flag` file relay.
- Text written alongside tool calls is internal narration and is NOT delivered.
- A turn that ends with no text sends nothing.
- Long turns emit a canned heartbeat at each iteration-cycle boundary.
- Optional `[[hooks]]` with `event = "reply_send"` can inspect or suppress each delivery.

---

### `jyc_send_message`

Send a channel message to an arbitrary recipient, with optional attachments.
It is part of the core advertised tool set, not the extended group.

**Parameters:**
- `recipient` (string, required): Channel-specific recipient identifier
- `message` (string, required): Message body to send
- `subject` (string, optional): Message subject (channel-dependent)
- `channel` (string, optional): Target channel for a cross-channel send
- `attachments` (string[], optional): Files within the current topic directory
  to attach. Supported by `email`, `feishu` and `wecom_bot` (including when they
  run in `jyc-pipe`: the hub relays the files through the pipe); every other
  channel rejects the send instead of dropping the files.

**Recipient Format:**
- WeCom KF: `wecomkf:{open_kfid}:{external_userid}`
- Email: `user@example.com`
- Other channels: channel-specific format

**Constraints:**
- Use for proactive alerts, cross-channel messages, and replies that carry attachments
- Use the final message of the turn for ordinary text-only replies
- Requires a pre-warmed outbound adapter (`ToolContext.outbound`)

**Example:**
```json
{"recipient": "wecomkf:kf001:wmE8OcHAAA...", "message": "Invoice package attached.", "attachments": ["invoices.zip"]}
```

---

## Cross-Topic Communication Tools

### `jyc_send_to_topic`

Send a message to another agent for agent processing: its own topic by default, or one of its topics with `topic`.

Unlike `jyc_send_message` (which bypasses agent processing for direct outbound delivery), `jyc_send_to_topic` injects the message into the target topic's queue so the target topic's agent picks it up and processes it.

The command form is `/send <agent> [--topic|-t <topic>] [--reply] [--attach <file>] [--recipient <id>] <message>`; both call the same `command::send_handler::send_to_topic`.

**Parameters:**
- `agent` (string, required): Target agent name (e.g. `"invoice"`, `"mail-bot"`). The name is matched against topic names first; a name no topic owns is an error — the topic has to exist already (create it with `/spawn` or `jyc_spawn_subagent`; a topic `/spawn` just made counts as existing, so the task that follows the spawn can be sent to it)
- `topic` (string, optional): One of that agent's topics (e.g. `"invoice-processing"`), for when it has more than one. A topic's name is its address — it is unique process-wide, so a named topic identifies its agent too. An agent's own topic is named after the agent, so passing the same name as `agent` addresses that same default topic
- `message` (string, required): Message body to inject into the target topic
- `attachments` (string[], optional): List of filenames within the current topic directory to attach
- `recipient` (string, optional): Recipient address/ID. Sets the sender_address on the injected message for channel-appropriate reply routing
- `require_reply` (boolean, optional, default false): When `true`, the target agent is instructed to send the results back to the source agent via `jyc_send_to_topic`

**Behavior:**
- Resolves the addressee to the `TopicManager` that serves it — the one running the topic's queue, the channel the topic's own state records (`.jyc/topic-meta.json`), or the agents channel for a topic that exists but has never been routed to (a child `/spawn` just made, which has no queue and no routing record yet); the channel is never named by the caller
- A name no topic owns is refused: a send reaches topics that exist, it does not make them — create the agent first with `/spawn` or `jyc_spawn_subagent`, and the refusal says so. A recorded channel that is not running is refused too, and so is a closed child (its home is gone)
- Builds an `InboundMessage` with source metadata (`source_channel`, `source_topic`, `require_reply`)
- Enqueues the message into the target topic's queue
- The target agent sees a `**Source:**` header in its incoming message prompt

**`require_reply` flow:**
- When `true`, the target agent's prompt includes: `⚠️ Reply requested` and an instruction to call `jyc_send_to_topic` with `agent="<the source topic's name>"`
- The target agent should call `jyc_send_to_topic` back to that agent when done

**Constraints:**
- Requires the running topic managers (`ToolContext.topic_managers`)
- Attachments must be within the current topic's working directory

**Example:**
```json
{
  "agent": "invoice",
  "topic": "invoice-processing",
  "message": "Please process the attached invoice.",
  "attachments": ["invoice.pdf"],
  "require_reply": true
}
```

---

## External MCP Tools

JYC supports loading additional tools from external MCP (Model Context Protocol) servers. These are configured in `config.toml` under `[[mcps]]` sections.

**Configuration:**
```toml
[[mcps]]
name = "my-mcp-server"
command = "npx"
args = ["-y", "@my-org/mcp-server"]
env = { API_KEY = "${MY_API_KEY}" }
```

**Tool Whitelist:**
Use `enabled_tools` to load only specific tools from a server, ignoring all others:

```toml
[[mcps]]
name = "jin_public_mcp"
command = "npx"
args = ["-y", "@jin/mcp-server"]
enabled_tools = ["product_list", "search", "checkout"]  # Only these 3 tools are loaded
```

> **Tip:** `enabled_tools` is much more convenient than listing 20+ tools in `disabled_tools` when you only need a few from a large MCP server.

**Scope Control:**
- **Global:** All `[[mcps]]` entries in `config.toml` are loaded by default
- **Channel-level:** `ChannelConfig.mcps` restricts to specific MCP servers for that channel
- **Pattern-level:** `ChannelPattern.mcps` further restricts per pattern
- Resolution priority: Pattern → Channel → Global

**Common External MCPs:**
- Vision analysis servers (image OCR/description)
- Custom domain-specific tool servers

**Remote MCP Authentication** (`type = "remote"`):

| Mechanism | Config | Use when |
|---|---|---|
| Static bearer | `auth_header = "<token>"` (no `Bearer ` prefix) | You have a long-lived token |
| client_credentials | `[mcps.oauth]` with client_id/secret/token_endpoint | Machine-to-machine with pre-provisioned credentials |
| MCP OAuth 2.1 | `[mcps.oauth_dcr]` (empty block is valid) | Server uses the spec auth flow: discovery + Dynamic Client Registration + browser consent (e.g. hosted Jira MCP) |

The three are mutually exclusive (validation rejects combinations). For
`oauth_dcr`, run the one-time interactive authorization after adding the
config block:

```bash
jyc mcp auth <name>   # prints URL → approve in browser → paste redirect URL back
```

Tokens (incl. the DCR-registered client id) are stored under
`<data-home>/jyc/mcp_auth/<name>.json` (0600); `jyc serve` then refreshes
them automatically at every MCP connect. Re-run the command only when the
refresh token itself expires or is revoked. See `config.example.toml` for
the optional `scopes` / `redirect_uri` fields.

### `load_mcp_schema`

An MCP server's tools are schemas like any other: they ride in every
request's stable prompt prefix, whether or not this conversation ever calls
them. A server with fifty tools puts fifty schemas in the request of a
topic that wanted one of them.

So the MCP tools stay out of the advertised list until the model asks for
them. What it sees instead is this tool, whose description is the index of
the configured servers: each server's name, and each of its tools with a
one-line description — enough to decide *which* server to load, and none of
the schemas themselves.

**Parameters:**

| Parameter | Type | Meaning |
|---|---|---|
| `mcp_name` | string (required) | The MCP server to expand, as listed in the tool's description |
| `tool_names` | string[] (optional) | Only these tools of that server; omit to expand all of them |

**Effect:** the named tools are advertised from the next model call on, and
they stay advertised for the rest of the topic's registry lifetime (a config
reload or an `/mcp` change rebuilds the registry and starts over). The call
is idempotent: calling it again for an expanded server reports that its
tools are already there. A repeated call for a *subset* says the requested
tools are already expanded and names the ones still hidden — it does not
claim the whole server is available.

**Malformed input is an error.** `tool_names` must be an array of non-empty
tool names; a string, a number, an empty list, or a list with a bad entry
is rejected rather than treated as "no subset" — silently expanding the
whole server would defeat the point of asking for a subset. Omit
`tool_names` to expand every tool of the server.

**Only advertisement is gated.** The tools stay registered and executable, so
a model that names one before expanding it gets a working call rather than a
"not found" error. `disabled_tools` and `disabled_mcps` remain the boundary
that removes a tool or a server for good — a server removed that way is not
in the index either. That includes the loader itself:
`disabled_tools = ["load_mcp_schema"]` removes it, and the MCP tools then
stay out of every request (they remain callable by name, as always).

**One name, one owner.** The registry holds one tool per name, so a name
belongs to whatever holds it last. An MCP server that exposes a built-in's
name (`webfetch`, say) takes that name over: the tool belongs to the MCP
group and stays hidden until the model expands that server, and the
extended group does not claim it as well. For the same reason the loader
names are reserved — `load_mcp_schema` and `load_extended_tools` stay the
framework's, and an MCP server exposing a tool under one of those names has
that tool skipped. The loaders are how the model reaches the MCP tools at
all, so a loader it cannot call is a server it can never expand.

**What it does not change:** the servers are still connected at registry
build time — the index needs their tool names — so this saves the schema
cost of every request, not the startup handshake or the subprocess.

---

## Tool Exclusion

Tools can be disabled at agent and topic levels. A pipe channel (email,
feishu, …) only forwards, so its own `disabled_tools` / `disabled_mcps`
are rejected by config validation — the exclusions belong to the agent
that handles the message.

### Configuration

```toml
[agents.jyc]
# Disable tools for every topic of this agent
disabled_tools = ["bash", "write"]
disabled_mcps = ["my-mcp-server"]
```

Per-topic exclusions live in the topic's own `.jyc/config.toml`:

```toml
# <topic>/.jyc/config.toml
disabled_tools = ["edit"]
disabled_mcps = []
```

### Fields

| Field | Scope | Description |
|-------|-------|-------------|
| `disabled_tools` | Agent / Topic | List of tool names to remove. Matches built-in tools, MCP bridge tools, and external MCP tools by registration name. **External MCP tools can also be targeted as `server_name/tool_name` for precise exclusion when multiple servers provide the same tool name.** |
| `disabled_mcps` | Agent / Topic | List of MCP server names to skip during tool loading. **Renamed from `disabled_mcp_servers`; the old key is still accepted.** |
| `disabled_builtin_tools` | Agent only | **Backward-compatible alias.** Merged into `disabled_tools` for built-in tool names. |
| `disabled_tools` | Topic (L3) | Same list in the topic's own `.jyc/config.toml` — where a subagent type's `disabled_tools` lands, so a spawned role can be bounded structurally (e.g. `["write", "edit", "jyc_spawn_subagent"]` for the `review` role) instead of by instruction. Only the tools listed are affected: `bash` is a shell and can still write, so a role that keeps it (a reviewer needs `git diff`) leans on plan mode and its instructions for the rest. |

### Merge Behavior

Agent-level and topic-level exclusions are **additive** (merged,
not overriding):

```
Effective disabled_tools = agent.disabled_tools ∪ topic.disabled_tools
Effective disabled_mcps = agent.disabled_mcps ∪ topic.disabled_mcps
```

**Validation:** Empty string entries in exclusion lists are rejected at config load time.

### Examples

**Disable bash globally for an agent:**
```toml
[agents.jyc]
disabled_tools = ["bash"]
```

**Disable write/edit for a read-only topic** (in the topic's `.jyc/config.toml`):
```toml
disabled_tools = ["write", "edit"]
```

**Disable a specific MCP tool by server:**
```toml
[agents.jyc]
disabled_tools = ["jin_public_mcp/product_list"]
```

> **Note:** The `server_name/tool_name` format filters MCP tools **before** they are registered. Built-in and bridge tools should always use plain names (e.g. `"bash"`, not `"builtin/bash"`).

**Disable all MCP tools for an agent:**
```toml
[agents.jyc]
disabled_mcps = ["*"]  # Disables all external MCP servers
```

---

## Security Boundaries

| Tool | Boundary Check | Notes |
|------|---------------|-------|
| `bash` | `check_write_boundary()` — scans unquoted absolute-path tokens | Not a sandbox; OS-level isolation recommended for untrusted input |
| `read` | `check_path_boundary()` working_dir + temp dir + read_roots + write_roots | Symlink exemption for symlinked repos |
| `write` | `check_write_boundary()` working_dir + temp dir + write_roots | Creates parent dirs automatically |
| `edit` | `check_write_boundary()` working_dir + temp dir + write_roots | — |
| `grep` | `check_path_boundary()` only when explicit `path` provided | Default working_dir is trusted |
| `webfetch` | None (network tool) | HTTPS only; 30s default timeout |
| `read_image` | `check_path_boundary()` working_dir + temp dir + read_roots + write_roots | URL mode requires http(s) |
| `context_browse` | None (in-memory transcript only) | Read-only; never touches the filesystem |
| `jyc_send_message` | Recipient format validation | Channel-specific format check |
| `jyc_send_to_topic` | Attachment path validation | Must be within topic directory; a named topic must be owned by a channel that is running, can locate it, or is recorded in its state |

`check_path_boundary()` itself only iterates `additional_read_roots`; write paths
appear readable because `resolve_additional_read_roots()` merges `access.write`
into that list when building the context.

### System temp dir

`std::env::temp_dir()` is always within the read *and* write boundary, so tools
have scratch space without any configuration. The path is canonicalized before
comparison, since macOS reports the temp dir as `/var/folders/...` while a
resolved path arrives as `/private/var/folders/...`.

A temp dir of `/` is ignored — every absolute path is inside the root, so
honoring it would disable the boundary altogether. This matters because
`env::temp_dir()` returns `$TMPDIR` unvalidated on Unix, and jyc daemons
typically inherit operator shell environment variables via their service
unit file.

Because the system temp dir is shared and world-writable, this also makes other
processes' temp files readable. Use `access.read` / `access.write` for anything
that needs a private location.

Additional read/write roots are configured per-pattern via the `access` sub-table:

```toml
[channels.xxx.patterns.access]
read = ["~/.cargo/registry/src"]   # readable by all tools
write = ["/tmp/jyc-builds"]         # writable + readable (write implies read)
```

---

## File Publishing Tools

### `jyc_publish_file`

Publish a file from the topic directory to make it accessible via an exchange
HTTP link served by the inspect server.

**Parameters:**
- `path` (string, required): File to publish, relative to the topic directory
- `name` (string, optional): Published filename; defaults to the source basename
- `move` (boolean, optional, default false): When `true`, the file is moved
  (source disappears); when `false`, it is copied

**Behavior:**
- The file is placed in the topic's **state dir**: `<state dir>/exchange/<name>`
- On first publish a per-topic access token (256-bit hex) is generated and
  stored in `<state dir>/exchange-token` (owner-only permissions); subsequent
  publishes reuse it
- Returns a shareable URL:
  `{base}/exchange/{channel}/{topic}/{name}?token={token}` where `base` comes
  from `[inspect] base_url`. When unset, `base` falls back to
  `http://<inspect.bind>`, with a wildcard bind host (`0.0.0.0`, `[::]`)
  replaced by this host's primary LAN IP — a wildcard address is not reachable
  from a client. Behind a reverse proxy that guess cannot be right: set
  `base_url` to the URL clients actually use (scheme required; port
  and subpath allowed)
- Files are served by the inspect server at `GET /exchange/...`; the bearer
  middleware does not apply — the per-topic `?token=` is the access control
- `/reset` and `/new` delete the state dir's `exchange/` and `exchange-token`,
  killing all previously shared links (a fresh token is generated on the next
  publish)
- The `/exchange` command re-prints the URLs of everything already published in
  the topic (`/exchange <name>` for a single file), so links can be recovered
  without publishing again

**Constraints:**
- Source must be a file inside the topic working directory
- `name` must be a plain filename (no `/`, `\`, or `..`)

**Example:**
```json
{"path": "output/report.pdf", "move": true}
```

---

## Tool Registration Flow

```
build_tool_registry()
  ├─ Register built-in tools: bash, read, write, edit, grep, webfetch, context_browse
  ├─ Register read_image (when model supports images OR vision_client configured)
  ├─ Register jyc_publish_file (base URL from [inspect] base_url)
  ├─ Register MCP bridge tools: jyc_send_message
  ├─ Register jyc_send_to_topic (when the topic managers are available)
  ├─ Load external MCP tools (filtered by disabled_mcps); a tool named
  │  load_mcp_schema or load_extended_tools is skipped — the loaders keep
  │  their names
  ├─ Apply exclusions: remove tools matching disabled_tools
  ├─ Wire the MCP groups: one per server, holding the tools that landed in
  │  this registry — they are advertised only after load_mcp_schema expands
  │  them
  ├─ Register load_mcp_schema: its description is the index of the servers
  ├─ Wire the extended group: register load_extended_tools and mark the
  │  group's tools as advertised-only-after-loading
  └─ Apply the exclusions to the loaders as well: they are registered after
     the pass above, so disabled_tools may name load_extended_tools or
     load_mcp_schema
```

The final registry is what the LLM sees in its `tools` list — minus the
extended group, until the model loads it, and minus the MCP tools, until the
model expands their server.
