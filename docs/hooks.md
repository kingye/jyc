# Hooks

External commands invoked at agent lifecycle points — guardrails, audit
trails, redaction, context provisioning. Modeled on Claude Code's hook
system; existing CC scripts can be reused verbatim (see
[Claude Code compatibility](#claude-code-compatibility)).

## Configuration

Hooks are declared at two levels, merged per topic exactly like
[`[[commands]]`](../config.example.toml):

1. global `[[hooks]]` — applies to every topic;
2. `[[agents.<name>.hooks]]` — applies to topics routed to that agent,
   executed **after** the globals.

```toml
[[hooks]]
event = "reply_send"
shell = ["sh", "-c", "tee -a /var/log/jyc-replies.log >/dev/null"]

[[agents.jyc.hooks]]
event = "pre_tool_use"
matcher = "^bash$"                 # optional regex filter
shell = ["./scripts/guard.sh"]     # argv form, no shell interpolation
timeout = 30                       # optional, seconds (default 30)
```

| Field | Required | Notes |
|---|---|---|
| `event` | yes | jyc or Claude Code name — see [Events](#events). The name also selects the payload dialect. |
| `matcher` | no | regex; subject depends on the event (tool name / topic name / source / reason). Unset = match all. |
| `shell` | yes | argv array. Want pipes? `["sh", "-c", "grep foo && bar"]`. |
| `timeout` | no | whole seconds before the process is killed; kill fails open. |

Validation at startup rejects unknown events, bad regexes, and empty
`shell`. Non-agent topics (no routed agent) get the global set only.

Hooks run **sequentially in config order**; the first exit-2 wins and
later hooks for that event do not run. An empty hook set costs nothing —
every site short-circuits on `is_empty()`.

## Events

| Event | CC alias | Fires at | Blocking? | Matcher subject |
|---|---|---|---|---|
| `message_received` | `UserPromptSubmit` | after commands/questions are stripped, immediately before AI dispatch | yes — the message stays in the chat log but never reaches the model | topic name |
| `pre_tool_use` | `PreToolUse` | every tool call (model-initiated and synthetic reply deliveries) | yes — the call fails with the hook's stderr as the error, visible to the model | tool name |
| `post_tool_use` | `PostToolUse` | after a tool call completes (success **or** failure) | result is marked error + stderr appended; the tool already ran | tool name |
| `post_tool_use_failure` | — | after a tool call failed (extra to `post_tool_use`) | notification only | tool name |
| `reply_send` | `Stop` | every AI reply delivery — synchronous tool send (gated in `ToolRegistry`), background watcher, worker auto-delivery, and fallback text | yes — delivery is suppressed and the reply is not retried | topic name |
| `session_start` | `SessionStart` | new topic (`startup`), after `/reset --force` / `/new --force` (`reset`/`new`) | notification only | `source` |
| `session_end` | `SessionEnd` | before `/reset --force` / `/new --force` / `/close --force` teardown | notification only | `reason` |

Command-result replies (`/?`, `/model` output, …) are mechanical, never
reach the model, and fire no hooks. Slash commands intercepted during a
running turn fire `session_end`/`session_start` normally.

`session_end` fires **before** the destructive action, so a hook can
still read or archive the topic directory while it exists.

## Protocol

The hook process is spawned with the topic directory as its working
directory. Environment: `JYC_HOOK_EVENT`, `JYC_AGENT`, `JYC_TOPIC`,
`JYC_CHANNEL` (when known).

Exit codes (both dialects): `0` proceed (stdout logged at debug level),
`2` block + stderr as the reason, anything else (including crash,
timeout, spawn failure) warn + **fail open**.

### Payload contract

The stdin JSON guarantees only `hook_event_name`, `topic`, `cwd` (plus
`agent` in the jyc dialect). Everything else is **best-effort**: fields
appear only when the data exists at the site — a websocket-only flow has
no sender, tool events have no message. Scripts must use tolerant access
(`.message.sender // "unknown"`).

jyc dialect (`event = "pre_tool_use"` etc.):

```json
{
  "hook_event_name": "pre_tool_use",
  "agent": "jyc",
  "topic": "jyc",
  "cwd": "/home/user/projects/jyc",
  "channel": "agents",
  "content": "message text, when the event carries one",
  "message": { "sender": "…", "sender_address": "…", "content": "…" },
  "metadata": { "…": "inbound metadata passthrough (forwarded context)" },
  "tool_name": "bash",
  "tool_input": { "command": "ls" },
  "tool_response": "output text, on post events",
  "reply_text": "on reply_send",
  "source": "startup | reset | new",
  "reason": "reset | new | close"
}
```

Event-specific keys (`tool_*`, `reply_text`, `source`, `reason`) are
present for their own events; `message`/`metadata`/`channel` appear
whenever the underlying inbound data has them.

### Claude Code compatibility

Write the event name in CC spelling and everything about that hook —
payload shape and field names — follows the CC convention:

| jyc | CC name | CC field mapping (best-effort) |
|---|---|---|
| `message_received` | `UserPromptSubmit` | `prompt` ← message text |
| `pre_tool_use` | `PreToolUse` | `tool_name`, `tool_input` (identical) |
| `post_tool_use` | `PostToolUse` | + `tool_response` (string) |
| `reply_send` | `Stop` | `stop_hook_active: false` (+ `reply_text`, a jyc extension CC scripts ignore) |
| `session_start` | `SessionStart` | `source`: reset → `"clear"`, startup/new → `"startup"` |
| `session_end` | `SessionEnd` | `reason`: close → `"logout"`, reset/new → `"other"` (+ `jyc_reason`) |

Common CC fields: `session_id` ← topic name, `cwd` ← topic directory.
Known deviations: `transcript_path` is not emitted (jyc history files
are day-partitioned and agent-local — CC scripts reading it must
tolerate absence), and the CC JSON stdout decision protocol is not yet
implemented — hooks that signal via exit codes work as-is.

Unsupported CC events (`SubagentStop`, `PreCompact`, `Notification`, …)
are rejected at config load, not silently ignored.

## Security boundary

Hooks are an **operator-trust surface**, identical to `[[commands]] shell` commands: they are configured only in trusted config files (global or
per-agent). Topic-local (`.jyc/`) hook definitions do not exist and must
never be added — the topic directory is writable by the agent, so hooks
loaded from it would be prompt-injection with a shell.

Conversely, the **payload is untrusted input** for your script: message
content, tool names, and sender fields originate from inbound channels.
Never eval/interpolate payload values into shell (`jq` output as data
only; quote everything).

## Examples

Deny tool use by sender scope (tool name matcher already filters):

```toml
[[agents.mail-bot.hooks]]
event = "pre_tool_use"
matcher = "^write$|^edit$"
shell = ["sh", "-c", "echo 'mail-bot is read-only' >&2; exit 2"]
```

Audit every AI reply (stdout passthrough keeps delivery intact):

```toml
[[hooks]]
event = "reply_send"
shell = ["sh", "-c", "jq -r '\"\\(.topic)\\t\\(.reply_text)\"' >> /var/log/jyc-replies.tsv"]
```

Archive a topic before `/close` deletes it (`session_end` fires while
the directory still exists):

```toml
[[hooks]]
event = "session_end"
shell = ["sh", "-c", "tar czf /backup/jyc-$JYC_TOPIC.tgz -C \"$PWD\" ."]
```

CC guard script against your jyc topics:

```toml
[[agents.jyc.hooks]]
event = "PreToolUse"            # CC dialect: payload matches CC exactly
shell = ["python3", "./cc-guard.py"]   # reads .tool_input.command as usual
```

## Phase 2 (planned)

- **stdout context injection**: `session_start`/`message_received`
  (CC: `SessionStart`/`UserPromptSubmit`) stdout appended to the next
  prompt — provisioning without editing config.
- **CC JSON decision protocol** (`additionalContext`, `systemMessage`,
  `continue`), with plain-text stdout degrading to context injection —
  matching CC's own fallback behavior.
