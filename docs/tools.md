# JYC Tools Reference

This document lists all tools available to the AI agent in JYC, including built-in tools, MCP bridge tools, and externally configurable MCP tools.

**Audience:** AI agents (for understanding available capabilities) and developers/operators (for configuration and debugging).

---

## Tool Categories

| Category | Description | Configuration |
|----------|-------------|---------------|
| **Built-in** | Core file system and execution tools. Always available unless explicitly disabled. | Hardcoded in `jyc-agent` |
| **MCP Bridge** | In-process wrappers for JYC-specific MCP tools (reply, send message). | Always registered |
| **Cross-Topic** | Cross-channel topic communication tool. | Registered when cross-channel `topic_managers` available |
| **External MCP** | Optional tools provided by external MCP servers configured in `config.toml`. | `[[mcps]]` config |

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

### `glob`

Find files matching a glob pattern.

**Parameters:**
- `pattern` (string, required): Glob pattern (e.g., `**/*.rs`, `src/**/*.ts`)
- `path` (string, optional): Directory to search in (default: working directory)

**Security:** If `path` is explicitly provided, it must be within `working_dir`.

**Example:**
```json
{"pattern": "**/*.rs", "path": "crates"}
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

## MCP Bridge Tools

These are JYC-specific tools implemented as in-process bridges (not external MCP subprocesses). They are always registered unless excluded via `disabled_tools`.

### `jyc_reply_message`

Send a reply back through the originating channel. This is the standard way for the agent to respond to the user.

**Parameters:**
- `message` (string, required): The reply text to send
- `attachments` (string[], optional): List of filenames within the topic directory to attach
- `stop_after` (boolean, optional, default true): Whether to stop working after this reply

**Behavior:** Writes `reply.md` and `reply-sent.flag` signal files; the monitor process detects the signal and delivers the message via the pre-warmed outbound adapter.

**Usage modes:**
- **Final reply** (`stop_after: true` or omitted): Agent stops immediately after sending. Use for the definitive response to the user.
- **Progress update** (`stop_after: false`): Agent sends the message as a checkpoint and continues working. Use for long-running tasks to keep the user informed.

**Constraints:** The agent must use this tool for in-topic replies.

**Examples:**
```json
{"message": "The fix has been applied. Let me know if you see any issues."}
```
```json
{"message": "Still working — 3 of 5 tests passing. Will continue.", "stop_after": false}
```

---

### `jyc_send_message`

Send a proactive out-of-topic message to an arbitrary recipient.

**Parameters:**
- `recipient` (string, required): Channel-specific recipient identifier
- `message` (string, required): Message body to send
- `subject` (string, optional): Message subject (channel-dependent)

**Recipient Format:**
- WeCom KF: `wecomkf:{open_kfid}:{external_userid}`
- Email: `user@example.com`
- Other channels: channel-specific format

**Constraints:**
- Use ONLY for alerts and notifications
- NEVER use for in-topic replies (use `jyc_reply_message` instead)
- Requires a pre-warmed outbound adapter (`ToolContext.outbound`)

**Example:**
```json
{"recipient": "wecomkf:kf001:wmE8OcHAAA...", "subject": "System Alert", "message": "Disk usage exceeded 90%"}
```

---

## Cross-Topic Communication Tools

### `jyc_send_to_topic`

Send a message to a topic in another channel for agent processing. The target topic will be auto-created if it doesn't exist.

Unlike `jyc_send_message` (which bypasses agent processing for direct outbound delivery), `jyc_send_to_topic` injects the message into the target topic's queue so the target topic's agent picks it up and processes it.

**Parameters:**
- `channel` (string, required): Target channel name (e.g. `"jin283"`, `"feishu_bot"`)
- `topic` (string, required): Target topic name (e.g. `"invoice-processing"`)
- `message` (string, required): Message body to inject into the target topic
- `attachments` (string[], optional): List of filenames within the current topic directory to attach
- `recipient` (string, optional): Recipient address/ID. Sets the sender_address on the injected message for channel-appropriate reply routing
- `require_reply` (boolean, optional, default false): When `true`, the target agent is instructed to send results back to the source channel/topic via `jyc_send_to_topic`

**Behavior:**
- Looks up the target channel's `TopicManager` from the shared cross-channel map
- Builds an `InboundMessage` with source metadata (`source_channel`, `source_topic`, `require_reply`)
- Enqueues the message into the target topic's queue
- The target agent sees a `**Source:**` header in its incoming message prompt

**`require_reply` flow:**
- When `true`, the target agent's prompt includes: `⚠️ Reply requested - use jyc_send_to_topic to send results back`
- The target agent should call `jyc_send_to_topic` back to the source channel/topic when done

**Constraints:**
- Requires cross-channel topic managers (`ToolContext.topic_managers`)
- Attachments must be within the current topic's working directory

**Example:**
```json
{
  "channel": "jin283",
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

---

## Tool Exclusion

Tools can be disabled at channel and pattern levels.

### Configuration

```toml
[channels.email]
type = "email"
# Disable tools for ALL patterns in this channel
disabled_tools = ["bash", "write"]
disabled_mcp_servers = ["my-mcp-server"]

[[channels.email.patterns]]
name = "readonly"
# Additional exclusions merged with channel-level (additive)
disabled_tools = ["edit"]
disabled_mcp_servers = []
```

### Fields

| Field | Scope | Description |
|-------|-------|-------------|
| `disabled_tools` | Channel / Pattern | List of tool names to remove. Matches built-in tools, MCP bridge tools, and external MCP tools by registration name. **External MCP tools can also be targeted as `server_name/tool_name` for precise exclusion when multiple servers provide the same tool name.** |
| `disabled_mcp_servers` | Channel / Pattern | List of MCP server names to skip during tool loading. |
| `disabled_builtin_tools` | Pattern only | **Backward-compatible alias.** Merged into `disabled_tools` for built-in tool names. |

### Merge Behavior

Channel-level and pattern-level exclusions are **additive** (merged, not overriding):

```
Effective disabled_tools = channel.disabled_tools ∪ pattern.disabled_tools
Effective disabled_mcp_servers = channel.disabled_mcp_servers ∪ pattern.disabled_mcp_servers
```

**Validation:** Empty string entries in exclusion lists are rejected at config load time.

### Examples

**Disable bash globally for a channel:**
```toml
[channels.github]
type = "github"
disabled_tools = ["bash"]
```

**Disable write/edit for a read-only pattern:**
```toml
[[channels.email.patterns]]
name = "readonly"
disabled_tools = ["write", "edit"]
```

**Disable a specific MCP tool by server:**
```toml
[[channels.email.patterns]]
name = "selective"
disabled_tools = ["jin_public_mcp/product_list"]
```

> **Note:** The `server_name/tool_name` format filters MCP tools **before** they are registered. Built-in and bridge tools should always use plain names (e.g. `"bash"`, not `"builtin/bash"`).

**Disable all MCP tools for a pattern:**
```toml
[[channels.email.patterns]]
name = "no-external"
disabled_mcp_servers = ["*"]  # Disables all external MCP servers
```

---

## Security Boundaries

| Tool | Boundary Check | Notes |
|------|---------------|-------|
| `bash` | `check_write_boundary()` — scans unquoted absolute-path tokens | Not a sandbox; OS-level isolation recommended for untrusted input |
| `read` | `check_path_boundary()` working_dir + temp dir + read_roots + write_roots | Symlink exemption for symlinked repos |
| `write` | `check_write_boundary()` working_dir + temp dir + write_roots | Creates parent dirs automatically |
| `edit` | `check_write_boundary()` working_dir + temp dir + write_roots | — |
| `glob` | `check_path_boundary()` only when explicit `path` provided | Default working_dir is trusted |
| `grep` | `check_path_boundary()` only when explicit `path` provided | Default working_dir is trusted |
| `webfetch` | None (network tool) | HTTPS only; 30s default timeout |
| `read_image` | `check_path_boundary()` working_dir + temp dir + read_roots + write_roots | URL mode requires http(s) |
| `context_browse` | None (in-memory transcript only) | Read-only; never touches the filesystem |
| `jyc_reply_message` | Attachment path validation | Must be within topic directory |
| `jyc_send_message` | Recipient format validation | Channel-specific format check |
| `jyc_send_to_topic` | Attachment path validation | Must be within topic directory; target channel must exist |

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
`env::temp_dir()` returns `$TMPDIR` unvalidated on Unix, and the systemd unit in
`SYSTEMD.md` sources the operator's shell environment.

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
- The file is placed in `<topic>/.jyc/exchange/<name>`
- On first publish a per-topic access token (256-bit hex) is generated and
  stored in `<topic>/.jyc/exchange-token` (owner-only permissions); subsequent
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
- `/reset` and `/new` delete `.jyc/exchange/` and `.jyc/exchange-token`, killing all
  previously shared links (a fresh token is generated on the next publish)
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
  ├─ Register built-in tools: bash, read, write, edit, glob, grep, webfetch, context_browse
  ├─ Register read_image (when model supports images OR vision_client configured)
  ├─ Register jyc_publish_file (base URL from [inspect] base_url)
  ├─ Register MCP bridge tools: jyc_reply_message, jyc_send_message
  ├─ Register jyc_send_to_topic (when cross-channel topic_managers available)
  ├─ Load external MCP tools (filtered by disabled_mcp_servers)
  └─ Apply exclusions: remove tools matching disabled_tools
```

The final registry is what the LLM sees in its `tools` list.
