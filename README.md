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

Add `--pipe` to start the peripheral `jyc-pipe` process as a child of the hub — required for the pipe-only channels (email, feishu, github, gitee, wecom, wecom_bot, wecomkf), which the hub itself skips. It is a global flag, so bare `jyc --pipe` forwards it to the `jyc serve` the dashboard auto-starts. Alternatively run `jyc-pipe` yourself (e.g. as a systemd unit) — see [docs/architecture/pipe-split.md](docs/architecture/pipe-split.md).

On first run without a config, `jyc serve` creates a default `config.toml` (plus empty `skills/` and `templates/` directories) in the platform config dir and exits — edit the file, then start again.

Add `--debug` for debug-level logging or `--verbose` for trace-level.

## Configuration & Data Layout

JYC separates **user-edited configuration** from **generated data**, following platform conventions:

| Platform | Config dir (L1) | Data dir (L2) |
|---|---|---|
| Linux/macOS | `$XDG_CONFIG_HOME/jyc` (`~/.config/jyc`) | `$XDG_DATA_HOME/jyc` (`~/.local/share/jyc`) |
| Windows | `%APPDATA%\jyc` | `%LOCALAPPDATA%\jyc` |

Three-level layering applies to `config.toml`, `skills/`, and `templates/`:

- **L1 (global)** — `<config dir>/`: shared `config.toml`, `skills/`, `templates/`, `subagents/` (one directory per subagent type — this one is L1-only, see below)
- **L2 (data dir / data root)** — `--data-dir` if given, else the data dir: its own `config.toml` (`--config`), `skills/`, `templates/`, and all generated state (`channels/<channel>/.imap/`, `channels/<channel>/.github/`, `agents/<agent>/<topic>/`)
- **L3 (topic)** — the topic's state dir (`<home>/.jyc`, a sibling of the `.data` it works in): `config.toml` (model overrides plus the topic's own `mode`, skill allow-list, `disabled_tools` and `instructions`), `skills/`, `templates/`, sessions, chat history

Merge/lookup rules:

- **config.toml**: L2 is deep-merged over L1 (tables merge recursively, arrays/scalars are replaced). L3 sets the topic's own model overrides, `mode`, `skills` (allow-list), `disabled_tools` (deny-list) and `instructions`; a broken L3 file is ignored with a warning. Model precedence: `.jyc/<mode>-model-override` file > L3 `config.toml` > pattern > L2/L1 config; mode precedence: `.jyc/mode-override` (your `/mode`) > L3 `config.toml` > pattern > build.
- **subagent types**: `jyc_spawn_subagent(type = "…")` and `/spawn <type>` name a role, defined by `[subagents.<type>]` in `config.toml` or by `<config dir>/subagents/<type>/config.toml` (a topic config, plus two keys only an installed role file has: a one-line `description` of when to spawn it, and a `[spawn]` table of its own `inherit_context` / `share_topic_path` defaults — an explicit argument at spawn time wins; defined in both sources, the in-config table wins key by key). The spawn writes it into the child's own `.jyc/config.toml`, dropping the runtime overrides the child inherited from its parent for the keys the type sets itself. Only the L1 config dir is scanned — unlike `skills/` and `templates/`, a type is not layered, so `{data dir}/subagents/` is not a source. This repo ships three: `subagents/review/config.toml` is a read-only reviewer (plan mode, the review skills as an allow-list, no `write`/`edit`), delegated to from the `dev-workflow` skill; `subagents/ci-watch/config.toml` waits for a pull request's checks, on the small model (`dev-workflow` again); and `subagents/develop/config.toml` is a build-mode developer that implements a plan the caller has already settled in a directory of its own and drives it to a reviewed PR (`delegated-dev`). Each installs the same way — `cp -r subagents/<type> <config dir>/subagents/`. See `config.example.toml` → "Subagent Types".
- **skills**: all levels are scanned; higher levels override same-named skills. **A topic lists none of them by default** — `skills` (channel/pattern/agent/topic) is the allow-list of what the *system prompt* lists, and it is opt-in: unset means none, not "everything found". Every discovered skill stays reachable with `/skill:<name>`, which appends that skill's name, path and description to one message so the agent reads its `SKILL.md`; `/skill on <name>` adds a skill to the topic's list for good, `/skill off <name>` takes it out.
- **templates**: looked up L3 → L2 → L1; first match wins.
- A pattern's custom `topic_path` (absolute or `~`) lives outside the data root; relative paths resolve against the data root. L3 applies to any topic directory, including ad-hoc ones (`jyc open <path>`).

