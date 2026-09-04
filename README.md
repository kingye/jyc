# JYC

[![CI](https://github.com/kingye/jyc/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/kingye/jyc/actions/workflows/ci.yml)
[![Overview](https://img.shields.io/badge/overview-kingye.github.io%2Fjyc-ffb02e)](https://kingye.github.io/jyc/)

Channel-agnostic AI agent that operates through messaging channels. Users interact by sending messages (Email, GitHub, FeiShu, etc.), and the agent responds autonomously using the configured AI model.

📖 **[Project overview](https://kingye.github.io/jyc/)** — architecture, channels, runtime model and configuration at a glance.

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
- **L2 (workdir / data root)** — `--workdir` if given, else the data dir: its own `config.toml` (`--config`), `skills/`, `templates/`, and all generated state (`channels/<channel>/.imap/`, `<channel>/.github/`, `<channel>/workspace/<topic>/`)
- **L3 (topic)** — `<topic_path>/.jyc/`: `config.toml` (restricted `[agent]` model overrides), `skills/`, `templates/`, sessions, chat history

Merge/lookup rules:

- **config.toml**: L2 is deep-merged over L1 (tables merge recursively, arrays/scalars are replaced). L3 only supports `[agent]` model overrides. Model precedence: `.jyc/<mode>-model-override` file > L3 `config.toml` > pattern > L2/L1 config.
- **skills**: all levels are scanned; higher levels override same-named skills.
- **templates**: looked up L3 → L2 → L1; first match wins.
- A pattern's custom `topic_path` (absolute or `~`) lives outside the data root; relative paths resolve against the data root. L3 applies to any topic directory, including ad-hoc ones (`jyc open <path>`).

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
- **Features:** Full email support with References, attachments, and HTML formatting
- **Protocols:** IMAP for inbound, SMTP for outbound
- **Authentication:** TLS/SSL with username/password or OAuth2

### ✅ GitHub
- **Status:** Production ready (implemented in v0.1.10)
- **Features:** Issue/PR comments, label-based routing, multi-agent workflow
- **Protocols:** REST API polling (inbound), REST API (outbound)
- **Authentication:** Personal Access Token (PAT)
- **Agents:** Planner, Developer, Reviewer templates for full PR workflow

### ✅ Feishu (飞书/Lark)
- **Status:** Production ready (implemented in Phase 7); pipe-only channel (see [docs/architecture/overview.md](docs/architecture/overview.md))
- **Features:** Real-time messaging via WebSocket, messages piped into a agent's websocket channel, replies relayed back (text + attachments)
- **API:** REST API with openlark SDK + WebSocket for real-time updates
- **Authentication:** App credentials with automatic token refresh

### ✅ WeCom (企业微信)
- **Status:** Pipe-only channel (see [docs/architecture/overview.md](docs/architecture/overview.md))
- **Features:** Bot webhook inbound piped into a agent's websocket channel, external-contact API outbound with `corp_id` + `corp_secret` authentication
- **Protocols:** Shared axum HTTP server (inbound), REST API (outbound)
- **Security:** AES-256-CBC decryption, SHA1 signature verification

### ✅ WeCom KF (Customer Service)
- **Status:** Pipe-only channel (see [docs/architecture/overview.md](docs/architecture/overview.md))
- **Features:** Customer-service messaging via event notifications and `kf/sync_msg` API pull, piped into a agent's websocket channel
- **Protocols:** Webhook events (inbound), REST API (outbound)
- **Model:** Topic scoping via `pipe.topic` placeholders (`${msg.open_kfid}_${msg.external_userid}`)

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
| `/model <id>` | Switch AI model for this topic |
| `/model` | List available models |
| `/model reset` | Reset to default model |
| `/plan` | Switch to plan mode (read-only) |
| `/build` | Switch to build mode (default) |
| `/reset` | Clear AI session (start fresh conversation) |
| `/exchange` | Show shareable URLs for this topic's published files |
| `/exchange <file>` | Show the URL of one published file |
| `/close` | Close topic and delete directory (requires `--confirm` or `-y`) |
| `/template` | Apply template files to topic (skip existing) |
| `/template update` | Re-apply template, overwrite existing files |
| `/context` | Show the context management strategy (`full` / `sliding_window`) |
| `/context full` | Send the full conversation context to the LLM (default) |
| `/context sliding [N] [M]` | Send the last N user/assistant turns from prior history plus the current turn verbatim (tool calls/results intact) to the LLM (default N=10); optional M limits tool-call history notes to the most recent M windowed turns (default: all N); `.jyc/agent-context.json` keeps the full history |
| `/context reset` | Remove the runtime override and revert to the configured default |

### Custom Commands

Define your own slash commands in `config.toml`. Each one appears in `/?` and
the dashboard command popup (press `/`).

```toml
[[commands]]
name = "review"
description = "Review the current branch for over-engineering"
mode = "plan"                                # optional: plan | build
skills = ["pr-review", "ponytail-review"]    # optional
user_prompt = """
Review the changes on the current branch against main.
Report findings grouped by severity. Do not modify any code.
"""
```

Typing `/review` then:

1. switches the topic to `mode` (when set),
2. names the `skills` to use — the agent already receives every discovered
   skill's path and description, so naming them is enough for it to read the
   right `SKILL.md`,
3. appends `user_prompt` to the message body.

Text you type after the command is preserved, with `user_prompt` appended last so
it is the most recent instruction. These two forms are equivalent:

```
/review focus on error handling
```

```
/review
focus on error handling
```

`name` must be lowercase (command lookup is case-insensitive) and must not
shadow a built-in command from the table above. Invalid names are rejected at
startup, not silently ignored.

### Topic-Specific Customization

Place a `system.md` file in a topic's workspace directory to customize the AI's behavior for that topic. See `system.md.example` for a reference.

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
#   --token <TOKEN>   Auth token. Falls back to $JYC_DASHBOARD_TOKEN,
#                     then to <workdir>/auth.token
                         #   Keyboard: q=quit, ↑/↓=select topic, r=refresh, c=chat pane
jyc open                 # Create a new ad-hoc websocket topic and open chat
                         #   (shortcut for `jyc dashboard open`)
                         #   -t, --topic <NAME>   Topic name (default: folder name of -p or CWD)
                         #   -p, --path <PATH>     Topic working directory (default: CWD)
                         #   -c, --channel <NAME>  Websocket channel (auto-detected if only one)
                         #   --addr <ADDR>        Inspect server address (default: 127.0.0.1:9876)
jyc config init        # Generate config template (in <config dir>, or --workdir)
jyc config validate    # Validate config file (layered: global + workdir)
                       #   --config <FILE>   Config file path (default: as serve)
jyc token show         # Print the dashboard authorization token
jyc patterns list      # List configured patterns
                       #   --config <FILE>   Config file path (default: as serve)
jyc agents list        # List available agent templates
                       #   --source <DIR>   Source dir containing templates/ (default: CWD)
jyc agents install [name]   # Install agent template(s) (omit name = all)
                       #   --source <DIR>   Source dir (default: CWD)
                       #   --target <DIR>   Target dir (default: platform config home)
jyc skills list        # List available skills
                       #   --source <DIR>   Source dir containing skills/ (default: CWD)
jyc skills install [name]   # Install skill(s) (omit name = all)
                       #   --source <DIR>   Source dir (default: CWD)
                       #   --target <DIR>   Target dir (default: platform config home)
```

The `dashboard` command requires the `[inspect]` section to be enabled in config.

## MCP Tools

JYC provides several MCP (Model Context Protocol) tools that the AI agent uses internally:

| Tool | Description |
|------|-------------|
| `reply_message` | Send reply via the channel's outbound adapter. Reads routing info from `reply-context.json`, appends to chat log, writes signal file for delivery. |
| `jyc_send_message` | Send proactive out-of-topic messages to any recipient via the pre-warmed outbound adapter. Used for alerts and notifications only, not for in-topic replies. |
| `analyze_image` | Analyze images using an OpenAI-compatible vision API. Accepts absolute file paths or HTTP(S) URLs. Configure via `[[mcps]]` in `config.toml` (see `config.example.toml`). |
| `ask_user` | Ask the user a question and wait for their reply (up to 5 minutes). The question is delivered immediately via background delivery watcher. |

These are internal tools used by the AI, not user-facing commands.

## Configuration

JYC uses TOML configuration with environment variable substitution (`${VAR}`).

Key sections:

- **`[general]`** -- Concurrency settings (max topics, queue size)
- **`[channels.<name>]`** -- Per-channel config (type, patterns)
- **`[channels.<name>.email]`** -- IMAP/SMTP settings (host, port, credentials)
- **`[channels.<name>.feishu]`** -- Feishu app credentials (app_id, app_secret, websocket)
- **`[channels.<name>.github]`** -- GitHub settings (owner, repo, token, poll_interval)
- **`[channels.<name>.agent]`** -- Per-channel agent override (model, system prompt)
- **`[agent]`** -- AI agent settings (model, system prompt, progress updates)
- **`[agent.providers.<name>]`** -- LLM provider (type, base_url, `api_key =
  "${ENV_VAR}"` (preferred) or `api_key_env = "ENV_VAR"` (legacy), `pricing`);
  `[agent.providers.<name>.models.<id>]` overrides per model.
  `type` is one of `"anthropic"`, `"openai-compatible"` (Chat Completions),
  or `"openai-responses"` (OpenAI Responses API — recommended for GPT-5.x /
  o-series reasoning models: streams reasoning **summaries** when the model
  sets `params = { reasoning = { effort = "...", summary = "auto" } }`, and
  unlike Chat Completions supports tools + reasoning together)
- **`[inspect]`** -- Inspect server settings (enabled, bind address,
  `base_url` for links that leave the server, e.g. `/exchange` share links)
- **`[vision]`** -- DEPRECATED: Vision is now configured via `[[mcps]]` (see `config.example.toml` for the new approach)
- **`[attachments]`** -- Inbound/outbound attachment settings

Per-pattern options such as `topic_path` (custom topic directory), `model`
(per-pattern model override), `access` (filesystem whitelist), and `mcps`
(per-pattern MCP tools) are configured under `[[channels.<name>.patterns]]`.
See `config.example.toml` for annotated examples.

Agents (`[agents.<name>]`) are the websocket-based counterpart to patterns:
they mirror `ChannelPattern`'s behavior surface (minus `rules` and the
pattern-identification fields `name`, `channel`, `enabled`, `pipe`,
`topic_prefix`). They also support inheritance:
`extends = "<base>"` reuses another agent's config and overrides only the
fields that differ (empty string `""` clears an inherited value; lists are
replaced, not merged). Both the base and the derived agent remain routable.

### Cost tracking

Set `pricing` on a provider (or an individual model, which takes priority) to
have jyc compute the cost of every LLM call:

```toml
[agent.providers.anthropic]
type = "anthropic"
# Rates are per 1,000,000 tokens. `currency` is a display label only --
# jyc never converts between currencies. Default: "CNY", so a provider
# billing in USD must say so explicitly.
# Anthropic splits cache tokens into read and write buckets that
# bill at different rates; set `cache_creation_per_million` to bill
# cache writes at their premium rate (~1.25× input for Opus / Sonnet 4.x).
pricing = { input_per_million = 3.0, output_per_million = 15.0, cache_hit_per_million = 0.3, cache_creation_per_million = 3.75, currency = "USD" }

[agent.providers.anthropic.models."claude-opus-4-7"]
pricing = { input_per_million = 15.0, output_per_million = 75.0, cache_hit_per_million = 1.5, cache_creation_per_million = 18.75, currency = "USD" }
```

Cost per call is:

```
  (input - cache_read - cache_creation) * input_per_million         / 1e6
+ output_tokens                         * output_per_million        / 1e6
+ cache_read_tokens                     * cache_hit_per_million     / 1e6
+ cache_creation_tokens                 * cache_creation_per_million / 1e6
                                        (defaults to cache_hit_per_million)
```

A CNY-priced provider can omit `currency` entirely, since `"CNY"` is the
default:

```toml
[agent.providers.siliconflow]
type = "openai-compatible"
pricing = { input_per_million = 3.0, output_per_million = 4.0, cache_hit_per_million = 0.5 }
```

**Time-of-day pricing.** Providers like DeepSeek bill different rates at
different hours (off-peak discounts). Add `time_windows` to a `pricing`
block — each window supplies its own rates for the hours between `start`
and `end` (`"HH:MM"` or `"HH:MM:SS"`), and the flat fields act as the
default outside every window:

```toml
[agent.providers.deepseek]
type = "openai-compatible"
base_url = "https://api.deepseek.com"
api_key = "${DEEPSEEK_API_KEY}"
# Standard ¥2/M in, ¥8/M out; 50% off 00:30–08:30 and 25% off
# 16:30–00:30 (Beijing time). `utc_offset` is a fixed UTC offset,
# default UTC — DeepSeek's schedule is Beijing time, so set "+08:00".
pricing = { input_per_million = 2.0, output_per_million = 8.0, currency = "CNY", utc_offset = "+08:00", time_windows = [
  { start = "00:30", end = "08:30", input_per_million = 1.0, output_per_million = 4.0 },
  { start = "16:30", end = "00:30", input_per_million = 1.5, output_per_million = 6.0 },
] }
```

Windows are start-inclusive / end-exclusive; a window whose `start > end`
wraps past midnight (the `16:30` → `00:30` evening discount above). The
first matching window wins (windows are expected to be non-overlapping).
Rates omitted on a window inherit the flat values, so a window that only
varies input/output keeps the flat cache pricing. Each LLM call bills at
the rates in effect when it completes — the same instant the ledger `ts`
is stamped.

Prompt-cache hits are billed at their own (usually much cheaper) rate rather
than the full input rate. `cache_hit_per_million` defaults to `0.0`; set it
equal to `input_per_million` for providers that bill cache hits normally.

`cache_creation_per_million` is **optional** and defaults to
`cache_hit_per_million` (i.e. cache writes bill at the same rate as cache
reads). Two providers distinguish cache writes from reads: Anthropic
(`cache_creation_input_tokens`, ~1.25× the input rate) and GPT-5.6
(`usage.prompt_tokens_details.cache_write_tokens`). Set this field only
for them; other providers surface a single cache bucket and ignore it.

**Long-context pricing.** Providers like GPT-5.6 re-bill the *whole*
request at elevated rates once its input exceeds a threshold (past 272K
input tokens: input 2×, output 1.5×, cache rates higher too). Add a
`long_context` block to the model's `pricing`:

```toml
[agent.providers.openai.models."gpt-5.6-sol"]
model_id = "gpt-5.6-sol"
pricing = { input_per_million = 3.42, output_per_million = 20.15, cache_hit_per_million = 0.34, cache_creation_per_million = 4.27, currency = "CNY", long_context = {
  # The request's total input (cache buckets included) exceeding
  # `threshold` switches ALL FOUR rates for that request — including
  # rates a matching `time_windows` entry resolved.
  threshold = 272_000,
  input_per_million = 6.77,
  output_per_million = 30.19,
  # Optional; an omitted cache-hit rate inherits the resolved base
  # rate, and an omitted cache-creation rate collapses writes into
  # the resolved cache-hit rate.
  cache_hit_per_million = 0.68,
  cache_creation_per_million = 8.46,
} }
```

**Opt-in prompt caching (GPT-5.6).** GPT-5.6 does not cache by default;
enable implicit caching by injecting `prompt_cache_key` /
`prompt_cache_options` into every request via `params`. Placeholders
`{channel}` and `{topic}` expand per session so each topic gets its own
cache-affinity bucket (cache *hits* are always matched by prompt-prefix
content — the key only stops different workloads from evicting each
other's entries):

```toml
[agent.providers.openai.models."gpt-5.6-sol"]
params = { prompt_cache_key = "jyc-{channel}-{topic}", prompt_cache_options = { mode = "implicit", ttl = "30m" } }
```

Explicit mode (message-level `cache_control` breakpoints) is not
supported.

Anthropic cache entries live 5 minutes by default, so turns more than 5
minutes apart re-create the whole prefix. Setting `cache_ttl = "1h"` on an
Anthropic provider (or a single model, which overrides the provider value)
opts into Anthropic's extended-cache-ttl beta: breakpoints are written with
a 1-hour lifetime, keeping reads cheap across bursty conversations where
turns are minutes-to-an-hour apart. Writes then bill at 2× the input rate
instead of 1.25× — pair it with `cache_creation_per_million` set to the 1h
write rate to keep the cost rows accurate.

For Anthropic and GPT-5.6 sessions the chat info pane and dashboard
topic info area show two cache rows: `Cache hits: N` (reads only)
followed by `Cache create: N` (writes). Other providers show a single
`Cache hits: N` row as before. The two rows map directly to
`cache_read_tokens * cache_hit_per_million` and
`cache_creation_tokens * cache_creation_per_million` in the cost formula.

The dashboard and chat **Topic Info** panes then show
`Cost: ¥0.0521 session · ¥1.3057 today`:

- **session** -- resets when the agent session resets (context auto-reset,
  `/reset`, or switching to a model with a smaller context window).
- **today** -- durable UTC-day total, appended per call to
  `<topic>/.jyc/bill-YYYY-MM-DD.jsonl` and never reset or truncated. Each
  line records the token counts alongside the cost, so the ledger stays
  auditable and a corrected rate can be replayed over past usage.

Summarization overhead is billed too -- the cycle-boundary progress summary
and the context-compression call on session reset each summarize the whole
transcript, so they are not cheap. Ledger lines carry a `kind` field
(`"call"` or `"summary"`) so the two can be told apart.

With no `pricing` configured, no cost is tracked and the row is hidden.

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

The MCP reply tool (subprocess spawned by the agent) logs to a per-topic file:

```
<workdir>/<channel>/workspace/<topic>/.jyc/reply-tool.log
```

This is useful for diagnosing reply delivery failures.

### Common Issues

**JYC starts but no emails are processed:**
- Check pattern matching: `jyc patterns list --workdir /path/to/data`
- Verify IMAP connection in logs (look for `IMAP connected and authenticated`)
- Check that sender/subject rules match incoming emails

**AI replies are not sent:**
- Check JYC logs for AI provider/API errors
- Check the MCP reply tool log (`.jyc/reply-tool.log` in the topic directory)
- Verify the `[agent]` section in `config.toml` has valid API credentials

**Session/context issues:**
- Send `/reset` in an email to clear the AI session for that topic
- Or manually delete `.jyc/agent-session.json` in the topic directory

**Container-specific issues:**
- See [docker/README.md](docker/README.md) troubleshooting section

## Documentation

| Document | Purpose |
|----------|---------|
| [DESIGN.md](DESIGN.md) | Architecture, data flow, component design, API reference |
| [docs/architecture/context.md](docs/architecture/context.md) | Context strategies, sliding window, history notes, token safety nets |
| [IMPLEMENTATION.md](IMPLEMENTATION.md) | Implementation phases and progress |
| [CHANGELOG.md](CHANGELOG.md) | Version history |
| [SYSTEMD.md](SYSTEMD.md) | systemd deployment and service management |
| [docker/README.md](docker/README.md) | Docker/Podman deployment |
