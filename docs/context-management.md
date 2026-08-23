# Context Management

How JYC decides what the LLM sees on each request: context strategies,
turn pairing, tool-call annotations, and the token-level safety nets that
prevent context overflow.

## Overview

Three core principles govern the whole system:

1. **The raw context is the source of truth.**
   `.jyc/agent-context.json` stores the full conversation (including tool
   calls and results) exactly as recorded. Strategies never reshape this
   file — only a session reset (manual `/reset`, auto-reset, or the
   mid-loop safety net) compacts it, using the same turn-pairing logic
   described below.
2. **Only the wire payload is shaped.** Every LLM request passes through
   `build_send_context()` (`crates/jyc-agent/src/agent_loop/context.rs`),
   which decides what slice of the raw context to send and in what form.
3. **Token budgets are enforced in layers.** Beyond the configured
   strategy, independent token monitors compress or reset the session
   before the provider can reject an oversized request (API 400).

## Data Plane

```
.jyc/agent-context.json (on disk — always the FULL raw context)
        │
        ▼  loaded once per incoming message
raw_context: Vec<serde_json::Value>   (provider wire format)
        │
        ├─ raw_context[..prior_len]    ← prior history (from disk)
        └─ raw_context[prior_len..]    ← current turn (this message's loop)
        │
        ▼  per LLM request
build_send_context(provider, raw_context, prior_len, strategy)
        │
        ▼
wire payload sent to the provider
```

`prior_len` is the length of the raw context at the moment the incoming
message was received. Everything after it — the new user message, the
assistant's intermediate steps, tool calls, tool results — is the
**current turn**, which is always sent verbatim so tool_use/tool_result
pairing stays valid mid-loop.

## Context Strategies