## Deployment

JYC ships with one first-class deployment mode:

| Mode | Docs | Use Case |
|------|------|----------|
| **Docker** | [docker/README.md](docker/README.md) | Containerized, isolated environment |

The Docker image supports automatic restarts and AI self-bootstrapping (the AI can rebuild and redeploy JYC from source).

## Supported Channels

JYC is designed to be channel-agnostic. Currently implemented channels:

### ✅ Email (IMAP/SMTP)
- **Status:** Production ready; pipe-only channel, runs in `jyc-pipe` — which owns the mailbox cursor (`<data dir>/channels/<name>/.imap/`) (see [docs/architecture/pipe-split.md](docs/architecture/pipe-split.md))
- **Features:** Full email support with References, HTML formatting, and reply attachments — note the known gap: **attachments on incoming mail are not relayed** (`jyc-pipe` logs them as dropped)
- **Protocols:** IMAP for inbound, SMTP for outbound
- **Authentication:** TLS/SSL with username/password or OAuth2

### ✅ GitHub
- **Status:** Production ready (implemented in v0.1.10); pipe-only channel, runs in `jyc-pipe` (see [docs/architecture/pipe-split.md](docs/architecture/pipe-split.md))
- **Features:** Issue/PR comments, label-based routing, multi-agent workflow
- **Protocols:** REST API polling (inbound), REST API (outbound)
- **Authentication:** Personal Access Token (PAT)
- **Agents:** Planner, Developer, Reviewer templates for full PR workflow

### ✅ Feishu (飞书/Lark)
- **Status:** Production ready (implemented in Phase 7); pipe-only channel, runs in `jyc-pipe` (see [docs/architecture/pipe-split.md](docs/architecture/pipe-split.md))
- **Features:** Real-time messaging via WebSocket, messages piped into a agent's websocket channel, replies relayed back (text + attachments)
- **API:** REST API with openlark SDK + WebSocket for real-time updates
- **Authentication:** App credentials with automatic token refresh

### ✅ WeCom (企业微信)
- **Status:** Pipe-only channel, runs in `jyc-pipe` — which binds the shared webhook server (`[wecom].bind_addr`) (see [docs/architecture/pipe-split.md](docs/architecture/pipe-split.md))
- **Features:** Bot webhook inbound piped into a agent's websocket channel, external-contact API outbound with `corp_id` + `corp_secret` authentication
- **Protocols:** Shared axum HTTP server (inbound), REST API (outbound)
- **Security:** AES-256-CBC decryption, SHA1 signature verification

### ✅ WeCom KF (Customer Service)
- **Status:** Pipe-only channel, runs in `jyc-pipe` (shared webhook server with WeCom) (see [docs/architecture/pipe-split.md](docs/architecture/pipe-split.md))
- **Features:** Customer-service messaging via event notifications and `kf/sync_msg` API pull, piped into a agent's websocket channel
- **Protocols:** Webhook events (inbound), REST API (outbound)
- **Model:** Topic scoping via `pipe.topic` placeholders (`${msg.open_kfid}_${msg.external_userid}`)

### ✅ WeCom Smart Robot (wecom_bot)
- **Status:** Implemented in v0.3.11; pipe-only channel, runs in `jyc-pipe` (see [docs/architecture/pipe-split.md](docs/architecture/pipe-split.md))
- **Features:** Smart Robot messaging via persistent WebSocket, streaming replies, and outbound attachment upload
- **Protocols:** WebSocket long connection for both inbound and outbound
- **Authentication:** Bot ID + long-connection secret
- **Attachments:** File, image, voice, and video upload via WebSocket media upload protocol

