# JYC

Channel-agnostic AI agent that operates through messaging channels. Users interact by sending messages (email, FeiShu, Slack, etc.), and the agent responds autonomously using OpenCode AI.

**Why Rust:** Single static binary, zero runtime dependencies, memory safety without GC, and predictable low-latency performance for long-running server processes.

## Prerequisites

### Build Dependencies

- **Rust** (stable toolchain): https://rustup.rs
- **protobuf-compiler** (required for Feishu WebSocket support):
  ```bash
  # Debian/Ubuntu
  sudo apt-get install -y protobuf-compiler

  # macOS
  brew install protobuf

  # Verify
  protoc --version
  ```

### Runtime Dependencies (Optional)

These tools are used by the AI agent (via OpenCode) when processing messages. Install them on the server where JYC runs:

```bash
# Debian/Ubuntu
sudo apt-get install -y \
  curl \            # Web requests (weather, APIs, etc.)
  pandoc \          # HTML ↔ Markdown conversion
  jq \              # JSON processing
  ripgrep \         # Fast code search
  git               # Version control operations
```

```bash
# macOS
brew install curl pandoc jq ripgrep git
```

Without these, the AI will still work but may fall back to less efficient methods (e.g., manually parsing HTML instead of using `pandoc`).

## Quick Start

### 1. Build

```bash
cargo build --release
```

### 2. Configure

```bash
# Generate a config template
./target/release/jyc config init --workdir /path/to/data

# Edit the config
vi /path/to/data/config.toml

# Validate
./target/release/jyc config validate --workdir /path/to/data
```

See `config_template.toml` for a full annotated example. Use `${ENV_VAR}` syntax for secrets (passwords, API keys).

### 3. Run

```bash
./target/release/jyc monitor --workdir /path/to/data
```

Add `--debug` for debug-level logging or `--verbose` for trace-level.

## Deployment

JYC supports two deployment modes:

| Mode | Docs | Use Case |
|------|------|----------|
| **systemd** | [SYSTEMD.md](SYSTEMD.md) | Native Linux, minimal overhead |
| **Docker** | [docker/README.md](docker/README.md) | Containerized, isolated environment |

Both support automatic restarts and AI self-bootstrapping (the AI can rebuild and redeploy JYC from source).

## Supported Channels

JYC is designed to be channel-agnostic. Currently implemented channels:

### ✅ Email (IMAP/SMTP)
- **Status:** Production ready
- **Features:** Full email support with threading, attachments, and HTML formatting
- **Protocols:** IMAP for inbound, SMTP for outbound
- **Authentication:** TLS/SSL with username/password or OAuth2

### ✅ GitHub
- **Status:** Production ready (implemented in v0.1.10)
- **Features:** Issue/PR comments, label routing, @j:role mention routing, multi-agent workflow
- **Protocols:** REST API polling (inbound), REST API (outbound)
- **Authentication:** Personal Access Token (PAT)
- **Agents:** Planner, Developer, Reviewer templates for full PR workflow

### ✅ Feishu (飞书/Lark)
- **Status:** Production ready (implemented in Phase 7)
- **Features:** Real-time messaging via WebSocket, rich message formatting
- **API:** REST API with openlark SDK + WebSocket for real-time updates
- **Authentication:** App credentials with automatic token refresh
- **Formats:** Markdown, text, HTML, and rich interactive messages

### 🔄 Future Channels (Planned)
- **Slack:** WebHook and Socket Mode support
- **Teams:** Microsoft Teams integration
- **Discord:** Discord bot integration
- **Custom:** WebHook API for custom integrations

The channel-agnostic architecture makes it easy to add new channels by implementing the `InboundAdapter` and `OutboundAdapter` traits.

## Usage

### Email Commands

Send commands at the top of an email body. These commands work across all channels (Email, Feishu, GitHub).

| Command | Description |
|---------|-------------|
| `/model <id>` | Switch AI model for this thread |
| `/model` | List available models |
| `/model reset` | Reset to default model |
| `/plan` | Switch to plan mode (read-only) |
| `/build` | Switch to build mode (default) |
| `/reset` | Clear AI session (start fresh conversation) |
| `/close` | Close thread and delete directory |
| `/template` | Apply template files to thread (skip existing) |
| `/template update` | Re-apply template, overwrite existing files |

### Thread-Specific Customization

Place a `system.md` file in a thread's workspace directory to customize the AI's behavior for that thread. See `system.md.example` for a reference.

## CLI Commands

```bash
jyc monitor          # Start the agent (main command)
jyc config init      # Generate config template
jyc config validate  # Validate config file
jyc patterns list    # List configured patterns
jyc state            # Show monitoring state (processed UIDs, etc.)
jyc dashboard        # Live TUI dashboard (connects via inspect server)
```

The `dashboard` command requires the `[inspect]` section to be enabled in config. Use `--addr` flag to connect to a different address (default: `127.0.0.1:9876`).

## MCP Tools

JYC provides several MCP (Model Context Protocol) tools that the AI agent uses internally:

