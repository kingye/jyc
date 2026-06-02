# JYC Tools Reference

This document lists all tools available to the AI agent in JYC, including built-in tools, MCP bridge tools, and externally configurable MCP tools.

**Audience:** AI agents (for understanding available capabilities) and developers/operators (for configuration and debugging).

---

## Tool Categories

| Category | Description | Configuration |
|----------|-------------|---------------|
| **Built-in** | Core file system and execution tools. Always available unless explicitly disabled. | Hardcoded in `jyc-agent` |
| **MCP Bridge** | In-process wrappers for JYC-specific MCP tools (reply, send message). | Always registered |
| **External MCP** | Optional tools provided by external MCP servers configured in `config.toml`. | `[[mcps]]` config |

---

## Built-in Tools

### `bash`

Execute shell commands in the working directory.

**Parameters:**
- `command` (string, required): The bash command to execute
- `timeout` (integer, optional): Timeout in seconds (default: 120)

**Security:** Best-effort path boundary check — scans for absolute-path tokens and verifies they are within `working_dir`. This is a heuristic, not a sandbox. Full isolation requires OS-level containment.

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

**Security:** Path must be within `working_dir` or `additional_read_roots`. Symlink exemption supported for `repo_group` setups.

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

**Security:** Path must be within `working_dir`.

**Example:**
```json
{"file_path": "src/lib.rs", "content": "pub fn hello() {}"}
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

**Security:** Path must be within `working_dir`.

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

### `jyc_reply_reply_message`

Send a reply back through the originating channel. This is the standard way for the agent to respond to the user.

**Parameters:**
- `message` (string, required): The reply text to send
- `attachments` (string[], optional): List of filenames within the thread directory to attach

**Behavior:** Writes `reply.md` and `reply-sent.flag` signal files; the monitor process detects the signal and delivers the message via the pre-warmed outbound adapter.

**Constraints:** The agent must use this tool for in-thread replies and stop immediately after calling it.

**Example:**
```json
{"message": "The fix has been applied. Let me know if you see any issues."}
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
- NEVER use for in-thread replies (use `jyc_reply_reply_message` instead)
- Requires a pre-warmed outbound adapter (`ToolContext.outbound`)

**Example:**
```json
{"recipient": "wecomkf:kf001:wmE8OcHAAA...", "subject": "System Alert", "message": "Disk usage exceeded 90%"}
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
| `disabled_tools` | Channel / Pattern | List of tool names to remove. Matches built-in tools, MCP bridge tools, and external MCP tools by registration name. |
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
| `bash` | Best-effort absolute-path heuristic | Not a sandbox; OS-level isolation recommended for untrusted input |
| `read` | `check_path_boundary()` working_dir + additional_read_roots | Symlink exemption for repo_group |
| `write` | `check_path_boundary()` working_dir only | Creates parent dirs automatically |
| `edit` | `check_path_boundary()` working_dir only | — |
| `glob` | `check_path_boundary()` only when explicit `path` provided | Default working_dir is trusted |
| `grep` | `check_path_boundary()` only when explicit `path` provided | Default working_dir is trusted |
| `webfetch` | None (network tool) | HTTPS only; 30s default timeout |
| `read_image` | `check_path_boundary()` working_dir + additional_read_roots | URL mode requires http(s) |
| `jyc_reply_reply_message` | Attachment path validation | Must be within thread directory |
| `jyc_send_message` | Recipient format validation | Channel-specific format check |

---

## Tool Registration Flow

```
build_tool_registry()
  ├─ Register built-in tools: bash, read, write, edit, glob, grep, webfetch
  ├─ Register read_image (when model supports images OR vision_client configured)
  ├─ Register MCP bridge tools: jyc_reply_reply_message, jyc_send_message
  ├─ Load external MCP tools (filtered by disabled_mcp_servers)
  └─ Apply exclusions: remove tools matching disabled_tools
```

The final registry is what the LLM sees in its `tools` list.