### ✅ Gitee
- **Status:** Implemented in v0.3.10; pipe-only channel, runs in `jyc-pipe` (see [docs/architecture/pipe-split.md](docs/architecture/pipe-split.md))
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

Any line starting with `/` at the top of a message is parsed as a command. An
unregistered name gets an "Unknown command" reply instead of reaching the agent;
to send a literal path (e.g. `/home/jiny/x`) put it in quotes or below the first
body line.

| Command | Description |
|---------|-------------|
| `/model <id>` | Switch AI model for this topic |
| `/model` | List available models |
| `/model reset` | Reset to default model |
| `/plan` | Switch to plan mode (read-only) |
| `/build` | Switch to build mode (default) |
| `/reset` | Clear AI session (requires `-f`/`--force`) |
| `/send <agent> [--topic\|-t <topic>] [--reply] [--attach <file>] [--recipient <id>] <message>` | Send a message to another agent — its own topic by default, or one of its topics with `-t` — for that topic's agent to process. Same implementation as `jyc_send_to_topic`, and the addressee is resolved through the running topic managers, so no channel is named; `--reply` marks the message as expecting results back |
| `/exchange` | Show shareable URLs for this topic's published files |
| `/exchange <file>` | Show the URL of one published file |
| `/close` | Close this topic: state deleted, emptied `~/.local/share/jyc/agents/<topic>/` shell pruned; the directory goes too, unless the topic is pinned (`-f`/`--force`) |
| `/close <subagent>` | Close one of this topic's **own** subagents, or every one of them with `/close all`: the command is sent to that topic, so it closes itself the same way and its directory rules stay its own. Only a topic's direct subagents can be named, the same `-f`/`--force` guard applies (`/close joe` warns, `/close joe -f` sends), and `all` is a keyword — a subagent actually called `all` goes in the batch |
| `/close --force --purge` (short: `-f -p`) | Also delete the topic directory — refused when the dir is shared with another topic (pinned or not), is another topic's parent, is one of jyc's own (`~/.local/share/jyc/agents`), or holds another topic's state |
| `/template` | Apply template files to topic (skip existing) |
| `/template update` | Re-apply template, overwrite existing files |
| `/context` | Show the context management strategy (`full` / `sliding_window`) |
| `/context full` | Send the full conversation context to the LLM (default) |
| `/context sliding [N] [EXT] [TURNCAP] [MAXHIST] [CHECKPOINT]` | Send the last N user/assistant turns as text-only history plus the current turn verbatim (tool calls/results intact) to the LLM (default N=10); EXT is the extension rate the window may grow by before sliding back (default 0.5); TURNCAP caps a single tool result on the current turn (`0`, or 130 B..1 MiB); MAXHIST is a token budget for the windowed history; CHECKPOINT is how many completed assistant messages the current turn may reach before its older steps fold into a text-only checkpoint (`0` disables folding; default 32). `.jyc/agent-context.jsonl` keeps the full history — see `docs/architecture/context.md` |
| `/context reset` | Remove the runtime override and revert to the configured default |
| `/skill` | Show this topic's runtime skill toggles (`on`/`off` lists) |
| `/skill on <name>` | Force-enable a discoverable skill for this topic, even if excluded by config; takes effect from the next message, persists until `/skill reset` |
| `/skill off <name>` | Force-disable a skill for this topic, even if whitelisted by config |
| `/skill reset` | Clear all runtime skill toggles for this topic |
| `/spawn [<type>] [-i\|--inherit] [-s\|--share] [-n\|--name <name>] [-d\|--dir <path>]` | Start a sibling topic with this topic's settings, in a directory of its own or in **this** one. The subagent type is the one positional and optional (`/spawn review` starts it as that role); `-d` names any directory — a non-empty one included (a checkout made elsewhere), whose files are left exactly as they are — and a directory alone names itself after its last component, no `-d` and no `-n` means a fresh `agents/<name>/` named `<this>-2`. `-s` makes the child work in this topic's directory with its own state, so the two share files and not history (what `/fork` used to be); `-i` copies the conversation and transcript too, which are otherwise left behind. Seeds the same state (`spawned-from` names the parent) and copies no workspace files, so it never merges into anything. A relative `-d` counts from this topic's dir (`-d ../test` = beside it) and has to land outside it: a child nested in this topic's own dir would inherit its git status, its file listings and its close semantics, so `-d ./lab` is refused. A role may declare `-s`/`-i` for itself in its own `[spawn]` table, and the command line only ever adds to that. Websocket-only; it does not switch focus — pick it in the topic list |
| `/skill:<name>` | Use that skill for **this one message** — no config entry, no state change. One such command exists per skill the topic can use, created at runtime and listed in `/?` and the `/` popup |
| `/mcp` | Show this topic's runtime MCP-server toggles |
| `/mcp on <name>` | Force-enable an MCP server defined in config (un-disables `disabled_mcps`), even if excluded by config; takes effect from the next message, persists until `/mcp reset` |
| `/mcp off <name>` | Force-disable an MCP server for this topic |
| `/mcp reset` | Clear all runtime MCP toggles for this topic |

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
2. names the `skills` to use — the prompt lists the skills the topic's
   allow-list enables, so a name outside it is named here and read by the
   agent from its `SKILL.md`,
