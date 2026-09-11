## [Unreleased]

### Fixed

- Dashboard TUI rendering corruption around URL-containing chat messages:
  `HyperlinkBackend`'s full-row re-emission is now clipped to the current
  terminal width — after a shrink-resize the shadow grid could hold longer
  rows, and emitting them auto-wrapped and physically scrolled the screen,
  desyncing ratatui's diff baseline (interleaved old/new text while
  scrolling); re-enabling mouse capture now also forces a full repaint,
  since the terminal's own scrollback could move content while capture was
  off.

### Added

- `/grant` / `/ungrant` commands: grant the topic's agent filesystem access
  to a path at runtime (read-only by default, `-w` for read+write, `-p` to
  persist into `[agents.<topic>] access` in `config.toml`); temporary grants
  live until `/ungrant` or restart and are consulted when each turn builds
  its tool context
- Dashboard TUI emits OSC 8 hyperlinks for URLs visible in chat — clickable
  (modifier+click) even across wrapped lines; requires tmux ≥ 3.4 when
  running inside tmux, degrades to plain text otherwise.
- `docs/channels/github.md` — up-to-date GitHub channel reference (polled
  events, label routing, `[Role]` echo guard, close events, skills).
- README: document how `jyc open` resolves the topic (folder name → topic
  name → name-keyed state), incl. co-pinned agents sharing one directory.
