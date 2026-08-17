# Channels / Agents / AI — Migration Design

**Status:** Target architecture and migration plan. Not yet implemented.
This document is the single source of truth for the rename/refactor
program decided on 2026-08-16. It supersedes `core-hub-adapters.md`
once the migration lands (that document is rewritten in the final phase).

## Target model

Three layers with strict responsibilities:

| Layer | Config table | Responsibility | Data dir |
|-------|--------------|----------------|----------|
| Channels | `[channels.<name>]` | Connection (protocol credentials) + patterns/rules matching + routing to an agent's topic. Just a conduit. | `~/.local/share/jyc/channels/<channel>/` |
| Agents | `[agents.<name>]` | Behavior (template, skills, model, mcps, tools, …) + owns topics. **No connection, no rules, no matching.** Works inside its topics. | `~/.local/share/jyc/agents/<agent>/<topic>/` |
| AI | `[ai]` | The shared brain: model providers, prompts, thinking, iteration limits. | — |

### Target config example

```toml
# --- Channels: connection + matching + routing ---
[channels.jiny283]
type = "email"
[channels.jiny283.inbound]
host = "imap.163.com"
# ...

[[channels.jiny283.patterns]]
name = "invoice"
agent = "invoice"                # route matched messages to this agent
topic = "invoice"                # optional; channel-derived when omitted
[channels.jiny283.patterns.rules]
subject = { contains = ["invoice"] }

[channels.feishu_bot]
type = "feishu"
[channels.feishu_bot.feishu]
app_id = "cli_xxx"
app_secret = "xxx"

[[channels.feishu_bot.patterns]]
name = "mentions"
agent = "jyc"                    # pipe dissolves: every pattern target is (agent, topic)
topic = "${msg.chat_name}"
[channels.feishu_bot.patterns.rules]
mentions = ["jyc"]

# --- Agents: behavior + topics only ---
[agents.invoice]
template = "invoice"
skills = ["invoice-processing"]

[agents.jyc]
template = "jyc"

# --- AI: the shared brain ---
[ai]
model = "deepseek/deepseek-v4-pro"
```

### Key properties

- **`pipe` is no longer a special mechanism.** Every pattern routes to
  `(agent, topic)`. A "pipe-only channel" is just a channel whose patterns
  target agents whose topics live elsewhere.
- **Agents are channel-agnostic.** One agent may receive messages from
  several channels; its topics accumulate the full cross-channel context.
  Replies travel back via each message's origin channel (the pipe-relay
  mechanism generalized). This is a primary goal of the migration.
- **The dashboard chat pane lists `[agents]` directly** and enters an
  agent's topic — no websocket pattern indirection.
- The websocket channel becomes a plain channel whose patterns reference
  agents; it keeps its interactive/conduit role (the "hub" name is gone).

## Decisions (locked 2026-08-16)

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | Three-layer naming: `channels` / `agents` / `ai` | Channel owns matching; agent owns topics and behavior; `ai` is the global brain. `[agent]` (singular) was confusingly close to the new `[agents]` middle layer. |
| D2 | Revert #573 (`[hub]`/`[adapters]` tables, `pipe.hub` rename) — commit `f6c442a` | Hub/adapters were never deployed; zero compatibility burden. `[channels]` returns as the single channel table, which is exactly the target name. |
| D3 | `[agent]` → `[ai]` keeps a serde alias + deprecation warning | Production configs use `[agent]`. |
| D4 | Hub/adapters aliases are **not** kept | Never deployed (D2). |
| D5 | One agent may be mounted on multiple channels | Explicit goal of the migration. |
| D6 | TopicManager becomes keyed by agent (deep refactor) | Forced by D5: a topic receiving messages from several channels cannot be hosted by one channel's TopicManager; reply routing must resolve per-message origin channel. |
| D7 | Channel side keeps a thin matching/forwarding/relay role | Working name: SubjectManager (final naming TBD during implementation). |
| D8 | `[ai].mode` valid values are `agent` / `static`, enforced by `validate_config` at load time | `agent_builder` bails on anything else at runtime; load-time validation catches it earlier. (Verified during PR-2: the whitelist already existed and the template already used `mode = "agent"` — the earlier "stale `auto`" concern was mistaken.) |
| D9 | **Websocket is not a channel.** It is the console every *declared* agent carries; the inspect server provides the endpoint. `[channels.local_dev]`-style declarations disappear entirely | The ws "channel" was never an external protocol — it is the agent's own interactive interface. |
| D10 | **Declaration = registration.** `[agents.x]` (even empty) registers an agent on the panel. A pattern's `agent = "<name>"` must reference a declared agent (strict validation error otherwise) | Empty tables are the explicit "exists with defaults" statement; typos fail loudly. Define-on-use was considered and rejected. |
| D11 | **Implicit-owner bridge** for unmigrated patterns: at runtime each legacy pattern gets an internal owner named `<channel>/<pattern>`; topics stay in the legacy workspace layout, appear in the panel's Channels area, and never enter the Agents area | Zero-config-change compatibility. Panel has two areas: Agents (declared, interactive console) and Channels (background topics, observable as today). Migration to the Agents area is per-pattern and opt-in. |