3. appends `user_prompt` to the message body.

You do not need a `[[commands]]` entry to reach a skill: every discovered
skill has a runtime `/skill:<name>` command that does exactly this (your
text is the prompt, the skill's name, path and description are appended so
the agent reads the right `SKILL.md`). Config wins on a name collision, so
`[[commands]] name = "skill:review"` overrides the derived one.

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

#### Shell Commands

A `[[commands]]` entry can also be a direct shell invocation instead of an
LLM-prompt command. Set `shell` to an argv array and omit `user_prompt`:

```toml
[[commands]]
name = "ls"
description = "List the topic directory"
shell = ["ls"]                       # /ls -la  →  runs ["ls", "-la"]

[[commands]]
name = "deploy"
description = "Deploy to staging"
shell = ["./scripts/deploy.sh"]      # /deploy staging prod  →  runs the script with those args
timeout = 300                        # optional: wait 300s before killing (default 30s)
```

On invocation the handler runs the argv via `tokio::process::Command` and
replies with stdout/stderr. **No LLM is involved, so no tokens are spent.**
Anything you type after the command is appended as more argv elements (so
`/ls -la` is `["ls", "-la"]`, not a shell string — there is no quoting or
interpolation).

**Security:** `shell` is argv-only, but any inbound channel (email, feishu,
websocket) can trigger any registered command. Treat `[[commands]]` as an
operator-trust surface — only add shell commands that you would accept any
inbound sender invoking. The process inherits jyc's working directory; if you
need a different cwd or environment, wrap with a script (`shell =
["./scripts/deploy.sh"]`).

**Limits:** the kill timeout defaults to **30s**, overridable per command
with `timeout = <secs>` (whole seconds; ignored on prompt-flavor commands).
The output cap is fixed at 8 KiB. Use a wrapper script when those don't fit.

### Topic-Specific Customization

Place a `system.md` file in a topic's workspace directory to customize the AI's behavior for that topic. See `system.md.example` for a reference.

## CLI Commands

### Global Flags

```bash
    --data-dir <PATH>   # Data dir (default: platform data dir,
                       #   e.g. ~/.local/share/jyc on Linux); no short form
-d, --debug            # Enable debug logging
-v, --verbose         # Enable verbose (trace) logging
    --pipe             # Run jyc-pipe as a child process (required for the
                       #   pipe-only channels); bare `jyc --pipe` forwards the
                       #   flag to the `jyc serve` it auto-starts
```

### Subcommands