- `scripts/chrome-debug-mac.sh` — dev helper: kill all Chrome instances and
  relaunch with remote debugging on port 9222 (macOS). (#737)
- New `/bill` built-in command: aggregates every topic's billing ledger
  into a usage/cost report grouped by provider → model → topic, with
  per-provider subtotals and a grand total. Scopes: `/bill` (today),
  `/bill YYYY-MM` (one month), `/bill all` (all time). Works on every
  channel.
- Subscription-plan billing: `pricing` accepts `billing = "subscription"`
  and `monthly_fee`. Ledger entries from such models are tagged
  `subscription` (cost = notional API-equivalent value, not real spend);
  `/bill` renders them in a separate section with a per-model utilization
  line (notional value vs the monthly fee prorated to the report scope).

### Fixed

- OSC 8 hyperlinks: a URL wrapped across rows now resolves every row
  fragment to the full URL (previously each fragment linked to its own
  partial text, and continuation rows were not clickable at all). Link
  detection is clipped to the chat message pane's rectangle, so text from
  an adjacent pane (e.g. the topic info pane) can no longer leak into a
  link target.

### Changed

- Topic state relocation: a pinned/ad-hoc topic's `.jyc` no longer lives
  inside its working dir. It is adopted to
  `<data_home>/agents/<name>/.jyc` (config-key agents keep their name,
  ad-hoc pins get a path-derived `_`-prefixed name), migrated
  automatically on first adoption, and granted to the agent's file-access
  sandbox. Unpinned topics are unchanged (`<topic_dir>/.jyc` fallback). (#739)

- State resolution is keyed by topic **name**, not working dir: several
  agents may pin the same `topic_path` (e.g. one repo shared by planner
  and developer), each keeping an isolated
  `<data_home>/agents/<name>/.jyc`; adoption order is deterministic and
  `/close --force` only ever deletes the closed topic's own state. MCP
  subprocesses get the identity via `JYC_TOPIC_NAME`. (#740)

- `/close` now requires `--force` (mirroring `/new`); `-y`/`--confirm` no
  longer bypass the guard. On a pinned/adopted topic it deletes the
  relocated state dir and unregisters the mapping, keeping the topic dir
  (e.g. a project checkout) intact; unregistered topics still delete the
  whole dir.

- `jyc-podman-tunnel.sh` moved to `scripts/` alongside the other helper
  scripts; usage comments and DESIGN.md reference updated.
- `FEISHU.md` moved to `docs/channels/feishu.md`, joining the other
  per-channel guides; internal doc link adjusted. (#734)


- Split oversized modules for maintainability: `serve/channels.rs` (3.4k
  lines) became per-channel submodules (`email`, `github`, `gitee`,
  `feishu`, `wecom_bot`, `wecom`+`wecomkf`) with shared pipe/relay helpers
  in `channels/mod.rs`; `agent_loop/mod.rs` (2.9k lines) shed its test
  modules into sibling files. Pure moves — no behavior change. (#735)
- CHANGELOG rotated: releases 0.3.13 and earlier archived verbatim to
  `CHANGELOG-archive.md`; the working file keeps Unreleased plus the three
  latest releases (0.3.15-0.3.17). (#735)

### Fixed

- System prompt no longer declares a blanket "MUST only access files within the
  working directory" rule when per-pattern `access.read`/`access.write` (or
  skill/attachment roots) are configured — it now enumerates exactly the roots
  the tool layer enforces, so agents stop refusing access they actually have.
  (#736)
- TUI screen corruption when a local MCP server wrote to stderr (e.g. the
  chrome-devtools-mcp startup banner): stderr is now piped and drained into
  the log (as `[mcp:<server>] ...` lines) instead of being inherited by the
  terminal. (#738)
- `/close` on a topic pinned to a project directory deleted that directory
  and left the relocated state dir behind forever; it now preserves the
  topic dir and removes the state.
- `/pin` false-positived "already pinned" (success, no config write) when
  the path appeared anywhere in the raw config text — including commented-
  out legacy blocks. The check now parses the config: only real
  `agents.*.topic_path` / legacy pattern `topic_path` values match, with
  tilde expansion (`~/x` pins are detected too).

### Removed

- `IMPLEMENTATION.md` — implementation-phase tracking is superseded by the
  CHANGELOG and merged PR history; stale reference links dropped from README
  and DESIGN.md.
- `GITHUB_CHANNEL.md` (repo root) — pre-pipe-era design doc, superseded by
  `docs/channels/github.md`.
- Unused agent templates (`templates/`: gitee/github role templates,
  `jyc-dev`, `jyc-review`, `frontend-designer`); only `invoice-processing`
  is kept. The template mechanism itself is unchanged. (#732)
- `jyc-ctl.sh` (repo root) — systemd/nohup service-control script; the
  systemd deployment was dropped. The service runs via `jyc serve` and is
  stopped via `jyc stop`. (#733)
- `agents.example.md` and `agents.invoice.example.md` (repo root) — topic
  AGENTS.md seed templates; the invoice variant overlaps with
  `templates/invoice-processing/AGENTS.md`, and the self-bootstrap variant
  had only one environment option left. No live references remained. (#734)

## [0.3.17] - 2026-09-07

### Added

- **`jyc stop --wait <secs>`** — configurable grace period. `jyc stop`
  previously polled for exit for a fixed 10 seconds after sending SIGTERM
  (or SIGKILL with `--force`) before reporting a stuck process. `--wait`
  overrides that timeout in whole seconds (`jyc stop --wait 30`); the
  default remains 10, and `--wait 0` reports immediately without waiting.
- **Shell-only `[[commands]]` variant.** A `[[commands]]` entry can now run a
  direct shell command instead of injecting a prompt into the LLM. Set
  `shell` to an argv array (e.g. `shell = ["ls"]`); the handler runs it
  via `tokio::process::Command`, replies with stdout/stderr, and never
  invokes the LLM — so **no tokens are spent**. User args typed after the
  command are appended as more argv elements (`/ls -la` → `["ls", "-la"]`).
  `shell` and `user_prompt` are mutually exclusive, validated at startup;
  `mode`/`skills` are silently ignored on shell commands. Kill timeout
  defaults to 30s, overridable per command with `timeout = <secs>`; output
  cap fixed at 8 KiB. Because any inbound channel can trigger
  any registered command, treat `[[commands]]` as an operator-trust surface
  and only add shell commands you would accept any inbound sender invoking.
- TUI chat: new leader command `ctrl+p T` ("toggle tool detail") expands
  tool-call lines in the progress tail to a multi-line field listing of
  the full tool input (each top-level argument on its own indented line,
  multi-line values preserved, capped at 20 lines); toggle again to
  return to the one-line extracted summary.
- **`/backlog` command for saving and replaying user messages per topic.**
  Storage: `<topic_path>/.jyc/backlog.jsonl` (JSONL, one item per line).
  Subcommands: `push <multi-line description>` appends a new item
  (the command owns the rest of the message — blank lines inside are
  kept as paragraph breaks, edge blank lines trimmed); `list` (alias `ls`) prints a numbered list showing the first
  line of each item (or `(empty)`);
  `pop [N]` removes the N-th item (default 1) and injects its text into
  the next agent turn as a user message via `append_body`; `rm <N>`
  removes the N-th item
  without injecting; `set <N> <new text>` replaces the N-th item's text
  in place (same multi-line continuation rules as `push`, no injection);
  `get <N>` shows the full text of the N-th item.
  Also teaches the command registry an opt-in
  `collect_subsequent_lines` mechanism (consume the entire remainder of
  the message, blank lines included) so future commands can accept
  multi-line first arguments the same way.
- **`/backlog` usability: `ls` alias + standalone help text.** Typing
  `/backlog ls` now works as a shortcut for `/backlog list` (it
  dispatches to the same arm via `match`). Typing `/backlog` with no
  subcommand previously returned a `missing subcommand` error; it now
  prints a short usage block listing all subcommands and the `ls`
  alias, matching the convention of other slash commands. Storage path
  and persisted schema are unchanged.
- **Per-agent `[[commands]]`.** Each `[agents.<name>]` can now declare
  its own slash commands via `[[agents.<name>.commands]]`. They are
  registered into the topic's `CommandRegistry` in addition to (not
  instead of) the top-level `[[commands]]`, so they only fire when the
  topic is routed to that agent. Useful for agent-specific workflows
  that don't belong in every other agent's prompt. Within one agent
  names must be unique; same names across scopes (global vs agent,
  agent vs agent) are allowed — at runtime the per-agent command
  overwrites the global one with a `tracing::warn` (last-registered
  wins via `CommandRegistry::register`).
- **`--log-file [PATH]` global flag for tracing logs.** Use
  `--log-file` to write logs to `<data_home>/jyc.log`, or
  `--log-file PATH` for a custom path. Bare `jyc` defaults to
  `jyc.log`; `jyc dashboard` / `jyc open` keep their implicit
  `dashboard.log` for TUI clobbering protection. Without the flag,
  tracing goes to stderr.

### Changed

- **`/new` and `/reset` now require `--force`.** A plain invocation no
  longer destroys the session (`/new` also chat history and published
  files) — it returns a warning showing the exact command to proceed
  with, mirroring the existing `/close --confirm` guard.
- **`--log-file` output is now daily-rotated.** The file the flag writes
  to gains a date suffix — `jyc.log` becomes `jyc.log.2026-09-07`,
  `dashboard.log` becomes `dashboard.log.2026-09-07`, with a fresh
  file created at local midnight. Backed by `tracing_appender::rolling::daily`.
  Old files are kept indefinitely (retention is a future PR; clean up
  with `find ~/.local/share/jyc -name '*.log.*' -mtime +30 -delete` or
  equivalent until then). Any tooling pointing at the literal `jyc.log`
  / `dashboard.log` paths needs to be updated to the dated suffix.
- **TUI progress indicator: `edit`/`write` tools unified under the
  `ctrl+p T` toggle.** Previously, `edit` and `write` tool entries
  rendered an always-on multi-line diff in the progress tail, independent
  of the tool-detail toggle. They now follow the same rule as every
  other tool: with the toggle off, they collapse to the one-line summary
  `Tool: edit — <basename>:<line>` (or `Tool: write — <basename>`);
  with the toggle on, they expand to the full diff (`-`/`+` lines,
  content for `write`, capped at 20 lines). Side effect: the redundant
  `  command: <cmd>` line that previously appeared below the `bash`
  summary is no longer shown — the summary already inlines the command.
  Also fixes a duplicated `⏳ ` prefix and double `elapsed` on the
  `edit`/`write` header line.
- **OpenAI Responses API provider.** New `type = "openai-responses"`
  provider targeting `POST {base_url}/responses` — the recommended way to
  run GPT-5.x / o-series reasoning models. Unlike Chat Completions it
  supports tools + reasoning together, and with
  `params = { reasoning = { effort = "...", summary = "auto" } }` it
  streams reasoning **summaries**, surfaced through the existing
  `/thinking show` display. Reasoning token counts are extracted from
  usage (Responses `output_tokens_details.reasoning_tokens`, Chat
  Completions `completion_tokens_details.reasoning_tokens`) and shown in
  `/info` as `Reasoning: N`; they remain informational only — billing is
  unchanged since reasoning tokens are already part of `output_tokens`.

- **Thinking display overhaul in the dashboard chat screen.** Agent
  thinking blocks now accumulate for the whole processing round instead
  of each new block overwriting the previous one (client-side, from the
  full-text WS `thinking` events). The live progress tail
  collapses thinking to a one-line summary (`💭 thinking — N blocks · M
  chars`); a new chat-scoped leader command `ctrl+p t` ("toggle
  thinking") expands/collapses it. When the round completes, its
  thinking is folded into the chat history as a collapsed
  pseudo-message above the AI reply (in-memory only; same toggle
  expands it).

- New `/info` slash command (all channels): replies with the topic's info
  mirroring the dashboard chat screen's Topic Info pane — name, channel,
  pattern, model, mode, branch, status, token usage, cache stats, cost,
  and changed files. Reads the same `TopicManager::list_topics` snapshot
  the TUI renders (#685)

- **Visible failure feedback when agent processing errors.** If the agent
  loop terminates with an error (e.g., provider context-limit or auth
  failure), a short `⚠️ Processing failed: …` reply is now sent through
  the outbound adapter and surfaced as a chat message. The dashboard chat
  pane also keeps a red `⚠` error line in the progress area until the
  next round starts, instead of silently dropping the progress tail.

- **History marker for compacted sliding-window region.** The
  `SlidingWindow` context strategy now prefixes the compacted history
  region with a machine-generated `[History messages]` divider, marking
  older turns that were flattened to plain text (tool calls/results
  omitted). This gives the model an explicit signal that those turns are
  degraded history rather than current user input, without affecting the
  cached system prompt or adding a configuration knob.

- **Configurable prompt-cache TTL for Anthropic providers.** New
  `cache_ttl` field (provider-level, with per-model override) accepting
  `"5m"` (default) or `"1h"`. With `"1h"`, every cache breakpoint is
  written with a 1-hour lifetime via Anthropic's extended-cache-ttl beta
  (`anthropic-beta: extended-cache-ttl-2025-04-11` header), so bursty
  conversations with turns minutes-to-an-hour apart keep cheap cache
  reads instead of re-creating the whole prefix every turn. Cache writes
  then bill at 2× the input rate instead of 1.25× — pair it with
  `cache_creation_per_million` set to the 1h write rate.

- **`jyc token reset` command for manual token rotation.** Generates a
  new random token, writes it to the workdir, and prints it. Restart
  `jyc serve` afterward for it to take effect; existing dashboards must
  reconnect with `jyc dashboard`.

- The Feishu live progress card now shows a 💭 thinking preview — the tail
  (≤1,500 chars) of the latest cumulative thinking snapshot — while
  processing. The finalized `✅/❌` card embeds the run's full thinking
  (all LLM rounds, tail-capped at 8,000 chars to stay under the card size
  limit) in a collapsed, expandable panel (Feishu client ≥ V7.9). Respects
  `/thinking hide` (no thinking events are published at all).

- **Feishu live progress indicator.** Piped Feishu messages now get a
  status card in the chat, updated as the agent works (elapsed time, tool
  call count, and the latest tool with its target — e.g. `edit —
  tools.rs`, `bash — cargo check`), driven by the topic event bus with
  updates throttled to ≥4s. On completion the card flips to
  `✅ 完成 · 52s · 工具 8` (or `❌ 失败`), and every relayed reply gains
  a `⏱ 耗时 Ns` footer. Degrades gracefully to the old Typing→DONE
  reactions when the event bus or card send is unavailable. (#674)

- The Feishu status card now also shows the topic's effective mode,
  model, and context-window usage — e.g. `⏳ 处理中 · 23s · 工具 2 ·
  plan · kimi/k3-256k · 41%`; build mode shows `build` (the mode segment
  is always concrete, never omitted). The segments refresh on every card
  update,
  so a mid-run `/plan` or `/model` switch shows up within seconds. The
  percentage appears once the first LLM call has recorded token usage.
  Resolution shares the exact override chain with the dashboard via a
  lightweight `TopicManager::topic_display_state` accessor (no workspace
  scan, no git calls). (#678)

- Feishu progress card: the live thinking tail is no longer appended as an
  open `💭 ...` markdown line (up to 1500 chars of noise on every refresh).
  It now renders inside a collapsed `collapsible_panel` — the same
  presentation as the finalized card — so the status card stays compact
  while processing runs; expanding the panel shows the current thinking
  tail. Note each PATCH re-sends `expanded: false`, so a manually expanded
  panel folds back on the next refresh (~4 s).

- TUI chat progress tail: tool-call lines now show the extracted argument
  (`Tool: bash — ls -la`, `Tool: edit — tools.rs`) instead of the raw JSON
  input, reusing the shared formatter also used by the Feishu status card
  (`jyc_types::inspect::tool_activity_summary`). Extraction happens at
  render time only — WS payloads and `activity.jsonl` keep the raw JSON;
  unknown/unparseable tools pass through unchanged

- Feishu live status card: the "最近" (recent tool) line now shows up to
  160 chars of the tool call (was 40), so full bash commands/patterns are
  visible in most cases; newlines collapse to spaces so multi-line
  commands can't break the card layout (#684)
- All duration displays now render through one shared formatter
  (`jyc_core::duration`, Ticking/Precise/Coarse styles) across the
  dashboard, chat screen, and Feishu surfaces. Side effects: the
  chat group footer and the Feishu reply footer (`⏱ 耗时`) now show
  minutes+seconds (`12m37s`) instead of truncating to whole minutes
  or bare seconds; the live ticker renders `1h00m00s` past the hour
  instead of `60m00s` (#680)
- Feishu status card renders elapsed time in human-readable form:
  `42s` below a minute, `1m38s` below an hour, `2h05m` beyond (#679)
- **/` +exchange` output is now a markdown bullet list.** Instead of `name: url`, multiple published files are rendered as a markdown list with the filename on one line and the raw shareable URL on the next, keeping URLs copy-pasteable while displaying better in chat and email.

- **System prompt no longer varies with plan/build mode.** The mode
  `<system-reminder>` appended to the end of the system prompt was removed;
  the identical `<mode>` block already travels with every user message
  (at the end of the request, always uncached). Mode switches no longer
  invalidate the cached prompt prefix — this keeps Anthropic cache
  breakpoints #2–4 and server-side automatic prefix caches (DeepSeek,
  Kimi, OpenAI, etc.) warm across plan/build flips.

- **Feishu progress indicator code moved out of the serve wiring.** The
  status-card watcher, card renderer, and tool-activity summaries now
  live in the Feishu channel module
  (`jyc-channels/src/feishu/progress.rs`); `cli/serve/channels.rs` only
  resolves the pipe target's `TopicManager` and calls it, keeping the
  serve layer channel-agnostic. (#677)
- **Long-context pricing tier.** `pricing.long_context = { threshold,
  ... }` on a provider/model switches ALL FOUR rates for a request whose
  provider-reported input (cache buckets included) exceeds `threshold` —
  matching providers like GPT-5.6 that re-bill the whole request at
  elevated rates past a prompt-size boundary (input 2×, output 1.5× past
  272K). Tier cache rates left unset inherit/collapse like
  `time_windows` fields; a matching time window never shields a request
  from the tier.
- **GPT-5.6 cache-write tokens are now tracked and billed.** OpenAI's
  `usage.prompt_tokens_details.cache_write_tokens` feeds the
  cache-creation bucket (previously Anthropic-only), so
  `cache_creation_per_million` applies and the `Cache create: N` row
  renders for GPT-5.6 sessions. GPT-5.6 caching is opt-in per request —
  `params = { prompt_cache_key = ..., prompt_cache_options = {...} }`
  now supports `{channel}`/`{topic}` placeholders, expanded per session
  so each topic gets its own cache-affinity bucket. (Cache hits are
  always matched by prompt-prefix content; the key only stops different
  workloads from evicting each other's entries.)

- **Unified server logging across spawn origins.** `jyc serve --log-file`
  and the TUI's auto-spawned `jyc serve` both write to the same
  `<workdir>/jyc.log` (plain append-only, no rotation). Previously the
  TUI captured the spawned server's stderr into a separate plain
  `jyc.log` (truncated on TUI start), so a TUI-spawned server and a
  `/deploy`-started server produced two distinct log files in the data
  directory. Now both origins share one file — `tail -f jyc.log` follows
  everything. Server stderr (panics, prints that bypass tracing) flows
  into the same file via the `--log-file` rotation-less append writer,
  so diagnostic capture no longer needs the separate plain file.
- **`scripts/deploy.sh` now uses a layered shutdown** (`jyc stop` →
  `pkill -f 'jyc serve'` → `sleep 3` → `pkill -9 -f 'jyc serve'`) before
  starting the newly-deployed server. Previously the script only ran
  `jyc stop`, which signals just the PID in `<workdir>/jyc.pid` — a
  TUI-spawned `jyc serve` survived the deploy, leaving the old and new
  servers running concurrently. The `pkill` step catches every
  `jyc serve` by command-line pattern (same graceful SIGTERM),
  `sleep 3` lets in-flight graceful shutdowns finish, and `pkill -9`
  is a last-resort SIGKILL for any process that ignored SIGTERM.

### Fixed

- **`/` popup now shows commands for the topic you're chatting in.**
  Previously `sync_commands_for_selection` read the highlighted row in the
  topic table and used *that* topic's commands — so opening an ad-hoc topic
  (e.g. `jyc dashboard open --topic jyc …`) whose name sorted below
  another topic would render the wrong topic's slash commands in the popup
  (e.g. `/deploy` from the `jyc` topic was missing because the table
  highlighted `dotfiles`). The popup now resolves commands by `chat.topic`
  (the topic the user is typing into), and commands are refreshed only
  when the popup is about to open — not on every poll cycle — so the
  popup is always correct and the log stays quiet.
- **Custom shell commands now run in the topic's workspace**, not
  whichever directory jyc was launched from. Matches the agent's `bash`
  tool; use a wrapper script (`shell = ["./scripts/other.sh"]`) to
  override.
- **Opaque "unknown Responses API error" on streamed failures.** The
  Responses provider extracted error text only from the official
  top-level `message` field, so gateways emitting Chat-Completions-style
  nested errors (`{"type":"error","error":{"message":...}}`) or any
  other shape surfaced a generic "unknown" string. Error extraction now
  also checks nested `error.message` / string forms, and — when nothing
  matches — embeds the raw payload (bounded) in the error message so
  failures stay diagnosable from logs. Additionally, streamed error
  events whose text indicates rate limiting or overload (e.g. "rate
  limit", "overloaded", "429") are now classified as throttled and get
  the patient retry schedule instead of dying immediately as terminal.

- **HTTP 400 when replaying mixed-format history after `/model` switch.**
  A topic that previously ran on an Anthropic provider persists assistant
  messages as content-block arrays (`tool_use` blocks, `tool_result` blocks
  in user messages). Switching the topic to an OpenAI-family model replayed
  them verbatim, and the upstream rejected the request with HTTP 400
  (`tool_use` is not a valid field there). OpenAI-family providers now
  convert Anthropic-format raw context to chat format before sending.
- Feishu progress card elapsed time no longer includes queue wait: the
  card clock was captured when the inbound message arrived, so a message
  queued behind a busy topic showed the previous run's processing time
  on top of its own. The clock now resets when the run's fresh
  `ProcessingStarted` arrives, matching the TUI's elapsed-time display.
- Feishu progress card now appears for custom slash commands (e.g.
  `/review`): the watcher was skipped for every `/`-prefixed message,
  but custom commands inject a prompt and continue into an agent run.
  Only built-in command names (which reply without an agent run) are
  skipped now.
- The tail of each thinking block no longer goes missing in live
  thinking displays: the stream loop only published throttled snapshots,
  so reasoning accumulated after the last publish of a request was never
  emitted. A final full-text Thinking event is now published when each
  LLM request's stream completes, fixing the truncated endings in the
  TUI and on the Feishu progress card.
- TUI thinking accumulation no longer duplicates cumulative snapshots:
  the agent publishes the current LLM request's full reasoning content
  per event, so a text extending the last block is now applied as an
  in-place update instead of appended as a new block (#688)

- Feishu duplicate live status messages: slash commands spawned a dormant
  progress watcher (they emit no run events, so it never armed or exited),
  which later double-posted a card alongside the next real message's
  watcher. Slash commands no longer spawn a watcher (or the ⏱ footer),
  and concurrent watchers of a topic now share one status message via a
  create-or-reuse registry (#683)
- Event streams (TUI chat screen, Feishu status card) went dead after
  `/cancel`: the worker-respawn path recreated the topic's event bus,
  orphaning long-lived subscribers on the old channel. Respawn now reuses
  the existing bus (#682)
- TUI chat progress line: the since-last-activity timer no longer drops
  seconds at or above one minute — `2m / 10m20s` now reads `2m05s / 10m20s`
  (hour scale renders `1h30m`) (#680)
- **Topic event bus publish no longer stalls on slow subscribers.**
  `forward_to_subscribers` awaited each subscriber's channel in order, so
  one lagging subscriber (e.g. a Feishu progress watcher blocked in a
  slow HTTP call) stalled the publisher — typically the agent loop — and
  starved every subscriber behind it, including the dashboard activity
  feed. Forwarding now uses `try_send`: a full subscriber has the event
  dropped for it (with a warning) instead of blocking everyone. Covered
  by a regression test. (#677)

- **Feishu status card is sent only when processing actually starts.**
  The progress watcher now waits for the first fresh `ProcessingStarted`
  event before sending the card, so messages that never reach the agent
  (slash commands like `/plan`, empty-body drops) no longer leave a
  `处理中` card spinning until the 2h safety cap. (Re-applies the
  reverted #676.) (#677)

- **Dashboard connections no longer break on `jyc serve` restart.** The
  inspect auth token is now reused across restarts instead of being
  regenerated each time. Previously every `jyc serve` generated a fresh
  random token, invalidating every running dashboard's cached token and
  surfacing as 401 "unauthorized" on reconnect. A fresh token is now
  generated only on the first run (no token file yet). To force rotation,
  use `jyc token reset`. (#672)

- **Config reload no longer kills the dashboard's `agents` websocket
  channel.** Reloading the config (`Ctrl+P` → `reload config` in the
  dashboard, or `POST /api/config/reload`) loaded a fresh config from
  disk that lacked the synthesized `channels.agents` websocket channel
  (built at startup from `[agents.<name>]` entries). The orchestrator's
  reload diff then treated `agents` as removed and cancelled its task,
  dropping the TUI's websocket connection — manifesting as a backend
  "crash", a websocket connection error, and later "unauthorized" on
  reconnect. The reload path now re-runs the agents-channel synthesis
  before storing the config, mirroring startup. The synthesis logic
  moved to `jyc-types` so both startup and reload share one source of
  truth; covered by an integration test. (#671)

- **Config reload now picks up newly added/changed MCP server configs.**
  The tool-registry builder read frozen MCP snapshots (`[[mcps]]`,
  channel-level `mcps`, pattern-level `mcps`, `disabled_mcp_servers`,
  `disabled_tools`) captured at `jyc serve` startup. A config reload
  swapped the shared `ArcSwap` (invalidating the registry cache and
  triggering a rebuild), but the rebuild re-read the same stale list —
  so newly added MCP servers were invisible until a full restart. The
  builder now derives MCP configs from the live `ArcSwap` on each build,
  mirroring how `agent_config()` already re-reads model/provider settings.
  Covered by a regression test. (#673)

- **Repeated identical `/`-command replies no longer scrolled off the
  chat pane.** Any `/`-command whose output is deterministic across runs
  (`/context`, `/exchange`, `/help`, `/model <x>`, …) produced a fresh
  `chat_message` event per run, but the dashboard's poll-driven sync
  from `live_chat` to `self.messages` deduplicated by `(sender, text)` —
  dropping the second and subsequent AI replies. Live AI replies are
  now keyed by the activity-tracker's monotonic per-topic id (each event
  has a fresh one), while the `(sender, text)` rule is retained only for
  user echoes so the local + server-echo timing mismatch does not
  duplicate the user's typed message. A separate `(sender, text)` fallback
  for historical rows with `id == 0` (from `chat_log_store.rs` JSONL
  hydrate, where every pre-seq-field row shares `id = 0`) prevents the
  500 ms poll loop from re-pushing the same historical entry on every
  cycle, which was the failure mode of the earlier `id`-only attempt.
  A new `ChatState::poll_sync_live_chat` helper centralises the rules;
  covered by five regression tests in `dashboard/chat/tests/mod.rs`.

- **Table-aware wrapping for over-wide markdown tables in the dashboard
  chat pane.** tui-markdown previously emitted table rows as single
  unwrapped lines; the pane's generic word-wrap then split box-drawing
  borders mid-row. Tables are now detected and rewrapped cell-by-cell,
  with columns shrunk greedily from the widest column and borders rebuilt
  to fit the pane width. (#664)

- **Slow or hung MCP servers no longer block agent-loop startup.** Three
  coupled fixes on the MCP-loading path:
  1. **Per-server timeout** — each `connect + list_tools` is now wrapped
     in `tokio::time::timeout`. A hung subprocess or unresponsive HTTP
     endpoint gives up after `timeout_ms` (default 10000 ms, configurable
     per server in `config.toml`) instead of blocking forever; the
     server is logged and skipped, the agent loop proceeds.
  2. **Bounded concurrent loading** — MCPs are loaded with
     `buffer_unordered(4)` instead of `await`-in-a-loop, so wall-clock
     cost is `max(per-server latency)` instead of `sum`, capped at 4
     concurrent subprocess/HTTP handshakes.
  3. **Tool-registry cache** — `build_tool_registry` results are now
     cached per `(topic, ArcSwap snapshot pointer)`. The subprocess
     spawn + handshake + `list_tools` only runs when configs change
     (via `ArcSwap::store`), not on every inbound message.

- **TUI edit diff `+`/`-` lines now render in distinct colors.** When
  `edit`/`write` tool detail is expanded under the `ctrl+p T` toggle,
  removed lines (`-`) render in gray and added lines (`+`) in green,
  instead of inheriting the yellow italic default. The previous prefix
  check ran on the post-padding label where the `  -` marker sat two
  columns in, so the check always missed and every diff line ended up
  yellow italic. Styling is now decided on the unpadded line via a new
  `style_diff_line` helper that uses shared `DIFF_REMOVED_PREFIX` /
  `DIFF_ADDED_PREFIX` constants so the producer and the stayer cannot
  drift apart.
- **Feishu progress indicator now appears for built-in commands that
  inject into the agent run.** `/backlog pop` (the only built-in that
  sets `append_body`) previously skipped the channels.rs watcher because
  it was registered as a built-in command, leaving users with no live
  status card even though the agent ran normally. Replaced the binary
  "is the name a registered built-in?" probe with a per-command
  `continues_to_agent` flag on `CommandInfo` (true for `/backlog`,
  default false). `all_commands_with` propagates the flag for custom
  commands — `shell` commands get `false` (no agent run), prompt
  commands get `true`. Unknown slash names continue to spawn the
  watcher via `unwrap_or(true)`, matching the previous behaviour for
  typos and unknown commands.
- **`/backlog push` preserves spaces in single-line descriptions.**
  Typing `/backlog push This is a backlog issue` previously stored
  the text as five separate lines (`This\nis\na\nbacklog\nissue`)
  because the registry's `collect_subsequent_lines` mode pushed each
  space-separated token into `args` as a separate element. The
  registry now collapses the first-line tokens into a single
  space-joined string at `args[1]` before collecting continuation
  lines as `args[2..]`. So `/backlog push hello world` stores
  `"hello world"`, `/backlog push\nline 1\nline 2` stores
  `"line 1\nline 2"`, and the mixed form
  `/backlog push first line\nsecond line` stores
  `"first line\nsecond line"`. An empty placeholder at `args[1]`
  preserves the existing "no description" error path when the user
  pushes nothing.
- **`/backlog pop` regression: registry placeholder broke index parsing.**
  The new args-builder that pushes an empty placeholder at `args[1]`
  (see previous entry) inadvertently changed the index slot for `pop`
  and `rm` from "absent" (None) to "present but empty" (Some("")).
  `parse_n` then returned `Err("invalid index \"\"")` instead of
  `Ok(None)`, so `/backlog pop` failed with a bogus index error even
  though the spec is "`/backlog pop` defaults to popping item 1".
  `parse_n` now treats both missing and empty slots as `Ok(None)` —
  the natural "no value provided" semantics — so `pop` defaults to
  item 1 again and `rm` returns the existing "missing index" error.
  New tests cover both the registry→handler end-to-end path and the
  handler-level index parsing.

- **TUI `/` command popup and `/?` help now list per-agent commands**
  (`[[agents.<name>.commands]]`) for the currently selected topic.
  Previously the popup only showed built-ins + global `[[commands]]`, so
  per-agent commands worked at runtime but were invisible in the UI.
  Each `TopicSummary` / `TopicInfo` now carries its own command list
  (built-ins + globals + per-agent for that topic's pattern; per-agent
  wins on collision, matching `CommandRegistry::register` semantics).
  The dashboard refreshes `chat.commands` whenever the user changes
  topic selection.
- **`/deploy` no longer leaves zombie `jyc serve` processes.** Axum's
  `with_graceful_shutdown` waits for open WebSocket connections to close
  before letting the server exit; previously nothing force-closed those
  connections, so each `/deploy` added another stuck process (the old
  one held the TUI's TCP socket, the new one bound the port; messages
  typed in the TUI went to the dying old server's `TopicManager` whose
  cancel token was already fired → "Worker cancelled" log on every
  message). WS handlers now subscribe to the inspect server's cancel
  token via a new `ws_shutdown` field on `InspectContext` and add a
  `select!` arm that sends a Close frame and returns on shutdown, so
  `jyc stop` completes within seconds and the TUI reconnects cleanly.
  Also fixes the TUI appearing "stuck" after `/deploy` (no
  progress indicator, no replies until restart).

### Removed

- **`InspectOverview.commands` and `InspectState.commands` fields.** These
  overview-level command lists were only consumed by the TUI's `/` popup,
  which now reads the per-topic `TopicSummary.commands` / `TopicInfo.commands`
  field exclusively (the single source of truth, computed server-side).
  Per-poll payload is slightly smaller; no other consumer was affected. (#710)

- **Feishu Typing→DONE reaction chips.** The emoji reactions the pipe bot
  added to the user's message while processing are gone — the live status
  card (`⏳ 处理中…` → `✅ 完成 · Ns · 工具 M`) now covers both progress and
  completion, and reaction chips (emoji + bot name) cluttered the user's
  message. The `⏱ 耗时 Ns` reply footer is unchanged. (#674)

- **Daily `--log-file` rotation.** Reverted (added in this version, now
  removed). Daily-rotated `jyc.log.YYYY-MM-DD` / `dashboard.log.YYYY-MM-DD`
  files produced by `tracing_appender::rolling::daily` cluttered the
  data dir and confused operators about which file to tail. Existing
  dated files are orphaned — delete manually if desired. External
  `logrotate` covers size-based rotation if needed.
- **`tracing-appender` direct dependency.** The `rolling::daily` use
  site was replaced with `std::fs::File` + `std::sync::Mutex` for plain
  append-only writes, so the third-party crate is no longer needed.
  Same behavior as the rolling writer minus the dated suffix; concurrent
  threads still get serialized writes via the `Mutex`.

## [0.3.16] - 2026-08-25

### Added

- **Weekday-restricted pricing windows.** `time_windows` entries now
  accept an optional `days = ["mon", "sat", ...]` filter
  (case-insensitive `"mon"`..`"sun"`; omitted/empty = every day), so a
  provider can price differently on specific days of the week — e.g.
  weekend-only all-day discounts via `start = "00:00"`, `end = "00:00"`
  (a `start == end` window now spans the whole day).

- **Feishu progress indicator.** Feishu is now a pipe-only adapter (no
  chat pane); the long silence while the agent is working used to leave
  users wondering whether the bot had crashed. When the inbound adapter
  receives a message it now drops a `Typing` reaction (⌨️) onto the
  user's original message, and the reply forwarder swaps it for `DONE`
  (✅) on the first reply for that topic. No new app permissions are
  required — the bot's existing `im:message` scope covers both
  `POST .../reactions` and `DELETE .../reactions/{id}`. State is
  in-memory; a daemon restart mid-task leaves the `Typing` reaction
  stuck until cleared manually. Multiple inbound messages on the same
  topic before a reply: only the latest `Typing` is swapped on reply;
  older ones remain until cleared manually.

- **Verbatim tool-result cap (`tool_result_cap`).** New optional field on
  `ContextStrategyConfig` (sliding_window only) that caps each tool
  result in the verbatim region ② + ③ at N bytes. Truncated results
  are suffixed with the standard `… [truncated N bytes]` marker — same
  format as the existing compaction truncation, so the model sees
  consistent truncation across all boundaries. Handles both OpenAI
  (`role: "tool"`, string `content`) and Anthropic (`role: "user"`
  with `[tool_result]` blocks) wire formats; non-tool messages and
  the compacted region ① pass through untouched. `Some(0)` or absent
  = off (every tool result sent in full). Set via `config.toml`
  (`[ai].context_strategy.tool_result_cap` or per-pattern), the
  runtime override file `.jyc/context-strategy.json`, or the new CLI
  arg `/context sliding [N] [M] [CAP]` (CAP in `0..1 MiB`).

- **Wire-payload debug dump (`/context dump on|off`).** Inspect what the
  model actually sees, one JSON line per LLM call, at
  `<topic>/.jyc/wire-payload.jsonl`. Toggle with
  `/context dump on|off` (no args shows current state + file path).
  Each line carries the wire payload, its parallel `regions` array
  (`1`=compacted history / `Full`-mode, `2`=verbatim, `3`=current turn),
  the active strategy, iteration number, and ISO-8601 timestamp. The
  file is capped at 50 lines (oldest dropped) — bounded footprint,
  small enough to diff between iterations with `git diff` or pipe
  through `jq`. The dump flag survives `/reset`, `/new`, `/close`;
  disable explicitly when done. Best-effort IO: file errors are logged
  at `warn` and never propagated, so a broken dump cannot break the
  agent loop. For a single in-process human-readable view (ASCII boxes
  per region), call `dump_send_context` from
  `crates/jyc-agent/src/agent_loop/context.rs`.

### Fixed

- **Up-arrow in chat input field recalled second-to-last entry after a
  submission made while browsing history.** `send_message_inner` pushed the
  new text into `input_history` but left `history_pos` pointing at its
  previous slot, so the next `Up` decremented from the stale cursor into
  the now-larger history — surfacing the wrong entry and making the
  just-sent message unreachable until the user Down-arrowed out. Reset
  `history_pos = None` after the push (and before any future push, the
  empty-text early return still preserves the cursor). PR #655.

- **No-reply nudge skipped when reply tool registered (MiniMax).** The
  no-reply gate required `!reply_tool_available`, so when `jyc_reply_message`
  was registered the loop never injected the
  `[System reminder] Your last turn produced no text and no tool call…`
  prompt — a model that issued tool calls across several iterations and
  then returned an empty response on the final one (observed with
  MiniMax M3: `grep` then two `read`s then nothing) exited with
  `text_len=0, reply_sent_by_tool=false` and the user saw no reply. The
  `no_reply_reminded` flag already keeps the nudge single-shot, so
  dropping the `!reply_tool_available` clause costs nothing. The
  fallback auto-delivery path below only fires for non-empty text and
  was therefore unreachable in this case. The failure-aware reminder
  (concrete tool error) is unchanged. (#652)

- **Thinking content leaked into delivered replies (MiniMax & DeepSeek).**
  The sliding-window prior folded each turn's tool calls into a user-role
  **text history note** (`[History note] assistant tool calls: …`).
  Thinking models learned from that text form to write their own process —
  tool calls and action announcements ("Now let me read…") — as content
  instead of using the structured tool-calling channel; when that channel
  came back empty, the narration was auto-delivered as the reply. The most
  recent `note_window` prior turns are now sent **verbatim** (structured
  `tool_calls` + tool results intact, like the current turn), older turns
  compact to text-only, and no history notes reach the wire. (Trigger
  isolated by `note_window = 0`; structured verbatim keeps the tool
  context that `0` loses.) `note_window` semantics change from "recent
  turns carry a note" to "recent turns are sent verbatim". (#651)
- **Reply text truncated after a backtick.** `split_think_tags` treated a
  literal `<think>` written mid-reply (e.g. inline code "`<think>`" while
  discussing the format) as a thinking-block start; with no closing
  `</think>` in the stream, everything after it was swallowed into
  `reasoning_content` and the delivered reply was cut off at the backtick.
  A `<think>` is now treated as a block start only when it is the first in
  the stream or is followed by a closing `</think>`; literal occurrences
  stay in the reply text. (#651)
- **Injected reply-tool reminder confused the model.** When a text-only
  finish skipped `jyc_reply_message`, the loop injected a
  `[System reminder] … call jyc_reply_message` user message and ran one
  more LLM turn restricted to the reply tool. The reminder's
  `silent: true` escape hatch led models to close the turn without
  delivering anything (the user saw no reply), and the extra turn could
  re-summarize the answer differently. The injection is removed: any
  non-empty text-only finish with the reply tool registered is now
  auto-delivered immediately via the synthetic `jyc_reply_message`
  execution (with the `— auto-delivered` trace), guaranteeing delivery,
  saving one LLM call, and keeping the delivered text identical to what
  the model wrote. The failure-aware reminder (concrete tool error) and
  the legacy no-reply reminder (reply tool unavailable) are unchanged.
  (#648)
- **Synthetic auto-delivery never published `ReplySent`.** When a
  text-only finish was auto-delivered in the agent's name via the
  synthetic `jyc_reply_message` execution, the reply was sent through the
  channel's outbound adapter and broadcast on the per-channel WebSocket
  bus, but no `ReplySent` event was published on the topic event bus.
  The dashboard chat pane ignores the raw `reply` broadcast (a de-dup
  refactor) and renders live replies only from `chat_message` events
  fanned out of `ReplySent` — so the reply appeared in the logs but never
  in the chat pane. The synthetic path now publishes `ReplySent` on
  synchronous delivery, mirroring the real tool-call path (exactly once
  per delivery; file-relay deliveries still leave the event to the
  worker). This also fixes cycle-boundary progress replies, which went
  through the same path. (#646)
- **Windowed history-note format confusion.** Three clarifications to the
  `[History note] assistant tool calls: …` summary injected into the
  sliding-window context: (1) the note is now emitted *before* the
  assistant text it describes instead of after, so an `assistant →
  user(note)` adjacency never sits right before the next real user
  message; (2) the `[SUCCESS] `/`[ERROR] ` prefix that
  `format_tool_result` bakes into OpenAI tool-result content is stripped
  from the note, with `[ERROR] ` honored as the error signal, so both
  OpenAI and Anthropic failures render uniformly as `→ [error] …`; (3)
  truncation is now marked explicitly as `… [truncated N bytes]` instead
  of a bare ellipsis, so the model can tell a real cut from content that
  genuinely ends in `…` and sees how much was dropped.
- **Feishu progress indicator used chat_id instead of message_id.** The
  Typing/DONE reactions now read `message.external_id` (the message_id)
  instead of `message.channel_uid` (the chat_id). (#642)

- **DeepSeek thinking mode rejected the forced reply tool.** The
  reply-recovery turn used to force `jyc_reply_message` at the API level
  (`tool_choice`); thinking-mode OpenAI-compatible models (DeepSeek v4,
  Kimi k3, MiniMax M3) reject any request carrying `tool_choice` with
  HTTP 400, which killed the whole agent round with no reply delivered.
  The API-level forcing is removed entirely: the recovery turn simply
  restricts the tool list to `jyc_reply_message`, and a text-only finish
  that persists after the nudge is now auto-delivered in the agent's name
  via a synthetic `jyc_reply_message` execution — delivery and chat-log
  entry are identical to a real tool call, with a subtle
  "— auto-delivered" trace, and the delivery is flagged in metrics as
  `reply_by_auto` so this degradation rate stays measurable — instead of
  the degraded fallback path. (#640, #644)
- **Reply-delivery guard enforcement.** The agent loop's reply reminders
  are now backed by a tool-restricted recovery turn: after a reminder,
  the next LLM call offers only `jyc_reply_message`, so the model cannot
  re-emit narration instead of delivering. Failed `jyc_reply_message`
  calls (missing/empty message, bad attachment) now trigger a
  failure-aware reminder quoting the concrete tool error, and reply-tool
  error messages state explicitly that the reply was not delivered (#637)

- **Tool results in the current turn (region ③) are no longer truncated
  by `tool_result_cap`.** The model reasons over region ③ tool results
  mid-loop — a just-returned `bash` output it needs to act on, an
  `read` result it is about to summarize — so truncating them silently
  fed the model incomplete data. `tool_result_cap` now applies only
  to region ② (the verbatim prior region); region ③ is passed through
  with `cap = 0` regardless of the configured cap. To bound region ③
  payload size, fix the underlying tool (`head`, `grep`,
  pagination, etc.) instead. (#657)

### Changed

- **/` +exchange` output is now a markdown bullet list.** Instead of `name: url`, multiple published files are rendered as a markdown list with the filename on one line and the raw shareable URL on the next, keeping URLs copy-pasteable while displaying better in chat and email.

- **Default `ContextStrategyConfig` is now `sliding_window / 10 / 3 / 2048`.**
  Users without an explicit `[ai].context_strategy` in `config.toml` and no
  `.jyc/context-strategy.json` runtime override previously fell back to
  `full` mode (entire raw context on every request); they now default to
  `sliding_window / window=10 / note_window=3 / tool_result_cap=2048` —
  the configuration we've been running internally. Resolution
  chain priority is unchanged, so users with any config.toml setting or
  runtime override are unaffected. To restore full-mode behaviour, set
  `context_strategy = { mode = "full" }` under `[ai]`.

- **History notes filter out `jyc_reply_message`.** Windowed-view
  annotations no longer list the reply tool call alongside real tool
  calls; the reply's `message` is the text the user already saw and is
  preserved in the assistant's own text. Exposing the call as a
  modelable pattern was the root cause of the model mimicking the
  `[History note] assistant tool calls: …` format as narration instead
  of invoking the tool. A turn that called only the reply tool now
  emits no note at all (#641).

- **Post-pipe-migration cleanup.** The five older pipe adapters
  (email/github/gitee/feishu/wecom_bot) now share the match/retarget/route
  helpers extracted in #635 (`match_pipe`, `retarget_or_drop`,
  `route_into_pipe_target`, `warn_on_bad_pipe_patterns`) instead of inline
  copies. `build_outbound_adapter` was inlined into its single call site
  (the websocket hub setup). config.example.toml's agent-runtime examples
  (channel-level MCPs, tool exclusion) now target the hub channel and note
  that pipe-only channels ignore `mcps`/`disabled_tools`/`skills` (#638)

- **wecom and wecomkf are pipe-only adapters.** Both channel types were
  migrated to the pipe architecture (docs/architecture/overview.md): pattern
  match → `pipe = { agent, topic }` retarget → hub routing, with reply
  forwarders subscribed to the hub broadcast. Patterns must now set
  `pipe`; matched messages without a pipe target are dropped with a
  warning. wecomkf keeps its sync cursor / msgid dedup as protocol state;
  KF replies remain text-only. The `user_name` topic.json fallback in
  chat logs is now channel-agnostic (no `wecomkf` special-case in core)
  (#635)

### Removed

- **Dead pipe-migration leftovers.** `ChannelRegistry` (zero call sites)
  and the `topic.json` write/read chain: `apply_pipe_retarget` rewrites
  `message.channel` to the pipe target, so the hub worker's
  `channel == "wecomkf"` gate could never fire — the writer, the
  `topic_json` module, and chat-log's `user_name` fallback reader were all
  unreachable (#638)

- **WeChat (OpenILink bridge) channel.** Removed entirely — it was the
  last full channel without a pipe-migration path and saw no production
  use. `[[channels]]` entries with `type = "wechat"` no longer start
  (#634)

### Added

- **`note_window` option for the `sliding_window` context strategy.**
  Within the window of N turns, only the most recent M turns carry
  tool-call history notes; older turns become text-only. Unset (default)
  keeps notes on all windowed turns — behavior unchanged. Settable via
  `context_strategy = { mode = "sliding_window", window = 10,
  note_window = 3 }` (pattern / `[agents]` / `[ai]`) or
  `/context sliding [N] [M]` (#632, #633)

- **New topical doc: `docs/architecture/context.md`.** Consolidates how
  the wire payload is shaped (context strategies, turn pairing, history
  notes with truncation budgets, `context_browse` for dropped turns,
  token safety nets, configuration resolution chain) into one reference;
  DESIGN.md keeps the design decision and links to it. (#631)

- **`jyc_reply_message` `silent: true` parameter.** Closes the turn
  WITHOUT delivering anything (no adapter call, no signal files, no
  fallback text) while still counting as reply-handled — the deterministic
  "nothing to send" escape, e.g. when a system reminder fired but the
  reply was already delivered (#630)

- **Windowed context now shows tool results.** The sliding-window
  annotation — `(incl. followed tool calls: bash(command="ls") → BRANCH
  main…)` — appends each call's truncated result (`→ <text>`) so the agent
  can see *what its tools returned* on prior turns, not just that they ran.
  Results are matched to calls by id (OpenAI `tool_call_id` / Anthropic
  `tool_use_id`) and capped at 500 bytes with `…`; a call with no known
  result gets no arrow. The full result is one `context_browse` call away.
  Prevents the agent re-exploring when prior tool output fell out of the
  visible window. (#627)

- **New built-in tool: `context_browse`.** Lets the agent page through the
  in-memory conversation transcript — user/assistant text pairs, including
  turns that fell out of the sliding context window. `offset` counts pairs
  skipped from the newest end (offset 0 = the most recent pairs),
  `limit` caps the page size (default 10, max 50). The tool reads the
  in-memory `raw_context` snapshot injected per tool batch, never the
  persisted `agent-context.json` (which is stale mid-loop). (#624)

- **Agent never stages JYC's private `.jyc/` runtime data.** The `bash`
  tool injects a global git excludes file (`$XDG_DATA_HOME/jyc/git-ignore-global`)
  via `GIT_CONFIG_*` env vars, so `git add .` / `git status` ignore `.jyc/`
  in **any** repo with zero repo footprint (no `.gitignore` entry, no
  `.git/info/exclude` — nothing visible to collaborators). The system
  prompt additionally instructs agents to never stage `.jyc/` (including
  `git add -f`, which bypasses all ignore rules). (#620)

- **`[agents.<name>]` inheritance via `extends = "<base>"`.** A new agent
  can reuse most of another agent's config and override only the fields
  that differ: child fields win over the base, arrays/lists are replaced
  wholesale (not merged), `extends` chains resolve recursively
  (`A extends B extends C`), and an empty-string value in the child
  (`topic_path = ""`) clears the inherited value so the field falls back
  to its default (there is no "unset to None" for list fields — they can
  only be replaced). A missing base agent or an extends cycle fails config
  loading with a clear error. The `extends` key is consumed at parse time;
  both agents remain routable independently.

- **New skill: `github-developer`.** Developer role for GitHub PRs —
  implements the planner's spec step-by-step on the existing PR branch,
  commits/pushes after each step, runs checks and tests, fixes CI
  failures and reviewer feedback, then hands off via the
  `ready-for-review` label. Converted from
  `templates/github-developer/AGENTS.md` following the `github-planner`
  skill conversion pattern; the template itself is unchanged. (#607)

- **Upstream close events now close the piped agent topic.** When a
  GitHub issue/PR is closed or a Feishu group chat is disbanded, the
  piped agent topics (e.g. `plan-<N>`) are closed and their directories
  deleted. Hard safety rule: only topics whose resolved path lies under
  the agents workspace root (`<data_home>/agents/`) are deleted — topics
  pinned to a custom `topic_path` (e.g. a real project checkout) are
  skipped with an info log, and canonicalization blocks symlink escapes.
  Manual `/close` (with `--confirm`) is unchanged. (#608)

- **GitHub is now a pipe-only channel adapter.** The poller matches its
  own patterns, re-targets each event into a hub channel (or an agent
  topic), and a per-target reply forwarder posts agent replies back as
  issue/PR comments. The channel no longer owns a TopicManager, agent
  service, outbound adapter, or orchestrator registration — all topics
  live in the pipe target. Dedup/cursor state moves to
  `<workdir>/channels/<channel>/.github/` (one-time rename from the old
  `<workdir>/<channel>/.github/`; if the rename fails, dedup starts fresh —
  the comment cursor starts at startup so no comment flood, but
  already-open issues/PRs re-trigger once as "opened" events, as on a
  first deploy). Comments keep the `[Role]` prefix (self-loop
  prevention) but no longer carry a model/mode/token footer.
  **BREAKING: every enabled GitHub pattern must now declare a `pipe`
  target — matching messages are dropped otherwise (warned at startup).**

- **Gitee is now a pipe-only channel adapter**, same architecture as
  GitHub. The poller matches its own patterns, re-targets each event into
  a hub channel (or an agent topic), and a per-target reply forwarder
  posts agent replies back as issue/PR comments. The channel no longer
  owns a TopicManager, agent service, outbound adapter, or orchestrator
  registration. Dedup/cursor state moves to
  `<workdir>/channels/<channel>/.gitee/` (one-time rename from the old
  `<workdir>/<channel>/.gitee/`; if the rename fails, dedup starts fresh —
  the comment cursor starts at startup so no comment flood, but
  already-open issues/PRs re-trigger once as "opened" events, as on a
  first deploy). Comments keep the `[Role]` prefix (self-loop
  prevention) but no longer carry a model/mode/token footer. Gitee uses
  **separate number spaces for issues and PRs**, so the reply-relay map
  records the item type and a close event only closes topics of the same
  type. `GiteeOutboundAdapter` was deleted.
  **BREAKING: every enabled Gitee pattern must now declare a `pipe`
  target — matching messages are dropped otherwise (warned at startup);
  per-pattern `template` no longer applies.**

- **`gitee-init`, `gitee-planner`, and `gitee-developer` skills** (`skills/`).
  Ported from `templates/gitee-{planner,developer}/AGENTS.md` following the
  github skill conversion pattern: the checkout is the topic directory
  itself (no `repo/` subdirectory). `gitee-init` clones via plain `git
  clone` (Gitee has no `gh` CLI) and excludes framework files via
  `.git/info/exclude`; the curl+jq Gitee API v5 workflow in the templates
  is kept verbatim. Copy them into `{workdir}/skills/` to replace the
  templates.

- **`${msg.pr_number}` / `${msg.issue_number}` / `${msg.repo}` pipe topic
  placeholders for GitHub.** The number aliases are type-gated: a PR event
  carries only `pr_number`, an issue event only `issue_number`, so a
  pattern configured with `topic = "plan-${msg.issue_number}"` that
  accidentally matches a PR event fails placeholder resolution and drops
  the message with a warning instead of silently landing PR traffic in an
  issue topic. `${msg.repo}` disambiguates topics when several GitHub
  channels pipe into the same agent
  (`review-${msg.repo}-${msg.pr_number}`). Typical routing:
  `pipe = { agent = "jyc_git", topic = "review-${msg.pr_number}" }` for
  review, `dev-${msg.pr_number}` for develop, `plan-${msg.issue_number}`
  for planning — collapsing roles into one shared topic or splitting them
  is purely a config choice.

- **`github-init` and `github-planner` skills** (`skills/`). `github-init`
  clones the repository **into the topic directory itself** (not a `repo/`
  subdirectory) and excludes framework files (`.jyc/`, `attachments/`) via
  `.git/info/exclude`; the repository's own `AGENTS.md` therefore lands at
  the topic root, where the prompt builder already loads it as project
  instructions every turn. `github-planner` ports
  `templates/github-planner/AGENTS.md` to a skill and delegates setup to
  `github-init`. Copy them into `{workdir}/skills/` to replace the GitHub
  pattern templates.

- **`${msg.topic}` pipe topic placeholder** — resolves to the inbound
  message's own derived conversation name, so a pipe topic can compose a
  prefix with it: `pipe = { agent = "jin", topic = "mail-${msg.topic}" }`.
  For email the adapter feeds it the subject-derived name, i.e. `Re:` /
  `Fw:` / `回复:` / `转发:` and configured pattern prefixes already
  stripped (a `Re: Fw: Invoice 42` subject yields topic
  `mail-Invoice 42`). Missing/empty values still drop the message with a
  warning rather than routing to a literal `${msg.topic}` topic. (#598)

- **Time-of-day pricing.** `ModelPricing` gains optional `time_windows`
  (each window supplies its own per-1M rates for the hours between
  `start` and `end`; rates omitted on a window inherit the flat values)
  and `utc_offset` (fixed UTC offset, default UTC).
  Every LLM call bills at the rates in effect when it completes: the
  first window containing the current local time wins, otherwise the
  flat rates apply. Supports DeepSeek-style off-peak discounts,
  including windows that wrap past midnight (e.g. `16:30` → `00:30`).

- **`[agents.<name>]` config table** — each entry becomes a websocket
  endpoint with behavior fields (template, topic_path, skills, access,
  attachments, model overrides, mcps, tools, disabled_*,
  live_injection, inject_inbound_images, mode, reset_compression,
  auto_reset_threshold, role). WebSocket transport is unchanged
  (`/ws/agents/<topic>`). Each topic gets its own directory under
  `<data_home>/agents/<agent_name>/<topic_name>/`; an explicit
  `topic_path` instead pins the agent's 1:1 topic (topic name == agent
  name) to that directory. Each `[agents.<name>]` is one
  pattern inside the synthesized channel "agents" (channel_type =
  "websocket"); the agent name is the routing identity, picked by
  `WebsocketMatcher::match_message` against `message.topic`.

- **`pipe = { agent = "<name>", topic = "..." }`** — new pipe target
  form. Mutually exclusive with `pipe.channel`/`pipe.pattern` (validated
  at load time). Legacy form still works with a deprecation warning.

- **Billing ledger records applied rate + time-window provenance.**
  Every line in `.jyc/bill-YYYY-MM-DD.jsonl` now carries
  `input_rate_per_million`, `output_rate_per_million`,
  `cache_hit_rate_per_million`, `time_window` (e.g. `"16:30-00:30"` or
  `null` for flat rates), and `utc_offset`. The pricing module also
  emits a `tracing::debug!` line on each call naming the rates in
  effect, so a misconfigured `time_windows` shows up in the logs
  rather than only at month-end reconciliation.

- **Context management strategy** — a new `context_strategy` field on
  `[ai]`, `[[channels.<name>.patterns]]`, and `[agents.<name>]`
  controls how prior conversation history is sent to the LLM, plus a
  `/context` slash command for runtime switching. Two modes:
  `full` (default — current behavior) and `sliding_window` (only the
  last N user+assistant turns, default N=10; the current turn is kept
  intact with all tool calls/results). The on-disk
  `.jyc/agent-context.json` always stores the
  full raw context unchanged — the strategy only shapes the wire
  payload — so switching back to `full` recovers the entire history.
  Runtime override is persisted at `.jyc/context-strategy.json` by
  `/context full | sliding [N] | reset`. `sliding` is accepted as an
  alias for `sliding_window`; `/context sliding N` accepts N in
  `1..=200`. The send context is reformatted via the active
  `Provider`, so Anthropic and OpenAI-compatible wire formats both
  stay valid.

- **Channels / agents / AI migration design doc** (`docs/agents-migration.md`):
  locked decisions, target three-layer model, and phased PR plan.
- **Config template regression test**: `config.example.toml` must parse and
  pass `validate_config` in CI.
- **Core / hub / adapters architecture document.** `docs/architecture/overview.md`
  describes the target three-layer architecture (core: per-topic queues +
  workers; hub: the websocket channel, the only layer owning topics and
  agents; adapters: protocol-only channels that pipe into the hub). Feishu is
  the first migrated channel; other channels will follow.

- **Topic file download endpoint.** `GET /api/topics/{channel}/{topic}/files/{file...}`
  serves topic-local files in place under the bearer middleware — unlike
  `/exchange/...`, which stays reserved for agent-published files
  (`jyc_publish_file`). Paths under `.jyc/` are rejected, including via
  symlink escapes.

- **Websocket replies broadcast attachments.** `reply` payloads gain an
  optional `attachments` array (`filename`, `content_type`, `path`) whose
  entries point at the new files endpoint.

- **Feishu pipe relays reply attachments.** The pipe reply forwarder
  downloads each broadcast attachment from the files endpoint, applies
  `[attachments.outbound]` policy, and re-uploads it to the feishu chat
  (image vs. file by content type).

- **Dynamic pipe topics via `${msg.chat_name}`.** A pattern's `pipe.topic`
  may embed the runtime placeholder `${msg.chat_name}`, resolved per message
  from the chat-name metadata (sanitized for filesystem use), so one feishu
  `mentions` pattern can route each group chat to its own topic. Messages
  without a chat name are dropped with a warning when the placeholder is
  used. (#568)

- **Pipe targets decouple pattern config from topic name.** `pipe` gains an
  optional `pattern` field naming the target channel pattern whose config
  applies, and `topic` is now optional (defaulting to `pattern`):
  `pipe = { channel = "local_dev", pattern = "group_chat", topic = "${msg.chat_name}" }`
  gives each dynamically-derived topic the `group_chat` pattern's
  `topic_path`/template/skills. Legacy `topic`-only form is unchanged.
  (#570)

### Changed

- **/` +exchange` output is now a markdown bullet list.** Instead of `name: url`, multiple published files are rendered as a markdown list with the filename on one line and the raw shareable URL on the next, keeping URLs copy-pasteable while displaying better in chat and email.

- **Windowed tool-call summaries moved out of the assistant's own text.**
  The sliding window used to fold bare tool calls into the assistant
  message as an in-text annotation (`(incl. followed tool calls: …)`);
  models learned to mimic that format and emit fake tool-call summaries as
  plain reply text, believing they had replied when no tool call happened.
  `extract_pairs` now returns turns whose assistant entry holds ONLY the
  assistant's text, with the tool-call summary (args kept, results
  appended as `→ <result>`) emitted as a separate user-role `[History
  note] assistant tool calls: …` message. Round-tripped notes are skipped
  on re-parse so heuristic compaction stays stable; `context_browse` shows
  the note as a `[N] TOOLS:` line (#630)
- **Reply reminders offer an explicit silent escape.** The no-text /
  text-only / mimicry system reminders no longer assert "your reply was
  NOT sent" (wrong whenever the reply HAD been delivered) and now instruct
  the model to call `jyc_reply_message` with `silent: true` when nothing
  needs sending — instead of producing narration that the fallback path
  would deliver to the user (#630)

- **`jyc_reply_message` now delivers synchronously.** The reply bridge tool
  delivers through the channel's outbound adapter immediately and returns
  the REAL delivery result (with `message_id`) to the model — previously it
  wrote `reply.md` and reported "Reply sent" before anything was delivered,
  leaving the model with a fake success receipt when the websocket
  broadcast or the 2s-polling watcher lost the message. On direct-delivery
  failure the tool falls back to the file relay and says "queued" instead
  of claiming success (#629)

- **Sliding-window pairing is now turn-based.** `extract_pairs` used to
  pair a user message with only the FIRST following assistant message,
  silently dropping every later assistant message of a multi-step tool
  turn — including the turn's final reply and the `jyc_reply_message`
  call the user actually saw. All assistant messages between two user
  messages now merge into one pair entry, each step keeping its text
  plus folded tool-call annotation. Tool results are collected in a
  single pass into a turn-scoped map, so a `tool_call_id` reused across
  turns can no longer misattach its result. The whole annotation is
  capped at 2000 bytes (calls past the budget fold into `…(N more
  calls)`), and `jyc_reply_message`'s `message` parameter keeps up to
  1000 chars (vs. the generic 200) since it is the text the user
  actually saw. Failed tool results render as `→ [error] …`. (#628)
- **Transcript rendering unified.** `render_raw_context_as_text` existed
  in two drifted copies; there is now one implementation built on
  `extract_pairs`, shared by the sliding-window view, session
  compression, and cycle-boundary progress summaries. (#628)

- **System prompt tells the agent when to use `context_browse`.** The Chat
  History prompt section now points the agent at the `context_browse` tool
  for recalling earlier turns of the current conversation that have fallen
  out of its context window (offset pages toward older pairs, limit caps the
  page), instead of only steering it to `read`/`grep` the per-day
  chat-history JSONL. The tool was previously registered and documented but
  never mentioned in the prompt, so agents rarely called it. (#626)

- **Sliding-window context now annotates assistant messages with their
  bare tool calls.** The windowed part (recent user+assistant pairs) folds
  the assistant's tool calls into the text as
  `(incl. followed tool calls: name(arg=value, …))`, keeping **all**
  parameters and truncating only a single argument value over 200 chars.
  Tool calls were previously stripped entirely, so the model saw a gap
  between an assistant text that ran tools and the following turns;
  tool-call-only turns (no text of their own) are kept as the annotation
  alone instead of being dropped. The
  annotation is text-only, so no tool-call/tool-result pairing constraints
  apply (those only matter for the verbatim `current` turn). (#623)

- **Transient retry backoff raised from 1s/2s to 10s/20s.** A transient
  failure — e.g. an SSE idle timeout that already waited
  `sse_read_timeout` (default 120s) on a silent stream — was retried after
  only 1 second, effectively "immediately", so the retry usually hit the
  same silent upstream again. The retry message (`next retry at …
  (in Ns)`) now shows a meaningful wait. Attempt budget unchanged
  (3 attempts, 2 retries). (#617)

- **email channel is pipe-only** — `email` joins `feishu`/`wecom_bot` as a
  pipe-only adapter (see `docs/architecture/overview.md`). Every enabled
  pattern must declare a `pipe` target; the adapter keeps only the IMAP
  monitor and SMTP reply forwarders (one per pipe target channel), which
  reply into the original mail thread (`In-Reply-To`/`References`).
  A pattern without `pipe.topic` uses the subject-derived topic name.
  Email replies are now plain agent text — no model/mode/tokens footer,
  so `[channels.<name>.footer]` no longer applies to email channels; nor
  does per-pattern `[channels.<name>.patterns.attachments]` (the global
  `[attachments.inbound]` policy still applies). The email channel no
  longer appears in the orchestrator / dashboard channel list, and
  `jyc_send_message` addressed to an email channel name is no longer
  supported (send to the hub topic instead).

- **IMAP mailbox cursor state moved to `<workdir>/channels/<channel>/.imap/`**
  (was `<workdir>/<channel>/.imap/`). Existing state is not migrated: after
  upgrading, the monitor starts from the newest message in the mailbox (no
  re-processing flood). The generic per-channel `StateManager` in
  `serve` is gone — email was its only consumer.

- **Dashboard overview Details panel panes are now framed** — the topic
  info pane is fully enclosed (`Borders::ALL`) and the activity log uses
  `Borders::TOP | Borders::LEFT | Borders::RIGHT`, giving the panel a
  continuous left/right edge with no open gap above or below the info
  pane. The chat-screen info and activity panes are unaffected.

- **`[agent]` config table renamed to `[ai]`** at all levels (top-level,
  `[channels.<x>]`, topic `.jyc/config.toml`). The legacy key `agent` is
  still accepted with a deprecation warning. Code: `AgentConfig` →
  `AiConfig`. See `docs/agents-migration.md` for the full migration plan.

- **wecom_bot channel is pipe-only** — `wecom_bot` (WeCom Smart Robot)
  joins `feishu` as a pipe-only adapter (see `docs/architecture/overview.md`).
  Every enabled pattern must declare a `pipe` target; the adapter owns
  the WebSocket long connection and the streaming-reply lifecycle
  (`finish=false` indicator on receipt, a self-terminating keep-alive
  task that re-sends `finish=false` with a spinning-elapsed indicator
  every 3s to keep the WeCom passive-reply window open during long
  agent runs, `finish=true` on reply, and proactive-`aibot_send_msg`
  fallback for both the text and attachments when the streaming
  window has already closed). Inbound attachments continue to flow
  through `media::process_bot_attachments` unchanged. The
  TopicManager/agent/orchestrator registration for `wecom_bot` is
  removed; the `channel_type() == "wecom_bot"` progress-spinner path
  in the worker is dropped (the pipe adapter owns the streaming
  lifecycle).

- **Pipe topic templates support `${msg.<key>}` for any metadata key**
  (previously hardcoded to `chat_name`). Convenience: `channel_uid`
  resolves to the channel's conversation identity (group chatid /
  single chat userid), so `topic = "bot-${msg.channel_uid}"` unifies
  group and single chats in one template. Compatible with the
  existing `${msg.chat_name}` form.

- **Reverted the `[hub]`/`[adapters]` config tables and `pipe.hub` rename
  (#573)** — never deployed; `[channels]` is again the single channel table.
- **Aligned HTTP `User-Agent` defaults.** The Anthropic native provider
  (`type = "anthropic"`) now honors `ai_providers.<name>.user_agent`
  instead of silently dropping it; behavior matches the OpenAI-compat
  provider. When no `user_agent` is configured, all provider requests
  fall back to reqwest's default `reqwest/<version>` UA — no jyc
  fingerprint leaks unless the operator opts in. The `webfetch` tool
  switched from a hardcoded `jyc-agent/0.1` to a modern Chrome UA
  (`Mozilla/5.0 ... Chrome/131.0.0.0 ...`) for compatibility with
  common anti-bot sites.

- **Feishu is now a pipe-only adapter.** Following the core / hub / adapters
  architecture, the feishu channel no longer creates its own outbound
  adapter, agent service, TopicManager, StateManager, or orchestrator
  registration — it only receives events, pipes matching messages into a
  websocket hub channel, and relays the hub's replies back. Consequences:
  - A matched pattern **without** `pipe` now drops the message with a
    warning (previously it was routed through feishu's own TopicManager);
    startup logs a warning for each such pattern.
  - Chat-disbanded events no longer close topics (already a no-op for piped
    topics).
  - Feishu no longer appears in the orchestrator / dashboard channel list.
  - Proactive messages to a feishu chat now go through the hub channel:
    `jyc_send_message(channel = "<hub>", recipient = "<piped topic>")` is
    relayed by the pipe forwarder (mapping is in-memory, rebuilt on inbound
    traffic).

- **Terminology: "thread" → "topic".** The conversation-workspace concept
  (formerly "thread") is now "topic" everywhere — `TopicManager`,
  `topic_name`/`topic_path`/`topic_prefix`, the WebSocket `topic` field,
  the `pipe = { channel, topic }` mapping, `.jyc/topic-name`, and all docs —
  to avoid confusion with OS threads. The email `topic_refs` field became
  `references` (it maps to the `References` header). A one-time migration
  renames existing `.jyc/thread-name` files to `.jyc/topic-name`.

- **Config now rejects unknown keys.** `[general]` and channel patterns use
  `#[serde(deny_unknown_fields)]`, so a typo or a legacy `thread_*` key
  (`max_concurrent_threads`, `max_queue_size_per_thread`, `thread_name`,
  `thread_path`) fails at startup with an error naming the key and the
  correct `topic_*` field — instead of being silently ignored and falling
  back to defaults.

- **Channel pattern `pipe` now takes an explicit mapping**
  (`pipe = { channel = "local_dev", topic = "jyc" }` instead of a bare
  target channel). Matching messages are re-targeted to the target
  channel/topic and routed through the target's own `MessageRouter` —
  the exact same path as a chat-pane message — so the target pattern's
  `topic_path`, template, skills and model apply identically; replies
  are relayed back to this channel's users. The former
  `pipe = "<channel>"` string form is replaced by this mapping.
  For feishu, `pipe` later became mandatory — see "Feishu is now a
  pipe-only adapter" above.

### Fixed

- **Annotation mimicry guard.** The sliding window's
  `(incl. followed tool calls: … → …)` history annotations were sometimes
  mimicked by the model as reply text, making it believe it had replied
  when no tool call happened; the narration was then delivered via the
  fallback path. The system prompt now states the format is a read-only
  history summary, and the reply-tool guard injects a mimicry-specific
  reminder ("your reply was NOT sent") when the final text contains the
  annotation marker (#629)
- **Lost tool replies are now user-visible.** When the reply tool signaled
  a reply but its content was lost before delivery, the user saw nothing
  (only a log warning); a distinct warning message is now delivered instead
  of being conflated with the "finished without calling jyc_reply_message"
  fallback case (#629)

- **Window fallback keeps the LAST user message, not the first.** When
  no complete pair exists, the fallback used to resurrect the oldest
  user message (usually ancient, unrelated history) in front of the
  current turn; it now keeps the latest text-bearing user message and
  skips Anthropic `tool_result` wrappers. (#628)

- **Agent "thinking" text no longer delivered verbatim as the final reply.**
  With `jyc_reply_message` registered, a text-only finish that never calls
  the reply tool (the model's process narration, previously delivered
  as-is and then the agent stopped) is now nudged once with a
  system-reminder to recover via `jyc_reply_message`; if the model still
  exits text-only, the text is delivered via the fallback path with a
  visible English warning marker appended so a degraded delivery is never
  mistaken for a normal reply. The nudge also surfaces as a Warning entry
  ("REPLY TOOL MISSING") in the activity pane. Behavior is unchanged when
  the reply tool is absent. (#625)

- Fix dashboard topic list showing "build" mode and `/model` writing the
  wrong mode-specific override file when a topic's mode comes from
  pattern/agent config instead of a mode-override file (#615)

- **GitHub close events now close piped agent topics after a restart.**
  The close handler resolved topic names from an in-memory map populated
  only by inbound traffic in the current process, so an issue/PR routed
  before a jyc restart closed nothing — silently, since the "closing
  topics" log is emitted before the handler runs. Topic names are now
  re-rendered from each enabled pattern's `pipe.topic` template (or the
  legacy `pipe.pattern` fallback) for the closed number (unioned with the
  in-memory map), which is restart-proof.
  Static templates are excluded — a shared topic must survive
  any single item closing — and `${msg.pr_number}` /
  `${msg.issue_number}` stay type-gated, so an issue close never resolves
  a PR topic. Only templates over number/repo/type placeholders are
  re-derivable; topics built from other metadata (`${msg.github_type}`,
  `${msg.channel_uid}`, …) are closed solely via the in-memory union. A
  close event that resolves no topics now logs at info instead of
  returning silently. (#611)

- **Agents-only configs no longer fail startup validation.** A config
  with `[agents.<name>]` entries but no `[channels.*]` block was
  rejected with "at least one channel must be configured", even though
  the `agents` websocket channel is synthesized at startup
  (`install_agents_channel`) — which runs *after* validation. Validation
  now requires at least one channel **or** agent. (#610)

- **`${msg.issue_number}` / `${msg.pr_number}` pipe topic placeholders
  now actually resolve.** The GitHub adapter stores these (and
  `github_number`) as JSON integers — the matcher consumes them via
  `as_u64()` — but the `${msg.<key>}` placeholder lookup only accepted
  JSON strings, so resolution returned `None` and every GitHub
  issue/PR trigger was dropped with "unresolvable target, dropping".
  Numeric metadata now stringifies during lookup; a pattern configured
  with `topic = "plan-${msg.issue_number}"` routes to `plan-<N>` as
  documented. (#606)

- **Pre-loop reset after switching to a smaller-window model now actually
  uses the compacted context.** The per-message flow loaded
  `agent-context.json` into memory *before* `maybe_reset_for_new_context`
  ran, so when the pre-check fired (old session over the new model's
  limit), the on-disk file was compacted but the agent loop still received
  the stale oversized context — re-inflating the wire context to ~80% on
  the very first call of the next round. `load_context` now runs after the
  pre-check (with `ensure_session_file` in between, since the reset
  deletes the session file and `load_context` returns empty without it).
  (#603)

- **Pipe-routed agent topics no longer collapse into one shared
  directory.** Every synthesized `[agents.<name>]` pattern pinned
  `topic_path` to `<data_home>/agents/<agent_name>/`, and a pinned
  `topic_path` is a *fixed* directory rather than a subtree root — so all
  topics of an agent resolved to the same path and shared one chat
  history, one session state, and one repository checkout. With
  `pipe = { agent = "jyc_git", topic = "plan-${msg.issue_number}" }`,
  issue #197 and #198 landed in the same directory and each saw the
  other's conversation. Topic directories now follow two rules: a
  configured `topic_path` pins only the agent's **1:1 topic** (the topic
  whose name equals the agent name — its own home, usually a code
  checkout), and every other agent topic gets
  `<data_home>/agents/<agent_name>/<topic_name>/`. Dynamic pipe topics
  therefore never land inside a pinned `topic_path`, and two topics of
  one agent never share a directory. This also fixes the same collapse
  for email/feishu/wecom_bot topics piped with `pipe.agent`. Startup
  restore now scans an agent's default root as well, so nested topics
  survive a restart even when the agent configures no `topic_path`.
  Topics created before this fix keep whichever directory is recorded in
  their `.jyc/topic-name` marker; new topics use the layout above.
  Narrowing note: a `topic_path` on a *regular* channel pattern
  (uncommon) likewise now pins only the topic named after that pattern
  instead of every topic it matches.

- **A `pipe` naming no destination is now rejected at load time.**
  `pipe = { topic = "x" }` without `agent`/`channel` was accepted by
  validation and then unwrapped `pipe.channel` per message at runtime —
  reachable via the email adapter, which fills in the subject-derived
  topic. Validation now requires `pipe.agent` or `pipe.channel` whenever
  `pipe` is present (replacing the narrower "required when pattern/topic
  is set" check).

- **Sliding-window strategy crashed Anthropic calls with empty text
  blocks.** When `context_strategy.mode = "sliding_window"` was paired
  with an Anthropic provider, `extract_user_assistant_pairs` read
  assistant text via `content.as_str()` (which returns `None` for the
  Anthropic array-of-blocks shape), so all assistant replies were
  silently dropped from the windowed pairs. The remaining fallback then
  emitted a user message whose `content` extracted to `""`, which the
  Anthropic provider rendered as `{"type":"text","text":""}` —
  rejected by the API with 400 `cache_control cannot be set for empty
  text blocks` once `apply_cache_breakpoints` marked it on
  `messages[n-3]`/`[n-2]`. Extraction now uses a shared
  `extract_message_text` helper that handles both string and
  array-of-blocks content, and `format_cleaned_message` skips any
  message with empty extracted text so empty blocks never reach the
  wire. Anthropic `tool_result` user-role wrappers are also excluded
  from windowed pairing. Heuristic compaction (mid-loop compression +
  reset) shared the same code path and was equally broken for
  Anthropic contexts — fixed by the same change.

- **Feishu pipe attachment relay failed with 401 after every restart.** The
  inspect auth token was regenerated and written to `auth.token` *after* the
  channel spawn loop, but the feishu pipe reply forwarder reads that file at
  task startup — leaving its inspect client holding a stale (or no) token for
  the process lifetime. The token is now generated and persisted before any
  channel is spawned. Relay-failure logs also print the full error chain
  (`{:#}`) so the HTTP status is no longer swallowed by the outer context.
  (#566)

- **Reply attachments dropped when the pending-delivery watcher won the race.**
  The background watcher delivered `reply.md` with attachments hardcoded to
  `None` and deleted the signal files, so replies with attachments lost their
  files whenever it beat the post-run delivery path. It now reads the
  attachment list from `reply-sent.flag` like the main path. (#566)

- **Feishu chat-message files now download correctly.** `FeishuClient::download_file`
  used the standalone `/im/v1/files/:file_key` endpoint, which only serves
  files uploaded by the app itself — files received in chat messages failed
  with 234008 "The app is not the resource sender" and were silently dropped
  (only a `[File: ...]` placeholder reached the topic). It now uses the
  message resource endpoint `/im/v1/messages/:message_id/resources/:file_key?type=file`,
  mirroring `download_image`.

- **Chat pane renders piped-channel messages on the human side.** The chat
  render treated only `sender == "user"` as the human side; a message piped
  from another channel (e.g. feishu via `pipe`) carries the remote user's
  display name and was mislabeled "AI:". Anything that is not the agent's
  reply (`sender != "ai"`) now renders on the human side.

- **`pipe = { agent, topic }` with dynamic topic lost the agent identity.**
  The agent form only set `channel`/`topic` without recording the agent
  name, so when `pipe.topic` used `${msg.<key>}` placeholders the
  WebsocketMatcher treated the resolved topic as an ad-hoc pattern name
  and the agent's `mcps` / `skills` / `template` / `model` never applied
  (the LLM saw only builtin tools and the global model). The agent name
  is now written as the `pipe_pattern` hint, so the matcher selects the
  right `[agents.<name>]` pattern by name regardless of the (dynamic)
  topic. (#589)

- **WeCom Bot `enter_chat` event parsing failed with `missing field chatid`.**
  The real WeCom `aibot_event_callback` body for events like
  `enter_chat` (captured from a live single-chat event) does not include
  a top-level `chatid` — the conversation identity is `chattype` +
  `from.userid`. `BotEvent.chatid` was declared as a required `String`,
  so the parser returned `missing field "chatid"` and the event was
  dropped with a non-fatal WARN. `chatid` now defaults to empty so
  `enter_chat` and other events without a top-level `chatid` parse
  cleanly. (#588)

### Removed

- **Dead code left over from the pipe-only migrations.**
  `WecomBotOutboundAdapter` (struct, its `OutboundAdapter` impl and its
  tests — the pipe adapter drives the `outbound.rs` wire-format free
  functions directly, so nothing constructed it), the
  `OutboundAdapter::{send,update,clear}_processing_indicator` trait
  methods (the adapter above was their only implementor; the worker-side
  calls went away with the wecom_bot migration), and the unused
  `mail-parser` dependency of `jyc-channels` (email parsing lives in
  `jyc-services`). Behavior unchanged — none of it was reachable. (#599)

- **`repo_group` shared-repo feature.** The `ChannelPattern.repo_group`
  field, the router's `repo_group_key` metadata injection, the symlink
  creation and 120s initialization lock in the topic worker, and the
  orphaned-shared-repo cleanup. Repo setup is now the agent's
  responsibility (init skill). **BREAKING: configs with `repo_group`
  fail to load (unknown field).**

- **GitHub direct-mode code.** `GithubOutboundAdapter` (whole file —
  comments now flow via the hub broadcast + the pipe reply forwarder) and
  the `"github"` arms in `build_outbound_adapter` /
  `InboundSpawner::spawn`. Per-pattern `template` injection for GitHub is
  gone too: topic initialization (cloning the repository into the topic
  directory) is now an agent-side skill. **BREAKING: `template` on a GitHub
  pattern is ignored.** The workspace-scanning close path
  (`scan_topics_for_number` plus the two `on_topic_close` blocks in the
  GitHub poller) went with it: the pipe adapter owns no workspace to scan
  and closes topics by number instead.

- **Email direct-mode code.** `EmailOutboundAdapter` (whole file — replies
  now flow via the hub broadcast + SMTP pipe forwarder), the dead
  `EmailInboundAdapter` and its duplicate `parse_raw_email` in
  `jyc-channels/src/email/inbound.rs` (the live parser is
  `jyc-services/src/imap/parse_email.rs`), and
  `email_parser::build_full_reply_text` (`build_footer` stays for the other
  channels).

- **Feishu direct-mode code.** `FeishuOutboundAdapter` (direct-mode reply
  delivery — replies now flow via the hub broadcast + pipe forwarder), the
  feishu `formatter` and `validator` modules (never wired into any code
  path), the adapter's own attachment-saving method (the hub topic's worker
  saves piped attachments), feishu-side topic-close handling, and the
  non-pipe routing fallback.

- **`skills/github-reviewer`.** The reviewer skill no longer ships — PR
  review is handled by the `github-planner` deep-review flow; `pr-review`
  remains available as a general skill for any topic that wants it. The
  channel doc and config examples no longer reference a reviewer agent.

## [0.3.15] - 2026-08-14

### Added

- **Leader `/` opens the `/` command popup.** New chat-screen leader
  action `Ctrl+P /` opens the `/` command popup from any focus (typing
  `/` requires an empty input and input focus). Chat scope only — the
  dashboard leader does not offer it. (#532)

- **Leader `c` focuses the chat message area.** New chat-screen leader
  action `Ctrl+P c` moves focus to the message area for keyboard
  scrolling (`j`/`k`/arrows). Leader-key dispatch is now scope-aware,
  so `c` can mean `open chat` on the dashboard and `focus chat` in the
  chat screen. (#527)

- **Independent chat-pane visibility toggles.** New leader-key actions
  in the dashboard chat screen: `Ctrl+P s` toggles the bottom status
  bar, `Ctrl+P i` toggles the topic info pane. Zen mode (`Ctrl+P z`)
  now snapshots all aux panes (activity, topic info, status bar,
  explorer) on entry and restores the exact pre-zen state on exit,
  instead of only restoring the info pane.

- **`/exchange` command.** Shows the shareable URLs of files already
  published in the current topic, one plain-text `filename: url` line per
  file (no header, no markdown, so links copy-paste cleanly). `/exchange
  <filename>` narrows the output to a single file. The token is read, never
  created, so listing a topic that published nothing cannot grant access.
  Links use the topic's registered name, which differs from its directory
  basename for shared-repo and custom-`topic_path` topics. (#520)

### Changed

- **/` +exchange` output is now a markdown bullet list.** Instead of `name: url`, multiple published files are rendered as a markdown list with the filename on one line and the raw shareable URL on the next, keeping URLs copy-pasteable while displaying better in chat and email.

- **Build: release profile + dependency slimming.** New `[profile.release]`
  (`opt-level = "s"`, `lto = "thin"`, `strip = "symbols"`; panic stays
  unwind for the dashboard's `catch_unwind`) shrinks the release binary
  (~47 MB → ~15 MB expected). `tokio-tungstenite` is pinned back to 0.29
  to match axum 0.8 and openlark 0.20, collapsing the duplicate
  `tokio-tungstenite`/`tungstenite` versions to one (`Utf8Bytes` API is
  identical, so no code changes). `comrak` is built with
  `default-features = false`, dropping its bundled CLI and
  `syntect-onig` dependency chain.

- **reqwest 0.13 + in-process SSE client.** `reqwest-eventsource`
  (which pinned reqwest 0.12) is replaced by a ~150-line SSE parser over
  `reqwest::bytes_stream()` in `jyc-agent`, deduplicating the tree to a
  single reqwest 0.13 (openlark already used 0.13). Non-2xx provider
  errors now embed status/`Retry-After`/body directly in the error, so
  the per-provider diagnostic re-POST plumbing (~300 lines) was deleted;
  retry classification is unchanged.

- **Second round of dependency upgrades.** tokio-tungstenite 0.30
  (`Message::Text` now carries `Utf8Bytes`), rmcp 3.1.2 (`Content` →
  `ContentBlock` model rename; protocol stays wire-compatible with
  older rmcp peers via version negotiation), openlark 0.20 (Feishu SDK;
  `open_lark::Config` path and infallible config build).

- **Dependency upgrades.** Lockfile refreshed to latest compatible
  versions (tokio 1.53, rmcp 1.8, openlark 0.15.0) and major bumps
  applied: axum 0.8 (route syntax `:param` → `{param}`, ws `Message`
  now uses `Utf8Bytes`), comrak 0.54, mail-parser 0.11, aes 0.9 +
  cbc 0.2 (cipher 0.5), base64 0.23, getrandom 0.4, toml 1, toml_edit
  0.25. Unused comrak deps dropped from jyc-channels/jyc-core. (#544)

- **Chat code-block syntax theme: Base16OceanDark → Base16MochaDark.**
  Fenced code blocks in chat messages now highlight with the warm,
  higher-contrast Base16 Mocha palette. Foreground colors only — the
  terminal background is kept, so the theme fits any dark terminal.
  (#534)

- **Chat input editor replaced: edtui → ratatui-textarea.** The chat
  input is now a plain multi-line editor (soft word wrapping, undo/redo
  `Ctrl+U`/`Ctrl+R`, readline-style keys) instead of a vi-style modal
  editor. Key behavior is unchanged from the former Insert mode: `Enter`
  sends, `Shift+Enter`/`Alt+Enter` inserts a newline, `Up`/`Down` recall
  history when the input is empty, `/` opens the command popup only as
  the first character. The status bar vim-mode chip is removed; the
  prompt arrow is always `❯`. (#530)

- **TUI stack upgraded to the ratatui 0.30 ecosystem.** `ratatui`
  0.29 → 0.30, `crossterm` 0.28 → 0.29, and `edtui` 0.9.9 → 0.11.6
  (adds find/till motions, dot-repeat, and paste-before `P` to the chat
  input). The chat message markdown renderer was replaced with
  [`tui-markdown`](https://github.com/joshka/tui-markdown); rendered
  messages are now word-wrapped to the pane width by our own
  `wrap_styled_lines` helper instead of inside the renderer. (#384)

- **Keypress refocus works from every chat pane, consuming the key.**
  Pressing a key while a chat pane (message area, topic info,
  activity, explorer) is focused returns focus to the input; the key is
  consumed, so no stray characters (e.g. `i` in Insert mode) land in
  the input field. Pane-local keys (`j`/`k`/`g`/`G`/arrows/`Enter`)
  are unchanged. Esc still does not leave the info/activity panes (use
  Tab or the leader). (#527, #528)

- **Per-topic exchange file publishing.** A new built-in agent tool
  `jyc_publish_file` copies (or moves, with `move: true`) a topic-local
  file into `<topic>/.jyc/exchange/` and returns a shareable URL served by
  the inspect server at `GET /exchange/<channel>/<topic>/<name>?token=...`.
  Links are guarded by a per-topic 256-bit token
  (`<topic>/.jyc/exchange-token`) created on first publish — the `/exchange/*`
  route is deliberately not gated by the dashboard bearer middleware so
  links work for end users. `/reset` and `/new` remove the published files and the
  token, invalidating previously shared links. The link base URL is
  configurable via the new `[inspect] base_url` setting
  (fallback: `http://<inspect.bind>`). (#519)

- **Show the selected topic's git branch in the TUI.** The dashboard
  topic info pane, chat topic info pane, and chat input header line
  (`╭─ build · local_dev · pattern`) now include the current branch of
  the topic's working directory when it is a git repo. The branch is
  resolved by the inspect server by reading `<topic_path>/.git/HEAD`
  (or `<topic_path>/repo/.git/HEAD` for the shared-repo layout) and
  included on `TopicSummary.branch` and `TopicInfo.branch`. Topics
  whose `topic_path` is not a git repo (most chat-channel topics)
  simply omit the branch segment. (#512)

- **Show files changed on the selected branch in the chat info pane.**
  When the selected topic's working directory is a git repo, the chat
  topic info pane now renders a separated `Files (N):` section at the
  end (after `Cost:` and any transient `⏳ AI thinking...` line). The
  section lists every changed file one per line; when the list is
  taller than the pane, the pane scrolls (Tab cycles focus to it,
  then `j`/`k`/`↑`/`↓`/`PgUp`/`PgDn`/`g`/`G` move the viewport, the
  same keys as the activity pane). Each row leads with a one-column
  prefix glyph conveying the git change kind: `+` for `Added`,
  `-` for `Deleted`, two spaces for `Modified` (kept so the path
  column aligns across rows). Files currently dirty in the working
  tree (modified or staged but not committed) are rendered in
  **yellow** — orthogonal to the kind, so e.g. an added-then-edited
  file shows as `+ path (yellow)`. Backed by
  `TopicSummary.changed_files` and `TopicInfo.changed_files` — now
  `Vec<{path, uncommitted: bool, change: ChangeKind}>` resolved
  server-side from two `git diff` invocations (`--name-status
  main...HEAD` ∪ `--name-only HEAD`), unioned and sorted
  alphabetically by path. The branch-side status letter (`A` /`,
  `D`, etc.) populates `change`; `uncommitted: true` wins when a
  path appears in both lists. Renames / copies / type changes from
  `git diff --name-status` are normalized to `Modified` server-side.
  Same skip rule as `branch`: non-git paths or both diffs failing
  yields `None` and the entire section is omitted. (#220)

### Changed

- **/` +exchange` output is now a markdown bullet list.** Instead of `name: url`, multiple published files are rendered as a markdown list with the filename on one line and the raw shareable URL on the next, keeping URLs copy-pasteable while displaying better in chat and email.

- **Chat screen shows topic info + status bar by default.** The
  dashboard chat screen no longer starts in zen mode: the topic info
  pane and status bar are visible on entry. Zen mode is now opt-in via
  `Ctrl+P z`.

- **Branch resolution moved server-side.** The CLI no longer reads
  `.git/HEAD` directly — the inspect server resolves it on every
  `list_topics` call and ships it on the wire. This enables the
  dashboard to connect to a remote inspect server and still display
  the branch. Old clients/servers (pre-this-field) continue to work:
  `branch` is `#[serde(default)]` so absent values become `None` and
  the segment is simply omitted. (#512)

### Removed

- **Vim modal editing in the chat input.** Insert/Normal/Visual modes,
  motions, text objects, and the `Esc`-to-Normal flow went away with the
  edtui → ratatui-textarea replacement. Message scrolling lives in the
  pane-focus model (`Tab` / leader `c`, any key refocuses the input).
  (#530)

- **`Space` as an alternative leader key.** The leader popup is now
  opened with `Ctrl+P` only, on both the dashboard and chat screens.
  (#530)

### Fixed

- **`jyc_send_to_topic` erased the target topic's pattern identity.**
  Injected messages carried an empty `pattern_name`, and the topic worker
  wrote it to `.jyc/pattern` unconditionally — the dashboard chat header
  lost the pattern segment and Topic Info showed `Pattern: -` (and
  pattern-level model overrides were skipped) until a manual message
  re-matched the pattern. The worker now only writes `.jyc/pattern` for
  non-empty pattern names, and `jyc_send_to_topic` resolves the pattern
  named after the target topic so injected messages carry the real
  `pattern_name`, template/role metadata, attachment config and
  `live_injection` flag, and the pattern's custom `topic_path` — newly
  auto-created topics now land in the configured directory instead of
  the default workspace. (#542)

- **Periodic input freeze from the inline overview poll.** The dashboard
  input loop awaited the 500ms overview REST poll inline, freezing
  keystroke handling and redraw for one HTTP round-trip twice per
  second — keys typed during the stall echoed late. The fetch now runs
  in a spawned task and its result is handled via a channel (at most
  one poll in flight, so ordering is preserved). (#540)

- **Chat input typing lag.** Every frame (each keystroke, 50ms poll,
  1Hz live tick) re-parsed the entire transcript's markdown — O(history)
  per keystroke. Rendered history lines are now cached in `ChatState`
  and rebuilt only when the messages or pane width change (fingerprint:
  message count, summed text lengths, last timestamp, width); the
  dynamic progress tail stays per-frame, and each frame clones only the
  visible window of cached lines (≤ one screenful). (#537)

- **Chat message pane scroll reversal lag.** Two compounding causes:
  the scroll offset grew past the rendered maximum (the overshoot had
  to be scrolled back off before the view visibly moved), and the event
  loop read one input event per frame, so wheel bursts queued up and
  kept replaying after reversing direction. The offset is now clamped
  at the source and all pending input events are drained once per frame.
  (#535)

- **Mouse escape garbage (`[<65;35;12M`) inserted into the chat input on
  fast wheel scrolling.** crossterm 0.29 treats input as complete unless
  a read fills its whole buffer, so a wheel burst that splits a mouse
  sequence right after ESC leaks the remainder as plain character keys.
  A lone `Esc` is now held for 20ms: a following `[` starts fragment
  swallowing up to the `M`/`m` terminator; otherwise the `Esc` is
  replayed as a real keypress. The terminal is also restored on panic
  (raw mode, mouse capture, alternate screen) so a crash no longer
  sprays escape sequences into the shell. (#535)

- **Multi-line chat messages rendered as one line in the message area.**
  Line breaks typed into the chat input were sent to the agent intact,
  but the local echo collapsed them: tui-markdown parses with hardcoded
  options (no `ENABLE_HARDBREAKS`) and renders markdown soft breaks as
  a space. Chat rendering now rewrites soft breaks to hard breaks
  (`"  \n"`) outside fenced code blocks before rendering, for both user
  and AI messages. (#534)

- **`/cancel` left the dashboard stuck at "AI thinking..." forever.** A
  cancel that landed while an LLM call was in flight returned an error out
  of the agent loop, skipping the post-loop `ProcessingCompleted` event —
  the only signal the inspect server uses to clear its per-topic
  `is_processing` flag. The topic kept reporting `Processing`, the chat
  progress line kept ticking, and the last activity entry read
  `ERROR: cancelled during LLM call`. A cancel during an LLM call is now a
  normal loop exit (not an error), and the worker publishes
  `ProcessingCompleted { success: false }` after *any* processing error, so
  no failure path can leave the state stuck. (#523)

- **Auto-retarget workflow never retargeted anything.** The job introduced
  in #518 has no `actions/checkout` step, so `gh` had no git remote to infer
  the repository from and every call failed with `fatal: not a git
  repository` — stacked PRs kept pointing at their merged base branch.
  Fixed by setting `GH_REPO` (cheaper than a checkout; the job needs no
  source code). The failure was silent because `for pr in $(gh ...)`
  discards the command's exit status, so `set -e` never fired and the job
  reported success; the PR list is now assigned to a variable first, so any
  future breakage fails the job instead of hiding. (#522)

- **Published links pointed at a wildcard host.** With
  `[inspect] bind = "0.0.0.0:9876"` and no `base_url`, the link
  base fell back to `http://0.0.0.0:9876` — a bind wildcard, never a
  reachable destination, so every published link was dead off-machine. The
  wildcard host is now replaced by this host's primary LAN IP (port
  preserved) and a warning names `base_url` as the real fix.
  `base_url` is also validated at startup: it must carry an
  `http://` or `https://` scheme, since a scheme-less value is read by
  browsers as a relative path and breaks silently. (#520)

- **`docs/api.md` out of sync with the implementation.** The API
  reference predated several recent additions and contained one
  incorrect statement. Updated to match current code:
  - §1.2 / intro: `auth_token` is auto-generated and persisted to
    `<workdir>/auth.token` (retrieved via `jyc token show`); it is
    not a user-configurable `[inspect]` field.
  - §2.4.7: documented the missing 422 `failed to load config` error
    raised when the layered config fails to load.
  - §3.3: split the `message` row to show the asymmetry — the
    dashboard-side `TopicProxyHandler` ignores a payload `topic`
    field (URL is the only source); the WS channel adapter accepts
    it and lets it override the URL.
  - §3.4.1: corrected the `is_internal` filtering claim — internal
    entries are filtered from **both** the REST activity endpoint
    and the WebSocket `activity` event, not just REST.
  - §3.4: added the missing `loop_tick` event (1 Hz wall-clock tick
    for the dashboard's live-duration ticker).
  - §4.3: `TopicInfo` / `TopicSummary` table now lists the actual
    fields (`context_input_tokens`, `total_input_tokens`,
    `total_cache_hit_tokens`, `total_cache_creation_tokens`,
    `branch`, `changed_files`, `cost`) instead of the stale subset.
  - §4.4: added `TopicCost`.

- **Chat input header regains model and context-window percentage.**
  Removing the `jyc ai v{}` chip in #512 also dropped the model name
  and pct% that lived alongside it. Both are restored as a right-side
  `[ {model} · {pct}% ]` chip on the input header line. The version
  remains in the status bar (per the original decision).

- **Anthropic cache-creation (write) pricing.** Anthropic splits
  prompt-cache tokens into two buckets that bill at different rates:
  `cache_read_input_tokens` (cheap reads) and
  `cache_creation_input_tokens` (writes at ~1.25× the input rate).
  Previously jyc collapsed both into a single `cache_hit_tokens` field
  and billed them at `cache_hit_per_million`, undercharging cache
  writes for any Anthropic user. Now:

  - New optional field `cache_creation_per_million` on `ModelPricing`
    (provider- and model-level). Omitting it preserves the legacy
    single-rate billing — `compute_cost_split` falls back to
    `cache_hit_per_million` for writes when the field is absent.
  - `compute_cost_split(input, output, cache_read, cache_creation)`
    is the new canonical cost function. Reads bill at
    `cache_hit_per_million`, writes bill at
    `cache_creation_per_million` (or the read rate as fallback).
    `compute_cost(...)` is now a thin wrapper that forwards `0` for
    the creation bucket.
  - `BillingEntry` gains `cache_creation_tokens: u64`
    (`#[serde(default)]` so existing ledger files still load).
  - `SessionState.total_cache_hit_tokens` semantics changed **for
    Anthropic only**: it now reports cache-**read** tokens only
    (writes accumulate in the new `total_cache_creation_tokens`).
    For every other provider it's still the single reported cache
    bucket. The dashboard "Cache hits" row therefore shows reads
    only on Anthropic sessions and the new "Cache create" row shows
    writes; non-Anthropic sessions show a single "Cache hits" row
    as before.
  - `TopicSummary` / `TopicInfo` / the inspect protocol gain
    `total_cache_creation_tokens: Option<u64>`, surfaced in the chat
    info pane and dashboard topic info area as a new
    "Cache create: N" row that only renders when the running total
    is non-zero (= only for Anthropic).
  - Per-provider wiring: the Anthropic provider emits
    `cache_read_tokens` and `cache_creation_tokens` separately from
    its SSE `usage` payload; every other provider (OpenAI / DeepSeek /
    Kimi / 火山引擎 / MiniMax) keeps filling `cache_creation_tokens = 0`.

  Backwards-compat: old configs, old `agent-session.json` files, and
  old `bill-YYYY-MM-DD.jsonl` ledger entries all load unchanged via
  `#[serde(default)]`. Cost math is unchanged for Anthropic users who
  don't set `cache_creation_per_million` (writes fall back to
  `cache_hit_per_million`); only the dashboard `Cache hits` count
  changes for Anthropic sessions — by design, so writes no longer
  inflate the read-bucket display.

- **OAuth2 client_credentials for remote MCP.** Remote MCP servers in
  `[[mcps]]` (global, workdir, or topic overlay) now accept an optional
  `oauth = { client_id, client_secret, token_endpoint, scopes? }` block.
  When set, the agent POSTs `grant_type=client_credentials` to
  `token_endpoint` at MCP connect time and uses the returned
  `access_token` as the Bearer header. Mutually exclusive with the
  existing static `auth_header` (validation rejects both being set on
  the same block). Token is fetched once per connect — restart on
  expiry to pick up a rotated token.

  Multi-level inheritance note: `oauth` participates in the standard
  L1/L2/L3 MCP overlay merge on `name`. If a deeper layer redefines
  the same MCP name without re-listing the `oauth` block, the parent's
  OAuth config is replaced (same behavior as `auth_header`).

### Changed

- **/` +exchange` output is now a markdown bullet list.** Instead of `name: url`, multiple published files are rendered as a markdown list with the filename on one line and the raw shareable URL on the next, keeping URLs copy-pasteable while displaying better in chat and email.

- **WeCom Bot ping ack log level.** Heartbeat ping acks (every 30s) now log
  at `debug` instead of `info`, keeping the info-level log free of heartbeat
  noise. Subscribe and other operation success acks remain at `info`. (#510)

- **Provider `api_key` field.** LLM providers now accept `api_key =
  "${ENV_VAR}"` for credentials, matching the `${VAR}` syntax used for every
  other secret field in the config (`token`, `password`, `app_secret`,
  `corp_secret`, `bot_secret`, `encoding_aes_key`):
  ```toml
  [agent.providers.anthropic]
  type = "anthropic"
  base_url = "https://api.anthropic.com/v1"
  api_key = "${ANTHROPIC_API_KEY}"   # preferred
  ```
  The legacy `api_key_env = "ENV_VAR"` field is retained for backward
  compatibility. When both fields are set, `api_key_env` wins (legacy
  precedence) and a warning is logged at startup so the user can clean up.

### Fixed

- **Chat pane for non-WebSocket topics no longer drops typed messages.**
  Opening a github/email/etc. topic in the dashboard chat pane used the
  legacy detail mode, which never opened a WebSocket connection — typed
  input (including `/reset` and other slash commands) was echoed locally
  and silently dropped, never reaching the server. All topics now open
  over the unified `/ws/<channel>/<topic>` endpoint, and the dead
  detail-mode code is removed.

- **WeCom progress updater no longer leaks on agent errors.** The agent
  wait loop returned early via `?` when the agent call failed (API error,
  429 retry exhaustion, `/cancel`), skipping the cleanup that stops the
  background progress updater. The leaked task kept sending stream updates
  every 3s for a long-expired req_id forever (until process restart),
  producing a constant `reply ack error errcode=846604` WARN storm even
  with no active session — one more leaked task per failed run. Errors are
  now propagated only after both background tasks are stopped, and messages
  buffered during a failed call are re-enqueued instead of dropped. (#509)

- **Pending-delivery watcher now fans out dashboard events.** When the
  background watcher (used by MCP reply/question tools during the SSE
  stream) won the race against the post-SSE delivery path, it delivered the
  reply to the channel but never published a `ReplySent` event. The dashboard
  chat pane therefore showed "processing completed" in the activity pane while
  the actual reply only appeared after re-entering the chat (when it was read
  from chat history). The watcher now publishes `ReplySent` so live chat
  messages are visible immediately. (#508)

- **Monotonic activity ids across monitor restarts.** `ActivityEntry`
  ids were assigned **after** appending to `activity.jsonl`, so the persisted
  log always contained id 0 and the dashboard's `last_seen_id` dedup was
  effectively disabled. Worse, after a monitor restart `next_id` began again at
  0/1, so any dashboard client that had not re-hydrated dropped all live events
  as "duplicates". Ids are now assigned **before** disk persistence and the
  ActivityTracker seeds `next_id` from the persisted log on first use, keeping
  live events visible after a restart. (#508)

- **Silent broadcast lag in WebSocket handlers is now logged.** When a
  dashboard client could not keep up with the broadcast bus, per-channel
  and inspect-broadcast events were dropped silently (debug-only). Both
  paths now log a warning so dropped live messages can be diagnosed. (#508)

- **Topic-level `${VAR}` expansion.** `<topic>/.jyc/config.toml` now
  expands `${ENV_VAR}` references in `[agent]` model overrides and
  `[[mcps]]` fields, matching the behavior of L1 (global) and L2 (workdir)
  configs. Previously, the topic-level loader bypassed `expand_env_vars`
  and stored `${VAR}` as a literal string in the deserialized
  `TopicConfig`, causing confusing runtime errors when env-driven model
  or MCP overrides failed to resolve. The shared `parse_and_deserialize`
  helper now backs all three config loaders (L1, L2, L3), eliminating
  duplication and closing the topic-level gap.

### Added

- **Live processing-duration ticker.** While the agent loop is running, the
  dashboard now shows a wall-clock elapsed-time indicator that ticks every
  ~1 s (with the very first tick fired immediately at t=0), so the loop's
  progress is visible even during silent LLM or tool work (long bash,
  slow LLM stream, retry backoff) when no iteration has produced a
  `ProcessingProgress` event yet. The ticker appears in three places:

  - The dashboard's per-topic Details panel (Status chip in
    `crates/jyc-cli/src/cli/dashboard/mod.rs`).
  - The chat-mode info pane (`⏳ AI thinking...` line).
  - The chat progress line (in-flight activity entry / "⏳ AI is
    thinking..." placeholder), now rendered as a dual-time display:
    `<since-current-activity> / <total-loop-elapsed>` (e.g. `5s / 12.4s`).
    The left number is from the polled activity timestamp (coarse, freezes
    during silent work); the right is the live ticker (1 Hz, fresh). When
    they diverge, the loop is in a long silent stretch.

  Implementation: new `TopicEvent::LoopTick { elapsed_ms }` variant emitted
  by a background `tokio` task spawned at loop start; routed by the inspect
  server as `is_internal` (no activity.jsonl pollution) and broadcast over
  WebSocket as `{"type":"loop_tick",...}`; consumed by the dashboard into
  the `ChatState::live_tick_ms` map. Format: `<s>.<tenths>s` below 60s
  (`12.4s`), `<m>m<ss>s` at/above (`1m05s`). The ticker task is bound to
  the loop's lifetime via a `TickerGuard` RAII handle so it terminates on
  every exit path (success, error, cancel, no-reply guard) — without this,
  the task would leak at 1 Hz until shutdown on natural completion.

- **System temp dir always within the tool boundary.** `std::env::temp_dir()` is
  now accepted by both the read and the write path check, so tools have scratch
  space without per-pattern `access` configuration. Previously every pattern had
  to repeat the same `access.write` entry, and topics with no matched pattern
  could not be granted access at all.

  Note the system temp dir is shared and world-writable, so other processes' temp
  files become readable. Use `access.read` / `access.write` for paths that need to
  stay private. A `$TMPDIR` of `/` is ignored, since honoring it would disable the
  boundary entirely. (#499)

- **Anthropic prompt caching.** Requests to `anthropic`-type providers now
  carry the four `cache_control` breakpoints Anthropic allows per request,
  laid out on the last element of each static span: the tools tail, the
  system prompt tail, and the two messages before the newest one (a rolling
  window over conversation history). The newest message is deliberately left
  unmarked — it changes every request, so a breakpoint there would be written
  and immediately orphaned.

  `system` is now sent as a single-element content block array rather than a
  bare string, since a `cache_control` marker has to attach to a block.
  Markers land on a message's *last* content block, never on the message
  object (the API rejects the latter).

  Tools and the system prompt keep separate breakpoints rather than sharing
  one: the tools array is identical across every topic, while the system
  prompt varies per topic (working directory, skills, `AGENTS.md`), so a
  tools-only prefix stays reusable between topics.

  Caching is always on and needs no configuration. Prompts below the model's
  minimum cacheable length (1024 tokens for Opus/Sonnet, 2048 for Haiku) are
  ignored by the API rather than erroring.

  A provider whose `params` already supplies its own `cache_control` keeps
  full control: jyc detects the existing markers and adds none of its own,
  since a 5th breakpoint is a hard API error rather than a silently ignored
  one.

- **User-defined slash commands.** `config.toml` accepts `[[commands]]`
  entries, each declaring a `name`, `description`, an optional `mode`
  (`plan`/`build`), an optional `skills` list, and a `user_prompt`.
  Invoking `/<name>` switches the topic mode, names the skills the agent
  should use, and appends `user_prompt` to the message body — so a single
  command can put the topic in plan mode, point the agent at
  `pr-review`, and hand it the review instructions.

  Custom commands appear in `/?` and the dashboard command popup
  alongside the built-ins. Text typed after the command is preserved,
  with `user_prompt` appended last so it is the most recent instruction —
  `/review focus on error handling` and `/review` followed by
  `focus on error handling` on the next line reach the agent identically.

  Names must be lowercase (command lookup is case-insensitive) and must
  not shadow a built-in; both are rejected at config validation, at
  startup and on hot reload.

  Skill *paths* are not duplicated into the command config: the system
  prompt already lists every discovered skill with its path and
  description, so naming a skill is enough for the agent to locate and
  read its `SKILL.md`.

- **Per-model cost tracking.** Models (or their providers) can declare
  `pricing` rates per 1M tokens — `input_per_million`,
  `output_per_million`, `cache_hit_per_million`, and an optional
  `currency` label (default `CNY`; no conversion is ever performed, so a
  USD-billed provider must set `currency = "USD"` explicitly).
  Cost per LLM call is
  `(input - cache_hit) * input_rate + output * output_rate + cache_hit * cache_rate`,
  so prompt-cache hits are billed at their own (usually cheaper) rate
  rather than the full input rate. Model-level `pricing` overrides
  provider-level; with none configured, no cost is tracked and the
  display is hidden entirely.

  Cost is computed **per call** from that call's own usage payload, not
  from session totals. This keeps the spend of a round that is cancelled
  or errors out, and bills each call at its own rate when the model
  changes mid-round.

  Two figures appear in the dashboard and chat **Topic Info** panes as
  `Cost: ¥0.0521 session · ¥1.3057 today`:
  - **session** — accumulated in `session_cost` in
    `.jyc/agent-session.json`; resets with the session (context
    auto-reset, `/reset`, or switching to a smaller-context model).
  - **today** — durable UTC-day total from the new per-topic ledger at
    `.jyc/bill-YYYY-MM-DD.jsonl`, one line per call, never reset,
    rotated, or truncated. Each line stores the token counts alongside
    the cost, so entries stay auditable and a corrected rate can be
    replayed over past usage. Day-stamped files (matching the existing
    `chat_history_YYYY-MM-DD.jsonl` convention) keep the dashboard's
    500 ms poll bounded to a single day of entries rather than
    re-parsing an ever-growing ledger.

  Ancillary LLM calls are billed too: the cycle-boundary progress
  summary and the context-compression call on session reset both
  summarize the whole transcript, so their input is on the order of the
  context window. Ledger entries carry a `kind` field (`"call"` vs
  `"summary"`) so summarization overhead can be separated from
  user-facing spend.

- **Mouse-capture status chip.** A right-aligned chip in the dashboard
  status bar mirrors the vim mode chip format. Peach ` MOUSE+ ` means
  capture is on (wheel scrolls in chat, tmux drag-to-select is
  hijacked); muted overlay0 ` MOUSE- ` means capture is off (tmux
  select works, wheel ignored). Always visible — both dashboard and
  chat screens.

- **Toggle mouse-wheel capture from the command palette.** New `toggle
  mouse` palette entry (Shared scope, reachable from both dashboard
  and chat screens). Default state remains ON (matches PR #484); flip
  it off when working inside tmux and the chip switches to ` MOUSE- `
  immediately. A brief status line confirms the change.

- **Accumulated `total_input_tokens` in the session state.** New field
  in `.jyc/agent-session.json` that records the running sum of every
  LLM call's `input_tokens` (= full context size) across the session's
  lifetime. Since each call re-sends the full conversation context,
  this value also represents the **lifetime input tokens billed by the
  API** for this session (use it for cost tracking). Distinct from
  `context_input_tokens` (which holds the most recent call's input
  size = current context, just renamed in PR #491). The `agent_loop`
  accumulates per-call input tokens and passes the running total
  into `persist_tokens`; on auto-reset the counter zeros out alongside
  `context_input_tokens` and `total_output_tokens`. Visible in the
  topic info pane (chat) and the dashboard topic info area as a
  new `Total input: N` row. (#490)

- **Prompt-cache hit tracking (`total_cache_hit_tokens`).** New
  accumulated field on `SessionState`, `TopicInfo`, and `TopicSummary`
  that sums every LLM call's prompt-cache-hit tokens across the
  session — the portion of input the provider served from its prompt
  cache rather than re-billing as fresh input. Each provider's
  `usage` JSON is parsed for the field its vendor uses — first
  non-zero match wins across the known shapes:
  `prompt_cache_hit_tokens` at root (DeepSeek), `cached_tokens` at
  root (Kimi) or under `prompt_tokens_details` (OpenAI / 火山引擎 /
  MiniMax), or `cache_read_input_tokens + cache_creation_input_tokens`
  at root (Anthropic). New `provider::usage::extract_cache_hit_tokens`
  helper centralizes the lookup. Visible in the chat topic info pane
  and the dashboard topic info area as a new `Cache hits: N` row.
  Not shown in the dashboard overview list (the `Context` column is
  already tight and this is a session-level analytic). Zeros on
  auto-reset alongside the other `total_*` counters.

- **Per-topic MCP overrides.** `<topic>/.jyc/config.toml` now accepts
  an optional `[[mcps]]` block in addition to the existing `[agent]`
  model overrides. Default merge is **additive** — topic MCPs are
  unioned with the pattern → channel → global MCPs and a topic MCP with
  the same `name` wins. Set `mcps_replace = true` to fully replace the
  inherited set (mirrors how `ChannelPattern.mcps` already overrides
  channel-level MCPs). Useful for one-off MCPs (local-only servers,
  per-topic remote endpoints) without polluting the global config.
  Implementation: `jyc_types::apply_topic_mcp_overlay` (pure helper,
  unit-tested) wired into `JycAgentService::build_tool_registry`. The
  `mcps_replace` field is a `bool` rather than an extensible enum to
  keep the schema minimal; if a second merge mode (e.g. prepend) is
  added later, the field will need to be renamed rather than gain a
  new variant.

- **Per-topic MCP load log.** Every `process()` invocation now emits a
  structured `INFO Resolved MCP servers for topic` line with the
  channel/topic/pattern, the resolved name list in `name:layer` form,
  and per-layer counts (`from_global`, `from_channel`, `from_pattern`,
  `from_topic`, `from_topic_replace`). Replaces the previous count-only
  `Loading external MCP tools` debug line so operators can directly
  answer "which MCPs is this topic actually using and where did they
  come from" from a single log line — useful for diagnosing remote
  deployments where the L3 topic-local overlay appears to be ignored.

- **L3 topic-config load heartbeat.** A dedicated `debug!` / `info!`
  line is emitted on every `process()` invocation that resolves the
  `<topic>/.jyc/config.toml` overlay. Three outcomes are
  distinguished: file absent (DEBUG), file parsed but no `[[mcps]]`
  block (DEBUG no-op), and overlay applied (INFO with `configured_mcps`,
  `mcps_replace`, `topic_mcp_names`). Remote deployments can now
  distinguish "no file at all" from "file present but unreadable" from
  "file applied" without instrumenting the agent.

### Fixed

- **Silent `load_topic_config` I/O failures.** A failed read (e.g.
  `EACCES` in remote deployments where the agent user can't read the
  topic-config file) was swallowed by `read_to_string(&path).ok()?`
  and the L3 overlay dropped with no log. The function now emits a
  `WARN Failed to read topic config; ...` log carrying the path and
  underlying error before returning `None`, so the failure mode is
  visible in production logs.

- **Anthropic cost undercounting with prompt caching.** Anthropic's
  `input_tokens` counts only the *uncached* portion of the prompt, with
  `cache_read_input_tokens` and `cache_creation_input_tokens` reported
  separately and additively — the opposite of every other supported vendor,
  where `prompt_tokens` already contains `cached_tokens`. Cost computation
  assumes the latter shape (it derives uncached input as
  `input - cache_hit`), so a cache-heavy call reported less input than cache
  hits, the subtraction clamped to zero, and genuinely uncached tokens were
  billed at nothing. Anthropic usage is now summed back into a total before
  it reaches the cost function. This was latent until prompt caching was
  enabled, since both cache buckets were always zero.

- **`total_output_tokens` no longer double-counts across `agent_loop`
  iterations.** `persist_tokens` previously did `state.total_output_tokens
  += output_tokens` while the caller (`agent_loop`) had already
  accumulated the running sum, so every iteration added the running
  total on top of itself — the on-disk value grew as a triangular sum
  (100 + (100+150) + (100+150+80) = 680 instead of 330 for three calls
  with outputs 100/150/80). Now `persist_tokens` stores `total_output_tokens`
  as passed in (matching the same contract as `total_input_tokens`),
  with the caller doing the accumulation. (#490)

- **`jyc open` no longer times out on a brand-new ad-hoc topic.**
  `set_topic_path` now creates `.jyc/` and `.jyc/topic-name` for the
  registered path. Previously only the bare folder was created, and
  `list_topics` filtered the entry out (the `path.join(".jyc").is_dir()`
  guard dropped it), so `wait_for_topic` polled the inspect overview for
  5 seconds and never saw the new topic — `jyc open` aborted with
  `Timeout waiting for topic <name> to be created`.

- **Selective borders for chat-pane side panels.** The topic info pane
  now draws only its `LEFT` edge (against the chat conversation), the
  topic explorer pane draws only its `RIGHT` edge, and the activity
  pane draws only its `TOP` edge. The outer / screen-edge borders and
  the redundant inner borders are gone, so the borderless chat area
  reads as a single flat surface with three thin separators.

- **No-reply state surfaced in activity pane.** When the agent loop
  exits with no text and no tool call, neither the `reply_message` tool
  path nor the raw-text fallback path would deliver anything to the
  user — `TopicManager` only logged `WARN: No reply text from AI` and
  the activity pane showed `ProcessingCompleted (success=true)` with no
  signal of failure. The activity pane now renders a `NO REPLY`
  warning entry (severity `Warning`) so operators can see the silent
  failure. The agent loop also gives the model a single system-reminder
  nudge: on the first no-reply iteration it appends a user message
  telling the model that its last turn produced no text and no tool
  call and instructing it to call `jyc_reply_message` with the final
  response. If the model still produces no reply after the reminder,
  the loop exits normally — the reminder is one-shot to bound cost.

### Changed

- **/` +exchange` output is now a markdown bullet list.** Instead of `name: url`, multiple published files are rendered as a markdown list with the filename on one line and the raw shareable URL on the next, keeping URLs copy-pasteable while displaying better in chat and email.

- **Tool definitions are sorted by name.** `ToolRegistry::definitions()`
  iterated a `HashMap`, whose order is randomized per process, so the
  serialized `tools` array differed on every restart. Prompt caching matches
  on an exact prefix, so a breakpoint on the last tool could never produce a
  hit. Sorting also helps prefix caching on OpenAI-compatible providers.

- **Dashboard overview list "Tokens" column → "Context".** The column
  now shows only `context_input_tokens / max_input_tokens` (e.g.
  `47K/128K`), dropping the previous `·XK out` suffix. The
  `total_input_tokens` and `output_tokens` fields are unchanged on
  `TopicSummary` and continue to render as separate rows in the chat
  info pane and the dashboard topic info area. (#490)

- **Pane title separators with `──` prefix.** The activity, topic info,
  and topic explorer pane titles now start with `── ` so the title row
  reads as a continuous `─` stripe against the top border. The topic
  info and explorer panes additionally gain a `TOP` border, giving them
  a clear separator between the title and the content below. Visual
  style matches the existing `LINE_DRAWING` palette used elsewhere in
  the chat screen.

- **Leader-key popup replaces the command palette.** `Ctrl+P` (and
  `Space` in Normal mode / on the dashboard) now opens a leader-key
  popup that lists every local command for the current scope with its
  assigned keys. Typing the keys dispatches the action immediately;
  `Esc` closes. Multi-char keys (`gg` for scroll top, `G` for scroll
  bottom) wait for the next key while the buffer is a prefix. The
  previous filter-palette popup (Ctrl+P + type to filter + Enter to
  dispatch) and the `:` shortcut are removed; `Ctrl+Q`, `Ctrl+C`, and
  `Enter` on the dashboard are preserved. Leader keys per scope:
  chat — `d`, `e`, `z`, `a`, `o`, `gg`, `G`, `n`, `r`, `q`, `m`;
  dashboard — `c`, `n`, `r`, `q`, `m`.

- **Activity pane leader key toggles on/off instead of cycling sizes.**
  Pressing `a` in the leader popup (`Ctrl+P` then `a`) now toggles the
  activity pane between hidden and the bottom 20% layout — the size
  most users kept it at in practice. The previous four-state cycle
  (hidden → 20% → 80% → activity-only → hidden) was removed: it took
  three presses to hide the pane again, and the larger sizes (80%,
  activity-only) were rarely useful. The internal `activity_split`
  field still uses the `u8` range so the rendering path is unchanged;
  only the dispatch behaviour is binary. Focus on the activity pane
  falls back to the chat input when the pane is hidden.

- **Renamed `total_input_tokens` to `context_input_tokens` in
  `.jyc/agent-session.json`.** The old name was misleading: despite the
  `total_` prefix, the field stores the input tokens reported by the
  most recent LLM call (i.e. current context size, since each call sends
  the full conversation context), not a sum across calls. Only
  `total_output_tokens` is actually accumulated. The Rust struct
  field in `SessionState` is renamed accordingly; behavior is
  unchanged. On-disk session files written by older versions will see
  the input counter reset to 0 on next load — sessions auto-reset when
  full so this is a one-time cost per existing topic. (#490)


---

Older releases (0.3.13 and earlier): see [CHANGELOG archive](CHANGELOG-archive.md).