## Phased plan

Each phase is one PR, independently mergeable, CI-green.

### PR-0 — Revert #573 ✅ (this PR)

`git revert f6c442a`. World returns to: `[channels.<name>]` as the single
channel table, `pipe = { channel = "...", ... }`, `[agent]` global config.
The template-validation test added in #573's review round is removed by the
revert and rebuilt in PR-1.

### PR-1 — This document

Migration design doc to preserve context (this file).

### PR-2 — `[agent]` → `[ai]` rename

- Serde alias `agent` accepted with deprecation warning (D3), at all three
  levels: top-level `[ai]`, `[channels.<x>.ai]`, topic-level `[ai]`.
- Code rename: `AgentConfig` → `AiConfig`, `TopicAgentConfig` →
  `TopicAiConfig`, config fields `.agent` → `.ai`.
- `config.example.toml` uses the new `[ai]` key.
- Rebuild the template-validation test (loads `config.example.toml`, asserts
  `validate_config` is clean) so template rot is caught in CI. The existing
  `mode` whitelist in `validate_config` (D8) is covered by it.

### PR-3 — `[agents]` behavior table + pattern routing target ✅

- New `[agents.<name>]` table carrying today's pattern-level behavior fields:
  `template`, `topic_path`, `model`, `plan_model`, `build_model`,
  `small_model`, `mode`, `mcps`, `disabled_tools`, `disabled_mcp_servers`,
  `skills`, `disabled_skills`, `reset_compression`, `auto_reset_threshold`,
  `access` — all optional. **No connection fields, no rules.**
- `ChannelPattern` gains `agent = "<name>"`. At load time
  (`AppConfig::apply_agent_overlays`, called by all loaders) the agent's
  fields are overlaid under the pattern's own fields — the pattern's explicit
  values win — so the existing matcher/router/resolution chains are
  **unchanged**. Topic naming stays on the pattern (`topic_name`,
  `topic_prefix`, `topic_path`) — no separate `topic` field was needed.
- Scope decisions made during implementation:
  - `live_injection` and `inject_inbound_images` stay pattern-only —
    bool-with-default fields cannot distinguish "unset" from "explicitly
    default", so they cannot be overlaid cleanly.
  - `role`, `repo_group`, `attachments` stay pattern-only — channel-specific
    semantics, not agent behavior.