```bash
jyc serve              # Start the agent (main command)
                       #   --config <FILE>    Config file path (default:
                       #     <config dir>/config.toml, or config.toml in --data-dir)
jyc dashboard            # Live TUI dashboard (connects via inspect server)
                         #   --addr <ADDR>     Inspect server address (default:
                         #     [inspect].bind from config, else 127.0.0.1:9876)
                         #                     Also used for WebSocket chat on /ws
#   --token <TOKEN>   Auth token. Falls back to $JYC_DASHBOARD_TOKEN,
#                     then to <data dir>/auth.token
                         #   Keyboard: q=quit, ↑/↓=select topic, r=refresh, c=chat pane
jyc open                 # Create a new ad-hoc websocket topic and open chat
                         #   (shortcut for `jyc dashboard open`)
                         #   -t, --topic <NAME>   Topic name (default: folder name of -p or CWD)
                         #   -p, --path <PATH>     Topic working directory (default: CWD)
                         #   -c, --channel <NAME>  Websocket channel (auto-detected if only one)
                         #   --addr <ADDR>        Inspect server address (default:
                         #     [inspect].bind from config, else 127.0.0.1:9876)
jyc connect <host>       # Run the dashboard against a hub on another machine,
                         #   over SSH (see "Remote hub over SSH" below)
                         #   HOST                 [user@]host, or an ~/.ssh/config alias
                         #   -s, --serve          Start `jyc serve` there if it is not
                         #                        running (default: only connect)
                         #   --bin <PATH>         jyc executable on the remote
                         #                        (default: jyc)
                         #   --remote-data-dir    Data dir on the remote (default: its own)
                         #   --remote-bind        Inspect bind on the remote (default:
                         #                        read from the remote's effective config)
                         #   --token <TOKEN>      Auth token (default: read from the remote)
                         #   -t/-p/-c             As `jyc open`, on the remote host
jyc config init        # Generate config template (in <config dir>, or --data-dir)
jyc config validate    # Validate config file (layered: global + data dir)
                       #   --config <FILE>   Config file path (default: as serve)
jyc config inspect-bind # Print the effective [inspect].bind address
jyc-pipe               # Peripheral pipe process (separate binary, not a `jyc`
                       # subcommand): hosts the pipe-only channels
                       # (feishu, github, gitee, wecom_bot, wecom, wecomkf, email),
                       # forwarding to the hub over websocket; required for
                       # those channels to work. See
                       # docs/architecture/pipe-split.md
                       #       --data-dir <DIR> Data dir (default: platform data dir)
                       #   -c, --config <FILE> Config file (default: as serve)
                       #   --hub <WS-URL>      Hub websocket origin (default: from
                       #     the config's [inspect] bind)
                       #   --no-idle           Use polling instead of IMAP IDLE (email)
                       #   --reset             Reset mailbox state before starting (email)
jyc send <agent> [--topic|-t <topic>] <message>...
                       # Enqueue one or more messages for an existing agent/topic
                       #   --addr <ADDR>     Inspect server address (same default
                       #                     as dashboard)
                       #   --token <TOKEN>   Auth token. Falls back to
                       #                     $JYC_DASHBOARD_TOKEN, then
                       #                     <data dir>/auth.token
                       # Messages are sent one by one, in argument order;
                       # stops at the first rejection (non-zero exit)
jyc token show         # Print the dashboard authorization token
jyc plugins install <archive>   # Install a plugin bundle into the config dir
                       #   <archive>        A .tar.gz (or a directory) holding
                       #                    skills/ subagents/ hooks/ templates/
                       #                    at its root
                       #   --target <DIR>   Target dir (default: platform config home)
```

The `dashboard` command requires the `[inspect]` section to be enabled in config.

Address resolution reads the same layered config as `jyc serve` (the platform
`<config dir>/config.toml`, plus `config.toml` in `--data-dir` when given). A
hub started with `jyc serve --config <custom file>` is not covered by that
lookup — pass the matching `--addr` explicitly.

**Non-interactive send (cron/scripts)**: `jyc send` talks to a running
`jyc serve` (it does not start one), enqueues each message in order, and exits
after the last one is accepted. It does not wait for an AI reply. A rejected
target or message stops the batch with a non-zero exit; messages already
enqueued stay queued.

