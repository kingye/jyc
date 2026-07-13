# JYC Tools Reference

This document lists all tools available to the AI agent in JYC, including built-in tools, MCP bridge tools, and externally configurable MCP tools.

**Audience:** AI agents (for understanding available capabilities) and developers/operators (for configuration and debugging).

---

## Tool Categories

| Category | Description | Configuration |
|----------|-------------|---------------|
| **Built-in** | Core file system and execution tools. Always available unless explicitly disabled. | Hardcoded in `jyc-agent` |
| **MCP Bridge** | In-process wrappers for JYC-specific MCP tools (reply, send message). | Always registered |
| **Cross-Thread** | Cross-channel thread communication tool. | Registered when cross-channel `thread_managers` available |
| **External MCP** | Optional tools provided by external MCP servers configured in `config.toml`. | `[[mcps]]` config |

---

## Built-in Tools

### `bash`

Execute shell commands in the working directory.

**Parameters:**
- `command` (string, required): The bash command to execute
- `timeout` (integer, optional): Timeout in seconds (default: 120)

**Security:** Best-effort path boundary check — scans for absolute-path tokens (outside quoted strings) and verifies they are within `working_dir` or configured read/write roots (`access.read` / `access.write`). This is a heuristic, not a sandbox. Full isolation requires OS-level containment.

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

**Security:** Path must be within `working_dir`, `additional_read_roots`, or `additional_write_roots`. Symlink exemption supported for `repo_group` setups.

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

**Limits:** Response truncated at 512 KB. User-agent: `jyc-agent/0.1`.

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

## MCP Bridge Tools

These are JYC-specific tools implemented as in-process bridges (not external MCP subprocesses). They are always registered unless excluded via `disabled_tools`.

### `jyc_reply_message`

Send a reply back through the originating channel. This is the standard way for the agent to respond to the user.

**Parameters:**
- `message` (string, required): The reply text to send
- `attachments` (string[], optional): List of filenames within the thread directory to attach
- `stop_after` (boolean, optional, default true): Whether to stop working after this reply

**Behavior:** Writes `reply.md` and `reply-sent.flag` signal files; the monitor process detects the signal and delivers the message via the pre-warmed outbound adapter.

**Usage modes:**
- **Final reply** (`stop_after: true` or omitted): Agent stops immediately after sending. Use for the definitive response to the user.
- **Progress update** (`stop_after: false`): Agent sends the message as a checkpoint and continues working. Use for long-running tasks to keep the user informed.

**Constraints:** The agent must use this tool for in-thread replies.

**Examples:**
```json
{"message": "The fix has been applied. Let me know if you see any issues."}
```
```json
{"message": "Still working — 3 of 5 tests passing. Will continue.", "stop_after": false}
```

---

### `jyc_send_message`

Send a proactive out-of-thread message to an arbitrary recipient.

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
- NEVER use for in-thread replies (use `jyc_reply_message` instead)
- Requires a pre-warmed outbound adapter (`ToolContext.outbound`)

**Example:**
```json
{"recipient": "wecomkf:kf001:wmE8OcHAAA...", "subject": "System Alert", "message": "Disk usage exceeded 90%"}
```

---

## Cross-Thread Communication Tools

### `jyc_send_to_thread`

Send a message to a thread in another channel for agent processing. The target thread will be auto-created if it doesn't exist.

Unlike `jyc_send_message` (which bypasses agent processing for direct outbound delivery), `jyc_send_to_thread` injects the message into the target thread's queue so the target thread's agent picks it up and processes it.

**Parameters:**
- `channel` (string, required): Target channel name (e.g. `"jin283"`, `"feishu_bot"`)
- `thread` (string, required): Target thread name (e.g. `"invoice-processing"`)
- `message` (string, required): Message body to inject into the target thread
- `attachments` (string[], optional): List of filenames within the current thread directory to attach
- `recipient` (string, optional): Recipient address/ID. Sets the sender_address on the injected message for channel-appropriate reply routing
- `require_reply` (boolean, optional, default false): When `true`, the target agent is instructed to send results back to the source channel/thread via `jyc_send_to_thread`

**Behavior:**
- Looks up the target channel's `ThreadManager` from the shared cross-channel map
- Builds an `InboundMessage` with source metadata (`source_channel`, `source_thread`, `require_reply`)
- Enqueues the message into the target thread's queue
- The target agent sees a `**Source:**` header in its incoming message prompt

**`require_reply` flow:**
- When `true`, the target agent's prompt includes: `⚠️ Reply requested - use jyc_send_to_thread to send results back`
- The target agent should call `jyc_send_to_thread` back to the source channel/thread when done

**Constraints:**
- Requires cross-channel thread managers (`ToolContext.thread_managers`)
- Attachments must be within the current thread's working directory

**Example:**
```json
{
  "channel": "jin283",
  "thread": "invoice-processing",
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
| `read` | `check_path_boundary()` working_dir + read_roots + write_roots | Symlink exemption for repo_group |
| `write` | `check_write_boundary()` working_dir + write_roots | Creates parent dirs automatically |
| `edit` | `check_write_boundary()` working_dir + write_roots | — |
| `glob` | `check_path_boundary()` only when explicit `path` provided | Default working_dir is trusted |
| `grep` | `check_path_boundary()` only when explicit `path` provided | Default working_dir is trusted |
| `webfetch` | None (network tool) | HTTPS only; 30s default timeout |
| `read_image` | `check_path_boundary()` working_dir + read_roots + write_roots | URL mode requires http(s) |
| `jyc_reply_message` | Attachment path validation | Must be within thread directory |
| `jyc_send_message` | Recipient format validation | Channel-specific format check |
| `jyc_send_to_thread` | Attachment path validation | Must be within thread directory; target channel must exist |

Read/write roots are configured per-pattern via the `access` sub-table:

```toml
[channels.xxx.patterns.access]
read = ["~/.cargo/registry/src"]   # readable by all tools
write = ["/tmp/jyc-builds"]         # writable + readable (write implies read)
```

---

## Tool Registration Flow

```
build_tool_registry()
  ├─ Register built-in tools: bash, read, write, edit, glob, grep, webfetch
  ├─ Register read_image (when model supports images OR vision_client configured)
  ├─ Register MCP bridge tools: jyc_reply_message, jyc_send_message
  ├─ Register jyc_send_to_thread (when cross-channel thread_managers available)
  ├─ Load external MCP tools (filtered by disabled_mcp_servers)
  └─ Apply exclusions: remove tools matching disabled_tools
```

The final registry is what the LLM sees in its `tools` list.