- `pipe = { channel = "...", ... }` keeps working with a deprecation
  warning (feishu pipe predates #573 and may be deployed); the new form is
  `agent = "..."` on the pattern.
- Validation: a pattern referencing an undefined agent is a
  `validate_config` error.

### PR-4 — Data directory layout ✅

- Topic dirs: `~/.local/share/jyc/agents/<agent>/<topic>/` for patterns with
  an agent reference — implemented in the router by mapping agent-routed
  topics onto the existing `topic_path_override` mechanism, so the worker,
  storage, and template init are unchanged.
- Channel state files: `~/.local/share/jyc/channels/<channel>/` (github
  `.github` and gitee `.gitee` state dirs moved; other channel types keep
  no on-disk state).
- Migration is **lazy** (not a startup scan): on first touch of a topic or
  state dir, the legacy `<workdir>/<channel>/...` path is renamed into the
  new location via `migrate_dir_if_needed`. Explicit `topic_path` overrides
  continue to win over both layouts.
- `list_topics` / `topic_path` use `workspace_dir` — which is
  `agents/<agent>` for agent-keyed TopicManagers (PR-5a) and
  `<channel>/workspace` for channel ones — so agent topics survive restarts
  under their owning manager without any agent-dir special-casing in the
  channel manager.
- Guards added to `validate_config`: `agents`/`channels` are reserved
  channel names (would collide with the new data-root dirs).
- Legacy patterns without an agent reference keep the old layout until
  migrated (D11).

### PR-5 — Runtime convergence (the deep one, D6) — split into 5a/5b

**5a — Agent-keyed TopicManagers ✅**

- One TopicManager per agent referenced by any channel pattern is
  constructed at startup (`agents/<agent>/` workspace), registered with the
  orchestrator as an agent (shared inspect/scheduler/cross-topic views,
  exempt from the reload diff, not added to the scheduler's workspace scan —
  the per-TopicManager custom-path scan covers it).
- `MessageRouter` gains `agent_topic_managers`; `route()` dispatches to the
  owning agent's manager when the matched pattern references an agent
  (`topic_manager_for`).
- `send_to_topic` follows the same dispatch: when the target pattern routes
  to an agent, the injected message is processed by the agent's manager.

**5b-1 — Reply path per origin channel + multi-channel lift ✅**

- `TopicManager` gains the cross-channel `OutboundsMap` (`set_outbounds`);
  the worker resolves the reply adapter per message from its **origin
  channel** (`resolve_outbound`), falling back to the manager's own adapter
  when the map is absent or the channel unknown (legacy behavior intact).
  This generalizes the feishu pipe relay to arbitrary channel→agent.
- The multi-channel sharing guard is **lifted**: an agent may now be
  referenced by patterns of several channels (D5).

**5b-2 — Default console + ghost fix ✅ (core)**

- **Synthesized default console (D9)**: when no websocket channel is
  declared, serve synthesizes an in-memory `local_dev` channel whose
  patterns are the declared agents — the interactive console for the panel.
  `[channels.local_dev]` can now be deleted from config entirely (the
  synthesized entry lives only in memory, never written to the file).
  Feishu pipes targeting `local_dev` keep working (the synthesized patterns
  cover every declared agent).
- **Ghost fix**: the channel TopicManager's `restore_custom_topic_paths`
  skips patterns that reference an agent (their topics belong to the agent's
  manager), eliminating the idle duplicate entries in the dashboard overview.

**5b-2 — Remaining (PR-6)**

- The dashboard panel's visual two-area grouping (Agents vs Channels) — the
  data is already there (agent entries have `channel_type = "agent"`).
- Direct `agent = "..."` on feishu patterns (feishu currently routes via
  `pipe`; the pipe path keeps working against the synthesized console).
- `pipe = { channel = ... }` fully retires in favor of `agent` (kept
  readable with a deprecation warning meanwhile).

### PR-6 — Docs & cleanup

- Rewrite `core-hub-adapters.md` as the channels/agents/ai model doc.
- Update `config.example.toml`, README, DESIGN.md, CHANGELOG,
  `agents.example.md`.
- Align inspect/dashboard "channel" display semantics where cheap.

## Compatibility & migration policy

- Every rename ships with a serde alias + deprecation warning, except
  hub/adapters which get none (D4).
- Aliases and legacy-pattern support are removed in the next major version
  after the migration completes.
- `jyc inspect-config` (load + `validate_config`) is the fast local gate;
  full verification is CI (`fmt`, `clippy -D warnings`, `llvm-cov`).