```bash
jyc send jyc "Run the hourly report"
jyc send jyc --topic reports "Collect data" "Generate report" "Summarize"
jyc --data-dir /srv/jyc send jyc "Run the hourly report"
```

A local cron entry running as the same account can use the defaults for the
inspect address and token. Use an absolute binary path because cron's `PATH` is
minimal:

```cron
0 * * * * /usr/local/bin/jyc send jyc "Run the hourly report"
```

If `jyc serve` uses a non-default data dir, pass the same `--data-dir` here; if
its bind cannot be resolved from the config under that data dir, pass the
matching `--addr` as well.

The shipped `jyc-cron` skill carries the full install runbook: confirming the
schedule, preserving existing crontab entries, idempotent `# jyc-send:<id>`
markers, quoting and logging. Enable it for an agent or topic that should
handle scheduling requests (`skills = ["jyc-cron"]` or `/skill on jyc-cron`).

**Remote hub over SSH**: `jyc connect <host>` points the dashboard at a
`jyc serve` on another machine. Unless `--remote-bind` says otherwise, the
remote bind is read from that install's own effective config with
`jyc config inspect-bind`; older remote versions fall back to
`127.0.0.1:9876`. `jyc connect` opens an `ssh -N -L` forward on the same port
when it is free locally (so `jyc_publish_file` links resolve unchanged), reads
the token from the remote once per session and never writes it to this disk,
and leaves the remote hub running when it quits (other dashboards and
channels may be using it). Starting that hub is opt-in:

```bash
jyc connect devbox                 # dashboard against an existing hub
jyc connect devbox -s              # start `jyc serve` there first
jyc connect devbox -p ~/src/app    # open that remote directory as a topic
jyc connect devbox --ssh-opt Compression=no   # extra ssh options, repeatable
```

**Multiple connects**: each `jyc connect` process opens its own `ssh -L`
forward — there is no tunnel discovery or reuse. When the remote's port is
already taken locally (by another connect's tunnel, or anything else), the
forward falls back to an OS-assigned port and the dashboard resolves it
automatically. Either way, every tunnel to a machine shares its single
`jyc serve`; the serve never starts on its own, and `-s` probes first and
starts one only when nothing answers.

```text
this machine                                      remote machine
------------                                      --------------

  jyc connect A
      ssh -L 127.0.0.1:9876 ----------------------------+
                                                         v
                                                      jyc serve
                                                         ^
  jyc connect B                                          |
      ssh -L 127.0.0.1:52443  (ephemeral fallback) ------+
```

Run `pgrep -af 'ssh.*-L'` to list the forwards.

The token is read from the remote (`<bin> token show`) once per session;
`--token` passes one outright, for a hub whose file this side cannot read.
`$JYC_DASHBOARD_TOKEN` is deliberately *not* read here — it names a local
dashboard's token, and forwarding it to another machine only produces a 401.
When the token cannot be read at all, `jyc connect` says so first instead of
blaming `[inspect]`: an unauthenticated probe answers the same 401 a wrong
token does, so the read failure is the one fact that separates them.

The tunnel runs `ssh -C`: the dashboard's traffic is repetitive JSON (a
thinking event re-sends the whole reasoning, so a long turn would otherwise
push its square), and compression is what keeps a thin uplink usable. On a fast
link where that CPU matters, `--ssh-opt Compression=no` turns it off.
`--ssh-opt` also overrides the options this command relies on (`BatchMode`,
`ConnectTimeout`) — do that only if you know what a password prompt with no
terminal in front of it does to the bootstrap.

Requires `[inspect] enabled = true` on the remote, non-interactive ssh auth
(`ssh devbox true` must not prompt): the bootstrap and the tunnel both run
before the TUI takes the terminal, so a password prompt would have nowhere to
appear. The remote login shell must be POSIX-ish (`sh`/`bash`/`zsh`/`dash`) —
the bootstrap expands `$HOME` and probes for `setsid` through it. That probe is
also why `-s` works on macOS: `setsid` is used when present, `nohup` alone
otherwise. Link rewriting covers a concrete non-wildcard bind host; with a
wildcard bind or a configured `[inspect] base_url` the tunnel still works, but
the links the server generates then keep their remote URL.