| Tool | Description |
|------|-------------|
| `reply_message` | Send reply via the channel's outbound adapter. Reads routing info from `reply-context.json`, appends to chat log, writes signal file for delivery. |
| `analyze_image` | Analyze images using an OpenAI-compatible vision API. Accepts absolute file paths or HTTP(S) URLs. Configure via `[vision]` section. |
| `ask_user` | Ask the user a question and wait for their reply (up to 5 minutes). The question is delivered immediately via background delivery watcher. |

These are internal tools used by the AI, not user-facing commands.

## Configuration

JYC uses TOML configuration with environment variable substitution (`${VAR}`).

Key sections:

- **`[general]`** -- Concurrency settings (max threads, queue size)
- **`[channels.<name>]`** -- Per-channel config (type, patterns)
- **`[channels.<name>.email]`** -- IMAP/SMTP settings (host, port, credentials)
- **`[channels.<name>.feishu]`** -- Feishu app credentials (app_id, app_secret, websocket)
- **`[channels.<name>.github]`** -- GitHub settings (owner, repo, token, poll_interval)
- **`[channels.<name>.agent]`** -- Per-channel agent override (model, system prompt)
- **`[agent]`** -- AI agent settings (model, system prompt, progress updates)
- **`[inspect]`** -- Inspect server settings (enabled, bind address)
- **`[vision]`** -- Vision API settings (enabled, api_key, api_url, model)
- **`[heartbeat]`** -- Heartbeat settings (enabled, interval_secs, min_elapsed_secs)
- **`[attachments]`** -- Inbound/outbound attachment settings

See [DESIGN.md](DESIGN.md) for full configuration reference and architecture details.

## Troubleshooting

### Checking JYC Logs

JYC logs to stderr via the `tracing` framework. Where you find the logs depends on your deployment:

**systemd:**
```bash
# Follow logs live
journalctl --user -u jyc -f

# Last 100 lines
journalctl --user -u jyc -n 100

# Since last boot
journalctl --user -u jyc -b

# Filter by level (grep for ERROR/WARN)
journalctl --user -u jyc --no-pager | grep ERROR
```

**Docker:**
```bash
docker compose logs -f jyc
# or
podman logs -f jyc
```

**Direct (foreground):**
```bash
# Debug level
jyc monitor --workdir /path/to/data --debug

# Trace level (very verbose)
jyc monitor --workdir /path/to/data --verbose

# Or use RUST_LOG for fine-grained control
RUST_LOG=jyc=debug,async_imap=warn jyc monitor --workdir /path/to/data
```

### Checking OpenCode Logs

OpenCode (the AI runtime JYC uses) writes its own logs separately from JYC.

**Log location:**
```
~/.local/share/opencode/log/
```

In Docker, this is persisted via the OpenCode data volume mount:
```
/root/.local/share/opencode/log/
```

Log files are named with timestamps (e.g., `2025-01-09T123456.log`). The most recent 10 log files are kept.

```bash
# List OpenCode log files (most recent last)
ls -lt ~/.local/share/opencode/log/

# View the latest log
ls -t ~/.local/share/opencode/log/*.log | head -1 | xargs tail -f

# In Docker
docker exec -it jyc ls -lt /root/.local/share/opencode/log/
docker exec -it jyc tail -f "$(docker exec jyc ls -t /root/.local/share/opencode/log/*.log | head -1)"
```

**OpenCode session data** (sessions, messages) is stored at:
```
~/.local/share/opencode/project/
```

### Checking MCP Reply Tool Logs

The MCP reply tool (subprocess spawned by OpenCode) logs to a per-thread file:

```
<workdir>/<channel>/workspace/<thread>/.jyc/reply-tool.log
```

This is useful for diagnosing reply delivery failures.

### Common Issues

**JYC starts but no emails are processed:**
- Check pattern matching: `jyc patterns list --workdir /path/to/data`
- Verify IMAP connection in logs (look for `IMAP connected and authenticated`)
- Check that sender/subject rules match incoming emails

**AI replies are not sent:**
- Check OpenCode logs for API errors (`~/.local/share/opencode/log/`)
- Check the MCP reply tool log (`.jyc/reply-tool.log` in the thread directory)
- Verify OpenCode config has valid API keys (`~/.config/opencode/opencode.jsonc`)

**Session/context issues:**
- Send `/reset` in an email to clear the AI session for that thread
- Or manually delete `.jyc/opencode-session.json` in the thread directory

**OpenCode server won't start:**
- Check that `opencode` is installed and in PATH
- Check OpenCode logs at `~/.local/share/opencode/log/`
- Try running `opencode --version` to verify installation

**Container-specific issues:**
- See [docker/README.md](docker/README.md) troubleshooting section

## Documentation

| Document | Purpose |
|----------|---------|
| [DESIGN.md](DESIGN.md) | Architecture, data flow, component design, API reference |
| [IMPLEMENTATION.md](IMPLEMENTATION.md) | Implementation phases and progress |
| [CHANGELOG.md](CHANGELOG.md) | Version history |
| [SYSTEMD.md](SYSTEMD.md) | systemd deployment and service management |
| [docker/README.md](docker/README.md) | Docker/Podman deployment |
