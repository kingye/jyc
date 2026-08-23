# Channels / Agents / AI — Migration Design

**Status:** Landed. The rename/refactor program decided on 2026-08-16 is
complete: every channel type is a pipe-only adapter and the websocket
channel is the sole hub. This document is kept as the design record;
[core-hub-adapters.md](core-hub-adapters.md) describes the reached state.

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

### PR-3 — `[agents]` behavior table + pattern routing target

- New `[agents.<name>]` table carrying today's pattern-level behavior fields:
  `template`, `skills`, `disabled_skills`, `model`, `small_model`, `mcps`,
  `disabled_tools`, `disabled_mcp_servers`, `live_injection`, `topic_path`.
  **No connection fields, no rules.**
- `ChannelPattern` gains `agent = "<name>"` and `topic` fields. Behavior
  resolution chain: pattern legacy fields → referenced agent → channel-level
  → global `[ai]`.
- Internally the matched pattern + referenced agent are merged into the
  effective behavior fed to the existing matcher/router — **no runtime
  changes in this phase**.
- `pipe = { channel = "...", ... }` keeps working with a deprecation
  warning (feishu pipe predates #573 and may be deployed); the new form is
  `agent = "..."` on the pattern.

### PR-4 — Data directory layout

- Topic dirs: `~/.local/share/jyc/agents/<agent>/<topic>/` for patterns
  with an agent reference.
- Channel state files: `~/.local/share/jyc/channels/<channel>/`.
- One-time startup migration: legacy `<workdir>/<channel>/workspace/<topic>`
  is moved (same-filesystem `rename`) to the new location, mirroring the
  existing topic-name migration in `topic_path.rs`. Explicit `topic_path`
  overrides continue to win.
- Legacy patterns without an agent reference keep the old layout until
  migrated.

### PR-5 — Runtime convergence (the deep one, D6)

- TopicManager keyed by agent instead of channel. Internals (queue, worker,
  semaphore, event buses, template init) stay; the change is in wiring:
  - construction: one TopicManager per agent;
  - lookup key: every `TopicManager`-by-channel-name call site
    (job_scheduler, inspect/topic_proxy, websocket inbound, `send_to_topic`
    tool, agent_builder) switches to agent key;
  - reply path: `outbound` is resolved per message from its origin channel
    (generalizing the feishu pipe relay), not held fixed by the manager.
- Channel side keeps the thin matching/forwarding/relay role (D7).
- Magnitude: medium-large, mostly mechanical lookup rewiring; reply-path
  and live-injection semantics need the most care.

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