Configured via `context_strategy` (see [Configuration](#configuration));
changeable at runtime with the `/context` command.

### `full` (default)

Sends the entire raw context on every request — `build_send_context`
returns `Cow::Borrowed`, a zero-copy borrow. Simple, accurate, and grows
linearly with conversation length until a token safety net fires.

### `sliding_window`

Sends two parts, in order:

1. **Windowed history** — the last `window` (default 10) user/assistant
   **turns** extracted from the prior context, reformatted in the active
   provider's wire format.
2. **Current turn, verbatim** — `raw_context[prior_len..]`, untouched.

```
 ┌────────────────────────────────────────────────────────────┐
 │ ① Windowed history (last N turns, re-paired + annotated)   │
 │                                                            │
 │  user      │ "u2"                                          │
 │  user      │ "[History note] assistant tool calls:         │
 │            │   bash(command="ls") → ok"  ← folded summary  │
 │  assistant │ "a2"                    ← pure text, no tools  │
 │  user      │ "u3"                                          │
 │  assistant │ "a3"                                          │
 ├────────────────────────────────────────────────────────────┤
 │ ② Current turn — verbatim                                  │
 │                                                            │
 │  user      │ "u4"                                          │
 │  assistant │ tool_use / tool_calls blocks  ← as recorded   │
 │  tool/user │ tool_result / tool messages   ← as recorded   │
 │  ...       │ further steps appended as the loop continues  │
 └────────────────────────────────────────────────────────────┘
```

Why the split: the history part is compressed to pure text plus summaries
(cheap, and prevents the model from mimicking machine formats); the
current turn is mid-flight, so its tool_use ↔ tool_result structure must
stay legally paired or the provider API rejects the request outright.

#### Turn pairing (`extract_pairs`, `crates/jyc-agent/src/session.rs`)

The pairing unit is the **turn**, not the single message:

- A **text-bearing user message opens a turn**; the next one (or end of
  context) closes it.
- **All assistant messages in a turn are merged into one entry** holding
  only their text — intermediate tool-call steps and the final reply
  alike. `reasoning_content` and `tool_calls` are stripped from the
  assistant entry.
- **The turn's tool calls fold into a separate user-role history note**
  (see below), never into the assistant's own text.
- **Anthropic `tool_result` user-role wrappers** carry no text, so they
  don't open turns; they only feed the results map.
- **Round-tripped history notes are skipped** on re-parse — they are
  metadata, not turn boundaries, so a compacted-then-reparsed context
  stays stable.
- Assistant messages seen before any user message are dropped; a trailing
  turn with no assistant reply yet produces no pair (a fallback keeps the
  last user message when no pairs exist at all).

#### History notes

Each turn with tool calls gets one user-role message emitted **before**
the assistant text (so no `assistant → user(note)` adjacency sits
right before the next real user turn — which would prime the model
to treat the note as a fresh instruction):

```
[History note] assistant tool calls: bash(command="ls -la") → ok, read(file_path="a.txt") → [error] No such file
```

- Results are matched to calls by id (OpenAI `tool_call_id` / Anthropic
  `tool_use_id`) within a **turn-scoped map**, so an id reused in a later
  turn can never misattach to an earlier call.
- Failed calls are prefixed `[error] `.
- `jyc_reply_message` calls are excluded. The reply's `message` is the
  text the user already saw and is preserved in the assistant's own
  text; exposing the call here would give the model a pattern to mimic
  as narration instead of actually invoking the tool. A turn that called
  only the reply tool emits no note at all.
- The assistant's own text stays pure — models were observed mimicking
  the older in-text annotation format and emitting fake tool-call text as
  their reply; the system prompt's "History Format" section states the
  contract explicitly.

Size caps (constants in `session.rs`):

| Limit | Value | Notes |
|-------|-------|-------|
| Single argument value | 200 bytes | truncated with `… [truncated N bytes]` |
| Single tool result | 500 bytes | full result is one `context_browse` away |
| **Whole note** | **2000 bytes** | overflow folds to `…(N more calls)` |

With the default `window = 10`, all notes together cost at most ~20 KB of
bytes. The dominant factor in window size is the untruncated
user/assistant text itself, not the notes.

**`note_window` (optional):** when set to `M`, only the **most recent M**
windowed turns carry a history note; older turns in the window are
text-only. Unset (default) = notes on all windowed turns. `M = 0` yields
a pure text window; `M > window` clamps to `window`. Rationale: notes on
stale turns are low-value noise, while the newest notes preserve "what
just happened / what just failed". Trade-off: a turn without a note loses
tool-error visibility — the model may re-run a command that failed there;
`context_browse` remains the fallback for recovering the original calls.
Heuristic session compaction and mid-loop compression always keep notes
on every pair — `note_window` only shapes the sliding-window wire
payload.

#### Anthropic compatibility defenses

- Synthetic user/assistant messages are built via the active provider's
  own formatter (`format_user_message` / `build_raw_assistant_message`),
  so both `content: "..."` (OpenAI-compat) and `content: [...]`
  (Anthropic) shapes stay valid.
- **Empty text blocks are never emitted** — messages with no extractable
  text are skipped, because Anthropic rejects empty blocks, especially
  under `cache_control`.

## Recovering dropped turns: `context_browse`

Turns that fall out of the window are not gone — they remain in the
in-memory `raw_context` and on disk. The built-in `context_browse` tool
pages through the full transcript (shared `extract_pairs` view):

- `offset` — pairs to skip from the **newest** end (0 = most recent).
- `limit` — page size (default 10, max 50).
- Returns numbered `USER:` / `ASSISTANT:` lines, oldest→newest within the
  page.
- Reads the in-memory snapshot (never the on-disk file, which is stale
  mid-loop); the snapshot is taken once per tool batch.

The system prompt tells the agent to use `context_browse` whenever it
needs earlier turns of the current conversation.

## Token safety nets

Independent of the configured strategy, three layers enforce the token
budget (threshold: `context_window × auto_reset_threshold`, default 95%
of the detected model context limit):

```
LLM response received (input_tokens known from SSE step-finish)
        │
        ▼  input_tokens ≥ context_window × auto_reset_threshold ?
┌──────────────────────────────────────────────────────────────┐
│ Mid-loop compression (agent_loop/mod.rs)                     │
│ Shrinks raw_context + history IN MEMORY to the last 3 turns  │
│ (hard-coded keep_pairs=3 — a 400-prevention safety net, NOT  │
│ the user-configured compression). The shrunken context is    │
│ what lands in agent-context.json when the loop persists at   │
│ its end. Publishes a "session_reset" status event.           │
└──────────────────────────────────────────────────────────────┘
        │
        ▼  between messages / at loop end
┌──────────────────────────────────────────────────────────────┐
│ Pre-loop pre-check (service/mod.rs): if the active model's   │
│ context window shrank below the loaded session, reset BEFORE │
│ the first LLM call.                                          │
│ Post-loop auto-reset (update_tokens): when the budget is     │
│ exceeded, reset the session using reset_compression config.  │
└──────────────────────────────────────────────────────────────┘
        │
        ▼  provider rejects anyway (agent-server path)
┌──────────────────────────────────────────────────────────────┐
│ ContextOverflow recovery: SSE session.error → log, create a  │
│ fresh session, retry the prompt once.                        │
└──────────────────────────────────────────────────────────────┘
```

Session resets apply the configured `reset_compression` mode:

| Mode | Behavior |
|------|----------|
| `heuristic` (default) | Keep the last `keep_pairs` (default 3) user/assistant turns — same pairing logic as the sliding window |
| `llm` | A separate LLM call summarizes the conversation |
| `none` | Delete all context |

Manual `/reset` uses the same `reset_compression` config as auto-reset.

## Configuration

```toml
# Global default for all channels/topics
[ai]
context_strategy = { mode = "sliding_window", window = 10, note_window = 3 }
max_input_tokens = 122880        # optional; default = 95% of detected model context
auto_reset_threshold = 0.95      # fraction of context window that triggers reset
reset_compression = { mode = "heuristic", keep_pairs = 3 }

# Per-pattern override (takes priority over [ai])
[channels.jyc_repo.patterns."**"]
context_strategy = { mode = "full" }
```

`mode` accepts `full` (default) or `sliding_window` (alias `sliding`).
`window` counts **turns**, not messages or tokens. `note_window` is
optional (unset = notes on all windowed turns; `0` = none).

**Resolution chain** (highest wins):

1. Runtime override `.jyc/context-strategy.json` (written by `/context`)
2. Matched pattern's `context_strategy` (or synthesized `[agents.<name>]`)
3. First pattern fallback
4. Global `[ai].context_strategy`
5. Built-in default: `full` / `window = 10`

**Runtime commands:**

| Command | Effect |
|---------|--------|
| `/context` | Show current strategy and its source (`override` / `default`) |
| `/context full` | Send the full context |
| `/context sliding [N] [M]` | Sliding window, N turns (default 10, max 200); optional M = note window |
| `/context reset` | Remove the runtime override, revert to configured default |

## Code map

| Component | Location |
|-----------|----------|
| Wire payload shaping (`build_send_context`) | `crates/jyc-agent/src/agent_loop/context.rs` |
| Turn pairing, history notes, truncation caps (`extract_pairs`, `flush_turn`) | `crates/jyc-agent/src/session.rs` |
| Mid-loop compression | `crates/jyc-agent/src/agent_loop/mod.rs` |
| Strategy resolution, pre-loop pre-check | `crates/jyc-agent/src/service/mod.rs` |
| System-prompt guidance (History Format, Chat History sections) | `crates/jyc-agent/src/service/prompt.rs` |
| `context_browse` tool | `crates/jyc-agent/src/tools/builtin/context_browse.rs` |
| `/context` command | `crates/jyc-core/src/command/context_handler.rs` |
| Override persistence (`context-strategy.json`) | `crates/jyc-core/src/session_state.rs` |
| `ContextStrategy(Config)`, `ResetCompressionConfig` types | `crates/jyc-types/src/channel.rs` |

## History

Key changes, newest first (see CHANGELOG.md for full entries):

- **#647** — windowed history-note format clarity: `[History note]`
  is emitted before the assistant text (was after); truncated args
  and results carry an explicit `… [truncated N bytes]` marker (was
  bare `…`); the OpenAI provider's `[SUCCESS] `/`[ERROR] ` prefix is
  stripped from history-note results so failures render as `[error] `
  for both providers.
- **#632** — `note_window`: history notes limited to the most recent M
  windowed turns; `/context sliding [N] [M]`.
- **#630** — history notes moved to separate user-role messages (fixes
  transcript annotation mimicry); silent-reply escape.
- **#629** — synchronous reply delivery, mimicry guard, lost-content
  warning.
- **#628** — turn-based pairing for the sliding window (window counts
  turns, not messages).
- **#627** — history notes include truncated tool results with `[error]`
  markers.
- **#626** — `context_browse` tool + system-prompt guidance for dropped
  turns.
- **#624** — sliding-window strategy, `/context` command,
  `context_strategy` config.