**Plugins**: a *plugin bundle* is a gzipped tar with `skills/`, `subagents/`,
`hooks/` and `templates/` at its root — the four user-editable collections this
repo ships — and `jyc plugins install` writes them into the config dir (a plain
directory laid out the same way works too). Every release publishes one as
`jyc-plugins.tar.gz`, which is the shortest way to put those skills, subagent
roles, hook scripts and templates in place: `skills/`, `subagents/` and
`templates/` are picked up by a directory scan, while an installed
`hooks/*.sh` runs only once a `[[hooks]]` entry in `config.toml` names it.

```bash
curl -LO https://github.com/kingye/jyc/releases/latest/download/jyc-plugins.tar.gz
jyc plugins install ./jyc-plugins.tar.gz
```

**How `jyc open` picks the topic** (relevant when several agents share one
directory via `topic_path`): topic identity is the *name*, not the
directory. Bare `jyc` uses the folder name as the topic name, so in
`~/projects/jyc` it attaches to topic `jyc` — i.e. the config agent whose
key is `jyc`, with its own state at `agents/jyc/.jyc` (sessions, history,
billing all isolated). To open a sibling agent co-pinned to the same repo,
name it explicitly: `jyc -t jyc-reviewer`. A `-t` name that matches no
config topic creates a fresh ad-hoc topic with its own path-derived state;
re-opening the same directory under a *different* ad-hoc name is refused to
protect history (`topic-name` breadcrumb check).

**Clickable links**: the TUI emits OSC 8 hyperlinks for `http(s)` URLs
visible in chat, so they are clickable (ghostty/WezTerm/Kitty: hold
⌘Cmd/Ctrl and click) even when wrapped across lines. Inside tmux this
requires tmux ≥ 3.4 on the host running tmux (older versions silently drop
the sequences — links degrade to plain text). ssh between your terminal and
the host is transparent and needs nothing.

## MCP Tools

JYC provides several MCP (Model Context Protocol) tools that the AI agent uses internally:

| Tool | Description |
|------|-------------|
| `reply_message` | Send reply via the channel's outbound adapter. Reads routing info from `reply-context.json`, appends to chat log, writes signal file for delivery. |
| `jyc_send_message` | Send a channel message with optional attachments to any recipient via the pre-warmed outbound adapter. Advertised as a core tool; used for proactive alerts, cross-channel messages, and attachment-bearing replies. |
| `analyze_image` | Analyze images using an OpenAI-compatible vision API. Accepts absolute file paths or HTTP(S) URLs. Configure via `[[mcps]]` in `config.toml` (see `config.example.toml`). |
| `ask_user` | Ask the user one or more questions with selectable options. The questions are delivered and the turn ends right there — nothing waits, so there is no timeout; the user's next message is the answer. A channel with a question box (the websocket/TUI channel) gets one `question` frame per question; every other channel (feishu, email, any pipe-only turn) gets the same set as a plain-text message on the ordinary reply path. Pass `questions` instead of a single `question` to settle up to 5 decisions as one set, answered together; in the TUI box `d` skips one question and Esc closes the box. |

These are internal tools used by the AI, not user-facing commands.

## Configuration

JYC uses TOML configuration with environment variable substitution (`${VAR}`).
Built-ins jyc resolves itself when the process environment does not define
them: `${JYC_CONFIG_PATH}` is the directory of the config file being loaded
(typically `~/.config/jyc`); `${JYC_TOPIC_PATH}` and `${JYC_TOPIC_STATE_PATH}`
are reserved for hooks, where they resolve per topic at spawn (see
`docs/hooks.md`).

Key sections:

