# JYC

Channel-agnostic AI agent that operates through messaging channels. Users interact by sending messages (Email, GitHub, FeiShu, etc.), and the agent responds autonomously using the configured AI model.

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

These tools are used by the AI agent when processing messages. Install them on the server where JYC runs:

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
# Generate a config template (in the platform config dir, see below)
./target/release/jyc config init

# Edit the config (Linux: ~/.config/jyc/config.toml)
vi ~/.config/jyc/config.toml

# Validate
./target/release/jyc config validate
```

See `config.example.toml` for a full annotated example. Use `${ENV_VAR}` syntax for secrets (passwords, API keys).

### 3. Run

```bash
./target/release/jyc serve
```

On first run without a config, `jyc serve` creates a default `config.toml` (plus empty `skills/` and `templates/` directories) in the platform config dir and exits — edit the file, then start again.

Add `--debug` for debug-level logging or `--verbose` for trace-level.

## Configuration & Data Layout

JYC separates **user-edited configuration** from **generated data**, following platform conventions:

| Platform | Config dir (L1) | Data dir (default workdir, L2) |
|---|---|---|
| Linux/macOS | `$XDG_CONFIG_HOME/jyc` (`~/.config/jyc`) | `$XDG_DATA_HOME/jyc` (`~/.local/share/jyc`) |
| Windows | `%APPDATA%\jyc` | `%LOCALAPPDATA%\jyc` |

Three-level layering applies to `config.toml`, `skills/`, and `templates/`:

- **L1 (global)** — `<config dir>/`: shared `config.toml`, `skills/`, `templates/`
- **L2 (workdir / data root)** — `--workdir` if given, else the data dir: its own `config.toml` (`--config`), `skills/`, `templates/`, and all generated state (`<channel>/.imap/`, `<channel>/.github/`, `<channel>/workspace/<thread>/`)
- **L3 (thread)** — `<thread_path>/.jyc/`: `config.toml` (restricted `[agent]` model overrides), `skills/`, `templates/`, sessions, chat history

Merge/lookup rules:

- **config.toml**: L2 is deep-merged over L1 (tables merge recursively, arrays/scalars are replaced). L3 only supports `[agent]` model overrides. Model precedence: `.jyc/<mode>-model-override` file > L3 `config.toml` > pattern > L2/L1 config.
- **skills**: all levels are scanned; higher levels override same-named skills.
- **templates**: looked up L3 → L2 → L1; first match wins.
- A pattern's custom `thread_path` (absolute or `~`) lives outside the data root; relative paths resolve against the data root. L3 applies to any thread directory, including ad-hoc ones (`jyc open <path>`).

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
- **Features:** Issue/PR comments, label-based routing, multi-agent workflow
- **Protocols:** REST API polling (inbound), REST API (outbound)
- **Authentication:** Personal Access Token (PAT)
- **Agents:** Planner, Developer, Reviewer templates for full PR workflow

### ✅ Feishu (飞书/Lark)
- **Status:** Production ready (implemented in Phase 7)
- **Features:** Real-time messaging via WebSocket, rich message formatting
- **API:** REST API with openlark SDK + WebSocket for real-time updates
- **Authentication:** App credentials with automatic token refresh
- **Formats:** Markdown, text, HTML, and rich interactive messages

### ✅ WeChat
- **Status:** Implemented in v0.3.9
- **Features:** Single-bot, single-thread messaging via OpenILink Bridge WebSocket
- **Protocols:** WebSocket for inbound and outbound
- **Attachments:** Images, files, voice, and video

### ✅ WeCom (企业微信)
- **Status:** Implemented in v0.3.10
- **Features:** Bot webhook inbound, external-contact API outbound with `corp_id` + `corp_secret` authentication
- **Protocols:** Shared axum HTTP server (inbound), REST API (outbound)
- **Security:** AES-256-CBC decryption, SHA1 signature verification

### ✅ WeCom KF (Customer Service)
- **Status:** Implemented in v0.3.10
- **Features:** Customer-service messaging via event notifications and `kf/sync_msg` API pull
- **Protocols:** Webhook events (inbound), REST API (outbound)
- **Model:** One thread per customer per KF account

### ✅ WeCom Smart Robot (wecom_bot)
- **Status:** Implemented in v0.3.11
- **Features:** Smart Robot messaging via persistent WebSocket, streaming replies, and outbound attachment upload
- **Protocols:** WebSocket long connection for both inbound and outbound
- **Authentication:** Bot ID + long-connection secret
- **Attachments:** File, image, voice, and video upload via WebSocket media upload protocol

### ✅ Gitee
- **Status:** Implemented in v0.3.10
- **Features:** Multi-agent workflow on Gitee issues and Pull Requests
- **Protocols:** REST API v5 polling (inbound), REST API (outbound)
- **Agents:** Planner, Developer, Reviewer templates

### ✅ WebSocket
- **Status:** Production ready (implemented in v0.3.12)
- **Features:** Interactive chat pane in `jyc dashboard`, multi-client support via broadcast
- **Protocols:** WebSocket server runs inside `jyc serve`, dashboard clients connect via `ws://`
- **Usage:** Press `c` in dashboard to toggle chat pane

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

