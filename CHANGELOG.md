## [Unreleased]

### Added

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

### Fixed

- **Forced reply tool on recovery turns.** The reply-recovery turn now
  also forces `jyc_reply_message` at the API level (`tool_choice`):
  offering a single tool proved insufficient for weak models, which
  re-emit narration text that then leaked as a fallback reply. Anthropic
  and OpenAI-compatible providers implement the forcing; providers
  without `tool_choice` support degrade to the previous reminder-only
  behavior.
- **Reply-delivery guard enforcement.** The agent loop's reply reminders
  are now backed by a tool-restricted recovery turn: after a reminder,
  the next LLM call offers only `jyc_reply_message`, so the model cannot
  re-emit narration instead of delivering. Failed `jyc_reply_message`
  calls (missing/empty message, bad attachment) now trigger a
  failure-aware reminder quoting the concrete tool error, and reply-tool
  error messages state explicitly that the reply was not delivered (#637)

### Changed

- **Post-pipe-migration cleanup.** The five older pipe adapters
  (email/github/gitee/feishu/wecom_bot) now share the match/retarget/route
  helpers extracted in #635 (`match_pipe`, `retarget_or_drop`,
  `route_into_pipe_target`, `warn_on_bad_pipe_patterns`) instead of inline
  copies. `build_outbound_adapter` was inlined into its single call site
  (the websocket hub setup). config.example.toml's agent-runtime examples
  (channel-level MCPs, tool exclusion) now target the hub channel and note
  that pipe-only channels ignore `mcps`/`disabled_tools`/`skills` (#638)

- **wecom and wecomkf are pipe-only adapters.** Both channel types were
  migrated to the pipe architecture (docs/core-hub-adapters.md): pattern
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

- **New topical doc: `docs/context-management.md`.** Consolidates how
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
- **Core / hub / adapters architecture document.** `docs/core-hub-adapters.md`
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
  pipe-only adapter (see `docs/core-hub-adapters.md`). Every enabled
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
  joins `feishu` as a pipe-only adapter (see `docs/core-hub-adapters.md`).
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

## [0.3.13] - 2026-08-01

### Added

- **Mouse-wheel scroll in the chat message area.** `jyc dashboard` now
  enables crossterm mouse capture and translates `ScrollUp` /
  `ScrollDown` events into chat-pane scroll commands when the cursor
  is over the scrollable message area (above the input editor). The
  input field, activity pane, topic explorer, and info pane silently
  absorb wheel events, so scrolling never steals focus from the
  editor. Hit-testing uses the message-area `Rect` cached during
  `render_chat_conversation` so the boundary stays correct as the
  input grows and as the activity / explorer panes toggle.

### Fixed

- **Token dashboard is visible immediately on new topics.** A brand-new
  topic — or one whose session was just deleted by `/reset` or `/new`
  — now has `.jyc/agent-session.json` pre-created at the start of
  agent processing via the new `session::ensure_session_file`. The file
  is seeded with the active model's `max_input_tokens`, zeroed
  counters, and a fresh `created_at`, so the dashboard and outbound
  context-limit probes stop returning `(None, None, None)` during the
  window between "user sends message" and "first LLM response
  arrives". The helper is a no-op when the file already exists, so
  existing token data is never overwritten.

- **Auto-reset now honors `reset_compression` config.** When the in-loop
  `update_tokens` auto-reset triggers (input tokens cross 95% of the model
  context), it now uses the same compression mode (`None` | `Heuristic` |
  `Llm`) and `keep_pairs` as manual `/reset`. Previously the auto path
  always used LLM summarization, ignoring the user-configured mode.

- **Build mode auto-compacts when the new model has a smaller context
  window.** Switching from a 1M-context plan model to a 256k-context build
  model now resets the session using `reset_compression` *before* the
  first turn, so the build no longer fails with a context overflow on the
  first LLM call. The new pre-loop pre-check shares the same
  `reset_compression` config and `reset_session` implementation as manual
  `/reset` and the post-loop auto-reset — one compaction path for all
  three triggers.

- **`max_input_tokens` is updated on every model or mode switch.**
  `/plan`, `/build`, and `/model` now write the new model's
  `max_input_tokens` to `.jyc/agent-session.json` immediately, so the
  dashboard and reply footer reflect the new threshold before the next
  turn instead of waiting for it to be recomputed on the next successful
  LLM call.

- **`/reset` now uses the matched pattern's `reset_compression`.**
  Previously `/reset` used the first pattern on the channel as a fallback
  (the command handler had no access to the matched pattern). It now
  reads `.jyc/pattern` and resolves against the actual matched pattern,
  falling back to the first pattern only when the pattern file is
  missing. Topics that span multiple patterns with different
  `reset_compression` configs will now use the correct one.

### Added

- **Token counts update mid-round, not only at end of round.**
  The agent loop now persists `agent-session.json` after every LLM
  response via the new `session::persist_tokens` (sibling of the
  existing `update_tokens`). Dashboard polls see fresh input/output
  token counts within ~500 ms instead of waiting until the round
  finishes. The post-loop `update_tokens` still owns the between-message
  auto-reset.

- **Output token count in the dashboard topic info pane.** The chat
  info pane, dashboard topics table cell, and dashboard status line
  now show the accumulated output token count alongside the input
  token usage. The session file already stored `total_output_tokens`;
  the reader just discarded it. New `session_state::read_token_state`
  returns all three fields in a single file read for the polling path.

- **Command palette on the dashboard screen.** `Ctrl+P` or `:` opens the
  palette on the dashboard (previously chat-only). Palette commands are
  now scoped — `dashboard` (open chat), `chat` (open dashboard, zen,
  explorer, activity pane, editor, scrolling), and `shared` (new chat,
  reload config, quit) — and each screen shows its own scope plus shared
  commands. Navigation between screens goes through the palette:
  `open dashboard` / `open chat` / `new chat`.

- **Bare `jyc` opens an ad-hoc websocket topic and launches chat.**
  Clap's "missing subcommand" error is caught and `open` is injected,
  so `jyc` is now equivalent to `jyc open`.

- **Topic explorer pane.** `Ctrl+E` (or `toggle explorer` in the
  command palette) opens a left-side pane (20% width, default hidden)
  listing all topics with a live status dot (processing / queued /
  waiting / idle / error); the current topic is highlighted. `Tab`
  cycles focus into it, `↑↓/jk` navigate, `Enter` switches the chat
  (websocket topics) or opens the legacy detail view. Zen mode hides
  it; exiting zen does not restore it.
- **User/AI turn separator.** A light dashed rule now separates the user
  message from the AI response within a chat round.
- **Adaptive command popup width.** The `/` popup and command palette
  now size to their longest entry instead of a fixed 52 columns.

- **TUI command palette.** `Ctrl+P` (any editor mode) or `:` (Normal
  mode) opens a palette of TUI-local actions — toggle zen, cycle activity
  pane, open input in external editor, scroll top/bottom — each shown
  with its keybinding. Palette selections execute locally and are never
  sent to the backend; the `/` popup remains backend commands only.

- **`jyc agents` and `jyc skills` commands.** `jyc agents install [name]`
  installs agent templates from `<source>/templates/` into
  `<target>/templates/`; `jyc skills install [name]` does the same for
  `<source>/skills/`. `--source` defaults to the current directory,
  `--target` defaults to the platform config home (e.g. `~/.config/jyc`),
  and omitting `name` installs everything. `list` subcommands show what a
  source directory provides.

- **Inspect server authorization token.** `jyc serve` generates a random
  token and writes it to `<workdir>/auth.token` (owner-only permissions on
  Unix). `jyc dashboard` auto-loads it from the same workdir, or accepts
  `--token` / `JYC_DASHBOARD_TOKEN` env var. `jyc token show` prints the
  current token. All inspect REST requests carry the token in the JSON
  envelope; WebSocket upgrades carry it as an `Authorization: Bearer`
  header. The server rejects mismatches.

- **Slim `/state_overview` REST endpoint.** New `get_state_overview` method
  returns `InspectOverview` with `TopicSummary` rows (no per-topic
  `activity` / `recent_messages` / `thinking_text`), keeping the dashboard's
  per-poll payload small. The dashboard now polls this instead of
  `get_state`. The full `get_state` endpoint is retained for backward
  compatibility with any external clients.
- **Per-topic REST endpoints for cold-start hydration.** New
  `get_topic_activity {channel, topic, since?, limit?}` and
  `get_topic_chat {channel, topic, since?, limit?}` methods read from
  `.jyc/activity.jsonl` and `chat_history_*.jsonl` respectively. The
  dashboard calls these once when a topic is selected to seed the live
  buffers before the WebSocket connection delivers subsequent events.
- **Unified `/ws/<channel>/<topic>` WebSocket endpoint.** The dashboard
  always connects to this URL regardless of channel type. The inspect
  server dispatches to `ScopedWsHandler` (wraps the existing
  `WebsocketInboundAdapter` for websocket-type channels) or
  `TopicProxyHandler` (new, proxies through `TopicManager::enqueue` for
  any other channel). The handler binds `(channel, topic)` from the URL
  so the WebSocket payload no longer needs to carry them.
- **Live activity / chat / thinking events over WebSocket.** All four
  event types are published by the `ActivityTracker` to a shared
  per-channel broadcast bus (`InspectContext.inspect_broadcast`,
  capacity 256) and forwarded to subscribed WebSocket clients filtered
  by `(channel, topic)`. Replaces the dashboard's 500ms polling for
  live data.
- **`resync` event on broadcast backpressure.** When a WebSocket
  subscriber falls behind and `Lagged(n)` is observed, the server emits
  a `{"type":"resync", "channel":..., "topic":..., "dropped":N}` event;
  the client clears its live buffer for that topic and re-hydrates via
  REST. Restores correctness after long disconnects.
- **Monotonic per-topic sequence id.** `ActivityEntry` and
  `ChatMessageEntry` gain an `id: u64` field (`#[serde(default)]` for
  backward compatibility with old `.jyc/activity.jsonl` entries). Used
  by WebSocket clients to drop duplicate / older events after a
  reconnect or `resync`.

- **Dashboard `Ctrl+C` shortcut for `/cancel`.** Pressing `Ctrl+C` in
  the chat pane (any focus) sends `/cancel` without modifying the input
  buffer, matching the behaviour advertised in CHANGELOG v0.3.12. The
  local echo goes through the same `/cancel` interception path the
  worker already uses for typed `/cancel` messages, so the per-topic
  `CancellationToken` fires immediately. The shortcut is also
  advertised in the chat-pane help bar.

- **`<think>` tag parsing for OpenAI-compatible providers.** Providers like
  MiniMax M3 that emit thinking content inline in the `content` field wrapped
  in `<think>...</think>` tags (rather than in a separate `reasoning_content`
  field as DeepSeek does) are now correctly parsed — thinking is routed to
  `ReasoningDelta` and shown via `/thinking show` / hidden via `/thinking
  hide`, instead of leaking into the user-facing reply. The `\n\n` separator
  MiniMax emits between the think block and the actual response is stripped.
  Assistant turns are also replayed with `<think>...</think>` tags inlined
  in `content` so multi-turn conversations with these providers keep their
  thinking context. (#424)
- **Dashboard vim-style pane navigation.** The chat and activity panes now
  support `j`/`k` for scrolling and `gg`/`G` for jumping to the top/bottom, in
  addition to the arrow keys. In the chat pane this applies in Normal mode when
  the input is a single line (where these keys are editor no-ops); multi-line
  input keeps them as editor motions. The pattern select list also accepts
  `j`/`k`. The chat input area now grows from 1 up to 10 text lines (was 4).
- **Command popup tab auto-complete.** Pressing Tab in the `/` command popup
  fills the selected command or model name into the filter field, allowing
  refinement before sending. The popup stays open. (#416)
- **Chat input history with Up/Down.** When the chat input is empty, Up arrow
  recalls the last sent message. Repeated Up cycles older, Down cycles newer;
  Down at the newest clears back to empty. History is scoped per topic. (#416)
- **Live LLM thinking preview in chat pane.** When a reasoning model (e.g.,
  DeepSeek thinking mode) produces `reasoning_content`, the agent now publishes
  throttled `Thinking` events (at most once per 500ms, preview truncated to
  300 chars) that render in the dashboard chat pane alongside the generic
  "AI is thinking..." indicator, so users can see the chain-of-thought in real
  time. **`Ctrl+T` toggles** whether the full thinking preview is shown; the
  activity pane and persisted `activity.jsonl` always show a minimal
  "Thinking..." marker (verbose reasoning text is kept in-memory only and never
  written to disk).

### Fixed

- **Reloading config now applies new/changed model settings without a
  server restart.** `JycAgentService` used to hold a frozen copy of the
  agent config taken at startup, so reloading `config.toml` (via the
  TUI `reload config` action) made new models appear in the model list
  but their `context_window`, `supports_images`, `params`, `user_agent`,
  and the agent-level `small_model`, `max_iterations`,
  `sse_read_timeout_secs` were still read from the stale snapshot — a
  full server restart was required for them to take effect. The service
  now holds the same `Arc<ArcSwap<AppConfig>>` that `MessageRouter` and
  the inspect server use, and derives the effective agent config on
  each request from the live shared config. A regression test asserts
  the new model becomes visible (with its per-model `context_window`)
  immediately after a config swap. (#478)

- **Topic list not globally sorted across channels.** The dashboard
  Topics table and chat explorer pane showed topics sorted within each
  channel but grouped by channel iteration order, not one alphabetical
  list. `build_overview_state` now sorts the combined topic summaries by
  `(name, channel)`. (#476)

- **Slash command results not shown live in chat.** Command replies
  (`/model`, `/close`, `/help`, etc.) were persisted to chat history
  (visible after re-entering the topic) but never appeared live in the
  chat pane. The command-result path called `send_reply` but not
  `publish_reply_sent`, so no `chat_message` event reached the dashboard
  WebSocket. Both command-result paths (main and during-AI-processing)
  now publish the `ReplySent` event, matching the AI-reply flow.

- **Chat progress stale after switching topics.** `live_processing` /
  `live_thinking` are only updated by WS events received while a topic is
  watched, so entries went stale for unwatched topics: a missed completion
  left phantom progress (last 2 activity events) and a missed start hid the
  progress of a busy topic. Both are now cleared on the REST hydrate when
  switching topics, falling back to the polled overview status until fresh
  WS events arrive.

- **`[agent].plan_model` and `[agent].build_model` now take effect at
  runtime.** Previously the slim `jyc_agent::types::AgentConfig` view
  hardcoded these to `None` when deriving the per-request config, so the
  top-level config fields were silently ignored (per-pattern and
  topic-level overrides still worked). The duplicate-type cleanup fixed
  this as a side effect — `derive_agent_config` now returns
  `jyc_types::AgentConfig` directly with these fields intact. (#477)

### Changed

- **Eliminated duplicate agent config types (single source of truth).**
  `jyc-agent` no longer defines its own `ProviderConfig`, `ModelConfig`,
  `VisionConfig`, or slim `AgentConfig`; it uses `jyc_types::{ProviderDef,
  ModelDef, VisionConfig, AgentConfig}` directly. `provider::create_provider`
  takes `&HashMap<String, ProviderDef>`. `derive_agent_config` collapses
  from an 80-line per-field conversion to a 10-line channel-override patch
  (clones `app.agent` and applies `channels.<name>.model` /
  `small_model`). Net diff: **-198 lines** (4 redundant type
  definitions + 80 lines of pure copy code → gone). The
  `max_iterations` default in `jyc_types::AgentConfig` is raised from
  200 to 500 to match the agent-runtime default (the slim
  `jyc_agent::types::AgentConfig` had already been silently 500 since
  v0.3.6 — this is the upstream fix for that divergence). (#477)

- **Decoupled Gitee release sync from the release CI pipeline.** The
  `sync-gitee` job is now a separate workflow (`sync-gitee-release.yml`)
  triggered by the GitHub `release` event instead of running inline in
  `release.yml`. This prevents slow Gitee API calls from blocking the
  next nightly build. The sync workflow uses `cancel-in-progress: true`
  so only the latest nightly is synced.
- **Input gutter arrows.** The prompt gutter now uses `❯` (Insert mode)
  and `❮` (Normal/Visual mode) instead of `>` / `<`. Box-drawing
  characters (`╭─`, `╰─`, header dash padding) are colored `#393552`;
  the arrows are yellow. Coloring is focus-dependent: when the input
  field is focused the border/gutter lines use the message-separator
  `DarkGray`, info text stays sapphire, and the arrows are yellow; when
  focus moves away, the border, gutter line, arrows, and info text all
  dim to `#393552`.

- **`Esc` no longer leaves the chat screen.** Returning to the dashboard
  is done via the palette (`open dashboard`, `Ctrl+P`). `Esc` keeps its
  other meanings: closing popups, returning focus to the chat input, and
  vim Insert→Normal.
- **Status bars show a minimal `[^P]palette [^Q]quit` hint.** The full
  shortcut lists were removed; shortcuts are discoverable in the palette.

- **External editor keybinding moved from `Ctrl+E` to `Ctrl+O`** —
  `Ctrl+E` now toggles the topic explorer pane.

- **In-repo skills moved from `.agent/skills/` to `skills/`** at the
  repo root, and `.agent/` is no longer git-tracked. Runtime skill
  discovery is unchanged (the `{workdir}/skills/` scan path already
  covers the new location, and `.agent/skills/` scan paths are kept
  for compatibility).

- **Chat input gutter and header color changed to Catppuccin sapphire.**
  The `╰─> ` prompt gutter and the `╭─ {mode} …` header line above the
  chat input were bright Yellow (gutter) / Catppuccin green-yellow
  (header, per mode). Both now use the softer Catppuccin sapphire
  (`#74C7EC`) when focused; DarkGray unfocused is unchanged. The
  per-mode green/yellow color distinction is removed.
- **Chat header line shows channel, pattern, version, model, and
  input-token percentage.** The single `╭─ plan` / `╭─ build` line
  above the dashboard chat input is now
  `╭─ {mode} · {channel} · {pattern}` with a right-aligned
  `[ jyc ai v{ver} · {model} · {pct}% ]` chip, separated by `─`
  padding that fills the chat-pane width. No new border is added; the
  line still has no bottom or right border. Falls back gracefully
  when channel/pattern/model/tokens are missing (renders `?` and
  `–%`). The chip dims along with the mode word when the input loses
  focus.
- **Mode header dims with focus.** The `╭─ build` / `╭─ plan` header
  above the chat input now dims to DarkGray when Tab moves focus away
  from the input field, synchronizing with the input prompt gutter.
- **Dashboard chat input prompt redesigned.** The single-letter `B`/`P`
  agent-mode chip (a lone colored-background cell in the backgroundless
  input gutter) is replaced by a two-line gutter: an fg-colored
  `╭─ build` / `╭─ plan` header (Catppuccin green/yellow, bold, no
  background) above a vim-aware `╰─> ` (Insert mode) / `╰─< ` (other
  modes) prompt, which keeps the Yellow/DarkGray focus dimming. The
  gutter is 4 columns wide and the input area reserves one extra row
  for the header.

- **`/close` now requires `-y` (or `--confirm`) to actually delete a topic.**
  Sending plain `/close` returns a warning message listing the topic name
  and the correct confirm syntax, and performs no destructive action. This
  protects against accidental topic deletion (chat history, AI session,
  attachments) via typo or wrong command. The 8 external `on_topic_close`
  callbacks (Feishu chat disbanded, GitHub/Gitee issue/PR closed, etc.)
  remain unchanged — they are not user-initiated and do not need a gate.

- **Unified dashboard chat transport.** All channels (email, github,
  feishu, websocket, etc.) are now reached via the single
  `/ws/<channel>/<topic>` WebSocket endpoint. Non-websocket channels
  previously had to fall back to REST `inject_message` plus a 500ms
  poll; the new `TopicProxyHandler` does the same job in real time
  over WebSocket using the per-channel `InspectContext.broadcast` bus.
  The websocket-channel handler (`WebsocketInboundAdapter`) is preserved
  for external clients connecting directly to that channel.
- **Dashboard activity pane + chat progress read from a single source.**
  Both panes now read exclusively from the WS-fed
  `ChatState::live_activity` buffer (keyed by `(channel, topic)`),
  populated by REST hydrate on selection and by `TopicEvent` fanout
  thereafter. The per-activity / per-chat rendering logic is unchanged
  — only the data source moved.
- **Activity `TopicEvent` fanout.** `ActivityTracker` now publishes
  `activity` / `chat_message` / `thinking` / `processing` events to the
  inspect-broadcast bus on every push, in addition to the existing
  in-memory buffer. The bus is consumed by `TopicProxyHandler` and
  (via `ScopedWsHandler` for websocket channels) the dashboard.

### Fixed

- **Topic explorer opens on a stale row.** Opening the explorer
  (`Ctrl+E` / palette) now snaps the selection to the topic currently
  open in the chat pane. `sync_explorer_selection` only followed the
  chat topic while the explorer was unfocused — and opening focuses
  it, so the follow-up never ran.
- **Topic explorer selection fills the full row width.** When the
  explorer pane has focus, the highlighted row now paints the entire
  row's width with the selection background instead of stopping at the
  end of the topic-name text. The status dot, separator, and trailing
  padding all carry the highlight so the selection visually represents
  the complete selectable row.

### Removed

- **Dashboard `r` (force refresh) and `s` (reset session) keys.** `r` was
  a near no-op with 500ms auto-polling; session reset remains available
  in-band via the `/reset` chat command. The now-dead out-of-band reset
  chain was removed with it: `InspectClient::reset_session`, the inspect
  protocol `reset_session` method, the WebSocket `reset_session` message
  type, and `InspectResponse::ResetSessionResult`.

- **`jyc templates` command and `templates/templates.toml`.** The old
  `deploy` mechanism (which materialized per-template skill copies under
  `.agent/skills/`) is replaced by `jyc agents install` +
  `jyc skills install`; skill scoping is handled by channel/pattern
  `skills` / `disabled_skills` config. The `--model` and `--as` deploy
  flags are gone.

- **`inject_message` inspect protocol method.** Replaced by the unified
  WebSocket endpoint. The dashboard no longer uses REST injection; the
  server handler, response variant, client method, defensive error
  arms, and tests are all removed. Only the TUI used this method.

- **Dashboard chat pane opens with the activity pane hidden by default.**
  The default layout is now 100/0 (chat only, no activity pane). `Ctrl+W`
  continues to cycle through the four layouts: `100/0 → 80/20 → 20/80 → 0/100`
  and back. Previously the default was `80/20` (both panes visible).
- **Moved vim mode indicator from the chat input area to the bottom status bar.**
  The current editor mode (Normal/Insert/Visual) is now rendered as a
  right-aligned, Catppuccin-colored chip on the bottom status bar while chatting,
  freeing up one row of vertical space in the input area. The input area now
  grows from 1 up to 10 text lines (instead of 11, previously counting an
  in-editor mode-status row).
- **Brighter color for AI thinking preview.** The live thinking-text lines in the
  chat pane use `Gray` instead of `DarkGray` for better contrast against the
  dark background.
- **Command popup: Enter sends immediately; Tab copies to input on complete filter.**
  Pressing Enter in the `/` command popup now sends the selected command
  right away instead of populating the input line. Pressing Tab still
  auto-completes an incomplete filter (e.g. `pl` → `/plan`); when the filter
  already matches a complete command name (e.g. `thinking` or `/thinking`),
  Tab copies the command to the input line so the user can add arguments
  (e.g. `/thinking show`) before sending. The model-selection sub-mode follows
  the same rules: Tab fills an incomplete filter (e.g. `model gpt` →
  `/model gpt-4`), and Tab on a complete filter (e.g. `/model gpt-4`) copies
  to the input line. Enter sends in both modes.
- **Command popup Enter keeps the input field.** Pressing Enter on a command
  in the `/` command popup now sends the selected command without clearing
  the chat input editor. The editor retains whatever was there before the
  popup opened (empty in Insert mode, pre-existing text in Normal mode).
  Only the popup-Enter send path is affected; typing-and-sending in the
  editor still clears the field as before. The Tab copy-to-input path is
  unchanged — Tab still populates the editor for editing before send.

- **Borderless chat-pane layout with new shortcuts.** The dashboard chat
  screen no longer renders the channel bar. The chat pane itself is now
  borderless; each conversation round is delimited only by a horizontal
  top rule with the timestamp on the left and a horizontal bottom rule
  with the duration on the right (no side borders, no middle divider).
  The compact one-line info bar has been replaced by a bordered Topic
  Info pane fixed at 20% of the screen width on the right. By default
  the Topic Info pane, status bar, and bottom activity pane are all
  hidden (zen mode). New shortcuts: `Ctrl+A` cycles the activity pane
  through hidden → bottom 20% → bottom 80% → activity-only → hidden
  (replaces the previous `Ctrl+W`); `Ctrl+Z` toggles zen mode and also
  hides any visible activity pane on enter (exiting zen mode restores
  only the info pane + status bar, not activity). `Tab` continues to
  switch Chat/Activity focus.

- **Message-area focus in the chat pane.** `Tab` now cycles focus through
  Input → Message area → Activity pane (the activity pane is skipped when
  hidden). While the message area is focused, `↑/↓` and `j/k` scroll the
  conversation, `PgUp/PgDn` page, `gg/G` jump to top/bottom, `Esc`
  returns focus to the input field, and typing any other key refocuses
  the input and forwards the key to the editor. The previous
  Normal-mode/single-line-input scroll behavior is removed — scrolling
  the conversation always happens through message-area focus.

- **Agent mode letter chip in the chat input prompt.** The chat input
  prompt is now prefixed with an always-visible single-letter chip: `B`
  (green) for build mode, `P` (yellow) for plan mode. The gutter grows
  from 2 to 4 columns (`B > `).

- **Breathing space in chat round rules.** The horizontal rules bounding
  each conversation round now pad the timestamp and duration:
  `── 09:50 ────────` (top) and `──────── 1m ──` (bottom).

- **Explorer pane: switch is functional.** Opening the explorer now
  moves focus into it (so `j`/`k`/`Enter` work immediately); after a
  successful switch the new topic's chat history is hydrated (was
  blank) and the explorer auto-hides so you land in the new chat. The
  explorer also spans the full chat-screen height as a left column.
- **Explorer pane: detail-mode state no longer leaks.** `open()` now
  clears `detail_channel`/`detail_topic_path`, so switching from a
  detail view back to a websocket chat exits detail mode.

### Fixed

- **`/cancel` now aborts tool execution immediately.** Previously the
  per-topic `CancellationToken` was honored at LLM-call boundaries
  and between tool iterations, but a long-running tool call (e.g.
  `bash` running `sleep 60`, a `webfetch` HTTP request) ran to
  completion before the loop noticed. The agent loop now races
  `tools.execute()` against `cancel.cancelled()` via `tokio::select!`;
  on cancel, the tool's in-flight future is dropped — which kills any
  spawned child process for `bash` via `tokio::process::Child::drop` —
  and the existing dangling-message cleanup in `agent_loop` runs
  unchanged. End-to-end verified by a regression test that fires
  `/cancel` 200 ms into a `bash sleep 60` and asserts the agent
  returns within 5 s with no reply text.

- **WebSocket dashboard disconnect rejected on websocket-type channels.**
  The CLI dashboard sends `{"type":"disconnect"}` to close the WebSocket
  connection cleanly on every topic navigation, chat open/close, and
  overview-WS swap. The server-side `ClientMessage` enum for
  `WebsocketInboundAdapter` only defined the `Message` variant despite
  its doc comment advertising `disconnect`, `reset_session`, and `ping`,
  so serde rejected the disconnect frame with `unknown variant
  'disconnect', expected 'message'`. The read loop never broke, the
  connection eventually RST'd from the client side, and the dashboard
  reconnected on the next poll — producing a ~1s connect/reset flap with
  `Connection reset without closing handshake` warnings and missed
  inspect-broadcast events. Mirrors `TopicProxyHandler::ClientMessage`
  so the protocol contract matches the doc comment. `Disconnect` breaks
  the loop and the existing post-loop helper sends a WS Close frame;
  `reset_session` and `ping` are accepted as no-ops.

- **`get_topic_activity` returns 0 entries despite data in JSONL.**
  Server-side filter at `handle_get_topic_activity` was inverted —
  `.filter(|e| !is_user_visible_activity(e))` dropped the user-visible
  entries and kept internal heartbeats. Since the JSONL contains only
  user-visible entries (internals are skipped at write time, see
  `ActivityTracker`'s `if !is_internal { ActivityLogStore::append(...) }`
  guard), the response was always empty and the dashboard's activity
  pane stayed blank on cold start. Removed the `!` to match the
  client-side filter (`jyc-cli/src/cli/dashboard/chat.rs:1137, 1184`)
  and the function's documented semantics. New `tracing::debug!` lines
  log the request and the served entry count, so a non-zero count in
  the log confirms the fix.

- **`get_topic_activity` hydration is now traceable.** The inspect
  server's `handle_get_topic_activity` handler emitted nothing on
  entry or exit, so the dashboard's empty-activity-pane issue could
  not be diagnosed from existing logs. Two `tracing::debug!` lines at
  the server (entry: channel/topic/limit/since; exit: count of entries
  served) let the operator distinguish the three failure modes — never
  fired, fired but returned 0 entries, fired and returned data — from
  the log alone.

- **Dashboard chat pane clips long thinking content.** Thinking/reasoning
  text wider than the chat pane was truncated at the right edge instead of
  wrapping. The preview is now hard-wrapped to the available width
  (Unicode-aware, preserving explicit newlines and blank lines) before being
  added to the scroll buffer, so all content is visible and scrollable.

- **Sanitize malformed tool call arguments in OpenAI-compatible provider.**
  Some models (notably MiniMax M3) occasionally emit tool call arguments that
  are not valid JSON. The previous code embedded them as-is into the request
  body, so the malformed arguments got persisted to `raw_context` and triggered
  a 400 "invalid function arguments json string" on the next API call when the
  conversation was replayed. Arguments are now validated before embedding; on
  parse failure they are replaced with `"{}"` (matching the existing Anthropic
  provider behavior), keeping the conversation context replay-safe.

- **Fix truncated tool call arguments when finish_reason arrives in the same
  SSE chunk as the final argument fragment.** Some OpenAI-compatible
  providers (notably MiniMax M3) stream the last fragment of the tool call
  arguments JSON in the *same* SSE message as `finish_reason: "tool_calls"`.
  The previous parser processed `finish_reason` before the `tool_calls`
  delta, which cleared the argument accumulator and emitted `ToolUseEnd`
  before the last fragment was appended — truncating the JSON tail (e.g.,
  the `file_path` parameter of a `read` tool call) and producing
  "Missing 'file_path' parameter" errors. The chunk processing order has
  been swapped so the tool_calls delta is accumulated first and
  `finish_reason` finalizes the call after. A safety-net flush in
  `collect_response` also ensures any in-progress tool call is saved at
  stream end even if `ToolUseEnd` was never emitted.

- **Display Anthropic extended thinking in the chat pane.** When Anthropic
  models with extended thinking enabled (e.g. Claude Opus 4.6) produce a
  response, the `thinking` content blocks and `thinking_delta` deltas were
  silently dropped by the SSE parser (fell through to the wildcard arm) and
  never reached the chat pane, even with `/thinking show` enabled. Both
  event types are now captured and emitted as `ReasoningDelta` so the
  thinking text streams to the dashboard in real time. `signature_delta`
  (the cryptographic signature at the end of each thinking block) is
  correctly ignored.

- **Dashboard: Esc from chat pane returns to topic overview instead of pattern select.**
  When chatting in a WebSocket topic, pressing Esc now consistently returns to the
  topic overview table rather than the pattern selection screen. The pattern select
  view is now only reachable via the `c` (new chat) shortcut from the overview.

- **Dashboard message injection loses routing metadata for non-websocket channels.**
  When injecting a message via dashboard into a GitHub/Gitee/WeCom/Feishu/Email
  topic, the synthetic `InboundMessage` had empty metadata, causing reply
  delivery failures (e.g., GitHub 404 — `github_number` missing). Now routing
  metadata is persisted to `.jyc/topic-meta.json` on first message and restored
  during injection. (#408)

- **Dashboard injection poisons `topic-meta.json` with empty metadata.**
  When the first message for a topic was a dashboard injection
  (`channel_uid == "dashboard"`), the empty metadata was persisted to
  `topic-meta.json`, causing subsequent injections to lose routing data
  (e.g., `github_number` missing → 404). Now `topic-meta.json` is only
  written for messages with real routing data. (#410)

- **Nightly release sync to Gitee.** Reordered `sync-gitee` job steps in
  `release.yml` so `actions/checkout@v4` runs before artifact download —
  checkout was resetting the workspace and deleting the `dist/` directory,
  causing `curl: (26) Failed to open/read local data from file/application`
  when uploading release assets. (#395)

- **Dashboard AI replies not visible in chat pane in real-time.**
  The `SimpleThreadEventBus` dropped events when no subscriber existed,
  creating a race with the ActivityTracker's 2-second discovery interval.
  If the AI replied before the tracker subscribed, `ReplySent` events were
  permanently lost. Now events are buffered and replayed to late subscribers. (#411)

- **Event bus not shared with worker clone, silencing all ReplySent events.**
  `create_and_enqueue` creates a cloned `TopicManager` for each worker with an
  empty `event_buses` HashMap. `publish_reply_sent()` called `get_event_bus()` on
  this clone, which always returned `None` — events never reached the bus and
  were silently dropped. Now the event bus reference is also inserted into the
  clone's `event_buses`, so `publish_reply_sent()` finds it. (#412)

- **Dashboard chat pane auto-scrolls to bottom every poll cycle.**
  The detail-mode message processing unconditionally reset `chat_scroll` to 0
  on every poll, overriding any user scroll position. Now auto-scroll only fires
  when new messages are actually added. (#413)

### Added

- **Dashboard cross-channel topic chat.** Pressing `Enter` on a non-websocket
  topic (email, feishu, github) opens a detail/chat mode. Shows live incoming
  messages and AI replies via `TopicEvent::IncomingMessage`/`ReplySent` events
  forwarded through `TopicInfo.recent_messages`. Message injection uses the
  new `inject_message` inspect protocol method, following the same
  `TopicManager::enqueue()` path as the `send_to_topic` tool. (#406)

- **`/pin` command.** Persist an ad-hoc websocket topic's configuration to
  `config.toml`. If a websocket channel already exists, adds a pattern with
  `topic_path` pointing to the adhoc directory; otherwise creates a new
  websocket channel. (#405)

- **`/unpin` command.** Remove a pinned topic's pattern entry from
  `config.toml`, reversing `/pin`. (#405)

- **Nightly release binaries for macOS (aarch64) and Linux (x86_64)**, built by
  GitHub Actions on every push to `main`, published to a rolling `nightly`
  GitHub Release, and synced to Gitee Release. (#394)

- **`/` command popup in dashboard TUI chat input.** Typing `/` opens a
  command palette with live filtering, arrow-key navigation, and Enter-to-send.
  Commands are dynamically fetched from the inspect server. (#402, #403)

- **Command popup improvements.** Filter no longer requires `/` prefix (typing
  "model" finds `/model`). Inline model selection for `/model`: typing "model "
  (with trailing space) shows available models for direct selection. Popup
  widened to 52 columns. (#404)

### Changed

- **Removed Shift+Tab mode switch shortcut** from the dashboard chat input.
  Mode switching is now done via `/plan` and `/build` commands in the popup.

### Fixed

- **Command popup shows empty list on open.** Fixed `filtered_commands()` to
  return all commands when filter is empty, instead of an empty list.

- **Multi-level configuration with platform-conventional paths.** JYC now
  separates user-edited config from generated data: `config.toml`, `skills/`,
  and `templates/` live in the platform config dir (Linux: `~/.config/jyc`),
  while topics, channel state, and chat history live in the platform data
  dir (Linux: `~/.local/share/jyc`). Three-level layering: L1 global
  (config dir) → L2 workdir (`--workdir`/`--config`) → L3 topic (`.jyc/`).
  `config.toml` is deep-merged L2-over-L1; skills are merged with
  higher-level-wins; templates are looked up L3 → L2 → L1. (#393)
- **First-run provisioning.** `jyc serve` without flags and without an
  existing config creates `~/.config/jyc/config.toml` (from
  `config.example.toml`) plus empty `skills/` and `templates/` directories,
  prints edit instructions, and exits. (#393)
- **Topic-level `.jyc/config.toml`.** Supports a restricted `[agent]`
  subset (`model`, `plan_model`, `build_model`, `small_model`). Precedence:
  `.jyc/<mode>-model-override` file > `.jyc/config.toml` > pattern > config.
  Invalid files are ignored with a warning. (#393)
- **Auto-start `jyc serve` from `jyc dashboard` and `jyc open`.** When the
  server is not running, dashboard and open commands auto-spawn `jyc serve`
  in the background (once per session). Logs are written to
  `<data_home>/jyc.log`. First-run provisioning output is shown to the user.
  Only works for localhost addresses. (#393)
- **jyc stop command** — Stop a running jyc serve process via `jyc stop` (SIGTERM) or `jyc stop --force` (SIGKILL). Works with the same `--workdir` resolution as serve. (#400)

### Changed

- **Default config/workdir locations.** `jyc serve` without `--workdir` no
  longer uses the current directory — it uses the platform data dir
  (Linux: `~/.local/share/jyc`); the default config is
  `~/.config/jyc/config.toml`. `jyc config init` now writes to the platform
  config dir by default. (#393)
- **Relative pattern `topic_path`** now resolves against the data root
  (workdir) instead of the process current directory. Absolute and `~`
  paths are unchanged. (#393)

### Fixed

- **Flaky skill-discovery tests.** `with_temp_home` now serializes tests
  that mutate the shared `HOME` env var via a static mutex (parallel-safe).
  (#393)

- **Per-model `model_id` override for provider model configs.** Each entry
  under `[agent.providers.<name>.models]` accepts an optional `model_id`
  holding the actual model identifier sent to the remote LLM; when unset,
  the models-map key is used as before. This lets multiple config aliases
  with different params (e.g. reasoning effort high/low) point at the same
  remote model. (#389)

- **Vi-style modal editing for the dashboard chat input.** The chat pane
  input is now a vim-inspired modal editor (via `edtui`) with
  Insert/Normal/Visual modes and a mode indicator in its status line. Supports
  motions (`hjkl w e b 0 $ gg G % {}` …), operators (`dd dw D cw J` …), text
  objects (`diw ciw vi" di(` …), yank/paste (`y yy p P`), undo/redo
  (`u`/`Ctrl+r`), and `.` repeat. The input starts in Insert mode; `Esc`
  switches to Normal mode, `Esc` again returns to pattern selection. `Enter`
  inserts a newline in Insert mode; send with `Shift+Enter`/`Alt+Enter`
  (Insert) or plain `Enter` (Normal). The input shows a yellow `> ` prompt;
  the cursor is a blinking underline in Insert mode and an inverted block
  otherwise. (#383, #387)

- **External editor for dashboard chat input.** Press `Ctrl+E` in the chat
  pane to open `$VISUAL` / `$EDITOR` (fallback: `vi`) with the current input
  in a temp file. The TUI suspends while the editor runs; on successful exit
  the edited contents replace the chat input, so you can compose messages
  with a real editor (vim, nvim, etc.). (#381)

- **Top-level `jyc open` shortcut for `jyc dashboard open`.** Open a directory
  as an ad-hoc websocket topic and launch chat mode directly from the top
  level CLI. Accepts the same flags (`-t/--topic`, `-p/--path`,
  `-c/--channel`) plus `--addr`. The `jyc dashboard open` form continues to
  work unchanged.

- **`jyc dashboard open` command for ad-hoc websocket topics.** Open a directory
  as a websocket topic directly from the CLI and launch dashboard chat mode.
  Works for brand new directories and for directories that already contain a
  `.jyc` subdir. Supports `-t/--topic` (defaults to the folder name of `-p` or
  CWD), `-p/--path` (defaults to CWD), and `-c/--channel` (auto-detected when
  only one websocket channel exists). The server creates the topic via a new
  `create_topic` WebSocket message, honoring the custom `topic_path` for file
  storage. Raises an error when the target directory already contains a
  `.jyc/topic-name` file with a different name than the one requested via
  `-t`, preventing accidental topic-name divergence.

- **`require_reply` flag for `jyc_send_to_topic`.** New optional boolean
  parameter `require_reply` (default: `false`) on the `jyc_send_to_topic`
  tool. When `true`, the target agent is instructed to send results back to
  the source channel/topic. The source metadata (`source_channel`,
  `source_topic`, `require_reply`) is now displayed in the target agent's
  incoming message prompt, enabling cross-topic request-response patterns.
  System prompt updated with reply guidance. (#361)

- **Per-pattern `mode` config for initial agent mode.** New optional field
  `mode` on `ChannelPattern` allows setting a default agent mode (`"plan"` or
  `"build"`) per pattern. Resolution chain: `.jyc/mode-override` runtime file >
  pattern `mode` > default `"build"`. (#356)

- **`stop_after` parameter for reply tool.** The `jyc_reply_message` tool now
  accepts an optional `stop_after` boolean parameter (default `true` for backward
  compatibility). When `false`, the agent sends a progress update and continues
  working instead of terminating the agent loop. This enables mid-task progress
  reporting without stopping. The `ToolOutput` struct gains a `stop_after` field
  and `success_continue()` constructor; the agent loop only sets
  `reply_sent_by_tool = true` when `stop_after` is `true`. Synthetic cycle-progress
  calls now pass `stop_after: false` explicitly. LLM understanding is reinforced
  at three levels: tool schema, tool description, and system prompt "Reply
  Instructions" section. (#341)

- **Configurable `sse_read_timeout_secs` for `[agent]` config.** New optional
  field `sse_read_timeout_secs` (default: 300) controls the maximum idle time
  between SSE events before the stream is considered stalled. Useful for models
  with long thinking phases that exceed the previous hardcoded timeout. (#341)

- **Per-pattern `topic_path` config for custom topic directories.** Patterns
  can now declare `topic_path = "~/my-project"` to override the topic's
  working directory location on disk. Supports `~` expansion to `$HOME`.
  Absolute paths are used as-is. The logical routing key (`topic_name` /
  `topic_prefix`) is unaffected — this field only changes where files are
  stored on disk. `TopicManager` tracks custom paths and resolves them
  correctly for chat history, activity logs, close/cleanup, and the
  `JobScheduler`. (#348)

### Changed

- **Renamed `jyc monitor` → `jyc serve`.** The main command for starting the
  agent is now `jyc serve`. `monitor` is kept as a hidden alias for backward
  compatibility, so existing scripts and systemd units continue to work.
  Updated deployment assets (`run-jyc.sh`, `docker/Dockerfile`, `SYSTEMD.md`),
  user-facing log messages, and all documentation.

- **Renamed `jyc_reply_reply_message` → `jyc_reply_message`.** The old tool name
  was redundant and awkward. Updated in tool registration (`mcp_bridge.rs`),
  agent loop synthetic cycle-progress calls, system prompt, all 7 GitHub/Gitee
  template `AGENTS.md` files, tool documentation (`docs/tools.md`), and
  `GITHUB_CHANNEL.md` architecture diagram. The agent loop's tool-name detection
  (`contains("reply_message") || contains("jyc_reply")`) matches the new name.
  (#341)

### Fixed

- **Topics failing on LLM rate-limit / overload errors (429/502/503/504).**
  LLM call failures are now classified as Transient / Throttled / Terminal.
  429 previously burned through the fast transient budget (3 attempts,
  1s/2s backoff) and 503 was not retried at all; both now use a patient
  throttled schedule (5 attempts, 5s/15s/30s/60s), honor the provider's
  `Retry-After` header as a floor (capped at 120s), and report the next
  retry's absolute time plus the Retry-After value in logs and dashboard
  status events. (#391)

- **Stale key bindings in the websocket channel doc.** `docs/channels/websocket.md`
  listed `Ctrl+D` (send) and `p` (back to pattern selection), neither of which
  exists in the dashboard; `Enter` sends and `Esc` returns to pattern
  selection. The Chat Pane Controls table now documents the actual bindings,
  including `Shift+Enter`/`Alt+Enter` newline, `Ctrl+B`/`Ctrl+F` scrolling,
  `Ctrl+W` split cycling, `Ctrl+C` cancel, `Shift+Tab` plan/build toggle, and
  `Ctrl+Q` quit. (#381)

- **Multi-line paste in dashboard chat input triggering premature send.**
  Pasting multi-line text into the chat pane delivered each line's Enter as a
  regular key event, so the first line was sent via `send_chat_message()`
  before the rest of the paste arrived. The dashboard now enables crossterm's
  bracketed paste mode, so terminals that support it deliver pasted content as
  a single `Event::Paste` that is inserted verbatim (newlines included) without
  triggering Enter handling. The existing timing-based Enter debounce is kept
  as a fallback for terminals without bracketed paste support. (#379)

- **WeCom bot file messages parsing when filename is omitted.** The
  `FileContent.filename` field was a required `String` but WeCom's API payload
  for `msgtype: "file"` messages sometimes omits `filename` (only `url` and
  `aeskey` are present). Changed to `Option<String>` with `#[serde(default)]`
  and updated both consumers (`extract_content` and `process_bot_attachments`)
  to handle `None` gracefully. (#374)
  websocket message arrived for a topic whose name did not match any configured
  pattern, the websocket matcher fell back to the first enabled pattern and wrote
  that name to `.jyc/pattern`. This caused ad-hoc topics (e.g., `adhoc`) to
  display the pattern of an unrelated topic in the chat pane. The matcher now
  uses the topic name itself as the pattern name when no configured pattern
  matches, so ad-hoc topics keep their own identity.

- **WeCom bot file attachments saved as `.bin` when MIME type is unknown.** When
  WeCom omits `filename` in a `msgtype: "file"` message or the file's magic bytes
  are not recognized, the attachment was previously saved as a generic `.bin`
  file. `download_media` now captures the HTTP `Content-Type` response header and
  `build_attachment` uses it as a MIME fallback; `extension_from_mime` was also
  expanded to cover common document, text, audio, and video types. (#375)

- **WeCom bot file attachments still saved as `.bin` when URL has filename.**
  Even after the MIME fallback in #375, CSV files (which have no magic bytes and
  often no HTTP `Content-Type`) still ended up as `.bin`. `process_bot_attachments`
  now extracts a filename hint from the media URL's last path segment when WeCom
  omits `filename`, so `https://cos.example.com/bucket/data.csv?sign=xxx` yields a
  `.csv` saved filename. (#376)

- **GitHub API error bodies spamming journal.** Non-2xx responses from GitHub may
  return multi-line HTML error pages. The GitHub client now collapses `\n`, `\r`,
  and `\t` in the error body before logging, keeping each failure on a single
  journal line instead of splitting "Hello future GitHubber" across many entries.
  (#377)

- **Cross-topic reply not sent back despite `require_reply=true`.** The
  in-message prompt instruction told the agent to call both
  `jyc_reply_message` and `jyc_send_to_topic` but did not specify the
  execution order. LLMs called `jyc_reply_message` (with `stop_after=true`)
  first, ending the agent loop before `jyc_send_to_topic` was invoked.
  Fixed by making the order explicit: step 1 calls `jyc_send_to_topic`,
  step 2 calls `jyc_reply_message`. (#367)

- **Cross-topic reply not sent back despite `require_reply=true`.** The system
  prompt instruction to use `jyc_send_to_topic` was placed in the general
  Cross-Topic Communication system prompt section, far from the actual
  incoming message. LLMs often missed it due to position bias. Moved the
  actionable instruction (`⚠️ ACTION REQUIRED`) directly into the incoming
  message prompt, immediately after the Source header and next to the message
  body — at the same recency level as the body and close to where the agent
  decides its response. (#366)

- **Cross-topic reply loses prior context with Anthropic provider.** The
  `raw_context_to_messages` function only handled OpenAI's string content format
  (`"content": "text"`) but not Anthropic's array block format
  (`"content": [{"type": "text", "text": "..."}]`). When using Anthropic models,
  all prior context messages were silently dropped during `load_context`,
  leaving the agent with no conversation history. Added `extract_text_content()`
  helper that handles both formats, and fixed the tool_calls detection to also

- **`jyc dashboard` no longer required a subcommand.** The initial `jyc dashboard
  new` implementation changed the top-level `dashboard` command to require a
  subcommand (`jyc dashboard dashboard` to open the dashboard). Fixed by making
  the `new` subcommand optional, so `jyc dashboard` behaves exactly as before
  and `jyc dashboard new` creates an ad-hoc topic. Also fixed ad-hoc topics
  sharing `agent-session.json` with unrelated topics: `TopicManager::enqueue`
  now prefers the registered per-topic custom `topic_path` over the pattern
  fallback, so messages sent after `create_topic` remain in the same topic
  directory.
  check for Anthropic `tool_use` blocks in content arrays. (#365)

- **Cross-topic reply not visible after new fix.** The system prompt instructed
  agents receiving cross-topic messages with `"⚠️ Reply requested"` to reply
  via `jyc_send_to_topic` but omitted the standard `jyc_reply_message` call
  for the current topic. Agents processed cross-topic results but never
  displayed them in the current topic's chat pane. Fixed by telling agents to
  always use `jyc_reply_message` for cross-topic messages, and additionally
  use `jyc_send_to_topic` for the ⚠️ case. (#364)

- **Session context deleted after auto-reset summarization.** The
  `summarize_context` function wrote the LLM-generated summary as a `"user"`
  role message instead of `"assistant"`. When `load_context` read the
  compacted context, it found no assistant messages, treated the file as
  corrupted, and deleted `agent-context.json` — causing the next session to
  start with no prior context. Fixed by using `"role": "assistant"` for the
  summary message. Also upgraded the deletion log to `error!` level with
  diagnostic details (message count, role list) for future troubleshooting.
  (#363)

- **Cross-topic reply not visible in WebSocket chat pane.** The
  `jyc_send_to_topic` tool set `InboundMessage.topic` to a hardcoded string
  `"Message from cross-topic tool"` instead of the target topic name. The
  WebSocket outbound adapter uses `topic` as the broadcast key, so replies
  from cross-topic sessions were broadcast to the wrong topic and never
  appeared in the chat pane. Fixed by setting `topic` to the target topic
  name. (#362)

- **Stale custom `topic_path` topics linger in dashboard after close.** When a
  topic directory was deleted (e.g. GitHub issue closed), the in-memory
  `topic_paths` mapping was never cleaned up. `list_topics()` now prunes
  entries whose `.jyc/` directory no longer exists, so closed topics disappear
  from the dashboard immediately. (#354)

- **Custom `topic_path` topics lost after restart.** Topics with a custom
  `topic_path` override disappeared from the topic list after process restart
  (e.g. Docker container restart). The in-memory `topic_paths` map was only
  populated on `enqueue()` and never restored from disk. Now, when a topic is
  first processed, its logical name is persisted to `.jyc/topic-name`. On
  startup, `restore_custom_topic_paths()` scans **only this TM's channel**
  patterns for `topic_path` overrides and reads `.jyc/topic-name` to rebuild
  the mapping before `ActivityTracker` loads historical activity. Restored
  topics also get a pre-created event bus so the first message's activity
  events are not lost to the ActivityTracker's 2-second subscription delay.
  (#350, #352, #353)

- **Dashboard chat pane shows stale content when switching topics.** When
  switching from topic A to topic B, if topic B had no chat history the
  server skipped sending a `history` message, leaving topic A's messages
  visible in the chat pane. `select_pattern()` now clears `chat_messages`
  before subscribing to the new topic. (#349)

- **Dashboard chat pane input not wrapping.** When typing in the chat pane
  input area, text exceeding the pane width was not automatically wrapped to
  the next line. The input paragraph now uses `Wrap { trim: true }` and the
  height/scroll calculations account for visual (wrapped) line counts using
  `unicode-width` for accurate CJK character width. (#343)

## [Unreleased]

### Added

- **Inspect REST API.** The inspect server now exposes a real HTTP/1.1
  REST surface in addition to the WebSocket routes. New endpoints:
  `GET /api/state`, `GET /api/state/overview`,
  `GET /api/topics/{channel}/{topic}/activity`,
  `GET /api/topics/{channel}/{topic}/chat`,
  `GET /api/channels/{channel}/patterns`,
  `POST /api/topics`, `POST /api/config/reload`. All endpoints and
  WebSocket upgrades share one Bearer token (`[inspect].auth_token`)
  via a single `require_bearer` middleware — the
  `Authorization: Bearer <token>` header (case-insensitive scheme per
  RFC 7235 §2.1) gates every route. The Rust client
  `jyc_inspect::client::InspectClient` now uses `reqwest` internally;
  its public method names are unchanged so the dashboard call sites
  are untouched. See `docs/api.md` for the full reference.

### Changed

- **BREAKING: removed line-delimited JSON inspect protocol.** The
  raw-TCP, one-JSON-object-per-line protocol that the inspect server
  used to accept is gone. All consumers must move to the new HTTP REST
  endpoints. The WebSocket protocol is unchanged. The
  `InspectRequest` / `InspectResponse` tagged-union enums are removed
  from `jyc-types`.

- **Inspect auth is now header-based.** The old `auth_token` field on
  the JSON request body is replaced by the
  `Authorization: Bearer <token>` HTTP header on every REST request
  and WebSocket upgrade. Same single config value, same token. The
  comparison is constant-time (defense in depth against timing leaks
  on the WS path's previous `!=` check).

### Removed

- `InspectRequest` and `InspectResponse` enums from
  `crates/jyc-types/src/inspect.rs`. Replaced by typed REST request
  bodies and JSON response shapes (status + body).

### Fixed

- **WebSocket chat to a websocket-type channel now works from the
  dashboard chat pane.** Connecting to `/ws/<channel>/<topic>` now
  propagates the URL topic name to the channel's WebSocket handler as
  `scoped_topic`. Previously the inspect server always passed `None`,
  so messages sent from the chat pane — which rely on the URL scope and
  omit `topic` from the payload — were dropped with "WebSocket Message
  without topic; ignoring".

## [0.3.12] - 2026-06-28

### Fixed

- **Anthropic provider: strip `oneOf`/`allOf`/`anyOf` from tool `input_schema`.** Claude Opus 4.6 rejects these JSON Schema composition keywords with a 400 error. External MCP tools may include them, so the Anthropic provider now defensively strips them at the provider layer via `sanitize_input_schema()`. Both `complete()` and `complete_raw()` paths are covered. (#294, #300)

- **jyc_send_to_topic attachments not delivered to target topic.** Fixed the
  tool creating `MessageAttachment` with `content: None`, causing
  `save_attachments_to_dir` to skip cross-topic files. Also surfaced attachment
  file paths in the target agent's prompt. (#267)

- **WeCom Bot `enter_chat` event parse error.** Added custom serde deserializer
  for `BotEvent.event` that accepts both plain string form (`"enter_chat"`) and
  object form (`{"eventtype": "enter_chat"}`), fixing a parse failure caused by
  WeCom API inconsistency. (#264, #266)

- **WeCom Bot scheduled task proactive message `req_id` error.** When `req_id`
  is missing (scheduled tasks), `send_reply` now derives `chatid` from
  `topic_path`, generates a fresh `req_id`, and uses `aibot_send_msg`
  (proactive) instead of `aibot_respond_msg` (passive reply). (#264, #266)

### Added

- **`/cancel` command and `Ctrl+C` dashboard shortcut.** New `/cancel` command
  cancels the current AI processing by triggering the per-topic
  `CancellationToken`. In the dashboard chat pane, `Ctrl+C` sends `/cancel`
  without modifying the input buffer. The worker's `select!` loop intercepts
  `/cancel` messages arriving during AI processing — the cancellation token fires
  immediately, causing the agent loop to break at the next iteration. Non-cancel
  messages arriving during processing are buffered and re-enqueued after the
  agent finishes, preserving FIFO order.

- **`Shift+Tab` dashboard shortcut for mode switching.** Toggles between plan
  and build mode by sending `/plan` or `/build` as a WebSocket message, reusing
  the existing command system for mode switching.

- **Dashboard focus pane border visibility.** The focused pane's border now uses
  `Yellow + BOLD` for better visual distinction, replacing the previous `Cyan`
  color that was difficult to notice.

- **Per-pattern filesystem access whitelist (`access.read` / `access.write`).**
  Patterns can now declare additional paths outside the topic working directory
  that the agent may read from or write to. Write paths are automatically
  readable. Tilde (`~`) expands to `$HOME`. Prevents repeated "Access denied"
  failures when the agent needs to read dependency source (e.g.,
  `~/.cargo/registry/src`) or write to build directories. (#275)

- **`jyc_send_message` enhanced with cross-channel and attachment support.** The
  builtin tool now accepts an optional `channel` parameter for sending proactive
  messages through any configured channel's outbound adapter (bypassing agent
  processing), and an optional `attachments` parameter for including file
  attachments (supported by email, Feishu, and WeCom Bot channels). Existing
  usage without these parameters is fully backward compatible. (#267, #270)

- **Cross-topic/channel communication tool (`jyc_send_to_topic`).** New
  builtin tool that enables AI agents to inject messages into topics in other
  channels. The tool is backed by `TopicManager::enqueue()` (same mechanism as
  the JobScheduler). ToolContext carries a cross-channel `topic_managers` map
  (keyed by channel name) wired from Monitor → JycAgentService → AgentLoopConfig
  → ToolContext. System prompt lists available channels. Attachment validation
  mirrors `ReplyMessageTool`. (#268)

- **Channel-agnostic scheduled job system.** Background JobScheduler runs
  alongside the monitor, scans all topics for due jobs (per-topic
  `.jyc/jobs/<id>.json` storage), fires recurring (cron) or one-time jobs by
  injecting InboundMessage into the originating topic. Agent tools (`job_list`,
  `job_create`, `job_delete`, `job_toggle`) let users manage jobs from any
  topic. Configurable max jobs per topic (default: 10). (#262, #263)

- **WebSocket channel with dashboard chat pane.** Replaces the standalone
  `jyc local` command. The `websocket` channel type runs inside `jyc monitor`
  and serves a WebSocket server for `jyc dashboard` clients. Press `c` in the
  dashboard to open an interactive chat pane with pattern selection, real-time
  messaging, and multi-client broadcast via `tokio::sync::broadcast`. (#284,
  #285)

### Changed

- **Unified config hot-reload for patterns and channel lifecycle.** `MessageRouter` now reads patterns dynamically from live `ArcSwap<AppConfig>` on every `route()` call instead of caching a static snapshot at startup. New `ChannelOrchestrator` component manages channel lifecycle: deleted channels are gracefully cancelled on reload, new channels are detected with a warning. Pattern additions/modifications take effect immediately without restart. `InspectContext` fields (`topic_managers`, `channels`, `workspace_dirs`) use `ArcSwap` for dynamic updates. (#338, #339)

- **Chat pane edit diff coloring and write tool multi-line display.** Edit diffs in the AI progress area now use distinct colors: old string lines in gray (`Color::Gray`), new string lines in yellow with italic. Write tool output renders as multi-line content (with `+` prefix, capped at 20 lines) for better readability. Activity pane display remains unchanged. (#340)

- **WebSocket channel now supports multiple topics per channel.** Topic names
  are derived from the client's `topic` field in WebSocket messages (e.g.,
  `{"type":"message","topic":"general","text":"hello"}`). When `topic` is
  non-empty, it is used as the topic name; when empty, it falls back to the
  channel name for backward compatibility. This enables separate conversation
  contexts and workspace directories for different topics within the same
  websocket channel.

### Removed

- **Local TUI channel and `jyc local` CLI command.** The standalone `local`
  channel and `jyc local` subcommand are removed entirely. Users should migrate
  to the `websocket` channel type and use `jyc dashboard` for terminal-based
  interaction. (#284, #285)

## [0.3.11] - 2026-06-21

### Added

- **WeCom Bot outbound attachment upload.** The `wecom_bot` channel now
  supports uploading and sending attachments in replies. Files are uploaded
  over the existing WebSocket using `aibot_upload_media_init`,
  `aibot_upload_media_chunk`, and `aibot_upload_media_finish`, then sent as
  separate `file`, `image`, `voice`, or `video` messages. Common document
  types (pdf, xlsx, csv, ppt, doc, etc.) are supported as file messages.
  Validation reuses the generic `OutboundAttachmentConfig`, consistent with
  Feishu and email. (#257)

## [0.3.10] - 2026-06-17

### Added

- **MCP SendMessage Tool (`jyc_send_message`).** New MCP tool for sending
  proactive out-of-topic messages via the pre-warmed outbound adapter.
  Accepts `recipient`, `subject` (optional), and `message` parameters.
  Recipient format is channel-specific (e.g. `wecomkf:{open_kfid}:{external_userid}`).
  ToolContext carries an optional `outbound` adapter reference injected at
  registry build time. (#242)

- **Flexible tool exclusion at channel and pattern level.** New config fields
  `disabled_tools` and `disabled_mcp_servers` on both `ChannelConfig` and
  `ChannelPattern`. `disabled_tools` removes any tool by registration name
  (built-in, bridge, or external MCP). `disabled_mcp_servers` skips entire
  MCP servers by name during tool loading. Both fields support叠加
  (channel-level + pattern-level merged). `disabled_builtin_tools` is retained
  as a backward-compatible alias merged into `disabled_tools`. (#243)

- **WeCom Bot image and file download support.** The `wecom_bot` inbound adapter
  now downloads and decrypts image attachments (via `image`, `mixed`, and `file`
  msgtypes) using the per-message `aeskey` delivered in WebSocket callbacks.
  Downloads use AES-256-CBC decryption with PKCS#7 padding, MIME type detection
  from magic bytes, and the existing attachment storage pipeline. No config
  changes required. (#245, #246)

- **Per-MCP-tool exclusion by server.** `disabled_tools` now supports
  `server_name/tool_name` format (e.g. `jin_public_mcp/product_list`) to
  precisely target tools from specific MCP servers before registration.
  Built-in and bridge tools continue to use plain names. This enables
  fine-grained control when multiple MCP servers expose the same tool name. (#244)

- **Slim Docker image variant.** New `slim` build target produces a minimal
  runtime image (~150-250 MB) with only essential tools (curl, jq, ripgrep,
  python3, libssl3). The `full` target extends `slim` with dev tools (git,
  gh, Rust, build-essential, pandoc). Removed unused `libprotobuf-dev` and
  dropped Node.js / oauth2-forwarder from the image. The `production` target
  remains as a backward-compatible alias of `full`. (#247)

- **Per-channel and per-pattern skill filtering.** New config fields `skills`
  and `disabled_skills` on both `ChannelConfig` and `ChannelPattern`. `skills`
  acts as a whitelist: when set, only skills whose names appear in the list
  are loaded. `disabled_skills` removes specific skills by name. Resolution
  chain: pattern-level `skills` > channel-level `skills` > all discovered
  skills. `disabled_skills` are merged across levels (channel + pattern).
  Includes validation and unit tests. (#248)

- **MCP server tool whitelist.** `McpServerConfig` now supports `enabled_tools`
  field. When set, only tools whose names appear in the list are loaded from
  that MCP server; all others are silently ignored. This is a more ergonomic
  alternative to listing many tools in `disabled_tools` when only a subset is
  needed. (#244)

- **Gitee channel support.** New channel type `gitee` for multi-agent workflows
  on Gitee issues and Pull Requests. Includes REST API v5 client, polling
  inbound adapter, comment-posting outbound adapter, and planner/developer/
  reviewer templates. Supports label-based routing, self-loop prevention,
  and close detection. CI status polling via Gitee Go build status API.

- **Channel-level MCP configuration.** `ChannelConfig` now supports `mcps`
  field, allowing MCP servers to be configured per-channel. Resolution
  priority: pattern-level → channel-level → global. (#241)

- **WeCom KF (Customer Service) channel.** New channel type `wecomkf` supporting
  inbound messages via `kf_msg_or_event` event notifications and `kf/sync_msg`
  API pull, outbound messages via `kf/send_msg` API. Includes cursor-based
  incremental sync with optional file persistence, in-memory message dedup
  (10K-entry FIFO cap), and shared token/wehbook infrastructure with the
  existing wecom channel. One topic per customer per KF account.
  (#229, #230)

- **WeCom (企业微信) Bot channel.** New channel type `wecom` supporting inbound
  messages via shared axum HTTP server and outbound messages via Bot webhook
  URL. Includes AES-256-CBC message decryption, SHA1 signature verification,
  and auto-detection of text/markdown message types. (#225, #226)

### Changed

- **WeCom channel: migrated from Bot webhook to external contact API.** The
  outbound adapter now uses `corp_id` + `corp_secret` for access_token-based
  authentication via `GET /cgi-bin/gettoken`, then sends messages via
  `POST /cgi-bin/externalcontact/message/send` with `chat_id` routing.
  The `webhook_url` configuration field is replaced by `corp_secret`. (#228)

- **`OutboundAdapter::send_alert` renamed to `send_message`.** All channel
  implementations (email, feishu, github, wechat, wecom, wecomkf, test mock)
  updated. The method sends proactive messages to arbitrary recipients;
  the old name implied alert-only usage which was overly restrictive. (#242)

### Fixed

- **Fixed WeCom Bot `mixed` message parsing.** The `MixedContent` and
  `MixedItem` structs now match the actual API format: `msg_item` array
  (was `items`) with nested `text`/`image` content objects (was flat
  `type`/`content`/`url`/`aeskey` fields). This fixes the bug where mixed
  (text+image) messages were parsed as empty, causing "no message body"
  and skipping AI processing. (#246)

- **Feishu channel: removed pre-route attachment save that prevented template
  initialization.** The `on_message` callback was saving attachments before
  message routing, which created the topic directory via `create_dir_all`
  before the worker could run `initialize_topic_from_template`. The guard
  condition `topic_path.exists()` returned true (directory existed from
  attachment save), but no `.jyc/template` marker existed, so template files
  (AGENTS.md, domain-knowledge.md) were silently skipped. Also eliminated
  duplicate attachment saves (pre-route + post-route). (#250)

- **`initialize_topic_from_template` guard changed from `topic_path.exists()`
  to `.jyc/` directory existence.** The previous check was too broad: any
  pre-existing directory (e.g., from attachment storage) was interpreted as
  "topic already initialized". Now only the `.jyc/` metadata directory is
  used as the initialization sentinel, making template init resilient to
  out-of-band directory creation. (#250)

### Deprecated

- **`jyc_vision` MCP tool.** Models with `supports_images = true` now
  load images natively via `read_image` and inbound auto-injection. The
  MCP tool stays in tree for one release to support text-only models;
  removal is planned in a subsequent release.

## [0.3.9] - 2026-05-28

### Fixed

- **`MessageAttachment.saved_path` now propagates from the post-route
  attachment saver back to the agent.** `topic_manager::process_message`
  was calling `save_attachments_to_dir(&mut message.clone(), ...)`,
  mutating a temporary clone that was immediately dropped. The original
  `item.message` kept `saved_path: None`, so when
  `build_user_blocks` checked the field it logged
  `"Image attachment has no saved_path; skipping injection"` and the
  multimodal payload was silently dropped — visible on the deployed
  WeChat channel where the attachment was correctly saved on disk yet
  never reached the model. `process_message` now takes
  `item: &mut QueueItem` and saves into the original message; the
  `agent.process()` dispatch downstream sees the populated `saved_path`
  on every attachment.

- **Attachment-only messages no longer drop silently before reaching the
  agent.** The topic-manager body-empty short-circuit
  (`No message body, stopping (no AI)`) bypasses when
  `message.attachments` is non-empty. This was masking image-only WeChat
  messages where OpenILink delivers `[image]` as a placeholder body that
  the channel correctly strips, leaving an empty body — without this fix
  the agent never ran for image-only inputs even with
  `inject_inbound_images = true`. Also applies to PDF / docx /
  attachment-only messages on any channel: the agent now runs and can
  process them via `read` / `bash` / `read_image` tools.

- **`build_user_prompt_text` falls back to a content-aware placeholder
  when the body is empty but attachments are present.** The model now
  sees `[no text body — N image attachment(s) follow]` (or a mixed
  variant) instead of an opaque `[no text content]`, so it has explicit
  context for what the multimodal blocks downstream represent.

- **Dashboard `list_topics()` now resolves the effective model.**
  Previously the inspect API only read `.jyc/model-override`, so topics
  with a pattern-level or channel-level model showed `null` / "(default)".
  It now mirrors the inference priority chain: override file > pattern
  model > channel model > global model.

- **Channel-scope pattern lookup for model override.**
  `JycAgentService` receives a flattened `all_patterns` list across all
  channels; the lookup now matches both pattern name *and* channel to
  avoid collisions when two channels define a pattern with the same name.

- **GitHub Reviewer pattern priority.** In `GithubMatcher::match_message`,
  patterns whose `role` is "Reviewer" are evaluated before all other
  patterns (stable sort, TOML order preserved within each bucket). This
  ensures a PR labelled `ready-for-review` routes to the reviewer even
  when leftover developer labels are still present.

- **GitHub per-cycle dedup no longer blocks CI failure events.**
  The `triggered_in_cycle` HashSet previously deduplicated by issue
  number, which also dropped follow-up CI status-change events for the
  same PR. CI failure events now bypass the comment/issue dedup and are
  routed independently.

- **Prevent duplicate triggers per issue per poll cycle in GitHub channel.**
  Comments and issue-open events for the same issue number within a single
  poll cycle are deduplicated so the agent is not invoked twice for the
  same update.

- **Agent retry logic.**
  - Retry on `diag-success` transport errors (connection pool staleness).
  - Retry transient SSE stream errors instead of failing the topic.
  - Capture and log upstream response body on Anthropic 4xx errors for
    easier debugging.

- **Inspect activity map scoped by `(channel, topic_name)`.**
  Previously two channels with identically-named topics collided in the
  global activity map. The key is now a tuple so each channel's topic
  has independent activity tracking.

- **WeChat fixes.**
  - Parse OpenILink Bridge nested envelope schema correctly.
  - Enable `native-tls` feature on `tokio-tungstenite` for `wss://` support.
  - Include `to` field in outbound WebSocket send frames.
  - Align attachment topic name with router; skip agent for placeholder
    bodies (`[image]`, `[file]`, etc.).
  - Remove duplicate pre-route attachment save.

### Added

- **WeChat channel support.** New `wechat` channel type connects via OpenILink
  Bridge WebSocket. Supports single-bot, single-topic model with auto-reconnect
  and exponential backoff. Outbound sends via the same WebSocket connection.

- **WeChat inbound attachments.** Images, files, voice, and video attachments
  are received via OpenILink Bridge, saved to the topic directory, and
  forwarded to the agent with `MessageAttachment.saved_path` populated.
  Placeholder bodies (`[image]`, `[file]`, etc.) are stripped so the agent
  processes attachment-only messages correctly.

- **Per-pattern and per-channel model configuration.** `model` and `small_model`
  can now be overridden at three levels (highest to lowest):
  `.jyc/model-override` file > pattern config > channel config > global
  `[agent]` config. The inference engine (`JycAgentService::process`) resolves
  the effective model using this priority chain; the inspect dashboard now
  mirrors the same resolution in `list_topics()`.

- **Per-pattern MCP server configuration.** `[[channels.<name>.patterns]]` entries
  can declare `mcps = ["server-a", "server-b"]` to load only those MCP servers
  for matching topics. Set `mcps = []` to disable all MCP tools for a pattern.
  Falls back to global `[[mcps]]` when omitted.

- **Per-pattern built-in tool disable.** `disabled_builtin_tools = ["bash",
  "write"]` on a pattern removes those tools from the agent's tool registry
  before the loop starts. Combined with `mcps = []`, this enables fully
  restricted sandboxes for sensitive patterns.

- **Tool boundary checks.** `write`, `edit`, `glob`, `grep`, `bash`, and
  `read_image` now validate that paths stay within the working directory and
  configured read roots. Violations return a descriptive error to the model
  instead of silently succeeding or failing with a generic message.

- **CI action with coverage.** GitHub Actions workflow (`.github/workflows/ci.yml`)
  runs `cargo test` with LLVM coverage reporting on every push and PR.

- **Native image input into the agent loop.** Two complementary entry points
  let vision-capable models receive images directly as prompt content blocks
  instead of via the out-of-band `jyc_vision` MCP detour:

  1. **Inbound auto-injection.** Set
     `inject_inbound_images = true` on a `[[channels.<name>.patterns]]` entry.
     When the active model also has `supports_images = true`, every `image/*`
     attachment on a matching message is base64-encoded from its
     `MessageAttachment.saved_path` and appended as `ContentBlock::Image`
     to the first user turn. Honors the existing
     `[attachments.inbound].save_path` configuration without re-implementing
     path resolution (uses
     `jyc_core::attachment_storage::resolve_attachment_save_dir`). Default
     for the new flag: `false`.

  2. **Agent-driven `read_image` built-in tool.** Registered automatically
     whenever the active provider has `supports_images() == true`. The
     model can call it with either `path` (an absolute local path,
     boundary-checked against the working directory and the configured
     attachments root) or `url` (http/https, fetched and base64-inlined).
     Supported MIME: png, jpeg, gif, webp. 10 MB inner cap.

  Wire format: Anthropic emits `{"type":"image","source":{...}}` blocks;
  OpenAI-compatible providers emit `{"type":"image_url","image_url":{"url":...}}`
  parts and switch the user message to array-content form only when at
  least one image is present (preserves legacy string-content form for
  text-only requests, which several OpenAI-compat servers require).

  Tools that load images mid-loop push onto a side-channel queue; the
  agent loop drains it after each tool batch and emits a synthetic
  user-role turn carrying the image blocks. This avoids cramming base64
  into `tool_result` content (unsupported by most OpenAI-compat servers
  for `role: "tool"`).

- **Per-model `supports_images` flag** in `[agent.providers.<name>]` and
  `[agent.providers.<name>.models.<id>]`. Model-level overrides
  provider-level. Default `false`. Gates everything described above.

- **`read_image` dual-mode with vision model fallback.** When the active
  model does not have `supports_images = true`, the `read_image` tool
  falls back to a vision-capable provider (e.g. DeepSeek-VL) for OCR /
  description, then returns the extracted text to the text-only model.
  This lets non-vision models still reason about images without requiring
  a separate `jyc_vision` MCP round-trip.

### Changed

- **`Provider::format_user_message`** now takes `&[ContentBlock]` instead
  of `&str`. Existing call sites that passed plain text wrap the string
  in a single `ContentBlock::Text { text }` — no behavior change for
  text-only flows.

- **`AgentLoopConfig`** field rename: `user_message: &str` →
  `user_blocks: Vec<ContentBlock>`. Same semantics for text-only callers
  (one text block); enables multimodal first turns.

- **`JycAgentService::new`** signature gains two parameters:
  `patterns: Vec<ChannelPattern>` (so the service can look up per-pattern
  agent flags by `InboundMessage.matched_pattern`) and
  `global_inbound_attachments: Option<InboundAttachmentConfig>` (so the
  agent can derive the additional read-roots that `read_image` is
  permitted to access). The CLI flattens
  `config.channels[*].patterns` and forwards the global
  `[attachments.inbound]` block.

- **`ToolContext`** carries two new fields used by `read_image`:
  `additional_read_roots: Vec<PathBuf>` (boundary widening) and
  `pending_images: Mutex<Vec<ImageSource>>` (the side-channel queue).
  Existing tools are unaffected.

- **Refactored**
  `jyc_core::attachment_storage::resolve_attachment_save_dir`. Extracted
  the previously duplicated path-resolution logic from
  `save_attachments_to_dir` and `save_attachments_to_topic_directory`
  into one public helper. Both call sites now share it; the agent code
  reuses it to keep its boundary rule in lockstep with the channel
  adapters' save-location rule.

### Deprecated

- **`jyc_vision` MCP tool.** Models with `supports_images = true` now
  load images natively via `read_image` and inbound auto-injection. The
  MCP tool stays in tree for one release to support text-only models;
  removal is planned in a subsequent release.

## [0.3.8] - 2026-05-22

### Added

- **`[agent].small_model` configuration option.** Lets you assign a smaller /
  faster / cheaper model to ancillary LLM work, distinct from the main task
  model. Currently used by:

  1. **Cycle-boundary progress summary** in `agent_loop` — the call that
     produces the user-visible progress reply every `max_iterations` rounds.
  2. **Between-message context-reset summary** in `session::summarize_context`
     — the call that compacts `agent-context.json` when input tokens cross
     95% of the model's context window.

  Both code paths are isolated from the main loop's structured `raw_context`:
  they render the prior conversation to a plain-text transcript, send it as
  a single user message with a "summarize this" system prompt (no tools, no
  prior assistant turns, no `reasoning_content` round-trip), and consume the
  reply text. The main loop's `raw_context` is never touched.

  Falls back to the main `model` if `small_model` is unset, or if the small
  provider fails to construct (logged as a warning, the agent continues).

  Example:

  ```toml
  [agent]
  model = "deepseek/deepseek-v4-pro"
  small_model = "deepseek/deepseek-v4-flash"
  ```

  Cross-provider works the same way — `small_model = "ark/glm-5.1"` is
  resolved through `[agent.providers.ark]`.

### Changed

- **`session::summarize_context` now uses an LLM call** when `small_model` (or
  the main model as fallback) is provided. The previous heuristic
  ("keep the last 3 user+assistant text pairs") is preserved as
  `summarize_context_heuristic` and used as a fallback when the LLM call
  fails. The user-triggered `reset_session` (via inspect server / dashboard
  `/reset`) continues to use the heuristic since that path has no provider
  context.

- `session::update_tokens` now takes a `summary_provider: &dyn Provider`
  parameter (caller-supplied so the function stays sync-friendly and
  testable).

- `agent_loop::AgentLoopConfig` gains an `small_provider:
  Option<&dyn Provider>` field. When `None`, the cycle-boundary summary
  reuses the main `provider`.

- `jyc_types::AgentConfig` gains a `small_model: Option<String>` field
  (channel-agnostic, lives in the [agent] section).

### Tests

- 3 new tests for `AgentConfig.small_model` (default `None`, TOML deserialize
  with and without the field).
- Existing `update_tokens` tests updated to pass a stub provider that panics
  if invoked (the auto-reset threshold is not crossed in those tests, so the
  stub is never called).
- All 425+ workspace tests pass.

## [0.3.7] - 2026-05-22

### Fixed

- **HTTP 400 from DeepSeek `thinking = enabled` mode** at the agent-loop cycle
  boundary, with response body
  `"The reasoning_content in the thinking mode must be passed back to the API."`.
  v0.3.6 introduced two changes that violated DeepSeek's contract: (a)
  stripping `reasoning_content` from non-final assistant turns, which broke
  the model's requirement that every assistant turn it produced be replayed
  with reasoning_content intact; and (b) injecting a synthetic assistant
  tool-call into `raw_context` at the cycle boundary, which created an
  assistant turn the model never produced (and thus had no reasoning_content
  to round-trip).

  v0.3.7 reverts both. The cycle-boundary block is rebuilt around the
  principle that the main loop's `raw_context` must NEVER be touched by
  ancillary work — the LLM's view of its own conversation is the source of
  truth.

### Changed

- **`filter_valid_messages` no longer strips `reasoning_content`.** The
  v0.3.6 stripping logic is fully reverted. Every assistant turn that
  carries `reasoning_content` keeps it on every subsequent request.
  Regression test pinned in `unit_tests.rs::filter_valid_messages::preserves_reasoning_content_on_all_assistant_turns`.

- **Cycle-boundary progress-summary call is now isolated from the main loop.**
  At the boundary the loop:

  1. Renders `raw_context` to a plain-text transcript (lossy by design, used
     only for summarization).
  2. Issues a separate LLM completion with the joined transcript as a single
     user message and a "summarize this" system prompt — no tools, no prior
     assistant turns, no `reasoning_content` round-trip.
  3. Posts the resulting summary text via the reply tool (GitHub comment /
     IM message) for user-visible progress.
  4. Resets `iter_in_cycle` to 0 and continues. **`raw_context` is NOT
     mutated.** No synthetic assistant turn is appended; no compaction is
     applied. The next iteration replays the model's own last assistant
     turn (with its real `reasoning_content`) followed by its `tool_result`,
     and the model continues from where it left off.

  Helper `agent_loop::generate_summary_from_joined_history` and
  `agent_loop::render_raw_context_as_text` added (crate-internal).

### Removed

- `agent_loop::summarize_raw_context_in_place` (introduced in v0.3.6).
  The "compact `raw_context` in place at cycle boundary" approach is gone.
  Per the new design the main loop's context stays untouched; bounded
  request size is achieved purely by the model continuing from a real
  conversation state.

### Notes

- The HTTP-body diagnostic in `openai_compat::complete_raw` (Piece 1 of
  v0.3.6) is retained — it produced the diagnostic body that pinpointed the
  v0.3.6 regression, and remains useful for any future provider 4xx errors.
- The default `max_iterations` of 500 (v0.3.6) is retained.

## [0.3.6] - 2026-05-22

### Fixed

- **Agent loop crashed at cycle boundary with `SSE stream error: Invalid status
  code: 400 Bad Request`** on long-running developer agent tasks (~200+
  iterations). Three independent issues conspired:

  1. The cycle-boundary block in `agent_loop.rs` appended a synthetic progress
     reply to `raw_context` and resent the entire context without trimming.
     The next request grew unboundedly across cycles.
  2. `filter_valid_messages` did not strip DeepSeek's `reasoning_content` from
     prior assistant turns. Replaying many turns of `reasoning_content` is
     not part of the standard chat-completions input schema and triggered the
     400 validation error around iteration 200 even though the absolute token
     count was nowhere near the model's context window.
  3. The OpenAI-compatible provider's SSE wrapper discarded the HTTP response
     body on errors, surfacing only `Invalid status code: 400 Bad Request`
     with no diagnostic detail.

### Added

- **In-loop context summarization at every cycle boundary.** When
  `iter_in_cycle` reaches `max_iterations`, `raw_context` is now compacted in
  place: the first user message (task anchor) and the synthetic
  assistant-tool-call / tool-result pair are preserved, plus a synthetic
  `<jyc-cycle-summary>` user message carrying the progress text. Output is a
  4-message context regardless of how long the previous cycle was. Crate-
  internal helper `agent_loop::summarize_raw_context_in_place`.

- **HTTP body diagnostics on provider errors.** When the OpenAI-compatible SSE
  stream fails at the transport layer (e.g., HTTP 400/429/5xx), jyc now issues
  a one-shot diagnostic POST with the same payload, captures the response
  body, and includes the status and body in the propagated error. Adds latency
  only on already-broken requests; happy path is unchanged.

### Changed

- **Default `max_iterations` raised from 200 to 500** per cycle. Higher
  iteration counts are now safe because in-loop summarization keeps the
  request size bounded regardless of cycle length.

- **`filter_valid_messages` strips `reasoning_content` from all assistant
  messages except the most recent one.** The latest assistant turn keeps
  `reasoning_content` so the model can see its own latest thinking; all
  earlier turns are stripped before sending. Channel-agnostic and applied
  regardless of provider, since `reasoning_content` is only emitted by
  reasoner models in the first place.

## [0.3.5] - 2026-05-22

### Added

- **Per-pattern `topic_prefix` configuration** for GitHub channels. Each
  `[[channels.<ch>.patterns]]` entry can now declare an explicit
  `topic_prefix`, which is combined with the GitHub number as
  `{prefix}-{N}` to derive the topic name. This lets two patterns that match
  the same issue/PR (e.g., split by labels) live in distinct workspace
  directories so each can carry its own template / `AGENTS.md` without
  collision. Omit `topic_prefix` to keep the default behavior (`issue-{N}`
  for issue events, `pr-{N}` for PR events).

- **Template-mismatch guard.** When a message is routed to an existing topic
  whose recorded template differs from the matched pattern's template, jyc
  now refuses to dispatch the message and logs a `TemplateMismatch` error
  along with a `template_mismatch` processing-error metric, instead of
  silently running the agent with the wrong `AGENTS.md`.

### Changed

- GitHub close-event handling now enumerates the workspace and closes every
  topic directory whose name matches `{anything}-{N}` for the closed
  GitHub identity, rather than hardcoding the `pr-{N}` and `review-pr-{N}`
  prefixes. This makes cleanup correct for any user-defined `topic_prefix`.

### Deprecated

- The implicit fallback that routes a pattern named `reviewer` (with no
  `topic_prefix`) to `review-pr-{N}` is deprecated. Existing deployments
  continue to work but log a deprecation warning. New configs should declare
  `topic_prefix = "review-pr"` explicitly. The implicit fallback will be
  removed in a future release.

### Breaking

- None in this release. The hardcoded `pattern_name == "reviewer"` topic-name
  special case has been replaced with a configurable `topic_prefix` mechanism,
  but a backwards-compatible deprecation fallback preserves the legacy
  `review-pr-{N}` topic name for unmigrated configs.

  Recommended migration:

  ```toml
  [[channels.my_repo.patterns]]
  name = "reviewer"
  template = "github-reviewer"
  topic_prefix = "review-pr"   # ← add this line to silence the warning
  ```

## [0.3.4] - 2026-05-20

### Added

- **Infinite cycle agent loop with progress checkpoints** — the agent loop is no longer
  a bounded `for 0..max_iter` over tool calls. It now wraps an inner per-cycle counter
  in an unbounded outer loop. When `iter_in_cycle` reaches `max_iterations`, the agent:
  1. Issues a separate LLM completion to summarize progress so far
  2. Synthetically invokes the `jyc_reply_reply_message` tool with that summary
  3. Appends the synthetic assistant + tool_result to `raw_context`
  4. Resets `iter_in_cycle` to 0 and continues working
  No upper bound on cycles; the existing 95 % context-window auto-reset still applies.
  If the progress-summary LLM call fails, a static template is used as fallback.

### Changed

- **Default `max_iterations`** raised from 100 to 200 (per cycle).
- **Agent system prompt** — removed the "Iteration Budget" section. Replaced with a
  brief note that long-running tasks should send periodic progress replies as
  checkpoints; the agent no longer needs to ration tool calls against a hard limit.

### Removed

- **Legacy heartbeat infrastructure**, fully superseded by the cycle loop:
  - `HeartbeatConfig` struct and `[heartbeat]` config section
  - `AppConfig.heartbeat` field
  - `ChannelConfig.heartbeat_template` per-channel field
  - `OutboundAdapter::send_heartbeat()` trait method and all impls
    (email, feishu, github, plus the test mock)
  - `TopicManager::event_listener_with_heartbeat` (~170 LOC) and the
    per-worker `tokio::sync::watch` channel that fed it
  - `TopicEvent::Heartbeat` enum variant and its handlers in `jyc-inspect`
  - Dead heartbeat constants in `jyc-utils::constants`
  - Heartbeat validation block in `jyc-types::validation`
  - Documentation in DESIGN.md, FEISHU.md, README.md
- **Kept** (unrelated, network-level keepalives):
  - WebSocket `heartbeat_interval_secs` in `feishu_config`
  - in-process agent SSE `server.heartbeat` ignore-rule (it's a TCP-level keepalive)

### Migration

If your `config.toml` contains a `[heartbeat]` section or any channel uses
`heartbeat_template = "..."`, those fields are now silently ignored. Remove
them to keep configs clean. No behavior change is needed — the cycle loop
will produce progress replies automatically for long-running tasks.

## [0.3.3] - 2026-05-20

### Fixed

- **Event ordering bug** — `SimpleThreadEventBus::forward_to_subscribers` was using
  `tokio::spawn` per event, which delivered events to the dashboard out of order.
  This caused the dashboard to show `Completed` BEFORE `ToolStarted` for the
  reply tool, making it look like the agent replied without finishing.
  Fixed by sending events sequentially (awaited) — preserves order, mpsc channel
  capacity (10) provides backpressure if a subscriber is slow.
- Added 3 regression tests for event bus ordering and multi-subscriber delivery

## [0.3.2] - 2026-05-20

### Added

- **Configurable max iterations** (`[agent].max_iterations`, default 100) — controls how many
  tool-call loops the agent can run per message before exiting (was hardcoded at 50)
- **Graceful fallback reply on max-iterations** — when the agent hits its iteration limit,
  it now sends a partial reply explaining the limit was reached, instead of failing silently
- **System prompt iteration budget guidance** — agent is told its iteration budget and
  encouraged to send partial replies for complex tasks rather than exhausting the budget

### Fixed

- Templates: rephrased "STOP SILENTLY" to action-only language (`end your turn`,
  `Do NOT call the reply tool`, `Do NOT produce any text output`) — Claude was verbalizing
  the meta-instruction by replying with text like "Stopping silently with no reply."
  DeepSeek/GLM didn't have this issue; specific to Claude's instruction-following.
- Service-account / system-user comments now explicitly listed as skip-and-end-turn cases

### Documentation

- DESIGN.md: documented runtime skills injection (lazy loading, YAML frontmatter,
  `format_skills_section`, `.jyc/skills.json` persistence)

## [0.3.1] - 2026-05-19

### Added

- Multi-path skill discovery for `jyc-agent` (#182) — 9-path priority order
  (system → repo → topic-local), with topic-local `.jyc/skills/` overriding all others
- Dashboard displays loaded skills per topic (#184)

### Fixed

- `read` tool now follows symlinks within the working directory — JYC's `repo_group`
  feature uses `repo/` symlinks, which were being rejected by the canonicalize check

## [0.3.0] - 2026-05-16

### Added

- **In-process AI agent (`jyc-agent` crate)** — replaces external agent service dependency
  - Native Anthropic Messages API provider (streaming via SSE)
  - OpenAI-compatible provider (DeepSeek, GPT, Groq, etc.)
  - 7 built-in tools: bash, read, write, edit, glob, grep, webfetch
  - MCP reply_message bridge (in-process signal files, no subprocess)
  - Core agent loop: prompt → tool_call → execute → repeat
  - Flexible `params` config for provider/model API parameters (thinking, temperature, etc.)
  - Per-model `context_window` configuration
- **Raw context persistence** (`agent-context.json`) — stores provider-formatted conversation
  exactly as sent/received, preserving provider-specific fields (DeepSeek reasoning_content, etc.)
- **Session management** — token tracking, auto-reset at 95% context window, summarize-on-reset
- **Event bus integration** — agent publishes ProcessingStarted/Completed, ToolStarted/ToolCompleted
  to dashboard ActivityTracker
- **Workspace crate structure** — project refactored into 8 workspace crates for faster incremental builds

### Changed

- Default agent mode changed from `"agent"` to `"agent"`
- Config: `model` and `system_prompt` moved from `[agent.agent]` to `[agent]` directly
- Config: provider definitions in `[agent.providers.*]` with per-model settings
- `read_input_tokens()` unified — works for both old in-process agent and new agent session files
- Token display: stores latest input_tokens (not accumulated) since full context is sent each turn

### Removed

- **in-process agent dependency** — no longer requires external agent service process
  - Removed `crates/jyc-services/src/agent/` module (6 files)
  - Removed in-process agent installation from Dockerfile
  - Removed in-process agent volumes from docker-compose.yml
  - Removed `model_handler.rs` (queried in-process agent's provider endpoint)
  - Removed `reqwest-eventsource` dependency from jyc-services
- Config: `mode = "agent"` no longer supported (use `mode = "agent"`)
- Config: `AgentConfig` struct removed (fields moved to `AgentConfig` top level)

### Fixed

- DeepSeek v4-pro SSE streaming (uses EventSource instead of raw bytes_stream)
- DeepSeek reasoning_content properly captured and round-tripped in raw context
- Assistant messages with null content filtered on save/load/send (prevents 400 errors)
- Empty response detection and logging for debugging provider issues

## [0.2.1] - 2026-04-26

### Added

- High-level planner with feature-plan label routing (#102)
- Template-driven MCP configuration with `--mcps` override (#104)
- OAuth2-forwarder integration for browser-based OAuth2 in Docker/Podman containers (#107)
- Idle topic directory auto-cleanup (#110)
- Live config reload via dashboard TUI — ArcSwap, reload_config protocol, R keybinding (#103)
- `repo_group` for shared repo among GitHub topics (#122)
- Replace NodeSource with fnm, pre-install Node 22

### Fixed

- Worker idle permit release and remove idle_cleanup (#125)
- Interrupt SSE stream when topic is closed (#118)
- Idle cleanup per-topic skip flag (#113)
- Docker build fixes — protobuf-compiler, Rust toolchain, unzip dependencies
- Retry /provider API up to 3 times for model context limit lookup
- Wait for the agent API to be ready after server starts listening
- Dashboard TUI status bar consolidation and keybinding hints
- Add mandatory test gate and strengthen current-message priority
- Remove invalid pub qualifiers from enum variant fields

### Changed

- Replace `Arc<AppConfig>` with `ArcSwap` for live config reload support
- Extend inspect protocol with `reload_config` command and typed responses

## [0.3.1] - 2026-05-18

### Added
- **Multi-path skill discovery** (#182) — Skills are now loaded from 9 priority paths (`.jyc/skills/` → `.claude/skills/` → `.agent/skills/` → user home → system), with higher-priority paths overriding same-named skills from lower-priority paths. Supports YAML frontmatter with block scalar descriptions.
- **Dashboard skills display** (#184) — Dashboard TUI detail panel now shows the list of skills loaded for each topic.

### Fixed
- **Read tool symlink support** — `read` tool now follows symlinks within the working directory (previously rejected symlinks as "outside working directory").

### Changed
- **40 unit tests** added for jyc-agent regression coverage (skill parsing, discovery, and formatting).

## [0.2.1] - 2026-04-26

### Added

- **Multi-agent workflow refactor** — Developer agent is now a persistent reactive agent (#59), label-based reviewer trigger (#74), removed trigger_mode (#76), unconditional hand-off (#78, #85)
- **Pattern mode triggers issue/PR directly** — No longer relies solely on comments (#69)
- **AND/OR label logic** — LabelRule supports CNF nested array boolean combinations (#83)
- **Dashboard TUI shows topic last active time** (#81)
- **deploy-templates supports --as flag** (#87), integrated into jyc CLI (#94)
- **Comment filtering for closed issues/PRs** (#89)
- **Main branch protection** (#65)
- **coding-principles skill integration** (#61)
- **Template-driven MCP configuration** — Named MCP servers defined in `[[mcps]]` in `config.toml`, referenced by templates via `mcps` list in `templates.toml`. Vision MCP is now a template-configurable MCP instead of hardcoded (#104)

### Fixed

- **Planner empty commit handling** (#93)
- **SSE loop exit fix** (#89)
- **Activity panel shows AI thinking and tool errors** (#78)
- **Pattern mode self-loop protection** (#69)

### Changed

- **README documentation updates** (#96, #98)
- **Developer agent template simplification** (#59)
- **Removed config-level model-override** (#67) — per-topic `.jyc/model-override` still supported via `/model` command
- **Removed @j:role mentions, use label-based handover instead** (#76)
- **Dockerfile optimization for dummy main caching** (#57)

### Removed

- **TriggerMode enum and trigger_mode field** (#76)

## [0.1.11] - 2026-04-20

### Added

**Inspect Server + TUI Dashboard** — Live monitoring of running jyc processes
- `[inspect]` config section: `enabled`, `bind` (default `127.0.0.1:9876`)
- TCP-based JSON line protocol for querying runtime state
- `jyc dashboard` CLI command with ratatui TUI
- Panels: channels bar, topics table (selectable), detail panel, status bar
- Shows: topic name, channel, pattern, status, model, mode, token usage, uptime, version
- Key bindings: q/Esc quit, Up/Down/j/k select, r refresh
- Auto-polls every 500ms, handles disconnected state gracefully
- Works across Docker (via `network_mode: host`) and bare metal

**MetricsCollector** — Lightweight replacement for AlertService
- Accumulates health stats (messages received/processed, errors, per-topic) in `Arc<Mutex<>>`
- Queryable by the inspect server — no email dependency
- `MetricsHandle` for components to report events (same API as old `AppLogger`)

### Removed

**AlertService** — Removed email-based alerting
- Startup notification email removed
- Error digest email removed
- Health report email removed
- `[alerting]` and `[alerting.health_check]` config sections deprecated (ignored if present)
- `AlertingConfig`, `HealthCheckConfig` structs kept for backward compatibility but unused

### Changed

**TopicManager** — Added introspection for dashboard
- `channel_name` and `workspace_dir` fields for identifying channel ownership
- `list_topics()` method: returns topic info by reading `.jyc/` state files
- `channel_name()`, `max_concurrent()` accessor methods

## [0.1.10] - 2026-04-20

### Added

**GitHub Label-Based Routing** — Auto-label matching for GitHub channel patterns
- Patterns with a `role` field get an implicit routing label: `Developer` → `jyc:develop`, `Reviewer` → `jyc:review`, `Planner` → `jyc:plan`
- Auto-label is combined (OR) with any explicit `labels` in pattern config
- PRs/issues must have the matching label to be routed (labels added by agents during hand-off)

**GitHub Label Change Detection** — Adding labels to existing issues/PRs triggers routing
- When labels are added to an existing issue/PR, the change is detected by comparing against cached labels
- This allows users to add a label (e.g., `jyc:plan`) to an existing issue and have it routed to the planner

**@j:<role> Mention-Driven Routing** — Refactored hand-off mechanism
- Patterns match on `@j:<role>` mentions in comments (e.g., `@j:developer`, `@j:reviewer`, `@j:planner`)
- Replaces earlier `[Role]` prefix filter approach
- Agent templates updated to use `@j:<role>` for hand-offs

**Persistent Comment Tracking** — Re-process edited comments
- Track comment ID + `updated_at` to detect edits
- Edited comments are re-processed through the routing pipeline
- Backward-compatible with old `processed-comments.txt` format

**SQLite Storage for Invoice Processing** — Persistent invoice database
- SQLite database for invoice records
- Enables duplicate checking and query capabilities
- Schema: invoice_number, receipt_date, amount, seller, buyer, status, etc.

**GitHub Enterprise Support** — Configurable API endpoint
- `api_url` config option for GitHub Enterprise instances
- Default: `https://api.github.com` for public GitHub

**Assignee Matching** — Route issues/PRs by assignee
- `assignees` field on `ChannelPattern` for GitHub channel
- Match issues/PRs where any of the specified users are assigned

**Pattern Rule Filtering** — Enforce `github_type`, `labels`, `assignees` rules in routing
- `GithubMatcher::match_message()` now validates all present rules before accepting a pattern match
- All rules use AND logic (all must pass); within each rule, OR logic (any value suffices)
- Case-insensitive matching for labels and assignees
- Patterns that fail rule checks are skipped, allowing fallback to the next matching pattern

**CLI Patterns List Enhancement** — Display all rule fields
- `jyc patterns list` now shows GitHub rules (`github_type`, `labels`, `assignees`), Feishu rules (`mentions`, `keywords`, `chat_name`), `role`, and `template`

**Planner Template: Copy Issue Metadata to PR** — Preserve routing context
- Planner template reads assignees and labels from the source issue
- Copies them to the created PR via `--assignee` and `--label` flags
- Ensures PRs inherit routing context for developer/reviewer pattern matching

**Docker Container Environment Injection** — Propagate `.env` to container
- Added `env_file: .env` directive to `docker-compose.yml`
- Environment variables from `.env` are now injected into the container runtime (previously only used for compose-file interpolation)

**Close Event Detection** — Improved event handling
- Fetch all open issues instead of `list_closed_since` for detecting close events
- Compare cached state to detect actual closes

**Bare Metal Deployment** — Deploy jyc on Ubuntu/Debian servers without Docker
- `deploy-bare-metal.sh` script for automated deployment
- `dotfiles/zsh/` - zsh configuration and environment template
- `dotfiles/agent/agent config` - in-process agent configuration
- `docs/bare-metal-deploy.md` - Deployment guide

**nohup Fallback** — Run on servers without systemd
- Detect systemd user session availability
- Fall back to `nohup` + redirect for process supervision

### Fixed

**Worker Semaphore Permit Release** — Fix resource leak on topic close
- Workers now properly release semaphore permits when closing topics
- Prevents thread pool exhaustion

**GitHub Self-Loop Prevention** — Replaces global `[Role]` prefix filter
- Previously: ALL comments prefixed with `[Planner]`, `[Developer]`, or `[Reviewer]` were globally filtered (invisible to all patterns)
- Now: each pattern only skips comments from its **own** role. `[Developer]` comments are visible to the reviewer pattern, and vice versa
- Enables cross-agent feedback visibility (reviewer feedback triggers developer)

**Developer-Reviewer Handoff** — Improved workflow for requesting changes
- Reviewer template now explicitly triggers `@jyc:developer` when submitting review with request-changes
- Ensures developer is notified when feedback needs to be addressed

**Invoice Processing Fixes**
- Add duplicate invoice check before adding to Excel
- Fix EXCEL.md with template copy step, clarify MONTH variable usage
- Template lookup logic fixes, zero amount handling, template cleanup

**Template Generalization** — Multi-language support
- Templates generalized for multi-language workflows
- Repository name included in trigger messages

**Model Override in Templates** — Per-template model configuration
- Support `model-override` in templates while ignoring `.jyc` elsewhere
- Updated GitHub developer template with model override support

### Changed

**GitHub Agent Templates** — Updated hand-off workflow with routing labels
- Planner: adds `--label "jyc:develop"` when creating PRs
- Developer: adds `jyc:review` label when handing off to reviewer
- Reviewer: adds `jyc:develop` label when requesting changes from developer

**Invoice Summary Export** — Generate summary xlsx
- Summary xlsx with correct naming (`summary_YYYY-MM.xlsx`)
- Template-based generation with proper file handling

**Docker Simplification** — Removed s6-overlay
- Removed s6-overlay from Docker setup
- Simplified entrypoint/CMD in Dockerfile
- Removed jyc-deploy-docker skill (no longer self-bootstrapping)

## [0.1.9] - 2026-04-15

### Added

**Live Message Injection Toggle** — Per-pattern control over sequential processing
- `live_injection` field on `ChannelPattern` (default: true)
- When false, messages queue and process sequentially instead of being injected into active AI session

**Invoice Processing Enhancements**
- Add duplicate invoice check before adding to Excel
- Two-level HTML download with Playwright fallback for invoice processing

### Fixed

**Email Processing**
- Check attachments/ directory first before searching email body for URLs
- Fix byte boundary panic when truncating filenames with Chinese characters

**Invoice Install Script**
- Fix install script: remove set -e, use if/elif for pip fallback
- Fix install script: show pip output, handle --break-system-packages

### Changed

**PDF Extraction**
- Reorder PDF extraction: try text extraction first, fall back to vision MCP
- Handle two-level download: extract real PDF/image URL from intermediate HTML

**Invoice Processing**
- Clarify invoice month folder is based on receipt date, not invoice date

## [0.1.8] - 2026-04-13

### Added

**Invoice Processing Skill** — Automated invoice extraction and bookkeeping
- Invoice processing skill with 7-step workflow (download, extract, Excel, summarize, export)
- Chinese invoice (发票) Excel template with 15 columns (发票号码, 开票日期, 购买方/销售方, etc.)
- Monthly folder organization (`invoice_YYYY-MM/`)
- Summary template (IIT deduction claim form) with category mapping
- Vision tool integration for PDF/image invoice extraction
- pypdf fallback for text-based PDF extraction
- Zip export of monthly invoice folders
- QR code image detection and filtering (skip small images, prefer download URLs)
- `agents.invoice.example.md` template for invoice processing topics

**Topic Name Override** — Fixed topic routing from config
- `topic_name` field on `ChannelPattern` for routing all matching messages to a fixed topic
- Channel-agnostic: works for email, Feishu, and any future channel
- Example: all invoice emails → `invoice-processing` topic regardless of subject

**MCP Question Tool** — Ask users questions and wait for answers
- `ask_user` tool with self-delivery (writes reply.md + signal file)
- Background delivery watcher (`pending_delivery.rs`) delivers messages during SSE stream
- 5-minute polling timeout for user response
- Topic manager routes next message as answer via `question-sent.flag`

**Skills**
- `invoice-processing` — complete invoice workflow with templates
- `plan-solution` — structured implementation planning for plan mode
- `incremental-dev` — small-step iteration with validation
- `pr-review` — read-only PR analysis via gh CLI
- `github-dev` — GitHub issue/PR development workflow (removed with GitHub channel)

**Topic Close** — `/close` command and Feishu disband event
- `/close` command to delete topic directory and clean up state
- Feishu `im.chat.disbanded_v1` event detection for automatic topic cleanup

**Central Path Resolution** — `topic_path.rs` module
- `resolve_workspace()` function for consistent path construction
- 10 end-to-end tests covering email, Feishu, config override, and attachment paths

### Fixed

**Email Parser Simplification**
- Removed quoted history from email replies (reply = text + footer only)
- Fixed forwarded email body extraction (was stripped as "quoted text")
- Removed 600 lines of dead quoted history code (`email_parser.rs`: 1296 → 693 lines)

**Attachment Handling**
- Fixed double-nested attachment directory path (`workspace/channel/workspace/` → `workspace/`)
- Moved attachment saving to after topic routing (correct directory with `topic_name` override)
- Reply delivery moved from `messages/<dir>/reply.md` to `.jyc/reply.md`

**Question Tool Delivery**
- Question tool now self-delivers via reply signal (AI doesn't need two-step flow)
- Background delivery watcher delivers during SSE stream (no 5-min wait)
- SSE handler detects `ask_user` tool completion alongside `reply_message`

**Permissions**
- `external_directory: allow` for topics with symlinks (prevents plan mode sub-agent deadlock)
- Auto-detect symlinks up to 3 levels deep for permission configuration

**Activity Timeouts**
- Both `ACTIVITY_TIMEOUT` and `TOOL_ACTIVITY_TIMEOUT` set to 30 min for thinking models

### Changed

- GitHub channel removed (reverted all GitHub-specific implementation)
- `dev-workflow` skill enhanced with gh CLI instructions and token scope documentation
- `deploy.sh` auto-detects paths from `JYC_BINARY` env var and script directory
- `run-jyc.sh` requires `JYC_BINARY` and `JYC_WORKDIR` environment variables
- `SYSTEMD.md` updated with deployment flow diagram and env var documentation
- SSE stream no longer exits early after reply tool (allows post-reply actions)
- `config_template.toml` consolidated into `config.example.toml`

### Removed

- GitHub channel (`src/channels/github/`, `GITHUB_CHANNEL.md`, `agents.github-dev.example.md`)
- `labels` field from `PatternRules`
- Dead quoted history functions and tests (600 lines)
- Dead `topic_path` functions (unused resolve helpers)
- `messages/` directory references (replaced by `.jyc/` and chat log)

## [0.1.7] - 2026-04-11

### Added

- **Topic Template** — Initialize topics with predefined files and directories
  - Pattern-level template configuration (`template = "name"` in config.toml)
  - Template files copied to topic directory on first message
  - `/template` command to re-apply template to existing topic
  - `copy_template_files` shared utility function

- **chat_name prefix matching** — Feishu chat_name pattern now uses prefix match instead of exact match

### Fixed

- Template initialization order bug (template now initialized before .jyc directory check)
- PR review comments addressed (.unwrap() → .expect(), logged file operation warnings)

## [0.1.6] - 2026-04-10

### Added

- PR Review skill for code review workflow
- Bump version skill for automated version bumping workflow

### Changed

- MCP tool names unified to `jyc_` prefix (`jyc_reply`, `jyc_question`, `jyc_vision`)

## [0.1.5] - 2026-04-09

### Added

**Vision MCP Tool** — Image and visual content analysis
- New `vision_analyze_image` MCP tool for analyzing images, PDFs, screenshots, and videos
- Provider-agnostic: works with Kimi, Volcengine/Ark, OpenAI, or any OpenAI-compatible vision API
- Configuration via `[vision]` section in `config.toml` (api_key, api_url, model)
- Supports local file paths (absolute) and HTTP(S) URLs
- Base64 data URI encoding for API transport
- 300s MCP timeout for large files
- File-based logging to `.jyc/vision-tool.log`
- Hidden CLI subcommand `mcp-vision-tool` (spawned by in-process agent)

**Unified Attachment Configuration** — Channel-agnostic attachment handling
- New `[attachments.inbound]` and `[attachments.outbound]` config sections
- Per-pattern attachment overrides in channel pattern config
- Shared `core/attachment_storage.rs` module (replaces duplicated code in email/Feishu)
- Consistent filename generation with extension preservation and 50-char truncation
- Path traversal protection at ingestion time (strips directory components, control chars)

**System Prompt Enhancements**
- Tool usage instructions: use `webfetch` for web searches (not curl/wget)
- Resilience instructions: try multiple approaches, don't give up after single failure
- Try alternative sites when a URL fails
- Enhanced plan mode with `<system-reminder>` tag for emphasis
- Updated PLAN mode system prompt with clearer allowed/prohibited actions

### Fixed

**Feishu Image Downloads** — Complete fix for image attachment handling
- Use message resource endpoint (`/im/v1/messages/:id/resources/:key`) instead of standalone image endpoint (was returning 400)
- Direct HTTP for tenant access token retrieval (openlark SDK returned empty responses)
- Validate download responses are actual file data, not JSON API errors
- Skip phantom attachments on download failure (no more zero-byte entries)

**Feishu Command Parsing** — Strip mentions for `/command` recognition
- Remove `@mention` placeholders entirely instead of replacing with display names
- `@jyc /model ls ark` now correctly parsed as `/model ls ark`

**Model Display in Logs** — Restored ai{m=...} span
- Use `Empty` + `.record()` pattern for model discovery via SSE
- SSE handler records model on parent span when discovered from `message.updated`
- Upfront recording when model is known from config or `/model` override

**Duplicate Footer Separators** — Prevent duplicate '---' in Feishu and email replies
- Added `strip_trailing_separators()` function to email_parser module
- Clean reply text before adding footer in both Feishu and email outbound adapters

**Attachment Security**
- Unified file size parser (removed duplicate `parse_human_size`)
- Consistent dot-prefix extension validation across all channels
- Size check before creating attachment (not after full download)

### Changed

- Consolidated `config_template.toml` and `config.example.toml` into single `config.example.toml`
- `config init` CLI command now uses `config.example.toml`
- Vision tool timeout: 300s (was 120s)
- `Updated processing state` log reduced from debug to trace
- Refactored `get_mcp_tool_command()` shared between reply and vision tools
- Remove `.agent/package-lock.json` from git tracking

### Removed

- Dead code: `save_attachment_to_disk` (websocket.rs), `parse_attachment_size` (attachment_validator.rs), `parse_human_size` (websocket.rs)
- Duplicate `config_template.toml` file

## [0.1.3] - 2026-04-08

### Added

**Token-based Session Management** — Replace time-based with token-based approach
- Real-time token tracking from SSE `step.finish` events
- Automatic model context detection (95% as safety threshold)
- Session reset when accumulated tokens exceed configured maximum
- Immediate persistence of token count after each processing step

**Token Usage Display** — Real-time token monitoring in user interface
- Reply footer displays current token usage: `Tokens: 20.7K/122K`
- Standardized K unit display (1024 basis) with 0.1K precision
- Shows actual reset threshold (model context 95% or configured value)

**Advanced Configuration** — Flexible token limit management
- `max_input_tokens` config option in `config.toml`
- Default threshold: 122,880 tokens (120K × 1024)
- Supports automatic detection of model context limits
- Override capability for specific use cases

### Changed

**SessionState Data Structure** — Updated for token tracking
- Removed `total_active_time` and `last_active_start` fields
- Added `total_input_tokens` and `max_input_tokens` fields
- Session lifecycle now based on token limits instead of time

**DESIGN.md Documentation** — Complete update for token-based system
- Revised session management architecture
- Updated flowcharts and process descriptions
- Added configuration and user interface documentation
- Removed obsolete time-based session management content

**Reply Footer Format** — Enhanced with token information
- Format: `---\n\nModel: <model> | Mode: <mode> | Tokens: <current>K/<max>K`
- Clean formatting with standardized units
- Clear display of remaining token capacity

### Fixed

**Token Counting Accuracy** — Standardized units for consistency
- Use 1024 instead of 1000 for K unit calculations
- Default max input tokens: 120 × 1024 = 122,880 (not 120,000)
- Precise display formatting to 0.1K

**Debug Logging** — Enhanced model context detection visibility
- Added detailed logging for model limit lookup process
- Log available models in provider and found model details
- Improved diagnostics for detection success/failure scenarios

### Technical Details

**Session Persistence** — State saved in `.jyc/agent-session.json`
- Includes current token count and maximum threshold
- Automatic reset detection at session creation
- AI prompt includes notification when session resets due to token limit

**Configuration Example**:
```toml
[agent]
# Optional: Maximum input tokens per session before resetting
# Default: 120*1024 = 122,880 tokens (95% of typical 128K model context)
max_input_tokens = 122880
```

**System Integration** — Seamless adoption in existing architecture
- Maintains all existing API contracts
- Compatible with both Email and Feishu channels
- Full backward compatibility with chat history system

## [0.1.2] - 2026-04-07

### Added

**Chat Log Storage System** — New unified storage architecture
- Replaced timestamped directory storage (`messages/YYYY-MM-DD_HH-MM-SS/`) with log-based storage (`chat_history_YYYY-MM-DD.md`)
- HTML comment metadata format: `<!-- timestamp | type:received/reply | matched:true/false | sender:... | channel:... | external_id:... -->`
- **Dual-write integration**: Smooth transition with backward compatibility (writes to both formats during migration)
- **AI chat history access**: System prompt instructions for accessing chat logs via tools (`glob`, `read`, `grep`)

**Feishu Footer Support** — Consistent model/mode display across channels
- Feishu replies now include model and mode information footer (same format as email)
- Format: `---\n\nModel: <model> | Mode: <mode>` (or variations when only one is available)
- Automatically reads from `reply-context.json` (existing infrastructure)
- No footer added when model/mode information is unavailable (backward compatible)

### Changed

**Message Storage Architecture** — Simplified and unified
- Removed timestamped directory creation logic from `MessageStorage::store_with_match()`
- All messages and replies now append to daily chat log files
- `store_reply()` no longer creates separate `reply.md` files
- **Backward compatibility**: `email_parser::build_topic_trail()` reads from logs first, falls back to directory storage if needed

**Email Parser Enhancements** — Log-aware history building
- New `parse_chat_log_entry()` function for parsing log entries
- `build_topic_trail_from_logs()` reads conversation history from chat logs
- Maintains compatibility with existing directory-based storage during transition

### Fixed

**Storage Consistency Issues**
- Prevented duplicate storage between MCP tool and outbound adapters
- Fixed current message appearing twice in quoted history
- Removed stale references to legacy `reply.md` and `received.md` files

**MCP Tool Integration**
- Fixed reply delivery failures caused by tool/adapter storage conflicts
- Ensured reply text is properly extracted and delivered via outbound adapters

### Technical Details

**Dependencies Updated**
- Added `glob` crate dependency for file pattern matching in chat log operations

**API Changes**
- `MessageStorage::store_with_match()` now only creates log entries, not directories
- `email_parser` module extended with log parsing capabilities
- `TrailCurrentMessage` now implements `Clone` trait for history building

**Testing**
- All 158 tests pass with new storage architecture
- New unit tests added for chat log parsing functionality

## [0.1.1] - 2026-04-06

### Changed

**Feishu Message Format Enhancement**
- Changed Feishu message sending from plain text (`msg_type: "text"`) to interactive cards with native markdown support (`msg_type: "interactive"`)
- Messages now render with full markdown formatting: bold, italic, code blocks, lists, links, and blockquotes
- Matches email channel behavior where markdown is converted to HTML for rich rendering
- Improves readability and formatting consistency across channels

## [0.1.0] - 2026-04-06

First multi-channel release: JYC is now a truly channel-agnostic AI agent framework with full Feishu (飞书/Lark) support alongside email.

### Added

**Feishu Channel — Full Implementation**
- Real-time WebSocket connection via openlark SDK (`LarkWsClient`)
- Message receiving: text, image, file, and interactive (card) message types
- Message sending via Feishu IM API (`CreateMessageRequest`)
- Chat/user name lookup with in-memory caching (readable topic directories)
- @mention placeholder stripping (replaces `@_user_1` with `@displayname`)
- WebSocket reconnection with configurable backoff
- `FEISHU.md` onboarding guide with required scopes, setup steps, troubleshooting

**Channel-Agnostic Architecture**
- `ChannelMatcher` trait: split from `InboundAdapter` for pure-logic pattern matching and topic name derivation
- `EmailMatcher` and `FeishuMatcher` stateless implementations
- `MessageRouter.route()`: channel-agnostic, delegates to `&dyn ChannelMatcher`
- `OutboundAdapter` trait: `clean_body()` for channel-specific body cleaning, `send_reply()` with full lifecycle (format + send + store)
- `TopicManager`, `AlertService`, `process_message`: all use `Arc<dyn OutboundAdapter>` instead of `Arc<EmailOutboundAdapter>`

**Pattern Matching**
- `mentions`: match Feishu messages by @-mentioned bot/user names or IDs (OR logic)
- `keywords`: match by message body content (OR, case-insensitive)
- `chat_name`: match by Feishu group chat name (OR, case-insensitive) — enables per-group behavior (e.g., reply to all messages in private groups, require @mention in public groups)
- All rules use AND logic within a pattern, first-match-wins across patterns

**Heartbeat Configuration**
- Configurable `[heartbeat]` section: `enabled`, `interval_secs` (default 600 = 10 minutes), `min_elapsed_secs` (default 60)
- Per-channel `heartbeat_template` with `{elapsed}` placeholder for multilingual messages (e.g., `"正在处理中，请稍候... (已用时 {elapsed})"`)

**SMTP Error Handling**
- Structured error handling using lettre's `SmtpError` API (replaces string-based matching)
- Permanent errors (5xx): fail immediately with SMTP code logged
- Transient errors (4xx): retry with exponential backoff (3 attempts, 5–60s)
- Connection/timeout errors: reconnect + retry (2 attempts)

**Security**
- `"external_directory": "deny"` in in-process agent permissions — blocks AI from accessing files outside the topic directory

**Build**
- `protobuf-compiler` added as build prerequisite (required by `lark-websocket-protobuf`)

### Changed

- **MCP Reply Tool**: no longer sends messages directly. Writes `reply.md` + signal file; monitor process delivers via pre-warmed outbound adapter. Eliminates cold-start timeouts for Feishu API calls.
- **BUILD MODE Prompt**: categorizes messages — information questions (→ use `curl`), coding tasks (→ use tools), general conversation (→ reply directly). Prevents AI from exploring the filesystem for simple questions.
- **Email Quoted History**: truncated to 1024 characters per entry (`MAX_QUOTED_BODY_CHARS`) with `...[truncated]` suffix
- **TopicManager**: uses `cancel.child_token()` — one channel shutting down no longer kills other channels
- **Heartbeat Interval**: default changed from 2 minutes to 10 minutes (avoids SMTP rate limits)
- **MCP Tool Timeout**: increased from 60s to 180s
- **System Prompt**: updated default to instruct AI to use tools for real-time information lookup

### Fixed

- Model name missing in `ai` log span (`m=?:build` → `m=ark/deepseek-v3.2:build`) — restored `tracing::field::Empty` + `.record()` pattern
- UTF-8 panic in Feishu outbound adapter (byte slicing on multi-byte Chinese/emoji characters)
- Feishu channel causing cascade shutdown of all email channels via shared cancel token
- Feishu reply tool timeout (>180s) due to cold-start HTTP calls in MCP subprocess
- Chat name lookup double-unwrap (`extract_response_data` already unwraps outer envelope)

### Removed

- Dead `[agent.progress]` / `ProgressConfig` config and `DEFAULT_PROGRESS_*` constants
- Dead `include_topic_history` config field
- Dead `workspace` field on `ChannelConfig`
- `feishu_` prefix from topic directory names (now consistent with email: just the chat/subject name)

## [0.0.13] - 2026-04-05

### Added

**Feishu (飞书/Lark) Channel Implementation - Phase 7**
- Complete Feishu channel support with real-time messaging capabilities
- **FeishuInboundAdapter**: WebSocket-based real-time message reception
- **FeishuOutboundAdapter**: API-based message sending using openlark SDK
- **FeishuClient**: Authentication, token management, and API integration
- **FeishuFormatter**: Multi-format message support (markdown, text, HTML)
- **FeishuWebSocket**: Real-time event handling with automatic reconnection
- Comprehensive error handling with `FeishuError` enum
- Full unit test coverage for all components
- Configuration support for Feishu app credentials and WebSocket settings

**Documentation Updates**
- Added "Feishu Channel Implementation" chapter to DESIGN.md
- Added Phase 7 to IMPLEMENTATION.md detailing Feishu implementation
- Updated README.md with "Supported Channels" section
- Configuration examples for Feishu channel setup

### Changed

- **OutboundAdapter trait**: Added `send_heartbeat()` method for progress updates
- **Channel registry**: Extended to support Feishu channel type
- **Topic naming**: Enhanced to support Feishu chat metadata
- **Test suite**: Expanded to 115 tests with Feishu component tests

### Fixed

- **OutboundAdapter implementation**: Fixed missing `send_heartbeat()` method in FeishuOutboundAdapter
- **Test failures**: Fixed config tests expecting 2.0 hours timeout (actual default is 1.0 hours)
- **Unused code warnings**: Cleaned up unused imports and variables in Feishu modules

### Technical Details

- **API Integration**: Uses official openlark Rust SDK for Feishu API
- **WebSocket Protocol**: Implements Feishu's custom WebSocket protocol
- **Authentication**: App token management with automatic refresh
- **Message Formatting**: Support for Feishu's rich message formats
- **Topic Compatibility**: Seamless integration with existing topic management

## [0.0.12] - 2026-04-02

### Added

**Skill-based bootstrapping (replaces per-prompt system.md)**
- Migrate bootstrapping instructions from `system.md` (sent every prompt) to in-process agent's native discovery mechanisms
- `AGENTS.md` (project-level): project context, tech stack, coding conventions, git rules, dev workflow
- `agents.example.md`: template for topic-level AGENTS.md with self-bootstrapping context and environment hint
- `.agent/skills/jyc-deploy-bare/SKILL.md`: on-demand skill for bare metal deployment (deploy.sh + nohup)
- `.agent/skills/jyc-deploy-docker/SKILL.md`: on-demand skill for Docker deployment (s6 process supervisor)
- Skills loaded by AI only when needed, reducing prompt size and improving performance

**Model listing with wildcard filtering**
- Add `/model ls [pattern]` command to list available models with wildcard support
- Support `*` (multiple characters) and `?` (single character) wildcards
- Handle email escaping (`ark\*` → `ark*`) for better UX
- Case-insensitive pattern matching
- Remove bare `/model` command (now requires arguments)
- Comprehensive tests for wildcard functionality

### Fixed

**Multiple reply support**
- Reply context file (`.jyc/reply-context.json`) now persists between replies instead of being deleted after each send
- Allows AI models to send multiple replies in the same topic without file-not-found errors
- Context file is overwritten on each new incoming message; cleanup only for tests and manual operations
- Updated documentation in `DESIGN.md` to reflect new lifecycle

**IMAP monitor resilience and timeout handling**
- Add 60s timeout to all IMAP operations (connect, select, fetch_range, fetch_uid) to detect dead TCP connections
- Add 2-min hard timeout guard around IMAP IDLE to detect half-open TCP connections
- Add 5s timeout to IMAP logout to prevent 15-min hang on dead connections (TCP retransmission timeout)
- Remove fatal retry limit — monitor retries indefinitely at max backoff instead of giving up after 5 failures
- Force disconnect after `check_for_new()` failure to avoid entering IDLE on a dead connection
- Clean up closed senders from topic_queues to prevent unbounded HashMap growth
- Drain completed worker JoinHandles when spawning new workers
- Add UID compaction to StateManager (auto-prune when exceeding 5000 entries)
- Share `reqwest::Client` across in-process agent requests (connection pool reuse)
- Move 10 regex compilations to `LazyLock` statics (email_parser and smtp/client)

**Deployment reliability**
- Use `systemd-run` to escape jyc cgroup during self-deploy (prevents deployment from being killed)
- Ensure `deploy.sh` survives parent process death
- Add `jyc/` path prefix to deploy skills for proper resolution

### Changed
- Send model as `{providerID, modelID}` object in prompt API (breaking API change in in-process agent)
- Show model in log span immediately at prompt time instead of waiting for SSE discovery
- Fix duplicate `m=` field in log span (was recorded twice: upfront + SSE)
- Remove deprecated `system.md.example` files with migration notice

## [0.0.11] - 2026-04-01

### Added

**Live message injection**
- Follow-up messages sent during AI processing are injected into the ongoing session via `prompt_async`
- Queue receiver (`rx`) flows through: TopicManager → AgentService → AgentService → SSE Client
- New `tokio::select!` arm in SSE loop monitors `pending_rx.recv()` for incoming messages
- Injected messages: stored as `received.md`, reply-context.json updated, body sent as raw prompt (same as in-process agent TUI)
- agent API `POST /session/:id/prompt_async` supports sending to busy sessions
- AgentService trait: added `pending_rx: &mut mpsc::Receiver<QueueItem>` parameter
- QueueItem made public for cross-module access

**Logging improvements**
- `<system-reminder>` filtered from `is_prompt_echo()` — prevents in-process agent plan mode reminders from appearing in fallback replies
- `<system-reminder>` filtered from AI response text DEBUG log
- Session retry logs include `message` field for better debugging
- `logged_tools` HashSet cleared on retry — retried tool calls are now visible in logs

### Changed
- Injection prompt: raw body only (no framing instructions) — matches in-process agent TUI behavior
- Dev build profile: reduced debug info (debug=1, no debug for deps) for faster builds

### Fixed
- Removed stale `mode` field from `GenerateReplyResult` struct

## [0.0.10] - 2026-03-30

### Added

**/reset command to clear agent session**
- New `/reset` command that deletes `.jyc/agent-session.json`
- Allows users to manually reset the AI conversation session
- Next AI prompt after reset starts with a fresh session
- Session state tracked per-topic in `.jyc/agent-session.json`

### Changed

- **SYSTEMD.md**: Added deployment warnings to `systemctl stop` commands
- **system.md.example**: Updated systemd stop command warning text

## [0.0.9] - 2026-03-30

### Added

**systemd service support for process supervision and self-bootstrapping**
- systemd user service at `~/.config/systemd/user/jyc.service` for process supervision
- `run-jyc.sh` wrapper script that sources `~/.zshrc.local` for environment variables
- `jyc-ctl.sh` control script for service management (status, logs, restart, stop, start)
- `SYSTEMD.md` documentation with setup, usage, and troubleshooting guide
- `system.md.example` updated with systemd bootstrap instructions
- Automatic restarts on crash (`Restart=always` with 5-second delay)
- Service configuration tracked in repository (no s6-overlay)
- Environment variables from `.zshrc.local` available to jyc (API keys, etc.)

**Combined provider/model name in reply context and log spans**
- Model field in reply-context.json now uses `<provider-id>/<model-id>` format (instead of just model_id)
- Log span `m` field also uses combined format (e.g., `ark/deepseek-v3.2:build`)
- Applied to both email reply footers and structured logging
- Example: `ark/deepseek-v3.2` instead of `deepseek-v3.2`

### Removed

**s6-overlay approach** (replaced by systemd)
- `s6-rc.d/` directory and service configuration files
- `start-jyc.sh` (s6 initialization script)
- `NATIVE_S6.md` (s6-specific documentation)

### Changed

- **DESIGN.md**: Added reference to `SYSTEMD.md` in References section
- **Cargo.toml**: Bumped version from 0.0.8 to 0.0.9

## [0.0.8] - 2026-03-28

### Changed

**Disk-based reply context (replaces REPLY_TOKEN)**
- Reply context saved to `.jyc/reply-context.json` per-topic before AI prompt
- MCP reply tool reads context from disk (cwd) instead of decoding a base64 token
- AI never sees or touches the context — zero corruption risk
- `token` parameter removed from `reply_message` tool schema — only `message` and `attachments`
- REPLY_TOKEN line removed from AI prompt entirely
- Token-related system prompt instructions removed (no more "pass as-is" warnings)
- Context includes `model` and `mode` fields for future footer use
- Context file deleted by reply tool after successful send (cleanup)

### Removed
- `serialize_context()` and `deserialize_context()` functions (base64 token approach)
- `REPLY_TOKEN=` from prompt text
- Token integrity checks (backtick detection, nonce validation) — no longer needed
- `build_footer()` function and model/mode from `build_full_reply_text()`
- `model` and `mode` fields from `AgentResult` (agent is channel-agnostic)
- `model` and `mode` parameters from `EmailOutboundAdapter::send_reply()`

## [0.0.7] - 2026-03-27

### Changed

**Session preservation — keep session whenever possible**
- Model passed per-prompt (`PromptRequest.model`) — `/model` switch no longer deletes session
- Mode passed per-prompt (`PromptRequest.agent`) — `/plan` and `/build` switches no longer delete session
- `agent config` config changes no longer delete session — server picks up changes per-directory
- Session survives: model switches, mode switches, config changes, container restarts
- Session only deleted for error recovery: ContextOverflow and stale session detection

**Prompt echo stripping fix**
- Changed from join-then-strip to per-part filtering
- Each text part individually checked for prompt echo markers (`## Incoming Message`, `REPLY_TOKEN=`)
- Fixes: AI fallback text was lost when prompt echo and actual response were in separate SSE parts

**Logging improvements (from pre-release fixes)**
- Duplicate `m` field in `ai` span fixed — recorded once when model discovered
- Duplicate tool logs deduplicated with HashSet per step
- Tool input shown in logs (`Tool running tool=bash input="cargo build"`)
- Duplicate "Reply sent by MCP tool" log removed from topic_manager
- Session reuse: `get_session` now sends `x-agent-directory` header
- Debug logging for `config_changed` and `get_session` response status

### Fixed
- Session reuse across container restarts: `get_session()` was missing `x-agent-directory` header → server couldn't find session → always created new
- Fallback reply empty when AI produces prompt echo + actual response in separate text parts
- `/model` and mode commands unnecessarily deleted session (model/mode are per-prompt, not per-session)
- Cleaned up agent task artifacts: removed model/mode from ReplyContext, AgentResult, build_full_reply_text, EmailOutboundAdapter (these are per-prompt concerns, not per-token/per-adapter)

### Added

**Docker: two image variants**
- `jyc:dev` (target `dev`, ~2GB) — Rust pre-installed for self-bootstrapping, no timeout during cargo install
- `jyc:latest` (target `production`, ~740MB) — no Rust, production use
- Both share the same `base` stage (cached) — building one caches the base for the other
- `docker-compose.yml` defaults to `dev` target, configurable via `JYC_BUILD_TARGET` env var

## [0.0.6] - 2026-03-27

### Changed

**Token format: `REPLY_TOKEN=`**
- `<reply_context>TOKEN</reply_context>` → `REPLY_TOKEN=TOKEN` — no XML tags, avoids triggering AI's "parse structured data" instinct
- Tool parameter description updated to reference `REPLY_TOKEN=` line
- Prompt echo stripping marker updated

**Conversation history removed from AI prompt**
- in-process agent session memory handles multi-turn conversation context
- `build_conversation_history()` function removed (dead code)
- `include_history` parameter removed from `build_prompt()`
- System prompt simplified — no "Conversation history" section reference
- `include_topic_history` config field deprecated (kept for backward compat but ignored)

**DESIGN.md comprehensive update**
- Removed all jiny-m references (moved to IMPLEMENTATION.md)
- Removed "Differences from jiny-m" comparison table
- PromptBuilder: updated for no history, REPLY_TOKEN format
- ReplyContext → Reply Token: minimal 5-field description
- Context Management Strategy: rewritten for session-based (not prompt-based)
- Data Flow Summary, sequence diagram, block diagrams: all updated
- MCP Tool section: reads from disk, not token
- Stripping Strategy table: removed AI Prompt Context row
- Config example: removed `include_topic_history`

**Cargo.toml description**
- Removed "Rust rewrite of jiny-m" — JYC is its own project

## [0.0.5] - 2026-03-27

### Changed

**Minimal reply context token (corruption-proof)**
- Token slimmed from 12 fields to 5: `channel`, `topicName`, `incomingMessageDir`, `uid`, `_nonce`
- All message metadata (sender, recipient, topic, References headers) now read from stored `received.md` frontmatter — NOT from the AI-passed token
- Prevents AI model corruption (e.g., `petalmail.com` → `petailmail.com` causing bounced emails)
- Token is now ~120 bytes base64 instead of ~400 bytes — shorter = less corruption risk
- Switched to standard base64 (with padding) matching jiny-m's format

**Token serialization moved to `mcp/context.rs`**
- `serialize_context()` and `deserialize_context()` now live together in `src/mcp/context.rs`
- Removed from `prompt_builder.rs` — the prompt builder imports from `mcp::context`
- All token logic (struct, serialize, deserialize, validate) in one place

**Enriched received.md frontmatter**
- Added `sender`, `sender_address`, `external_id`, `reply_to_id`, `references`, `matched_pattern` to YAML frontmatter
- Reply tool reads all metadata from disk (authoritative source) instead of trusting token
- `parse_stored_message()` extracts all new frontmatter fields

**Docker: 3-stage build + image size optimization**
- Restructured to base (tools, cached) → builder (Rust compile) → final (base + binary)
- Removed Rust toolchain from runtime image (~1.23GB saved, image ~740MB)
- AI installs Rust on-demand for self-bootstrapping (~30s)
- `CARGO_TARGET_DIR=/tmp/jyc-target` avoids cross-platform conflict with host macOS builds
- Cargo registry + git cached in named Docker volumes
- in-process agent data volume for session persistence across container restarts
- Builder uses `rust:bookworm` matching runtime's glibc version

**Logging**
- `system.md loaded` / `No system.md found` log when building system prompt

## [0.0.4] - 2026-03-27

### Added

**Phase 6: Resilience + Polish**
- Alert service (`src/core/alert_service.rs`): background task buffers ERROR events, flushes as digest emails at configured intervals. Health check reports with per-topic stats at configured intervals. Self-protection via `eprintln` for send failures (no feedback loop).
- `AppLogger` — unified logging + alerting handle. Components call `app_logger.info()`, `.error()`, `.message_received()`, `.reply_by_tool()` etc. Each call delegates to `tracing` for console output AND sends structured events to the alert service for stats tracking + error buffering. Replaces separate `tracing` + `AlertHandle` dependencies.
- Progress tracker (`src/core/progress_tracker.rs`): sends periodic "still working" emails during long AI operations. Configurable initial delay (default 3 min), interval (default 3 min), max messages (default 5). Polling every 5s with `tokio::time::interval`.
- Startup notification email: sent on monitor start with version, timestamp, channel count, agent mode
- Graceful shutdown: alert service final flush before exit, agent service stopped, all worker tasks awaited

### Changed
- `/model` with no args now shows current model (from override or config default) instead of "not yet implemented"
- `AlertHandle` renamed to `AppLogger` to reflect its dual role as logger + alerter
- Structured logging: `channel=` and `topic=` fields added consistently to all key log lines across IMAP monitor, message router, topic manager, and agent service. Enables easy filtering by channel or topic in production logs.

### Fixed
- Error handling audit: all production `unwrap()` calls verified safe (static regex, guarded strip_prefix)

## [0.0.3] - 2026-03-27

### Added

**Phase 5: MCP Reply Tool + Commands**
- MCP reply tool (`src/mcp/reply_tool.rs`): `rmcp` stdio server with `reply_message` tool. Decodes context token → loads config → reads received.md → builds full reply with quoted history → sends via SMTP with file attachments → stores reply.md → writes signal file
- `jyc mcp-reply-tool` hidden subcommand wired to rmcp server
- Reply context deserialization (`src/mcp/context.rs`): base64 → JSON → validation with tamper detection
- `/model <id>`, `/model reset` command handler — writes `.jyc/model-override`, forces new session
- `/plan`, `/build` command handlers — writes/removes `.jyc/mode-override`
- Commands wired into topic_manager: parse → execute → reply results → strip → check body → dispatch to agent

**Architecture: AgentService trait**
- `AgentService` trait (`src/services/agent.rs`): `process(message, topic_name, topic_path, message_dir) → AgentResult`
- `StaticAgentService` (`src/services/static_agent.rs`): fixed text reply with quoted history
- `AgentService` implements `AgentService`: owns full reply lifecycle (AI interaction + fallback send + storage)
- TopicManager dispatches via `Arc<dyn AgentService>` — zero mode-specific code
- Adding new agent modes requires only: implement trait + match arm in `cli/monitor.rs`

**File attachment support**
- SMTP client: `MultiPart::mixed` with `Attachment` parts, MIME type detection by extension
- Email outbound adapter: reads files from disk, builds `EmailAttachment` structs
- MCP reply tool: validates attachment paths, builds `OutboundAttachment`, passes to outbound

**Email body extraction fix**
- Prefers HTML→Markdown conversion (via `htmd`) over raw plain text — mobile email clients generate poor plain text with no line breaks
- HTML cleaning before conversion: strips `<style>`, `<script>`, `<head>`, `<meta>`, `<link>`, CSS `@import`/`@media` rules, HTML comments

### Changed
- `message.channel` now contains config channel **name** (e.g., "jiny283"), not type ("email") — fixes MCP reply tool config lookup
- Session reuse restored: `get_or_create_session()` reuses existing session if valid on server, only creates new on config change or server restart — AI maintains conversation memory across messages
- Session state file renamed: `session.json` → `agent-session.json` — avoids future naming conflicts with other service sessions
- Removed unused `emailCount` field from `SessionState`
- MCP server name: `"rmcp"` → `"jiny_reply"` with `#[tool_handler]` macro — fixes tool discovery (was `toolCount=0`)
- Noisy IMAP polling logs moved from DEBUG to TRACE level
- Empty AI text parts no longer logged at DEBUG level
- Session error logging: fallback to raw property extraction when struct deserialization fails
- SSE model_id/provider_id: no longer overwritten with None by subsequent events

### Fixed
- MCP tool not discovered by in-process agent: missing `#[tool_handler]` attribute on `ServerHandler` impl
- Channel lookup in reply tool: `config.channels.get("email")` → `config.channels.get("jiny283")`
- `strip_quoted_history`: added `发件时间` to Chinese reply header detection

## [0.0.2] - 2026-03-27

### Added

**Phase 4: AI Integration**
- agent service manager: auto-start `jyc serve`, free port discovery, stdout-based readiness detection, health check, graceful shutdown with `kill_on_drop`
- agent HTTP client: `create_session`, `get_session`, `prompt_async`, `prompt_blocking` with `x-agent-directory` header and `?directory=` query param
- SSE streaming: subscribe to `/event?directory=`, parse events from JSON `{"type": "...", "properties": {...}}` format, activity-based timeout (30min default, 60min when tool running), progress logging with model info
- SSE event handling: `server.connected`, `server.heartbeat`, `message.updated` (model/provider capture), `message.part.updated` (tool state tracking), `session.status`, `session.idle`, `session.error`
- Session management: per-topic `.jyc/session.json`, fresh session per prompt (avoids stale sessions across server restarts), `agent config` generation with staleness check
- Prompt builder: system prompt (config + directory boundaries + reply instructions + system.md), user prompt (conversation history + incoming body + base64 reply_context token)
- AgentService (`src/services/agent/service.rs`): encapsulates all AI logic — server lifecycle, sessions, prompts, SSE, error recovery. Returns `GenerateReplyResult` to TopicManager.
- ContextOverflow recovery: delete session, create new, retry with blocking prompt
- Stale session detection: tool reported success in SSE but signal file missing → delete + retry
- Fallback reply with quoted history: `build_full_reply_text()` shared function for both fallback and future MCP reply tool
- Prompt echo stripping: removes `## Incoming Message`, `<reply_context>`, `## Conversation history` markers from AI output when tool fails

**Architecture: TopicManager ↔ AgentService separation**
- TopicManager: queue management, concurrency control, agent mode dispatch, fallback send
- AgentService: AI-specific logic isolated from infrastructure. Does NOT send emails.

### Changed
- IMAP ID command: now logs `server_name`, `server_vendor`, `trans_id` as structured fields (no raw map dump)
- IMAP monitor: backoff on SELECT failure (was tight retry loop)
- DESIGN.md: added agent HTTP API reference, responsibility separation docs, updated Worker Processing Flow diagram, agent service shutdown lifecycle table

### Fixed
- IMAP `SELECT INBOX` rejected by 163.com with "Unsafe Login" — added RFC 2971 ID command after login
- agent service command: `jyc server` → `jyc serve` with `--hostname=` / `--port=` syntax
- agent service readiness: detect by parsing stdout for `"jyc server listening on http://..."` instead of HTTP polling
- SSE event parsing: event type is in JSON `data.type` field, not SSE `event:` field
- SSE subscription: added `?directory=` query param to scope events to topic project context
- Explicit `agent_server.stop()` on graceful shutdown

## [0.0.1] - 2026-03-27

### Added

**Phase 1: Foundation**
- CLI skeleton with `clap` — subcommands: `monitor`, `config init`, `config validate`, `patterns list`, `state`, and hidden `mcp-reply-tool`
- TOML configuration with `${ENV_VAR}` substitution for secrets
- Configuration validation with structured error reporting
- Core types: `InboundMessage`, `InboundAdapter`/`OutboundAdapter` traits, channel pattern matching types
- `ChannelRegistry` for adapter lookup by channel name
- Unified `CommandRegistry::process_commands()` — single-pass parse, execute, and strip commands from message body (improved over jiny-m's split design)
- `CommandHandler` trait for extensible email commands (`/model`, `/plan`, `/build`)
- `tracing` + `tracing-subscriber` for structured async-aware logging with `--debug` and `--verbose` CLI flags
- Error types via `thiserror`, application errors via `anyhow`
- Utility functions: `parse_file_size`, `validate_regex`, `extract_domain`, `sanitize_for_filesystem`
- Default constants for timeouts, context limits, and configuration defaults

**Phase 2: Email I/O Layer**
- IMAP client wrapper (`async-imap` + `async-native-tls`) with TLS, login, SELECT, FETCH by UID/range, IDLE support, and disconnect
- IMAP ID command (RFC 2971) sent after login — required by 163.com (NetEase) to avoid "Unsafe Login" rejection
- Email parser: `strip_reply_prefix` (Re:/Fwd:/回复:/转发:), `derive_topic_name`, `strip_quoted_history`, `clean_email_body`, `truncate_text`, `parse_stored_message`, `parse_stored_reply`, `format_quoted_reply`
- Email inbound adapter: `mail-parser` raw bytes → `InboundMessage` with boundary cleaning; pattern matching (sender exact/domain/regex + subject prefix/regex, AND logic, first match wins)
- SMTP client (`lettre`) with TLS, References headers (`In-Reply-To`, `References`), markdown→HTML via `comrak` (GFM), auto-reconnect on connection errors
- HTML→Markdown conversion via `htmd`
- Email outbound adapter: `send_reply`, `send_alert`, `send_progress_update` — thread-safe via `Arc<Mutex<SmtpClient>>`
- Per-channel state manager: `.imap/.state.json` + `.processed-uids.txt` for IMAP sequence tracking and UID deduplication

**Phase 3: Core Processing Pipeline**
- Message storage: `received.md` with YAML frontmatter, `reply.md`, attachment saving with extension allowlist, size limits, collision resolution
- Topic manager: per-topic `tokio::sync::mpsc` channels with `Semaphore`-bounded concurrency (configurable `max_concurrent_topics`)
- Message router: delegates pattern matching to channel adapter, derives topic name, dispatches to topic manager
- IMAP monitor: connect → SELECT → check_for_new → IDLE/poll → loop; exponential backoff on errors; recovery on message deletion; first-run only processes latest message
- Full `jyc monitor` wiring: load config → validate → Ctrl+C handler → per-channel SMTP connect → TopicManager → Router → StateManager → spawn ImapMonitor tasks → await shutdown
- Placeholder reply in in-process agent mode (sends confirmation email with message metadata until Phase 4 AI integration)

### Directory Layout

```
<root>/
├── config.toml
├── <channel>/
│   ├── .imap/
│   │   ├── .state.json
│   │   └── .processed-uids.txt
│   └── workspace/
│       └── <topic>/
│           ├── messages/<timestamp>/
│           │   ├── received.md
│           │   └── reply.md
│           ├── .jyc/
│           ├── .agent/
│           ├── agent config
│           └── system.md
```