- **`[general]`** -- Concurrency settings (max topics, queue size)
- **`[channels.<name>]`** -- Per-channel config (type, patterns)
- **`[channels.<name>.email]`** -- IMAP/SMTP settings (host, port, credentials)
- **`[channels.<name>.feishu]`** -- Feishu app credentials (app_id, app_secret, websocket)
- **`[channels.<name>.github]`** -- GitHub settings (owner, repo, token, poll_interval)
- **`[channels.<name>.agent]`** -- Per-channel agent override (model, system prompt)
- **`[agent]`** -- AI agent settings (model, system prompt, progress updates)
- **`[agent.providers.<name>]`** -- LLM provider (type, base_url, `api_key =
  "${ENV_VAR}"`, `pricing`);
  `[agent.providers.<name>.models.<id>]` overrides per model.
  `type` is one of `"anthropic"`, `"openai-compatible"` (Chat Completions),
  or `"openai-responses"` (OpenAI Responses API — recommended for GPT-5.x /
  o-series reasoning models: streams reasoning **summaries** when the model
  sets `params = { reasoning = { effort = "...", summary = "auto" } }`, and
  unlike Chat Completions supports tools + reasoning together)
- **`[inspect]`** -- Inspect server settings (enabled, bind address,
  `base_url` for links that leave the server, e.g. `/exchange` share links)
- **Pipe-only channels** (no config section needed) -- channel types the pipe
  process can host run in the external `jyc-pipe` process, and `jyc serve`
  skips them in-process; each channel entry below says whether it is one, and
  the authoritative list is `SUPPORTED_CHANNEL_TYPES` in `jyc-types` (see
  [docs/architecture/pipe-split.md](docs/architecture/pipe-split.md))
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
topic info area also show the cache write volume — the `Create` row under
the pane's `Cache:` header, beside the hit rates. The two figures map
directly to `cache_read_tokens * cache_hit_per_million` and
`cache_creation_tokens * cache_creation_per_million` in the cost formula.

The dashboard and chat **Topic Info** panes then show
`Cost: ¥1.3057 today · ¥0.0521 session`:

- **today** -- durable UTC-day total, appended per call to
  the state dir's `bill-YYYY-MM-DD.jsonl` and never reset or truncated. Each
  line records the token counts alongside the cost, so the ledger stays
  auditable and a corrected rate can be replayed over past usage.
- **session** -- resets when the agent session resets (`/reset`).

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
jyc serve --data-dir /path/to/data --debug

# Trace level (very verbose)
jyc serve --data-dir /path/to/data --verbose

# Or use RUST_LOG for fine-grained control
RUST_LOG=jyc=debug,async_imap=warn jyc serve --data-dir /path/to/data
```

### Checking MCP Reply Tool Logs

The MCP reply tool (subprocess spawned by the agent) logs to a per-topic file:

```
<agents dir>/<agent>/<topic>/.jyc/reply-tool.log
```

This is useful for diagnosing reply delivery failures.

### Common Issues

**JYC starts but no emails are processed:**
- Verify IMAP connection in logs (look for `IMAP connected and authenticated`)
- Check that the `[[channels.<name>.patterns]]` rules in `config.toml` match the incoming emails

**AI replies are not sent:**
- Check JYC logs for AI provider/API errors
- Check the MCP reply tool log (`reply-tool.log` in the topic's state dir)
- Verify the `[agent]` section in `config.toml` has valid API credentials

**Session/context issues:**
- Send `/reset --force` in an email to clear the AI session for that topic
- Or manually delete `agent-session.json` in the topic's state dir

**Container-specific issues:**
- See [docker/README.md](docker/README.md) troubleshooting section

## Documentation

| Document | Purpose |
|----------|---------|
| [DESIGN.md](DESIGN.md) | Architecture, data flow, component design, API reference |
| [docs/architecture/context.md](docs/architecture/context.md) | Context strategies, sliding window, what the request looks like, token safety nets |
| [docs/hooks.md](docs/hooks.md) | Lifecycle hooks — config, events, payload, Claude Code compatibility |
| [CHANGELOG.md](CHANGELOG.md) | Version history |
| [docker/README.md](docker/README.md) | Docker/Podman deployment |