### Global Flags

```bash
-w, --workdir <PATH>   # Working directory / data root (default: platform data dir,
                       #   e.g. ~/.local/share/jyc on Linux)
-d, --debug            # Enable debug logging
-v, --verbose         # Enable verbose (trace) logging
```

### Subcommands

```bash
jyc serve              # Start the agent (main command)
                       #   --config <FILE>    Config file path (default:
                       #     <config dir>/config.toml, or config.toml in --workdir)
                       #   --no-idle         Use polling instead of IMAP IDLE
                       #   --reset           Reset monitoring state before starting
jyc dashboard            # Live TUI dashboard (connects via inspect server)
                         #   --addr <ADDR>     Inspect server address (default: 127.0.0.1:9876)
                         #                     Also used for WebSocket chat on /ws
                         #   Keyboard: q=quit, ↑/↓=select thread, r=refresh, c=chat pane
jyc open                 # Create a new ad-hoc websocket thread and open chat
                         #   (shortcut for `jyc dashboard open`)
                         #   -t, --thread <NAME>   Thread name (default: folder name of -p or CWD)
                         #   -p, --path <PATH>     Thread working directory (default: CWD)
                         #   -c, --channel <NAME>  Websocket channel (auto-detected if only one)
                         #   --addr <ADDR>        Inspect server address (default: 127.0.0.1:9876)
jyc config init        # Generate config template (in <config dir>, or --workdir)
jyc config validate    # Validate config file (layered: global + workdir)
                       #   --config <FILE>   Config file path (default: as serve)
jyc patterns list      # List configured patterns
                       #   --config <FILE>   Config file path (default: as serve)
jyc templates list     # List available templates and their skills
                       #   --source-dir <PATH>   Source directory containing templates/
jyc templates deploy <target_dir>   # Deploy templates to a target directory
                       #   <template_name>       Deploy only this template (optional)
                       #   --as <NAME>           Rename the deployed template directory
                       #   --model <MODEL>       Write a model override file
                       #   --source-dir <PATH>   Source directory containing templates/
```

The `dashboard` command requires the `[inspect]` section to be enabled in config.

## MCP Tools

JYC provides several MCP (Model Context Protocol) tools that the AI agent uses internally:

| Tool | Description |
|------|-------------|
| `reply_message` | Send reply via the channel's outbound adapter. Reads routing info from `reply-context.json`, appends to chat log, writes signal file for delivery. |
| `jyc_send_message` | Send proactive out-of-thread messages to any recipient via the pre-warmed outbound adapter. Used for alerts and notifications only, not for in-thread replies. |
| `analyze_image` | Analyze images using an OpenAI-compatible vision API. Accepts absolute file paths or HTTP(S) URLs. Configure via `[[mcps]]` in `config.toml` (see `config.example.toml`). |
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
- **`[vision]`** -- DEPRECATED: Vision is now configured via `[[mcps]]` (see `config.example.toml` for the new approach)
- **`[attachments]`** -- Inbound/outbound attachment settings

Per-pattern options such as `thread_path` (custom thread directory), `model`
(per-pattern model override), `access` (filesystem whitelist), and `mcps`
(per-pattern MCP tools) are configured under `[[channels.<name>.patterns]]`.
See `config.example.toml` for annotated examples.

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
jyc serve --workdir /path/to/data --debug

# Trace level (very verbose)
jyc serve --workdir /path/to/data --verbose

# Or use RUST_LOG for fine-grained control
RUST_LOG=jyc=debug,async_imap=warn jyc serve --workdir /path/to/data
```

### Checking MCP Reply Tool Logs

The MCP reply tool (subprocess spawned by the agent) logs to a per-thread file:

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
- Check JYC logs for AI provider/API errors
- Check the MCP reply tool log (`.jyc/reply-tool.log` in the thread directory)
- Verify the `[agent]` section in `config.toml` has valid API credentials

**Session/context issues:**
- Send `/reset` in an email to clear the AI session for that thread
- Or manually delete `.jyc/agent-session.json` in the thread directory

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
