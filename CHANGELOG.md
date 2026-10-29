## [Unreleased]

### Added

- `TopicInfo` records the last completed LLM call's context composition — how its wire messages split between history, the current turn's checkpoint and its recent suffix (or one `Full` region), each with its message count, serialized bytes, and an estimated token size. It is persisted in `agent-session.json`, rendered as a colored stacked bar in the TUI chat info pane, and printed as text by `/info`; system prompt and tools are excluded, and the estimate is labelled as such (#1025)
- The MCP tools are loaded on demand: an MCP server's tools are **registered but not advertised** until the model calls the new `load_mcp_schema` with that server's name, whose description is the index of the configured servers — each server's name and each of its tools with a one-line description, and none of the schemas themselves. A server with many tools can be expanded a few at a time (`tool_names`), and a malformed `tool_names` is rejected rather than silently expanding the whole server. The expansion is per topic's registry lifetime, like the extended group. Only *advertisement* is gated — the MCP tools stay executable, so a model that names one before expanding it gets a working call — and `disabled_tools` / `disabled_mcps` remain the boundary that removes a tool or a server for good, the loader included: `disabled_tools = ["load_mcp_schema"]` removes it and the MCP tools then stay out of every request. A server removed by `disabled_mcps` is not in the index either. A name belongs to exactly one deferred group — an MCP server that exposes a built-in's name takes it over, and the loader names `load_mcp_schema` / `load_extended_tools` are reserved, so an MCP tool cannot displace the loader that reaches it. The servers are still connected at registry build time — the index needs their tool names — so this saves the per-request schema cost, not the startup handshake (#1022)
- `load_mcp_schema` reports the group's actual state rather than the shape of the request: a repeated subset call names only the tools that are still hidden, and a request that happens to name every tool of a partly-expanded server no longer reads as "the whole server is expanded"

### Changed

- The verbatim replay of a long `sliding_window` turn is now bounded: once it passes 32 assistant steps, the older ones fold into a text-only checkpoint (same representation as prior-turn history) while the recent suffix stays verbatim, so older reasoning and tool results stop riding every remaining call. The folded checkpoint text still grows with visible assistant text and retained user messages; what is bounded is the verbatim suffix, not the total prompt. The boundary advances one group of 32 at a time, making it byte-stable between advances, and `full` mode is unchanged. The wire dump marks the new region `3` and `LLM call started` logs `checkpoint_rebuilt`. The journal is untouched (#1023, #1024)
- The text-only transcript (prior-turn history, checkpoints and `context_browse`) keeps an assistant's visible reply only: a leading inline block is stripped only when it exactly duplicates `reasoning_content`, so reasoning is no longer replayed as assistant text in history while a block-shaped reply without that field is preserved. Active raw current-turn messages still carry whatever their provider needs (#1023)

## [0.7.3] - 2026-10-28

### Added

- `jyc send <agent> [--topic <topic>] <message>...` enqueues one or more messages, in argument order, for an existing agent/topic through a running `jyc serve`, for use from cron and scripts. It reuses the `/send` resolver and adds authenticated `POST /api/send`; it stops at the first rejection, leaves already-enqueued messages queued, and does not wait for an AI reply. A full target queue is reported as `429` instead of being silently dropped (#1009)
- The `jyc-cron` skill gives an agent the crontab runbook for `jyc send`: confirm the schedule and messages, use the installed binary's absolute path, preserve unrelated crontab entries, update one idempotent `# jyc-send:<id>` block, and log the job's output (#1009)
- The extended tools: twelve tools most topics use rarely — `jyc_spawn_subagent`, `jyc_send_to_topic`, `list_topics`, `jyc_publish_file`, `read_image`, `webfetch`, `context_browse`, `lesson_add`, `lesson_read`, `task_list`, `task_create`, `task_update` — are registered but kept out of the request's `tools` array until the model calls the new `load_extended_tools`, whose description is the index of the group and names the core tools already available. The advertised schema text goes from 6,925 to about 3,000 characters (−57%); the group is loaded once per registry lifetime — a config reload rebuilds the registry and the group hides again until it is loaded once more. Only *advertisement* is gated — the tools stay executable, so a model that names one before loading it gets a working call, and `disabled_tools` remains the boundary that removes a tool for good, the loader included (#1004, #1007)
- `list_topics` lists the channels and their active topics — the addressing table for `jyc_send_to_topic` and `jyc_send_message` — on demand, in the shape the per-turn runtime-state block used to carry (#1004)

### Changed

- The `sliding_window` context strategy is anchored on a persisted journal offset instead of sliding per turn: `agent-session.json` keeps `context_offset`, the byte offset where the window's oldest retained turn begins, and every load reads the journal forward from there. Between slides nothing trims the history's head — the history block (older turns, text-only) only grows — so the cross-turn wire prefix stays byte-stable as far as it reaches. The window grows to `max_window = window + round(extend_rate × window)` turns (new `extend_rate`, default 0.5) and then slides back to `window`, dropping whole turns; `max_history_tokens` is now a second trigger for that same slide and only fires at the top of the cycle, so a budget the window can never get under no longer moves the anchor on every turn. Nothing trims the history's head between slides — the previous design did it on every turn, could never actually get under the budget (folding whole turns into notes left the total unchanged), and so advanced the head on every turn: the cross-turn prefix broke on every first call and production wire dumps showed the history region collapsing to a bare marker (#995, #1016, #1019)
- `jyc_send_message` stays in the core advertised tool set instead of the on-demand group: it carries attachment-bearing replies (for example a ZIP on WeCom), so the model must be able to discover it without a loader call. The tool, outbound-adapter path, hub relay and channel attachment support are unchanged; ordinary text-only replies still use the turn's final message (#1007)
- `context_browse`, `lesson_add` and `lesson_read` moved to the on-demand group: they serve occasional transcript paging and lesson maintenance rather than every turn. The task tools (`task_list`, `task_create`, `task_update`) moved there too: whether work needs a plan is a project convention, not a framework-wide cost, so the loader's index points multi-step work at them and drops that sentence when `disabled_tools` removes them. `jyc_send_message` remains the core tool that must work without a discovery call (#1007)
- The channel/topic roster no longer rides every turn: it was a per-turn block that changed whenever a topic appeared or closed, and it was repeated in the history of every later turn. The subagent roster stays in the `<runtime-state>` block (it names the children this topic may reuse), and the channel/topic table is fetched by `list_topics` when the model wants to address someone (#1004)
- The built-in tool set is smaller and its text is leaner: the four `job_*` tools and `glob` are gone, and the 18 remaining descriptions went from 9,479 to 6,925 characters (-27%) — that much less in every request's cached prefix. `grep` and `webfetch` stay: they work without a `ripgrep`/`curl` binary, cap their output and set a browser user agent. The `edit` tool's description now names its actual parameter (`old_string`, not `oldString`). Two rules the trim had cut are back: `ask_user` says the turn ends, and `share_topic_path` says the child's state dir stays its own (#1003)
- Skills are opt-in: a topic lists **no** skill unless a layer declares `skills` for it (channel, pattern, agent, or the topic's own `.jyc/config.toml`), and the allow-list is what the *system prompt* carries — the default is an empty list, not "every skill found". A config that relied on the old default (no `skills` key → everything listed) now lists nothing until it names the skills it wants; a topic that wants one skill for one message asks for it by name (#1002)
- `/skill:<name>` now reaches every discovered skill, not only the ones the topic's allow-list names: the command appends that skill's name, path and description to the message, so the agent reads the right `SKILL.md` even when the prompt does not list it. The runtime `/skill on <name>` / `off <name>` still add to and remove from the topic's list, and `/skill reset` clears both (#1002)

### Fixed

- The dashboard reports a broken connection instead of looking healthy: the topic info pane says `Failed to load topic details — will retry` when the detail request failed (it used to read `Loading topic details...` forever, because a failed fetch and a pending one left the same empty detail), and the chat status block shows `Not connected: …` for a failed poll and `WebSocket disconnected — reconnecting...` for a lost socket (#998)
- The dashboard's WebSocket client now pings the server and treats a silent socket as dead, so a half-open tunnel — a VPN switching the network under a `jyc connect` session — is detected and reconnected instead of leaving the chat pane quietly frozen (#998)

### Removed

- `note_window` and the history-note machinery are gone (#1020): the wire payload is two blocks — prior turns as text-only pairs, then the current turn verbatim — so nothing is left to configure. `TurnPair`'s note, `WindowedResult`, `tool_call_summary`, `is_history_note` and the note section of the system prompt went with it; `ContextStrategyConfig` no longer has the field, and a config still carrying `note_window` loads with the key inert. `/context sliding` is now `[N] [EXT] [TURNCAP] [MAXHIST]`, so an old `sliding 10 0.5 3` is refused rather than silently capping results at 3 bytes, and a sixth positional is refused rather than ignored. A configured `turn_tool_result_cap` below 130 — the smallest cap that keeps both ends of a trimmed result instead of degrading to a head-only cut — is lifted to 130, since a config file never passes through the command. `context_browse` still renders each turn's tool calls from the journal (at most 20 calls and 4000 bytes per turn, 2000 bytes per message, 32 000 bytes per page; every bound drops whole items and says how many it hid, and the page bound drops the oldest pairs of the range so the note's `offset` reaches them).
- Scheduled jobs are gone: the `job_list` / `job_create` / `job_delete` / `job_toggle` tools, the background scheduler behind them (`[scheduler]` in the config, `JobScheduler`, the per-topic `<topic>/.jyc/jobs/` store) and the cron parsing. A config still carrying a `[scheduler]` block loads unchanged — the key is inert — and job files left in a topic's state dir are neither read nor deleted. Waiting for a CI run is the `ci-watch` subagent's job now (#1003)
- The `glob` tool is gone: `rg --files` or `find` cover the same ground, and the tool's own description claimed a modification-time order it never applied. The `glob` crate stays in `jyc-core` — the `/new` command still uses it (#1003)
- `disabled_skills` is gone from every config layer (channel, pattern, agent, topic config): with skills opt-in there is no implicit set of skills left to subtract from — a name is either in the allow-list or not, and the one way to take a skill out of a topic remains the runtime `/skill off <name>`. A config still carrying the key loads unchanged; the key is inert (#1002)
- `tool_result_cap` is gone from the context strategy: it capped tool results in the verbatim history region, which no longer exists — history is text-only. The current turn still has `turn_tool_result_cap`. A config still carrying `tool_result_cap` loads unchanged — the key is inert (#1019)
- The legacy `api_key_env = "ENV_VAR"` provider field is removed; use `api_key = "${ENV_VAR}"`. Provider blocks now reject unknown fields at config load, so an unmigrated key fails loudly instead of silently omitting the auth header. Keys are now resolved once at config load, so a rotated key requires a restart or config reload (#999)

## [0.7.2] - 2026-10-24

### Added

- `jyc config inspect-bind` prints the effective `[inspect].bind` address using the same layered config resolution as `jyc serve`; `jyc connect` uses it on the remote to discover a non-default inspect port (#997)

### Changed

- `jyc`, `jyc open`, and `jyc dashboard` resolve a missing `--addr` from the local `[inspect].bind` (falling back to `127.0.0.1:9876`), so a config that changes the inspect port no longer requires a matching manual `--addr` (#997)
- `jyc connect` resolves a missing `--remote-bind` from the remote hub's effective config instead of assuming `127.0.0.1:9876`; older remote versions that lack `config inspect-bind` keep the previous default behavior (#997)

### Fixed

- `jyc connect` re-reads an automatically fetched remote token once when the first authenticated probe is answered with 401, covering token rotation between the read and the probe (#997)
- A local dashboard/open startup now detects that an existing inspect port belongs to a different hub when its token is rejected, and reports the port collision with `--addr` / `[inspect].bind` guidance instead of surfacing a bare request 401 later (#997)

## [0.7.1] - 2026-10-24

### Added

- The topic info pane and `/info` report how many of the topic's messages the model has not read yet: the status row carries the backlog as one block per message the topic's queue can hold — `▊` in yellow for a message waiting for a turn of its own, `▊` in gray for a free slot — with the counts (`2 / 10`) beside it, under the word `queue` so the blocks cannot be read as the progress of the turn: `Status: Processing · queue ▊▊▊ 2 / 10`. The backlog is counted where the messages wait: the ones still in the queue are the channel's own depth, the ones the running turn pulled aside for a turn of their own are that turn's buffer, and the ones the running turn took in and the model has not read yet are that turn's injection channel — a message the user sent while the agent was working is waiting like any other, and the row says so. A topic whose source reports no capacity has no backlog to report, and a column too narrow for the blocks keeps the counts alone (#993, #994)

### Changed

- The dashboard's status chip counts topics, not "threads": `2 active / 5 topic`. "Thread" was the older name for a topic and is gone from the user-facing surfaces — the config layer already refuses the older keys (`max_concurrent_threads`, `max_queue_size_per_thread`) instead of accepting them silently, and the chip was the last place the old word showed (#993)

## [0.7.0] - 2026-10-24

### Added

- `jyc connect <host>` runs the dashboard here against a `jyc serve` that lives on another machine: the hub keeps the channels, topics and state where the work is, the TUI stays local, and an `ssh -N -L` child joins them — same port when it is free locally, so the links the server generates still resolve; the token is read from the remote per session instead of being copied onto this disk; the remote keeps its loopback bind, so nothing is exposed that the tunnel does not already tunnel. Starting the remote hub is opt-in (`-s/--serve`) — connecting never mutates the other machine by default — and quitting closes the tunnel while leaving the hub running, because other dashboards, channels and scheduled jobs may be using it (#989)

### Changed

- `jyc connect` compresses the ssh tunnel (`-C`) and takes `--ssh-opt KEY=VALUE` (repeatable): the dashboard's traffic is repetitive JSON — a thinking event re-sends the whole reasoning — so a thin uplink spent most of its bandwidth on bytes zlib collapses (#990)

### Fixed

- Closing a topic no longer leaves a parentless skeleton of it behind: `/close` deletes the topic's state dir, but work already in flight keeps going after it, and its writes recreated the deleted dir — the turn's last persistence (`save_raw_context`, the per-iteration `agent-session.json` update, the context-window write), a new message the worker dequeued in the instant the close landed (it recreated the dir before reading the message), and state writes a self-closed turn could still reach (`tasks.json`, a stacked `/model`-style override, the reply file relay) all brought back a dir holding a file or two and nothing else. Since a state dir's existence is what the topic list counts as an open topic, the skeleton showed up as a normal topic with no parent (its `spawned-from` breadcrumb died with the dir) and no role (no `config.toml`), and messaging it would have started an agent that has neither. Every one of those writes now consults the closed-topic freeze the other state writers already honoured, so a closed topic stays gone; a close that *fails* lifts the freeze again, so a topic that survives still records state. Commands stacked behind a close in the same message (`/close` then `/model x`, `/thinking show`, `/backlog push`, `/spawn …`) are refused with `this topic was just closed` instead of acting on a topic that no longer exists — `/close` itself stays idempotent (#992)

- `jyc connect` reports a token it could not read instead of blaming `[inspect]`: reading the token on the remote (`<bin> token show`) failing — a dropped ssh, a `jyc` missing from the remote's non-login PATH — left the probe unauthenticated, and an unauthenticated probe against a working hub answers exactly the 401 a wrong token does, so the error text sent the user to look at a config that was fine. The read is retried once, and a failure that survives it now leads the message (`could not read the dashboard token from <host>: …`) with the probe error as evidence below; every 401 also names the token it tried (from `--token` / read with which command), because that is what separates "wrong token", "this hub uses another `--data-dir`" and "the token was rotated after it started" — and the advice to check the token's freshness is only printed when a token was actually sent, since a probe without one is answered with 401 whatever the hub's token is. `jyc connect` no longer takes its token from `$JYC_DASHBOARD_TOKEN`: that variable holds a *local* dashboard's token, and using it for a remote hub silently sent the wrong secret (#991)

## [0.6.1] - 2026-10-23

### Added

- A `require_reply` request now gets an answer even when the target forgets one: the target's turn ends, and if nothing it did went to the asking topic, its last message is forwarded to that topic in its place — so the requester hears the answer, or the fact that there was none. `require_reply` was a request with nothing checking that it was kept, so a target that ended its turn without replying (a small model that forgot the tool call, a turn that died on a provider error) left the requester waiting forever — silence being indistinguishable from "still working". A turn that did reply is not forwarded on top of it — the record of where the turn sent is what tells the two apart (a turn that died after answering may still get a second, redundant notice) — and a request that arrives while the turn is running counts as one the turn owes an answer to, even when the turn dies on a provider error; only the sends made after the turn folded the request in count as its answer — the model cannot have answered a request it had not been shown yet (#977)

- The agent can keep what it learned: `lesson_add` records one distilled rule in the topic's own `lessons.md` — a trap it fell into, a fix that worked, a convention of this repo or this user — and `lesson_read` reads the earlier ones back. The newest lessons (2 KiB, up to 4 KiB when the cut backs up to a whole lesson) ride along in the system prompt, so a recent lesson is in view on every later turn without a tool call; the cut lands on a line boundary, so a lesson is whole in one end or the other — and one too long to carry whole is marked as cut rather than passed off as a whole rule. The tool only appends and reads, never rewrites the file, so a failed write cannot take the archive with it; pruning is the user's call (#971)

- The info pane and `/info` report the cache rate of the *latest turn* next to the session's: each row carries the cache-hit tokens, the input tokens they are a share of, and that share in percent — the turn's bar with `12.00K / 15.00K (80%)` under it, the session's with `964.51K / 1.18M (80%)` under it, under the pane's `Cache:` header. The turn's pair is zeroed at every turn start and accumulates per call, the way the turn's latency figures do, so a long turn's own rate is visible without waiting for the session's to move. The two scopes are separate facts on the topic's detail (`turn_cache_hit_tokens` / `turn_input_tokens`, next to the session's pair) and both surfaces print them through one shared formatter, so the pane and `/info` cannot drift (#964)

- The info pane and `/info` report the *latest turn's* output beside the session's, on one row: `Output: 12.00K turn · 1.18M session`. Only the session's running total was shown, so the turn's own spend was a subtraction the reader had to do — and nothing recorded it at all. `turn_output_tokens` is now the third turn-scoped figure: accumulated per LLM call from that call's own `output_tokens`, the way the turn's cache pair is, and zeroed at every turn start. Both surfaces print the row through one shared formatter, so they cannot drift, and a turn that has not called yet reads `0 turn` rather than vanishing (#984)

- The day's spend across *every* topic, as its own row: `Today (all): $12.34` — the billing ledger's day total, per currency, beside the topic's own `Cost` row (a day spanning two currencies shows both, `$12.34 · ¥3.20`). The day's ledger is now read once per list instead of once per topic, so the row costs one parse rather than one per topic (#964)

### Changed

- The shipped `jyc` is 1.80 MB smaller (`panic = "abort"` in the release profile): a panic in any path now ends the process instead of unwinding out of one task, so a deployment must restart it (docker `restart:`, systemd `Restart=`). The dashboard's panic hook still restores the terminal before the abort, and `cargo test` is unaffected — Cargo ignores the setting for the test profile. (#987)

- The `Cost:` row now leads with the day's figure: `Cost: $1.31 today · $0.05 session`, in the info pane and in `/info`. The session's came first, so a figure that the day's already contains was read before the day itself; both surfaces now spell the row through one shared formatter, so they cannot drift apart (#986)

- A chat row now has an identity of its own: every message and reply gets a per-topic sequence number where it enters the system, the chat log stores it, and the live `chat_message` event carries it — so the pane appends a row when the number is new and needs nothing else. The three-way text matching it replaces (user echoes by `(sender, text)`, historically logged rows by `(sender, text)`) silently collapsed a repeated message: saying `ok` twice and reopening the topic drew one `ok` while the log held two. A message the dashboard client sent now comes back tagged with the client's own id and takes the place of the optimistic echo instead of being matched to it by text. Rows logged before the sequence existed carry no number and are no longer drawn — the pane starts at the upgrade, the rows stay on disk. Numbers are allocated on arrival but rows are written on delivery, so a mid-turn message is logged after replies that took higher numbers: the pane asks whether the number is already on screen instead of comparing against the highest drawn, and a restart seeds from the highest number in the day's log instead of its last line's (#985)

- The dashboard's chat pane no longer waits for the overview poll: the live buffer is folded into the pane where it is written — on a websocket `chat_message` and after a REST hydrate — so a reply shows up at websocket speed instead of up to one poll later. The overview poll drops from 500 ms to 1 s, since it now only carries the topic rows and the status line, and a `resync`/`Lagged` re-hydrate no longer re-pushes the rows already on screen (it duplicated up to the last 50 AI messages, which only a reconnect made visible) (#983)

- The prior context is read back from the journal instead of being parsed whole: `ContextReader` walks `agent-context.jsonl` **backwards**, in blocks, and hands out only the window the context strategy asks for — the last `window` turns and the last `note_window` turn openers. A turn's cost no longer grows with the topic's history: a journal that reached a hundred megabytes used to be parsed in full on every turn, which is where the multi-second turn setup came from, and it is now a `stat` plus a few kilobytes of tail. The payload is unchanged — the slice starts at a turn boundary, so the strategies see the same turns they saw in the full journal, and a differential test pins that the wire payload is byte-identical either way. The write side matches: the journal is append-only, a turn's messages land at the end of the file, and the reader keeps the journal's pair count in process (keyed by its length and mtime) so an unchanged journal is never recounted. An append never truncates: the cut point is the last newline, found by scanning back block by block, and when it cannot be found the append is skipped — a journal that is merely unreadable used to be cut back to zero, taking the whole history with it. A torn tail is also given a second chance: when the fragment after the last newline is a complete message that lost only its newline — the usual shape, a write cut at its last byte — it is kept and terminated instead of dropped, so a crash no longer loses that turn. `context_browse` reads the older turns back from the journal too — it still pages the whole transcript, pair numbers included, but the running turn comes from memory, because the journal does not have it yet. Dangling tool calls are still repaired before the strategies see the context, as the load path always did — repairing only on the wire would drop the current turn along with the dangling message. Two things change shape around it: a corrupt line now ends the *backward* scan, so the newer turns stay and the older ones are dropped (the whole-file parser kept the older prefix), and a journal without an assistant message is only dropped when the scan reached its head — a window that happens to hold no assistant message says nothing about the turns behind it (#980)

- Plan mode keeps two of the agent's own tools: the task list and the lessons. `task_create` / `task_update` / `task_list` and `lesson_add` stay available while the mode is on — they write to the topic's own state (the plan of record and `lessons.md`), not to the project, so the read-only rule still holds where it matters: the files, the build, the commits, the dependencies. A plan-mode turn is exactly where a task list earns its keep and where a lesson is worth writing down, and neither is a change to the project — the mode is a convention in the prompt rather than a sandbox, so the tools were never disabled, only forbidden in prose (#979)

- A send to another agent no longer makes one: `jyc_send_to_topic` (and `/send`) now refuses a name no topic owns, and the refusal names the ways to make the topic — `/spawn`, `jyc_spawn_subagent`, or the dashboard's create-topic call. It used to create the addressee's own topic on the spot, so a name that was merely misspelled produced a brand-new agent: its own identity, its own state, its own place in the agent list, and none of the role's mode, skills or disabled tools — a plain agent that a `type="review"` role was never going to be. A send reaches topics that exist; making one is a separate, explicit act. A topic that exists but has never been routed to still counts as existing — a child `/spawn` has just made owns its home and its state but has no queue and no routing record yet, and the task that follows the spawn has to be able to reach it — while a closed child, whose home is gone, is refused like a name nobody owns — and a topic that exists while no channel serves it (the agents channel is not running) is reported as what it is: the name is taken, the channel is missing (#973)

- The agent loop's log brackets what it waits on: every LLM call logs `LLM call started` (iteration, model, messages sent, the time spent building the payload) and `LLM call finished` (wall time, input/output/reasoning tokens, cache hits), every tool batch logs `Tool calls finished` with its duration, and the end of a turn logs its split (`total_ms`, `llm_ms`, `tool_ms` — a split that need not sum, since a call cancelled in flight is timed nowhere). A call in flight used to write nothing — its "Thinking…" state travels on the event bus, not the log — so a slow model and a stalled process looked the same, and `jyc.log` simply went quiet between a tool step and the next entry (#970)

- The sliding-window send context builds its history regions once per turn, not once per call: they depend only on the persisted prefix, which is fixed while a turn runs, so the first call builds them and the turn's remaining calls reuse them. The per-call scan of the whole history — which deep-cloned every user/assistant message only to keep the last few turns — is gone, and each `LLM call started` line carries `history_rebuilt` so the log shows which calls paid for a build. `extract_user_assistant_pairs` starts at the tail it keeps as well, so even the first call clones the kept turns instead of the history behind them (#970)

- The info pane's percentage bars reach for the pane's edge: the figures of a bar row have a row of their own — in the value's column, under the bar they belong to — so the bar no longer has to leave room for them beside it. The bar is as wide as the column allows (what the label's room leaves, less the two cells it keeps to the pane's edge), where it used to stop at twenty cells: `Tokens: ────────··············` with `44.17K / 117.19K (38%)` under it, the figures lining up with the bar above. A column too narrow for an honest bar still shows its figures alone (#969)

- The info pane's cache rows are grouped and renamed: the turn's hit rate, the session's and the cache's write volume now sit under one `Cache:` header and name their own scope — `Turn:`, `Session:` and `Create:`, where they read `Cache (turn):`, `Cache (session):` and `Cache create:`. The pane's label column is four cells narrower for it (thirteen cells, the length of the longest label), so every value of every row starts that much further left and a narrow column gets the table layout four cells sooner. `/info` prints the same group the same way (#968)

- The `jyc_spawn_subagent` tool now says how to close a subagent: send it `/close --force` — a bare `/close` only answers with a warning that asks for the `--force`. An agent that spawned one had no way to learn this from the tools — the rule lived only in the `dev-workflow` skill — so a subagent that had reported and was done stayed open until its parent closed it (#968)

- A pipe channel now carries routing only, and config validation enforces it: every processing key is rejected on a pipe channel and on its patterns — `model`, `plan_model`, `build_model`, `small_model`, `mode`, `skills`, `disabled_skills`, `mcps`, `disabled_mcps`, `disabled_tools`, `disabled_builtin_tools`, `live_injection`, `inject_inbound_images`, `template`, `topic_path`, `access`, `context_strategy`, `reset_compression`, `auto_reset_threshold` — whatever the value, including a value equal to the default. They were dead configuration: the hub handles a piped message under the configuration of the agent that matches it, never under the forwarding channel's. The keys belong to the agent, so declare them under `[agents.<name>]`; `jyc serve`, `jyc config validate` and the dashboard's config reload all report the offending key and its path (#967)

- The info pane's `Turn` row is now always there: before the turn's first call it reads `0 / 0 (0%)` instead of vanishing. The turn's counters are zeroed at every turn start, so the row used to disappear and come back with each turn — it read as missing data rather than as a zero. The session's row still waits for the session's first input tokens, so a topic that has never made a call shows a zero turn row and no session row — in the chat pane and in the overview's selected topic alike (#965)

- Token counts read at a glance: the info pane, `/info` and the overview's context column print them in K and M with two decimals (`12.00K`, `1.18M`), and raw below 1024. One shared formatter does it, so the three surfaces cannot spell the same number two ways — the overview used to round to whole thousands while the pane printed the raw count. Cost figures carry two decimals (`$0.42`) rather than four: a cent is the smallest unit that reads as money (#964)

- The info pane's cache figures are one row per scope: the `Total input` and `Cache hits` rows are gone, their numbers folded into the `Session` row, which now shows the hits, the input and the share together — the title names the subject rather than one of its figures (#964)

- The `coding-principles` skill gained three house rules, and they apply to all coding — not only to code that reads: **Single Point of Responsibility** (one responsibility, one owner: the one place that knows how it is done; whoever may change a thing owns *how* it changes), **Reuse Before Adding** (search the tree before writing; lift a rule to the layer that owns it and call it from every caller) and **No Overdesign** (every abstraction must earn itself — a trait with one implementor, a struct mirroring another, a wrapper that adds nothing). The `pr-review` skill checks all three on every review, and pays extra attention to them when the diff's *functional* lines — not comments, docs or tests — grow: the review must then state the added-vs-moved functional line count and where the growth went, even when the verdict is clean (#963)

- The topic info pane draws its two percentage rows as bars: the context share and the cache hit rate now lead with a bar — `─` for the filled part (bold yellow), `·` for the rest (dim gray) — so each share reads at a glance and the two can be compared against each other: `Tokens:         ──···· 44.17K / 117.19K (38%)`, `Cache hit rate: ──────· 88%`. Both labels are padded to one width, so the two bars start in the same column. The bar is as wide as the column allows (up to twenty cells) and the wrapping never cuts it in half: a narrow column gets a shorter bar and the figures wrap below it. A column too narrow for an honest bar — fewer than five cells, where the rounding could put the share more than ten points off — gets the figures alone and no bar; the percentage is printed either way, so the bar is a glance aid and never the only source (#960).

- A topic's lineage has one owner: the topic manager reads the `spawned-from` breadcrumbs once per list and answers with the parents *and* the tree alike, so the overview's `parent`, a topic's own `subagents` tree and the explorer's tree all come from one answer — the inspect server no longer reads the same breadcrumbs a second time to fill the list's parent. The tree's input is one shape as well (`jyc_core::topic_manager::topic_tree::Lineage`): the trait that let two shapes feed the tree, and the struct that mirrored one of them, are gone. The topic detail now carries the topic's `parent`, which the list already carried (#963)

- The topic info pane and `/info` read one source for the subagent tree, and that tree is now part of the topic's *detail*: `/info` lists the topics a topic spawned — as a tree, each with its own subagents under it — where before it listed none, and the info pane draws the very same tree out of the same detail instead of building one out of the overview's topic list. The lineage behind both is the `spawned-from` breadcrumb each subagent wrote into its own state dir — the same file the overview's `parent` comes from — and the rules that turn a lineage into a tree now live in one place (`jyc_core::topic_manager::topic_tree`), so the explorer's tree, the pane's tree and a topic's own tree cannot drift apart. The overview's list payload carries no tree: a poll would pay a walk per topic for it, and the explorer nests every topic from the one field it needs — `parent` (#962)

- The topic info pane's values line up: in the table layout every label of the pane is padded to one width now, so every value — the two percentage bars included — starts in the same column and the pane reads as one table instead of a ragged list. The pane lays its rows out one way, decided by its inner width: while it can hold the label column and a value column (34 cells or more) every row keeps its label and its value on one line, and a value too long for its column wraps *inside* it — the lines that continue it are indented to the column, so the column keeps the values it holds. A narrower column stacks the rows instead, the label on a row of its own and the value flush left under it, so a narrow column cuts no value short — each field then takes two rows, so a narrow column shows fewer fields at once. The bar follows the layout: in the table layout it shares its row with the label, in the stacked one it has the row to itself and can use the column's full width — so a bar now appears wherever the column can carry an honest one, not only in a wide terminal. The table layout needs a column of 34 cells and the column is 20% of the terminal, so it is a terminal of roughly 175 columns that gets the table; a narrower terminal gets the stacked layout, where every value starts in the column's first cell. The pane's other sections — the task list, the changed files — are not `label: value` rows and keep their own shape (#961)

- The metric the topic info pane and `/info` call "Cache util" is labelled **cache hit rate** everywhere it reaches the user: the info pane's row, `/info`'s output and `/bill`'s table header. The number is unchanged — cache-hit tokens ÷ total input tokens — only the name is; the code keeps the older `cache_util_pct` name, and DESIGN.md's terminology table now records the two as the same figure (#960).

- The context-window figures now count the whole context and measure it against the whole window: the numerator is the most recent LLM call's `input_tokens` plus that call's `output_tokens` — the size of the context the *next* call will carry — and the denominator is the model's own context window, not a fraction of it. Before, the denominator was the auto-reset cap (the window scaled by the 0.95 threshold) and the numerator the input alone, so `28K/996K` really meant 2.8 % of a 1 Mi-token window. The dashboard's overview column, the topic info pane, `/info`, the chat status line and the Feishu progress card all read the same two fields through one shared helper (`jyc_types::context_token_pct`), and the session file keeps the per-call output tokens next to the per-call input tokens so the pair always describes one call

- The topic info pane and `/info` read one source: the topic's *detail* (`GET /api/topics/{channel}/{topic}/info` — in-process, the same `TopicManager::topic_info` call the endpoint serves). They now show the same rows for the same topic: the pane gained `Status` and `Reasoning`, `/info` gained `Cache util`, and the two percentages come from one shared helper, so the pane and the command cannot drift apart; the pane says its details are still loading when the topic is on screen but its detail has not arrived, instead of claiming that no topic is selected. The overview's list payload carries only what the topic table reads — the topic's identity, its status, its context usage, when it was last active — instead of a second copy of every detail figure; the pane, the chat status line and the topic's mode now read the detail, and the branch/changed-files stamping the overview used to do is gone (the pane reads them off the detail); the list and the detail report the same last-activity stamp — the event stream's, for both (#958)

- The dashboard overview's topic table no longer starts flush against the top of the screen: it gets the same blank row of top padding the topic info pane beside it carries, plus a full-width horizontal rule under its header in the resting pane border colour (`PANE_BORDER`), so both columns share one top edge and the header no longer runs straight into the rows. The table stays borderless and its `→ ` selection gutter is untouched — the rule is drawn into the blank row the header's `bottom_margin(1)` leaves, and is dropped entirely when the table area is too short to hold it (#956)

### Fixed

- The info pane's and `/info`'s `Reasoning:` row now reports the latest turn beside the session total, the way the `Output:` row already did: `Reasoning: 512 turn · 4.00K session`. The row had kept the session scope alone — the number was the whole session's hidden chain-of-thought, not the turn's — while its neighbours (output, cache, latency) had all grown a turn figure. The turn's reasoning is accumulated per call and zeroed at every turn start, from the same session file, and both surfaces spell the row with the shared pair formatter so they cannot drift apart (#988)

- Switching a topic from an OpenAI-family model to Claude works again. The journal stores the *active* provider's wire format, and only the Anthropic→chat direction was normalized, so a history written on GPT (`{"content": null, "tool_calls": [...]}`, `{"role": "tool", "tool_call_id": …}`) went to `/messages` verbatim and the strict routes rejected the body at the first tool-calling turn (`messages.1 … INVALID_REQUEST_BODY`) — after the switch the topic could not answer at all. `chat_to_anthropic_messages` is the missing mirror: `tool_calls` become `tool_use` blocks, `role:"tool"` becomes a `tool_result` block in a user turn, chat text and `image_url` parts become `text` / `image` blocks, `reasoning_content` is dropped (replayed chain-of-thought and unknown fields are both rejected), and a message whose converted content would be empty is dropped. The Anthropic provider runs it before filtering, and the existing merge still folds consecutive tool results into the one user turn the API expects. The journal is never rewritten — normalization is per request — and both directions leave messages already in the target shape untouched, so a topic that switched back and forth stays valid (#982)

- The Anthropic provider no longer sends two `user` turns in a row: a tool result and the message that followed it — an injection, or an image a tool loaded — went on the wire as two turns, and the strict routes built on the Messages API reject that pair outright (`messages: roles must alternate between "user" and "assistant"`). Consecutive `user` turns are merged into one before sending, which is what the Messages API does anyway, and the tool result stays ahead of the text that follows it (#966)

- `live_injection` does something again: the knob was parsed, threaded onto every queued message and then ignored — the port to the in-process agent dropped the one place that read it, so a follow-up written while the model was working always waited for the turn to end. It is live again, and the in-process agent is where it acts: a plain message that arrives during a turn now joins that turn as a user message and the model sees it in the next call it makes — the whole batch at once, in the order it arrived — instead of the message waiting for a turn of its own. An injected message gets no reply of its own; the running turn's reply covers it, and the transcript shows what was said. The flag still decides: `live_injection = false` keeps the old behavior, where the message waits its turn. Only plain text is injected — a message with an attachment or a template keeps its own turn, where the full pipeline can serve it, and so does a message headed for another directory than the running turn's — and a message the turn never picks up (it ended first) is re-queued rather than lost. The moderation gate still applies: a `message_received` hook that blocks the message keeps it out of the running turn — it waits for a turn of its own, where the same hook blocks it again — because an injected message reaches the model just as much as a message that starts a turn

- A session's token totals are the session's again: every turn overwrote them, so the `Session`, `Output`, `Create` and `Reasoning` rows described the *last* turn while their own documentation promised the session's. The agent loop now seeds its running totals from the session file, so each turn continues the session's counts instead of restarting them, and the cache rate can tell a turn's share from the session's. A session reset still zeroes them — the file is rebuilt (#964)

- A topic whose turn recovered from a failed tool no longer stays in `Error`: the runtime's live status counts a tool failure only when the model did not recover from it. The agent loop keeps the tools whose last call failed and reports them once, at the end of the turn — a tool that works again leaves no mark (the recovery is counted per tool, not per call), so a turn that stumbled (a wrong argument, a path that was not there) and carried on ends as the turn it was; a failure that stands still leaves the topic in error, and the next turn starts clean as before. A topic closed mid-turn is not brought back as failed by the report, nor by the turn's end (#962)

- The topic list no longer dies on a topic whose state dir is not registered: the scan that lists the topics pruned its pinned paths through `resolve_state_dir`, which panics when a topic has no registration — a lost adoption stays unregistered on purpose, so one such entry took the whole list, and with it the overview endpoint, down. The prune asks the registry instead: a topic without a registration is kept in the map but left out of the names (its state cannot be read, so it is not a topic the list can report) instead of taking the list with it (#958)

- `/info` no longer reports every topic as `Idle`: the status comes from the runtime's live status — written as turns start and end, and the one source every reader shares — so a topic that is working says `Processing` and a topic whose turn failed says `Error`. The dashboard's overview used to compute its own status from the event stream while `/info` read a hardcoded `Idle`, which is why the two surfaces disagreed; the live status is now the only place a status is decided, and the overview, the info pane, `/info` and the progress frame all report it (#958)

- `jyc_send_to_topic` no longer refuses a `topic` that names the agent it is addressed to: an agent's own topic is named after the agent, so `agent="c21-sfm-reuse-lib"` with `topic="c21-sfm-reuse-lib"` names the default topic, not a named one — the two spellings now mean the same thing, and the message lands on the agent's own topic, created on demand exactly like the `topic`-less form. The refusal stays for a *named* topic — one whose name differs from the agent's — when nobody owns it, since re-creating it under the same name elsewhere would collide with state dirs keyed by topic name process-wide. The tool's `topic` parameter now says which name counts as the agent's own, and the confirmation names the destination the way the `topic`-less form does (#957)

### Removed

- The one-time `agent-context.json` → `agent-context.jsonl` conversion is gone. The journal is the only context store: a pre-journal array file is neither read nor converted, and it is left where it is (nothing replaces it, so deleting it would destroy a conversation). A deployment still on the array format therefore loses its raw context on the next load — the migration had already been a no-op for every deployment that had run since the journal landed (#986)

- The explicit `[channels.<name>]` form with `type = "websocket"` is gone: channels are forwarders owned by the pipe process, and the only websocket channel in the hub is the one synthesized from `[agents.<name>]`. Config validation now rejects the old form outright — the startup deprecation warning is gone with it, and so is the pattern-level configuration (template, topic_path, skills, …) that only such a channel could carry. Declare the agent as `[agents.<name>]` instead: each entry becomes one pattern on the synthesized "agents" channel, its name being the agent name (#967)

- The info pane's `Channel:` row is gone, and `/info` no longer prints one: every topic of a channel shares its channel, and the pane it sits in already says which channel it shows. The topic's detail keeps the field — it is the topic's identity in the tree and on the wire (#964)

- The auto-reset machinery is gone: the post-loop auto-reset (the session was reset once the context crossed `context_window × auto_reset_threshold`), the pre-loop pre-check (the session was reset when the active model had a smaller window than the loaded one), the mid-loop safety net (the in-memory context was shrunk to the last 3 turns before the next call) and the reset-compression modes (`heuristic` / `llm` / `none` — a reset now simply clears the context, with no summary and no compaction call). The sliding-window context strategy is what keeps a request inside the model's window, so the layers underneath it had nothing left to do; they never fired in practice anyway. `auto_reset_threshold` and `reset_compression` are no longer read — the per-pattern and per-agent keys are still accepted (ignored) so configs written before the removal keep loading, and the `[ai]`-level keys are gone

## [0.6.0] - 2026-10-19

### Added

- The agent is told which CLI helpers this machine actually has, and nothing else: `rg`, `fd`, `fzf` and `jq` are looked up on `PATH` once per process (by walking it, no shell spawn) and the ones that exist are announced in a short `## CLI Helpers` section of the system prompt, so a model that only ever reaches for the tools its training named stops running `grep -r` and `find` on a box that has ripgrep and fd installed. No built-in tool is replaced — `grep`, `glob` and `read` stay the Rust implementations. A helper that is not installed is never mentioned, so the prompt cannot send the agent after a tool that is not there; the section reads off a const list, and adding a helper is one line there. `fzf` is annotated non-interactive only (`fzf --filter <query>` on stdin) because bare `fzf` opens a TUI that would hang on the `bash` tool's TTY-less pipe. `bat` and `delta` are left out deliberately: bat's highlighting needs a TTY (it degrades to `cat` or leaks ANSI escapes into the model's input) and delta is a git pager, which never runs on this path (#941)

### Changed

- The dashboard overview's topic table leads with a topic's two identities and drops the channel column: the columns are now `Agent`, `Topic`, `Pattern`, `Status`, `Context`, `Last Active`, where `Agent` is the first path segment of the topic's home under the agents dir (`<agents dir>/<agent>/…`) — the same string as `Topic` for an agent-keyed topic, and a `-` for one whose home cannot be resolved. The `Agent` value is new on `TopicSummary`, filled server-side from the state dir registered under the topic's name (by name, not by `topic_path`, so a pinned topic still resolves through the home its state lives in). `Channel` went because every topic in the overview sits on one channel — on a hub that is the synthesized `agents` channel — so the column repeated itself on every row (#954)

- The dashboard overview's channels bar is gone, and the one thing it still had to say survives as a single unbordered line at the top, shown only while it applies: `Not connected: …` when the overview cannot reach the server, `Connecting...` before the first poll lands. A loaded overview spends no line on it and the table starts at the top of the screen. The bar was showing the channels *this* process runs, which on a hub is the single synthesized `agents` channel — the real channels (feishu, github, email, wecom) run in the pipe process, and the hub knows neither their worker counts nor whether they are up, so the bar was reporting its own process's configuration while looking like a system overview. Nothing about the pipe was read to remove it: the bar's data was already in hand (#954)

- The dashboard overview shows the chat screen's panes instead of its own set of lookalikes: the topic info pane (with the chat screen's border, rows and status block) sits beside the topics table, and the activity pane runs across the bottom, both rendered by the chat module's own functions. The overview's separate details box — a second rendering of the same topic rows, with its own skills/status/last-active lines — and its bottom status bar are gone; the version and server stats the bar carried live in the info pane's status block, the one the chat screen pins to the bottom of that column. Pane focus is one enum and one value for both screens (`Focus`, the chat screen's panes plus the overview's table), so `Tab` and the scroll keys (`j`/`k`, `↑`/`↓`, `PgUp`/`PgDn`, `G`, `gg`) mean the same thing on either; the activity pane stays keyboard-only, the overview does not answer the wheel. The two screens keep their own pane visibility: the overview's three panes are always there and it offers no way to close them, while `Ctrl+P` `i` / `a` / `s` stays a chat-screen command over the chat screen's own flags, restored to their defaults (info pane and status block shown, activity pane hidden) each time a chat is opened (#952)

- Code blocks in the dashboard chat render as plain text: `tui-markdown` is built without its default `highlight-code` feature, which takes `syntect`, `onig` and `onig_sys` (a C regex engine) and `plist` out of the `jyc` link graph. The highlighter colored the source the agent had just written, in a pane whose surrounding output is already dimmed — trading that for a smaller binary that every install carries (#945)

- The agent's HTTP client no longer carries a character-set decoding table: `charset` is dropped from the `reqwest` features `jyc-agent` lists, so `encoding_rs` leaves the `jyc` link graph. It had been re-listed only to keep parity with `reqwest`'s defaults, but everything jyc fetches over HTTP is UTF-8 — LLM APIs, the local inspect server and MCP's streamable HTTP all speak JSON — so the table was linked in and never consulted (#945)

- The `## CLI Helpers` section is now phrased as an instruction rather than a suggestion: the helpers were announced with "using them is your call, not an instruction", and the model read that section, quoted it back verbatim, and still ran `grep -r` on a machine that has ripgrep installed — announced is not the same as acted on. The section now says the model MUST use a listed helper when it covers the task better, and to fall back to the standard tools only when no listed helper fits; the per-helper caveats (`fzf`'s non-interactive-only rule) are unchanged (#944)

- A subcommand's values are declared once instead of twice: `/context`, `/thinking`, `/backlog`, `/skill`, `/mcp`, `/bill`, `/model`, `/template` and the flag-row levels were written both as `Shape::Subcommand::values` (the usage line) and as a `command_args` branch (the `/` popup), with a guard test asserting the two lists agreed — the duplication was visible enough to need a test to hold it together. One declaration per command now carries the key, the description the popup shows and what the key opens; the popup resolves it against per-topic state and the usage line names the same keys, so the two cannot drift and adding a value is one edit. Every usage line reads out the same words as before, except `/context`: it offers `sliding` first, where it read `full | sliding`, in both the popup and the usage line (#940)

- The tool descriptions that ride in every request's cached prompt prefix got 17.4% lighter: the built-in tools' natural language went from 10,108 to 8,349 characters (those 20 tool files; the 21 tools the budget covers, bridge tool included, run about 9,000), cut from examples and prose that repeated what a parameter description already said, while every tool name, parameter name, type and `required` list stayed byte-identical — the model calls exactly the same tools with exactly the same arguments as before. The heavy hitters were `jyc_spawn_subagent` (whose description restated its own `type` parameter), `jyc_send_to_topic`'s six parameters and `ask_user`; the highest-frequency tools that were already terse (`read`, `write`, `edit`, `grep`, `webfetch`) are untouched, since a shorter `read` schema buys nothing if the model starts guessing at line ranges, while `bash` and `glob` took only a light trim. `jyc_send_message`, the MCP *bridge* tool `register_mcp_tools` adds to every registry, was trimmed lightly too and is now covered. A test derives the expected tool set from the same registry-building calls the agent service makes — so registering a tool without budgeting it fails — and budgets the text per tool and in total (`crates/jyc-agent/tests/tool_schema_budget.rs`), making a growing schema a visible act: tool descriptions are part of the prefix that #935 made cache-stable, so a description edit costs one round of cache misses and belongs in its own change. `jyc_spawn_subagent`'s `type` is the one description not budgeted — it ends with the role catalog of the machine the agent runs on, so its length is configuration. MCP *server* tools remain passed through untouched (#938)

- The system prompt no longer changes when the instance's topics come and go, so the providers that cache by prefix can reuse it across turns: the subagent roster (`Your subagents (N): …`) and the channel/topic roster (`Available channels and their active topics:`) moved out of it into a `<runtime-state>` block (`build_turn_reference()`) prepended to the current turn's user message — the one block that is new every turn anyway, so a topic appearing or closing no longer invalidates the cached prefix, while the same information still reaches the model on every turn. `build_system_prompt()` keeps what changes only when config or files do: access boundaries, skills, `AGENTS.md`, the reply/history/VCS rules and the static cross-topic prose; the mode block was already carried on the user message for the same reason. The block is framed as an auto-generated snapshot that is not user text and may go stale within a long turn, matching the `<mode>` block's existing precedent. On a busy topic the first call of each turn was paying ~53-64k input tokens at a 0% cache hit while follow-up calls inside the turn hit over 90% — the standing context, not the loop, was where the uncached half of the bill lived, and a topic list that rewrites itself several times a day (topics appearing and closing) was knocking that prefix over. Spawning or closing a child still moves the prompt — the child's directory is an access grant and the boundary section has to list exactly the roots the tool layer enforces — so what leaves the prompt is the churn that changes no access. The `System prompt built` trace line gains a `prompt_hash` as the tripwire: on an idle topic, absent any `/grant` or spawn/close, it must not move (#935)

- A spawn hands its child the model its parent is running as the *floor* every model slot falls back to, and a role's own value only replaces it where it resolves: `[ai] model` in `model-override`, `plan_model` in `plan-model-override`, `build_model` in `build-model-override`, each resolved at spawn against the same model config the chain itself resolves against (`[ai]` with the channel's own model layer applied, `derive_agent_config`; `resolve_model_reference`, the one resolver every caller shares). Before, a role that named a model of its own got no snapshot at all, and a role model written as `${ai.models.x}` with no `[ai.models].x` defined therefore reached the runtime as an unresolvable reference — reported at error level every turn and treated as no override, so the child silently ran the configured default instead of the model its caller already had. That case now keeps the parent's model and logs the slot that did not resolve, as does a value that resolves to nothing (`model = ""`); the child's `config.toml` still carries the role's own value, reference included, so what the role asked for stays readable beside what took effect (#933)

- The raw provider context is an append-only journal instead of a whole-file JSON array: `.jyc/agent-context.jsonl` holds one message per line, and turn-end saves append only the new tail (the in-memory prefix is already on disk) rather than re-serializing and rewriting up to tens of MB every turn; rewrites still happen via tmp file + rename, and compaction paths rewrite in full as before. Reading is journaled too: `load_context` keeps an in-process parse cache keyed by topic (invalidated by file len + mtime and by every write through the single writer), so a long-lived topic no longer re-reads and re-parses its whole history per turn — the cost that showed up in `Turn setup` on topics with a 50 MB+ context. A torn tail line from a crash mid-append is dropped on read, and a journal missing its one assistant message is deleted exactly like the old corrupt-file case. The legacy `agent-context.json` migrates once on first load (converted, then removed; the migration lives in `jyc-agent::session::read_context_values`) and reset paths delete both names; a stale legacy file without a journal is never inherited by a spawn. Rolling back to an older binary after migration starts the topic with an empty context — the old build cannot read the journal — so back up the array file before a downgrade (#930)

### Added

- `max_history_tokens` bounds the two history regions (① compacted + ② verbatim) by weight, the lever `window` cannot be: `window = 10` costs 2k tokens on a quiet topic and 200k on one whose turns carry megabyte-sized tool results. Over budget the oldest history goes first — region ① sheds its oldest messages (the history marker survives, it is what explains the block), then region ② sheds its oldest *whole* turns, because a partial cut leaves a tool result whose `tool_use` is gone and providers reject that. Region ③ is deliberately outside the budget: it is the turn in flight and `turn_tool_result_cap` is its lever. The estimate is a heuristic rather than a tokenizer — CJK characters ×1 token, every other byte ÷4, plus a small per-message constant — because a tokenizer dependency would buy precision this knob does not need, and a plain bytes÷4 estimate would undercount this project's mixed CJK/English transcripts by a quarter. Settable from config (`context_strategy = { …, max_history_tokens = 30000 }`) or at runtime as the fifth positional of `/context sliding N M CAP TURNCAP MAXHIST`, where `0` switches it off and omitting it falls back to the configured value. Unset by default: without it the wire payload is byte-for-byte what it was. (#937)

- `turn_tool_result_cap` bounds the current turn (context region ③), which `tool_result_cap` deliberately left uncapped: an oversized tool result keeps its head and tail with the middle replaced by `… [N bytes dropped from the middle — re-read with offset/limit to see the gap] …`, since a result is usually read for what sits at its ends. Unset by default, so nothing is cut until asked. Settable from config (`context_strategy = { …, turn_tool_result_cap = 32768 }`) or at runtime as the fourth positional of `/context sliding N M CAP TURNCAP`, where `0` switches it off and omitting it keeps the configured value. Unlike `tool_result_cap`, whose marker sits on top of the cap, this one counts the marker inside it. (#936)

- `[ai.models]` defines named model slots, referenced as `"${ai.models.<name>}"` from any place the model chain picks a value: `[agents.<name>].model`, a subagent type's `[ai].model`, a pattern, a `[channels.<name>]` table or a topic's own config. Names are letters, digits, `_` and `-`, and the table takes any number of entries, so a new shared model needs no code change — the previous resolver knew exactly `model` and `small_model`. `resolve_model_reference` now also resolves `${ai.plan_model}` and `${ai.build_model}`, and every existing guard holds for the new names: a name nobody defined, an empty slot value, or a slot whose value is itself a reference is an error naming the key, never a literal id handed to a provider. The slot table lives in the global `[ai]` only — a subagent type references it from its own `[ai]` and resolves against the global values. The two places that build the `small_model` provider directly (turn setup and `reset_session` compression, both in `jyc-agent`) now resolve the reference too: before, a `small_model` written as `${ai.small_model}` reached the provider as literal text — a warning every turn and a silent fall back to the main model, or a silently skipped compression model (#932)

- The latency row reports the **latest turn**'s average next to the session's: `agent-session.json` grows `turn_total_call_duration_ms` / `turn_call_count`, accumulated by `persist_tokens` in lockstep with the session pair and cleared at every turn start by a new `jyc_agent::session::begin_turn_latency` (called from the turn setup right after `ensure_session_file`, and writing nothing when the counters are already zero, so a turn pays a write only when there is something to clear). `read_call_latency` now returns a `CallLatency` (a pair per scope instead of one tuple), `TopicSummary` / `TopicInfo` carry `avg_turn_call_ms` + `turn_call_count`, and both surfaces format through one shared `jyc_core::duration::format_latency`, so the wording cannot drift: `Latency: 1.10s/call (3) · 820ms/call (37)` — latest turn first, session second, each figure followed by the number of calls it averages over. A turn that has made no measured call yet (just started, or the session was reset and its counters rebuilt at zero) collapses the row to the session figure, and a session with no measured call still renders no row rather than `0ms` (#931)

- The topic info payload now carries how long one API call takes: `TopicInfo` and `TopicSummary` gain `avg_call_ms` and `call_count`, read in `list_topics` through a new single-purpose `jyc_core::session_state::read_call_latency` — the `read_session_cost` shape, a separate reader rather than a wider tuple. The agent loop times each main-loop call with one `Instant` around `complete_with_retry` (retries and their backoff included, since that is time the user actually waited) and hands that duration to `persist_tokens`, which adds it to `total_call_duration_ms` and bumps `call_count` exactly the way the same call's cost is added to `session_cost`; ancillary calls (compression, cycle summaries) are billed but not timed, and the post-loop `update_tokens` passes `0` so nothing is counted twice. All three renderers read those same two fields — the chat info pane and the dashboard topic info area through a new `push_latency_span` in `token_render`, and `/info`'s text with the same words — as `Latency: 820ms/call (avg of 37)`, formatted by a new `DurationStyle::Millis` in the one duration formatter (milliseconds below a second, seconds above, so a sub-second average never reads as `0s`). The row stays absent until a call has been measured, the same rule the cache rows follow, so a fresh session shows no `0ms` (#924)

- `/close` can close a subagent: `/close <subagent>` (or `/close all`) sends that topic the `/close --force` it would run itself, so the *child* closes on its own queue — its directory rules, purge guards and subtree come along exactly as when it closes itself — and the parent never touches a child's directory. Only a topic's own direct subagents can be named (anything else is refused with the list), the guard is the one closing self has (`/close joe` warns, `/close joe --force` sends), and `all` is a keyword: a subagent actually called `all` goes in the batch rather than by name. The `/` popup offers the subagent names, `all`, a space row — completing it writes `/close  `, the bare form that closes the topic itself — and the same flag level under every name; `/send` now offers the agents it can address (the agents channel's active topics, the same list the prompt shows) with its flags, since `/send` addresses any agent while `/close` accepts only its own children (#918)

- A `/send` command reaches another agent by name: `/send <agent> [--topic|-t <topic>] [--reply] [--attach <file>] [--recipient <id>] <message>`. It is the command form of `jyc_send_to_topic` and shares its implementation — both parse their own input and call the one `jyc_core::command::send_handler::send_to_topic`, the way `/spawn` and the spawn tool share `spawn_handler` — and it is one row in the built-in table, so `/?`, the `/` popup and the inspect server pick it up from there (#917)

- `jyc plugins install <archive>` installs a **plugin bundle** into the config dir: the user-editable collections this repo ships — `skills/`, `subagents/`, `hooks/` and `templates/` — packed as a gzipped tar with those directories at its root. Every release builds one as `dist/jyc-plugins.tar.gz` and publishes it with both the stable and the nightly release (the Gitee mirror carries it too), so a machine can go from a bare install to the repo's skills, subagent roles, hook scripts and templates in one command. A plain directory laid out the same way is accepted as well, which is what makes a bundle installable before it is ever packed. `skills/`, `subagents/` and `templates/` go through one subdirectory-per-name, replaced-cleanly path — the semantics the per-collection installers had — while `hooks/` is copied file by file, since its entries are flat shell scripts rather than one directory per name. A collection the bundle omits is skipped, a directory the command does not know (a repo's `scripts/`, say) is named in a log line and ignored, and a source holding none of the four refuses with the layout it wanted. Two shapes are refused outright because they would lose data: a source that *is* the target (`jyc plugins install ~/.config/jyc` — an entry is removed before it is copied, so the source would vanish with it) and a target inside the source (the copy would walk into what it is writing) (#916)
- TUI: `[` on an empty chat input hands the keyboard to the message pane and lands on the previous user turn — the jump `[` already makes there, reached without a `Tab` first — and the bracket is not typed into the field. With a draft in the input it stays an ordinary character and the focus does not move, and only a bare `[` is special (no counts, no `]`, no Shift form). Since focus moved, the character typed next is consumed by the refocus — `[ ] todo` arrives as `] todo`, the same loss `Ctrl+P c` already had (#906)
- The repo ships a third subagent type: `subagents/develop/config.toml` is a developer — build mode, `dev-workflow` as its whole skill allow-list, and deliberately **no** `disabled_tools`, because this is the role that has to write and edit files, run `bash` for the gate and `git`, and spawn the `review` and `ci-watch` children that judge it (mode, the allow-list and its own instructions are what restrain it, not a tool list). No `[spawn]` table either: it wants the framework defaults — a directory of its own, and a conversation that starts empty, since the plan arrives as the task. Its instructions make it the implementer of the caller's plan rather than its author (write the plan into a task list first, stay at the compile gate tier, delegate the review before pushing and the CI wait after, report only the PR link and the two verdicts, never merge, and read its own report as the end of the turn rather than of the engagement — the caller keeps it for the user's decision on the branch and may send a change to make on the same one). `cp -r subagents/develop <config dir>/subagents/`, then `jyc_spawn_subagent(name = "develop-<what>", type = "develop")`. A test pins that file through the real installed-type path — it loads, `description` is there, `[spawn]` is not, the model is pinned, the instructions are substantive — and the prose assertions in that module collapse the role file's line wrapping before matching a phrase, since a sentence that happens to break across two lines otherwise reads as a missing instruction (#905)
- A new skill, `delegated-dev`, covers handing a settled plan to a `develop` child: when it is worth the hop (more than one file, changed logic, a PR — not a typo), what to pass the spawn (nothing: not `inherit_context`, not `share_topic_path`), and how to give the child a working directory of its own — `git worktree add <child dir>/repo -b <branch>`, **inside** the child's directory rather than as the directory itself, because the child's state dir `.jyc` lives there and a checkout at the top would put `.jyc` inside the repository. It also says who clears that up and when — yourself, **before** the child is closed, and only once the **user** is done with the branch (accepted or abandoned), never on the strength of the child's report, which is evidence for the user to judge and not a verdict of its own: once it is gone a message to that name starts a *different* topic (one that would even adopt the leftover directory), and that order is also what gets the child's directory deleted, since an emptied topic dir goes with its state — closing first leaves a directory holding only the checkout, which keeps the name unusable for a later spawn and is nothing `git worktree prune` clears (prune only drops entries whose directory is already gone). The task message carries two things besides the plan, since a fresh worktree is not the main checkout: a target dir outside it (`CARGO_TARGET_DIR=…`, or the first gate re-pays the whole dependency graph into a copy), and the checkout path for every `review`/`ci-watch` task the development child sends. Nothing in it is framework knowledge: jyc only gives a child a directory of its own and lets the parent write into it, so a project that is not git prepares that directory another way (#905)

- A topic's own config can carry a context strategy: `[context_strategy]` in `<topic>/.jyc/config.toml` (or in a subagent type, which is expanded into that file at spawn time) sets the window size, the tool-result cap and the note window for that topic. It sits at the same layer as `mode` — above the pattern/channel/global `[ai].context_strategy`, below the runtime `.jyc/context-strategy.json` that `/context` writes — so a role can start small without overriding a user's explicit `/context` choice
- The repo ships a second subagent type: `subagents/ci-watch/config.toml` waits out something slow that only finishes later — CI on a PR, a deploy, a long batch job — and reports when it lands. `cp -r subagents/ci-watch <config dir>/subagents/` once, then a parent hands the wait off (`jyc_spawn_subagent(name = "ci-watch-<what>", type = "ci-watch")`, then the task with `require_reply = true`), and the child polls on `bash` — `gh run list --branch <branch> --limit 1 --json databaseId,headSha,status,conclusion,workflowName`, with the sleep inside the call, ~30s between polls and a ~30 minute budget after which it reports what it has and stops — so no turn has to stay open to learn how a run ended. It starts in plan mode on the small model (`[ai] model = "${ai.small_model}"`, the first user of the model references above), with no `write`/`edit` and no delegation of its own, and in the caller's directory (its `[spawn]`), which is what identifies the branch or repository it is watching. Its report is evidence and nothing more — the role is told not to diagnose a failure, guess a cause or plan the caller's next move — and it carries its own context strategy (`window = 3`, `note_window = 0`, `tool_result_cap = 512`), so a polled `gh` output cannot pile up in the context it polls with — a *finished* turn, that is: the turn being run is never truncated, which is why the poll prints one compact JSON line rather than a check table and why a failure log is filtered and tailed inside the call that fetches it. `workflowName` is in that line because `--limit 1` is the newest run on the ref of *any* workflow (`main` carries several per commit), so the instructions pin the workflow with `--workflow` where the caller named one, name where the current head comes from (`gh pr view --json headRefOid`, or `git rev-parse HEAD` in the checkout), and prefer `gh run list --commit <sha>` where the PR is known (with the full 40-character SHA: an abbreviated one returns an empty list rather than an error), and require the newest run to be reported as superseded when its own head SHA is not that head. The report carries the head SHA **the run itself reports**, and the role may state no fact **about the run** — head SHA, run id, workflow, quoted lines, verdict — that is not in a tool result from that same turn: a head it remembers from an earlier poll, or one the caller mentioned in a message, is no evidence about what ran
- A model value may reference the `[ai]` value it would otherwise repeat: `model = "${ai.small_model}"` (or `"${ai.model}"`) resolves at the end of the model chain, so an agent entry or a subagent type can say "run on the small model" and one change to `[ai].small_model` moves every role that asked for it. Only those two names resolve; anything else — an unset referent, an unknown name, a referent that is itself a reference — is an error naming the reference, not text handed to a provider (#903)
- A subagent type can now say how it should be spawned and what it is for: an installed `<config dir>/subagents/<type>/config.toml` takes `description` — one line of "when to spawn me", listed beside each type's name in the `jyc_spawn_subagent` tool's own `type` argument, so a parent that was never told about a role can still recognize the moment for it — and `[spawn]`, the role's defaults for the two start flags (`inherit_context`, `share_topic_path`). An explicit `inherit_context` / `share_topic_path` in the tool call still wins over both (the `/spawn` command has only `--inherit`, which can turn it on), so `jyc_spawn_subagent(type = "review")` alone now borrows the caller's directory — the shipped reviewer declares `share_topic_path = true` in its own file — and `/spawn --type=review` gets the same default. The inline `[subagents.<type>]` form stays a plain topic config and carries neither key (#902)
- The TUI draws topic lineage as a tree in both panes: the chat explorer indents a subagent under the topic that spawned it, and the info pane gains a `Subagents` section listing the selected topic's whole subtree (`├─` / `└─` nesting, with the same status dot and colours the explorer uses), as deep as the lineage goes. `TopicSummary` gains `parent`, read server-side from the `spawned-from` breadcrumb in the topic's state dir — the same file the subagent grants are validated against — so the client builds the tree from the topic list it already polls, with no extra request. A topic whose parent is no longer in the list (the parent was closed, or the child was spawned from something outside it) stays a root instead of vanishing, and the explorer's selection index follows the tree rows in the order they are drawn (#900)
- A spawn can now hand its child a **role** by name: `jyc_spawn_subagent(type = "review")` (or `/spawn --type=review`) expands that type's settings into the child's own `.jyc/config.toml`, so a delegated reviewer starts in plan mode, with the review skills as an allow-list and without the tools that could change the files it is reviewing — a structural constraint, not an instruction it may talk itself out of. A type is a topic config selected by name, defined either by `[subagents.<type>]` in the config or by an installed `<config dir>/subagents/<type>/config.toml` (same one-directory-per-entry shape as `skills/` and `templates/` — though unlike those, a type is L1-only and not layered); defined in both, they merge key by key with the `[subagents.<type>]` table winning. The name is resolved against the config the instance is *running* with — the one `--config` pointed at, and one a reload updated, not a fresh read of the default path — and only the two entry points that hold it resolve it (`/spawn`'s command context, the spawn tool's shared config), so both answers agree. Names are validated as single directory components, an unknown type refuses before anything is created and lists the types that do exist, and a malformed installed type names its file instead of being ignored. A type set its child *starts* with, not a cage: the child's own `/mode`, `/model` and `/skill` still win over it, and the parent's runtime overrides — which outrank a topic config in every chain — are dropped for the keys the type sets itself, so the role is not silently undone where it has an opinion of its own. The child's state dir records the type name (`subagent-type`); nothing reads it yet (the dashboard is where it is headed), and no behavior depends on it (#899)
- The jyc repo ships a subagent type: `subagents/review/config.toml` is a read-only reviewer — plan mode, the review skills (`pr-review`, `ponytail-review`, `coding-principles`, `ponytail`) as its allow-list, no `write`/`edit` and no further delegation. `cp -r subagents/review <config dir>/subagents/` once, then `jyc_spawn_subagent(type = "review")` delegates a review to a child that reads the branch as it is — uncommitted, or the commits just made — and cannot change it. `bash` is deliberately left in place (a review needs `git status`, `git diff` and `git log`, and no read-only shell exists), so plan mode and the role's own instructions are what restrain it — the file-editing tools are absent, which is the part wording cannot undo (#899)
- A topic config (`.jyc/config.toml`) can now set more than models and MCPs: `mode` starts the topic in plan or build, `skills` narrows it to an allow-list, `disabled_skills` and `disabled_tools` subtract, and `instructions` adds a `## Topic Instructions` section to its system prompt. This is the layer a subagent type expands into, and it is available to any topic: a single topic can now be read-only, or see only two of the installed skills, without touching the channel or pattern it belongs to. Precedence matches the model chain the file already had — the runtime override files (`.jyc/mode-override`, `plan-model-override`, `/skill`, `/mcp`) stay above it, and a configured `mode` is deliberately *not* written to `.jyc/mode-override`, which remains what the user set by hand (#899)
- A subagent can now be given a job that has to see your files as they are: `jyc_spawn_subagent` takes `share_topic_path=true`, which puts the child in **your** working directory instead of one of its own — the `/fork` shape, and the only way a delegated review can read uncommitted work (a copy would review yesterday's tree, and a `git worktree` only what was committed). The child's state stays its own in `<agents dir>/<name>/.jyc`, so closing it deletes that state and leaves the shared directory — including your own in-dir `.jyc` — exactly as it was, and the purge guards read the pin's lineage, so a shared child never claims your directory as somebody else's. A closed parent now closes its subagents with it as well (depth-first, children first, each `agents/<child>/` shell pruned like any other close), so nothing is left pointing into a directory the parent's close may have taken (#897)
- Inheriting the conversation is now opt-in, on all three triggers: `/spawn --inherit`, `/fork --inherit`, and `jyc_spawn_subagent`'s `inherit_context=true`. The flag is read off the arguments rather than mistaken for a topic name, and the receipt says which side of the default it took (#897)
- Agents can delegate: the new `jyc_spawn_subagent` tool (agents channel only) creates a subagent topic — a sibling topic with a copy of the parent's state (task list, mode, pattern — its conversation only when the copy is asked for) in its own state dir under the agents root — and grants the parent read+write access to its topic_path (the working directory it runs in) through the existing runtime grant registry. The parent seeds that topic_path with `write` and sends the task with `jyc_send_to_topic` (`require_reply=true`), whose result arrives as a new message in the parent's topic; reconfiguring the child is a command away (send it `/build`, `/model`, … as the message body — an empty body stops before the model call, so it costs the child no turn). The system prompt lists the topic's current subagents on every turn so an existing one is continued instead of duplicated, and a closed subagent revokes the parent's access. Subagents nest: a subagent may spawn one of its own, and closing a topic closes the whole subtree. Grants are in-memory like `/grant`, so a hub restart drops the access (not the subagent)
- TUI: `ctrl+p space` switches the open topic between plan and build mode, by sending the server-side `/plan` or `/build` command (#889)
- TUI: the `ask_user` question box ends its option list with a `✎ type your own answer` row — a decision point sometimes needs words no option holds. `↑/↓` reach it like any other row, `Enter` opens a one-line draft (typed into at its end, `Backspace` edits it, the row shows its tail while it is longer than the box), `Enter` keeps it and moves on — the last question still sends the whole batch — and `Esc` hands the keys back to the list with the draft still on its row (the cursor stays there, so `Enter` reopens it; only an empty draft is dropped, so `Esc` cannot leave a blank answer behind). A typed answer travels on a line of its own with no option number (`Q2: their own words`), so the model can tell it from a pick, each question keeps its own draft while `←/→` steps around the batch, picking an option replaces a draft (and coming back to a question reopens it for editing; `d` after typing still wins, being the more explicit "no answer"), and the box's hint changes with the state, measured by the same call that sizes the box (#923)
- `ask_user` questions carry the assistant's context with them: `QuestionRequest` gains an optional `context` (the narration for the turn that asked), `ask_user` fills it from the agent loop, the websocket frame passes it through, and the TUI question box draws it dim above the question — wrapped and capped at ten rows, measured like the rest of the box. A box showing only the question left the user guessing what was being asked, and the same text delivered as a separate mid-turn message could go missing; plain-text channels (feishu, wecom_bot, email, …) are unchanged, since there the context is the message before the question (#890)

### Changed

- An activity entry now carries the raw event instead of operator prose: `ActivityEntry` replaces the backend-composed `text` (and the `severity` / `is_internal` decisions the backend made for its clients) with the event itself — `event` holds the serialized `TopicEvent` (the variant is its `type` field) alongside the transport envelope (`timestamp`, monotonic `id`). The inspect server's `event_to_activity` shrank from a ~40-arm sentence composer to a converter that stores what happened, and neither the REST `GET /api/topics/{channel}/{topic}/activity` response nor the dashboard WebSocket's `activity` frame filters visibility any more — the backend records facts, each client decides what to render. The dashboard rebuilds the exact lines the old backend produced (wording, ordering, and the red/yellow error styling all unchanged) from the raw event at render time — severity and heartbeat-visibility included — so every pane reads as before while other consumers get the structured data the prose used to throw away; the edit/write entries that smuggled a JSON document inside the text field (the workaround that let the diff view parse it back out) disappear with it, since the event now *is* structured. High-frequency heartbeats (`ProcessingProgress`, `LoopTick`) are still dropped at write time — a log-volume decision, unchanged — and rows written before this change keep their prose: `text` survives on the struct (absent for new entries) and clients render it verbatim when no `event` is present, so old `activity.jsonl` files read cleanly until rotation retires them (#929)
- The dashboard's per-topic `git` status no longer rides the overview poll: `GET /api/state/overview` stops resolving `branch` and `changed_files` for every topic (a `.git/HEAD` read and up to four `git diff` runs each, which is what made one poll cost seconds), and a new `GET /api/topics/{channel}/{topic}/info` carries them for one topic. The client fetches that for the topic the screen shows — the visible chat's own topic, else the row under the cursor, the same rule both renderers already use — and stamps the two fields onto the summary it renders, so the panes read exactly the rows they read before (#927)

- Listing topic names no longer resolves each topic's state. The system prompt's per-turn, per-channel topic list uses the new `list_topic_names` — the scan half of `list_topics`, names only — and the inspect server's two-second activity poll uses `list_topic_names_and_paths`, so neither reads a session file, the billing ledger, the skills, or runs `git` to produce a list of names (`/info` and the dashboard overview keep the full listing). On a live instance that list was rebuilt every turn for every channel, and each rebuild spawned `git` synchronously inside an async task (#925)

- A command's arguments are declared once, on its row in the built-in table (`jyc_core::command::builtin`): the same row that already named the command and described it now also declares its **shape** (`command/spec.rs` — no arguments, fixed positionals, one positional then trailing text, or a subcommand) and hands the table its flags, so the parser a handler reads its line with, the `/` popup's levels and the usage line all come from one declaration. The usage line is now **derived** rather than written into the description by hand (`/close [subagent|all] [--force] [--purge]`, `/send [--topic <topic>] [--reply] [--attach <file>] [--recipient <id>] <agent> [--] <message>`), which is why `/?` and the popup put the flags in their real positions, why `/send`'s `--` is advertised at all, and why `/new`, `/reset` and `/grant` no longer say "requires --force" or "‑w write" in prose — `/grant`'s `-w`/`-p` are the one spelling pair that now appear nowhere, since that command has no popup level and the derived line prints long names (its `--write`/`--persist` still work). Five refusals are now worded once for every command — an unknown spelling (`unknown flag --froce (known: --force, --purge)`, and `(expected: show|hide)` for `/thinking` and `/bill`, which declare no flag of their own), a value that did not arrive, a repeat the declaration forbids, a value handed to a switch (`--inherit=false`) and a word too many — so a line that used to be quietly ignored is refused: `/close --froce` warned instead of closing, `/close joe ann` closed `joe` and dropped `ann`, `/model a b` ignored `b`, and `/skill on a b` took `a` as the name. `/close`, `/new`, `/reset` and `/template` had no refusal at all for an unknown spelling; `/model`, `/thinking` and `/bill` had one of their own, which the shared sentence replaces. `/context` and `/backlog` keep their old sentences and their own reading of the line — their shapes are declared for the popup and the usage line, not yet read back by a parser. A lone `-` stays a word where the shape keeps text (`/send joe -` sends a dash, `/spawn -n -` names one) and is a dash-word nobody declared everywhere else, where before it was skipped — `/close -`, `/new -` and `/grant -` are refused rather than read as "no flag given" or as a path called `-`. A flag's value may now ride along in the same token everywhere (`--topic=invoices`, `-n=scratch`), not only under `/spawn`. What a value *means*, which arguments are required, and every other sentence stay with the command (#922)

- Every command's flags are declared once (`jyc_core::command::flags`): the TUI's `/` popup builds its flag level from the same table each handler reads its command line through, so a flag cannot be offered in the popup and refused by the parser — and a short spelling is no longer prose in a description but part of the declaration. `/close`, `/new` and `/reset` now take `-f` with `--force`, and `/close -p` with `--purge`, the way `/spawn` and `/send` already took theirs: before, `-f` was silently read as "no guard given", so `/close joe -f` warned instead of closing (#921)

- A close takes the topic's **home** with it, whichever source closed it — `/close`, `/close <name>`, `/close all`, a close cascade, and an upstream close event off a channel (github/gitee) are one teardown now, deleting one thing in one place: the directory the topic's state was registered in, contents and all. A pipe topic (`<agents dir>/<pipe>/<topic>`) therefore goes with its `.data`, its `.attachments` and anything else written there, instead of leaving the dir behind the moment it held a single file, while the pipe's own dir (`<agents dir>/<pipe>/`) stays — nothing *above* a home is deleted any more, so an emptied parent is no longer pruned away either. Two dirs are still never taken: one **outside the agents tree** (a checkout a `-d` or a pin points at — only its state goes) and one **another topic works in or lives inside** (a `-s` child, a fork, the topics a self-named pipe topic shares its dir with — read off the disk, because the pin map only knows the topics that are live). `auto_close_topic` lost a rule of its own in the process (`is_dynamic_topic_dir`, the exact `<agent>/<topic>` shape) and keeps only the guard it always had: a topic whose working dir is not under the agents tree is skipped whole. `/close --force --purge` is unchanged — still the "and the directory it worked in too" for a pin (#920)

- `/send` reads flags only **before** the message, so a dash-word after the first message word is message text now: `/send joe hi --reply` used to set `--reply` and send `hi`, and now sends `hi --reply` as the body. Put the flags ahead of the text — and for a body that itself begins with a dash, separate it with `--`: `/send joe -- --reply something` (#918)

- `jyc_send_to_topic` addresses an **agent**, not a channel: `agent` (required) plus an optional `topic`, so a message goes to the agent's own topic by default (created when it does not exist yet) or to one of its topics with `topic`. The channel was only ever the lookup key, and it is now resolved rather than named — by looking, not by trusting config: the manager running the topic's queue, otherwise the channel the topic's own state records (`.jyc/topic-meta.json`, written on the message that first routed to it and backfilled on a later one when the record predates the key, which is what keeps a topic idle since the last restart addressable instead of making it wait for its next message), otherwise the agents channel for a name no topic owns. A named topic nobody owns is refused rather than re-created under the same name in another channel, the reply instruction a `require_reply` target reads now names the source **topic** — its name is the address — and the tool's confirmation says where the message actually went (#917)
- The running topic managers are one thing in the process rather than two spellings of the same information: `TopicManagers` (`jyc-core`) owns the set the `ChannelOrchestrator` publishes on every channel change, each manager holds a `Weak` back-pointer to it (a strong one would be an `Arc` cycle, so a command handler built with only its own manager can still resolve an addressee), and the derived channel-keyed `HashMap` the CLI built for the agent layer is gone — `ToolContext`, `AgentService`, the system prompt, the job scheduler and the inspect server (whose four hand-written `channel_name()` scans became `by_channel`) read the same instance (#917)
- The `ci-watch` role is a PR-check watcher again and nothing else. Its instructions ran to a hundred lines of run-chasing — which run is newest on the ref, which workflow a ref carrying several belongs to, the head SHA the run itself reports (as the **full 40 characters**, because an abbreviated one makes `gh run list --commit` an empty list rather than an error), what to do while no run exists yet — and they are now one call and one report: `gh pr checks <pr> --repo <owner/name> --watch`, read the table rather than the exit code (a check is final at pass / fail / skipping, and no checks reported is not a pass), and send it back with `jyc_send_to_topic` whether or not the caller asked, naming neither the failing check nor its log — the report is one line — the outcome, never the table — so the log is the caller's own query when it wants one. Which run, workflow or head a PR carries is the **caller's** business, not the watcher's: the role reads the check table of the PR it was handed and reports the verdict on it — pass or fail — and makes no claim about which head ran, so a green cannot come back mispaired, and a caller that wants one tied to the head it pushed asks for that head itself (`gh pr view <pr> --repo <owner/name> --json headRefOid`) and the runs for that commit (`gh run list --repo <owner/name> --commit <full sha>`) when the report lands. The role's header, `description`, the `dev-workflow` dispatch template and the installed-type test follow — the test now pins the orders that stayed (report on its own initiative, an empty reading is not a verdict, a call that printed no table is not a verdict either, no digging into a failure, never announce a poll instead of making it, a turn ends and nothing resumes it) while the run-identity assertions went with the text they quoted

- The `ci-watch` role keeps what it *is*, not only what it says. Trimming its instructions took its model reference (`${ai.small_model}`), its disabled-tool set (`write`, `edit`, `jyc_publish_file`, `jyc_spawn_subagent`) and its context strategy (`window = 0`, `note_window = 0`, `tool_result_cap = 512`) with it — the installed-type test caught the missing model reference and turned `main` red. A role's instructions and a role's configuration are two things: the first is what the role says, the second is what it runs on (#979)
- A topic's default working directory is now its **home's `.data`**, and a topic's home stays what it was: `<agents dir>/<agent>/<topic>/` for a pipe topic, `<agents dir>/<name>/` for an agent-keyed one (an agent's 1:1 topic, a `/spawn` child). The state dir does not move — `<home>/.jyc` — so the two are siblings, and **no working directory contains framework state at all**. The rules that read a topic's directory now read its home: `topic_dir_for` and `jyc_send_to_topic`'s mirror return `<home>/.data`, `activate_workspace_state` registers the home's `.jyc` (not `<dir>/.jyc`, which would have moved a state dir into the new working dir), and `is_dynamic_topic_dir` looks through the `.data` so an upstream close still takes a pipe topic's own dir with its contents. It is chosen once, at a topic's first activation (`topic_path::topic_work_dir`): a topic that already exists in an older layout — state in the home itself, or nested at `<home>/<topic>/.jyc` — keeps the directory it was created with, so an upgraded instance holds both layouts and nothing moves under an agent. `close_topic` removes a topic's own emptied working dir with its state (a rule superseded by #920: the home now goes with its contents), and the `DESIGN.md` terminology, state-dir table and layout diagram, `AGENTS.md`'s naming rules, `README.md` and `config.example.toml` follow the split (#912)
- Inbound attachments are saved into `<topic_path>/.attachments/` by default — dot-prefixed like `.jyc`, `.data` and `.inbound`, because a topic's working directory is often somebody's checkout and `attachments/` is a name a real project owns. A configured `save_path` still wins (absolute as given, relative under the working dir). Existing `attachments/` dirs are left where they are, and the shipped skills that read or exclude them follow: `github-init`/`gitee-init` now exclude `.attachments/` only (the `.jyc/` entry is gone — it can no longer appear in a working directory), `delegated-dev` prepares a child with `git worktree add <dir>` — the directory itself, since the child's state is a sibling rather than inside it — with the order constraint that the directory must be empty when the worktree is made, and `invoice-processing` reads `.attachments/` (its samples included) (#912)

- `/spawn`'s arguments are read as a command line, not as shapes: the subagent type is the one positional (`/spawn review`) and the rest are flags — `-i/--inherit`, `-s/--share`, `-n/--name <name>`, `-d/--dir <path>` — so `--type=<name>` is gone and a name or a directory is never guessed from a slash again. A name used to be whatever was not path-shaped, which made `/spawn scratch` a spawn and left no way to write a directory whose name had no slash in it; now a positional that is not a resolvable type refuses, listing the types that exist and saying that a name goes in `-n/--name` and a directory in `-d/--dir` — a mistyped name costs one turn instead of creating a topic nobody asked for. `/spawn -d ../test` still resolves against this topic's dir, a directory alone still names itself after its last component, and neither means `<this>-2` under the agents root. `-i/--inherit` and `-s/--share` are booleans with no negative form, so what the command line leaves unsaid falls to the role's own `[spawn]` defaults and to `false` after that — the precedence the tool already had, one rule for both entry points. The TUI's `/` popup gets the types a spawn accepts as `/spawn`'s first level — the same catalog the `jyc_spawn_subagent` tool describes its own `type` argument with, so a role reads the same wherever it is listed — with each type's declared start defaults spelled out in its line (`shares your directory`), which is the only place those defaults are visible: on the command line, not passing `-s` is exactly what lets a role's `share_topic_path = true` stand. The four flags sit beside the types and again under every one of them, so `/spawn --na` completes as well as `/spawn re` (#911)

- `/fork` is `/spawn -s/--share` now, lineage included: the child is the parent's subagent the way every other spawn is — a `spawned-from` breadcrumb in its state dir, the parent's read+write grant on its topic_path, a row in the parent's subagent list, and a `/close` that revokes the grant and that the parent's own close cascades into. A fork was deliberately outside all four: it wrote `forked-from` so no reader would count it as a subagent, which also made it an independent topic that outlived its parent. One mechanism, one marker, one lifecycle therefore — and `/spawn -s` takes a subagent type, which `/fork` never did. The state stays private either way (`<agents dir>/<name>/.jyc`, never in the shared dir), so closing a child touches nothing in the directory it borrowed (#911)

- CI's result is something a turn now arranges to receive, not something it may skip. `AGENTS.md`'s CI rules and `dev-workflow`'s CI section said "do not block a turn waiting for CI — hand over the PR link and stop", which reads as *the result is optional* when the turn that needs it is an ordinary one; the rule's point was never the result, it was that *you* must not hold your own turn open polling for it. The wait is now always set up — a `ci-watch` child by default (the earliest report; one child polling on the small model), or the older one-time `at` job when you would rather not hold a child open (one wake-up, cheaper, but an early failure is only known at the deadline) — and what stays forbidden is blocking: no `gh pr checks` / `gh run watch` polling, no `sleep` retries, no putting the push or the reply off because a run is still going. The one exception is a conclusion already in hand and about to be acted on (a merge): there, one check is allowed. The wait is arranged once, for the head the turn leaves behind — a push still to come supersedes the head you would otherwise be watching — and a PR whose head moves continues the watcher it already has (another task to it is a new turn in it) rather than spawning or scheduling a second one (#909)
- `dev-workflow`'s fix rule stops claiming that all fixes go to main first — it contradicted both AGENTS.md and this repo's own branch history, where `fix:` commits live inside `feat/*` PR branches. It now reads: a fix gets its own `fix/` branch off main, never smuggled into another feature's branch, and a follow-up to an open PR goes on that PR's branch — a pointer at AGENTS.md's PR rules, which are where that policy is stated, rather than a second copy of it. What AGENTS.md does not have — do not smuggle a fix into someone else's branch — is kept. The `develop` role file, which quoted the old sentence, quotes the new one (#906)
- `dev-workflow` gains the rule that a delegated developer needs: work only in the directory you were given — for a developer that is a checkout its caller prepared (this repo: a `git worktree`, usually `repo/` inside the topic directory, with the `git diff` base named in the task message) — and never create or remove a checkout yourself, since the caller does both and a closed child cannot be sent the command that would (#905)
- The `forked-review` skill is gone; what it carried is now part of `dev-workflow`, which gains the delegated-review procedure (spawn `type = "review"`, the task to send, the rules, and a pointer at the role file instead of the role's keys spelled out a second time) and, in its CI section, the `ci-watch` hand-off — one skill now covers the flow from branch to pushed PR, and the task it says to send names the repo and asks for the head SHA the run itself reports, since the head can move while the watcher watches. Its `description` absorbs the review and CI-waiting triggers so the requests still find it, and the places that named the old skill (`AGENTS.md`, `README.md`, `config.example.toml`) name `dev-workflow`. The reviewer role's deny-list follows the rename: `disabled_skills = ["dev-workflow"]` — belt-and-braces over that role's `skills` allow-list, which replaces the inherited list and so excludes the guide on its own, and a `/skill on dev-workflow` sent to the child cancels a deny entry anyway (what actually keeps a reviewer from handing a review on is its plan mode and its missing `write`/`edit`)
- A subagent type supersedes only the parent's runtime overrides it holds an opinion of its own about: with no `[ai]` in the role, the child now keeps the parent's model files — running on the parent's model for the mode it is in, rather than the configured default, whenever the parent had set one — where before all of them were dropped (the review role this repo ships declares only `mode` and `skills`, which is the case that surfaced it). A type that declares `mode`, `[ai]` models, `skills` or `mcps` still drops the matching `mode-override`/model/`/skill`/`/mcp` file, because each of those outranks a topic config in its chain — key for key, now with no exceptions: the chain reads the file for the mode the topic is actually in, so a declared `plan_model` takes out the plan file, a declared `build_model` the build one, and `[ai].model` both, while the other mode's file stays put and a later `/mode` switch still inherits that side from the parent. The child's **starting model** no longer depends on which file it happens to read: `/spawn`, `/fork` and the spawn tool resolve the parent's model for the child's own mode — the parent's choice for that mode if it set one, else the model the parent is running, which is the snapshot written into the child's `.jyc/model-override`, the mode-agnostic fallback, unless the role declares a model of its own. `seed_state` copies *files*, and a parent whose model comes from a topic config, a pattern or the global config has none to copy — the case that left the review role this repo ships on the configured default — and the mode-agnostic fallback the chain reads next (`.jyc/model-override`, the file `/model` wrote before the per-mode split, still read when both per-mode files are absent) is now copied with the two per-mode files, where a child of a parent running on it used to fall through to its own config chain (#901)
- A spawned or forked topic no longer inherits the parent's **task list**: every child starts with an empty one and writes its own, so a reviewer is not handed a plan it had no part in. The seed was added for `/fork` (#810), where continuing the parent's plan was the idea — but the same list ran for `/spawn` and the spawn tool, where the parent's plan is somebody else's work, and `--inherit` does not bring it back either, since a list is not the conversation. A child that does want those items gets them the way any other context arrives: in the task it is sent (#901)
- The chat info pane's `Files` section shows each file's line counts (`+12 -3`, dim) after its path — for a staged change as well as a committed one: both halves of the list now join their own `git diff --numstat` run by path, and `ChangedFileEntry` gains an optional `lines` (`{insertions, deletions}`), absent for a binary file. `#[serde(default, skip_serializing_if)]` keeps payloads of different vintages readable in both directions, so a TUI and a server that disagree about the field still render. A path that is dirty on top of a branch commit keeps the branch-side counts, matching where its kind already came from. Untracked files remain outside the list: the section is about tracked changes relative to HEAD. `/info` shows the same counts in plain text, before its uncommitted `*` marker (#898)
- `/fork` and `/spawn` no longer copy the conversation: a new topic inherits the parent's settings (backlog, mode, pattern, model/context/MCP/skill overrides — and an empty task list) and starts with an empty conversation and no transcript. The copy was the expensive half — a long development conversation re-billed on every turn of a topic that was usually created to do something else — and it anchored the new topic to reasoning it never took part in, which is exactly wrong for the delegated review this feature exists to serve; `/fork --inherit`, `/spawn --inherit` and `inherit_context=true` bring it back for the case it was for (continuing the same thread down a second path). The system prompt's subagent list and the grant are untouched (#897)
- One vocabulary for JYC's paths, and the CLI data-root flag renamed with it: `--workdir` is now `--data-dir` (the `-w` short form is gone, so a script that passes it fails loudly at startup instead of resolving some other directory), `jyc_utils::paths::{config_home, data_home}` are `config_dir`/`data_dir`, `jyc_types::state_dir::jyc_dir` is `resolve_state_dir`, and `agents_root`/`state_root`/`agents_workspace_root` collapse into `topic_path::{agents_dir, resolve_agents_dir}`. Two names the vocabulary was missing get one — the **channels dir** (`<data dir>/channels`, `paths::channels_dir`) and the **channel dir** (one channel's `.imap`/`.github`/`.gitee` cursors, `paths::channels_dir()` + the channel name) — which is what makes it explicit that a channel holds no topics: topics live under the agents dir, and the older `<data dir>/<channel>/workspace/<topic>/` shape is dormant pre-pipe data no code writes any more. No file moved: every path resolves exactly where it did before, only the names changed. DESIGN.md's Terminology is the reference (#894)
- `/spawn` now records the parent's access to what it spawned: the same runtime grant `jyc_spawn_subagent` writes, so a spawned topic appears in the parent's subagent list no matter which side triggered it — and whichever form the command took, since the lineage is read from the child's state dir by name rather than from `<topic_path>/.jyc` (`/spawn ../test` counts too, and closing it gives the parent's access back)
- TUI: an `ask_user` box shows up to ten rows of context instead of three, and gives those rows back before it would ever shrink the question or its options. The box's height is now bounded by the body it is drawn in — three rows stay with the message pane above it — rather than a fixed fifteen, so on a terminal too short for ten rows the context is shortened by exactly what the box cannot spare. `PgUp`/`PgDn` in the chat pane now page by the rows the renderer last drew for the messages, so scrolling and drawing cannot disagree about how tall the box came out (#891)

### Fixed

- **An agent using the default root no longer gets a state dir before it has ever run** (#955): `restore_custom_topic_paths` registered each agents-channel default root by *creating* `<agents dir>/<agent>/.jyc` and writing its `topic-path` breadcrumb, so every agent named in the config acquired a state dir on every `jyc serve` start — and a state dir that exists is exactly what the topic listing reports, so the overview grew a row per configured agent (its `Agent` and `Topic` the same string) whether or not that agent had received a single message. Restore no longer records a default root that has no state dir, so nothing is created for it; the state dir is created by the enqueue funnel when the first message arrives, so a topic joins the listing when it starts working. Agents that have run keep their state dir and are restored from it as before, as are the pipe topics nested under an agent root (#955)

- **`/info` answered with a child topic when topics share a directory** (#926): a topic is addressed by its name, but `/info` picked its topic by `topic_path` with a first-match `find` — and a subagent spawned with `share_topic_path` (every `ci-watch-*`, `review-*`) works in its parent's directory, so `/home/jiny/projects/jyc` was carried by `jyc`, `ci-watch-925` and `review-prompt-names` at once. The list is sorted by name, so the first path match was `ci-watch-925`: `/info` asked in `jyc` reported the child's model, mode, branch and status. Every topic lookup now goes through one rule — `jyc_types::find_topic` / `topic_index` / `has_topic`, over the new `TopicRef` trait that both `TopicInfo` (a full resolve) and `TopicSummary` (the polled overview) implement — so a topic is resolved by name in one place instead of the six hand-rolled `.find(|t| t.name == …)` the TUI carried (chat header, chat status check, command popup, `awaiting_response` clear, subagent-tree root, `wait_for_topic`) plus the path match in the info handler, and `/exchange`, which derived the topic name from the path — a child's link named the parent and pointed at the parent's state dir, where its file does not exist. No command or pane resolves a topic from a path any more (#862 applied the same rule to `/close`, `/cancel`, `/pin` and `/reset`). The channel narrows the match where the caller knows one, and is not required — it keeps the lookup honest where a binding carries no channel at all: `ChatState::open` accepts `None` and then builds the un-scoped `ws://host/ws` URL, so the name alone finds that topic. The info pane also stops mixing the two screens: it answers for the topic a *visible* chat is bound to, and only otherwise for the row under the dashboard cursor — without the gate, a visible chat whose topic had not appeared in the poll yet fell back to that row and showed another topic's model, mode and branch (the activity log already refused the same fallback). A test pins the reported case: three topics on one shared path, each name resolving to its own topic.

- `/close`'s value help offers the confirmation flags after `all` too: `all` stands in for the subagent names listed above it and the parser takes `/close all --force` the same way, but the row was built without them — so typing `/close all` popped up nothing to continue with. The test now pins that every name row and the `all` row carry the same flags (#920)

- A spawn's access grant for its parent is rebuilt at serve startup, from the child's `spawned-from` breadcrumb: the grant lives in memory and a restart drops it, while the breadcrumb is on disk — so a restarted parent used to stop finding its subagents (`/close` offered it no subagent names at all) and lost its reach into their directories. `topic_path::restore_state_registry` does the rebuild at serve startup — the pass that already walks the same breadcrumbs and reads each state dir's `topic-path`, so it reproduces the spawn's own grant, shared-dir children included; `forget_subagent` still takes the grant away when a child closes, and a `/grant` is untouched by this (it stays process-lifetime unless `-p` writes it into the topic's config) (#919)

- `/send` reads flags only **before** the message: from the first message word on everything is message text, so `/send joe /close --force` hands joe that body verbatim instead of refusing `--force` as an unknown option (which is how a subagent gets told to close). `--` starts the message explicitly, for a body that itself begins with a dash, and one pair of quotes around the whole message is dropped — so the quoted spelling works too — while an unpaired quote stays part of the text (#918)

- A `ci-watch` child is now told that a promise ends the watch: a turn is the unit of work and nothing resumes one that has ended — a turn ends when it stops calling tools and its last text *is* the reply, so "polling now" with no call has ended the watch — and the caller side is written down where the hand-off is described: no report in the caller's topic by the end of the watcher's ~30-minute budget reads as a dead watch, so the caller checks the run once itself when it is next awake there (or schedules that check with a one-time `at` job) or sends the *same* watcher another task — a new message opens a new turn in it, and a second watcher on one PR is a duplicate the rules forbid — and a reply that announces a poll instead of reporting a result is the same failure seen early, visible in the watcher's own topic rather than in the caller's. The case that surfaced it: a watcher answered "Resuming the poll now" and issued no call, twice in one run, with the orders saying the opposite (poll every ~30s, one call each) and never naming the failure mode. It polls under `plan` mode, whose block, appended to every message, says it may only analyze, present a plan and wait for approval — and the role now reads that sentence narrowly: a poll is not an implementation, and a plan presented instead of a poll ends the watch; the block's other lines, its file rules included, still stand. The same escape is written one level up (`delegated-dev`): when the caller above the developer is next awake with no report in hand — a watcher's ~30-minute budget is the clock it judges by — that is a dead watch, so it asks the child or checks the PR once. No runtime code changed — the only code here is a test assertion pinning the rule and its premise (#908)
- Per-channel IMAP state no longer hides the UID it just recorded: `StateManager::track_uid` appends the UID and now waits for that append to land before returning, instead of leaving it in flight. `write_all` resolves as soon as tokio has buffered the bytes and *started* the blocking write — its `File::poll_write` returns while that write is still outstanding — so a reload that followed the append could read `.processed-uids.txt` and find the previous UID but not the new one, which is what CI caught: a coverage-run failure of `test_state_manager_round_trip` (`assertion failed: sm2.is_processed(101)`, with `is_processed(100)` passing in the same run) on a suite that was green on either side of it, which is what makes it a timing bug rather than a state bug. `sync_all` is deliberately not called — flushing hands the bytes to the OS, syncing only tells the OS to commit them, and this is a visibility bug rather than a crash-durability one, so an fsync per message would be paid for nothing — and a regression test now reads the file straight off the disk the moment `track_uid` returns (#907)
- The `ci-watch` role now reports back on every outcome and on its own initiative: it sends its report with `jyc_send_to_topic` to the `Source:` topic **whether or not** the caller asked for a reply, and it treats an empty `gh run list` as unread rather than as a verdict — the run for a head it was just handed may not exist yet, so it keeps polling inside its ~30 minute budget and only then reports "no run exists for `<sha>` after ~30 minutes" as a result. The case that surfaced it: a `develop` child pushed a PR and handed the wait off before CI had created a run, and the watcher read the empty list once and answered only in its own topic. The caller heard nothing, for two reasons at once: the send-back was written as conditional on the caller asking for a reply, and a run that does not exist yet was not one of the outcomes the orders defined. The `develop` role's own instructions now name the call to make, `jyc_send_to_topic(..., require_reply=true)` — the call `dev-workflow` already prescribes for that hand-off, repeated at the point of use, since the flag is what has the framework mark the message as one awaiting a report (the watcher sends its report back either way) (#906)
- `/model` now takes effect in a topic whose mode comes from a config layer — a typed subagent, whose `mode = "plan"` lands in `.jyc/config.toml` — and the dashboard, the Feishu status card and `/info` show the model the agent actually runs there. The model chain existed three times and had drifted: the runtime picked the `<mode>-model-override` file by the *file's* own mode while `/model` and the display picked it by the topic's effective mode, so in such a topic `/model X` wrote `plan-model-override` while the turn read `build-model-override` — reporting success, showing X in the pane, and running the old model; the display was also missing the topic-config layer entirely and put a channel's generic `model` above the global per-mode fields. `jyc_core::session_state::resolve_model` is now the one implementation, called by the runtime, the display source, the context-window resolver and the spawn triggers, and the channel-over-global `[agent]` derivation (`derive_agent_config`) moved to `jyc-types` so both crates read the same one (#901)
- A staged file no longer loses its `+`/`-` glyph in the chat info pane's `Files` section until it is committed: the working-tree half of the list resolved through `git diff --name-only HEAD`, whose output carries no status letter, so every dirty row fell back to `Modified` (a blank prefix) while the branch half — which does read letters — got the glyph. That pass reads `--name-status HEAD` now, through the parser the branch half already used, so a `git add`ed file shows `+` and a staged deletion shows `-` before the commit lands (#898)
- A closed topic no longer keeps another topic's directory out of reach: nothing removes a runtime pin when its topic closes, so the purge guards — which ask "who else works here?" — read the pin's lineage now, ignoring one whose topic is gone and one held by a subagent of the topic being purged. Before, a `/close --force --purge` (or an upstream close) of a directory that had once been co-pinned refused with "is also the topic dir of <closed topic>" — and the shared subagent this release adds would have hit exactly that (#897)
- A closed PR or issue now clears its pipe topic's working dir even when the topic wrote something into it: an upstream close (GitHub/Gitee issue or PR closed, a disbanded chat) deletes `<agents dir>/<agent>/<topic>` with its contents, where `close_topic` alone only removed a dir it had already emptied — so a topic whose dir had gained files kept that dir forever, next to the state dir it had correctly deleted. Only that exact two-level shape is touched, and the purge guards still decide: an agent's own dir one level up, a dir another topic works in (a `/fork` shares its parent's), a dir nesting somebody else's, a symlink out of the tree and pins outside the agents root are all left in place. `/close` keeps its empty-only rule (#896). A topic reopened upstream (issue closed, then reopened) starts with an empty dir; its state location is kept.
- `jyc stop` no longer reports a clean pipe shutdown as a failure: `jyc-pipe`'s hub connection task returns `Ok` exactly when its cancellation token has fired, so an `Ok` that wins the `select!` race against the token arm is a clean stop — it used to print `Error: hub pipe connection task exited unexpectedly` on a stop (the child's stderr is the hub's terminal), and to log `error: jyc-pipe exited; pipe-only channels are offline` on stop paths that log instead of print (#890)
- A mid-turn narration delivery that failed is no longer swallowed: the agent loop dropped `deliver_progress_text`'s return value, so an `ask_user` context that only reached the reply file — whose watcher ends with the turn, leaving nothing to send it — left no trace at all; it now logs at `warn`, and travels with the question regardless (#890)

### Removed

- `ActivityEntry`'s `severity` and `is_internal` fields, and `text` as the written payload: the backend no longer composes prose or decides visibility for its clients — an entry is the raw `event` plus its envelope, and rendering (wording, error colours, heartbeat hiding) is the client's job. `text` survives only as an `Option` for reading pre-change `activity.jsonl` rows; nothing writes it (#929)
- The `/fork` command. `/spawn -s/--share` is what it did — the same shared directory, the same seeded settings, the same private state — and it is the same implementation now: `/fork` had its own `fork_under` until this change and shared only the helper functions, which `spawn_handler` imported (the implementation `/spawn` already shared was the tool's, not the fork's), so what is gone is that second implementation, and with it the `forked-from` marker (a stale one is simply not a lineage breadcrumb any more: the topic is no longer listed as anybody's subagent, and nothing else reads it). Typing `/fork` gets an unknown-command answer; write `/spawn -s` (#911)

- The GitHub review, review-comment and CI check-run polling are gone, together with the `scan_active_pr_topics` gate that guarded them and `GithubConfig.poll_ci_status`. The gate resolved the pre-pipe channel workspace (`<data dir>/channels/<channel>/workspace`) and looked there for `{prefix}-{N}` directories, while PR topics have lived under the agents dir (named after the target agent — a pipe retargets into the synthesized `agents` channel, whose per-agent pattern carries the agent's name) since the pipe split — so its set was empty on every cycle and none of the three pollers ever issued an API call. Deleted with them: `ci-status.txt` tracking, the client calls left without a caller (`list_reviews`, `list_review_comments`, `list_check_runs`, `get_pr_head_sha`), the Gitee build-status wrappers that were never wired to a poller, and `GiteeConfig.poll_ci_status`. Runtime behaviour is unchanged — the deleted code never ran (#895)
- The developer skill's `check_run` section: no CI event arrives on its own any more, so `gh pr checks <number>` is the only way that role learns about a red run (#895)
- The `jyc patterns list`, `jyc agents` and `jyc skills` subcommands. `jyc plugins install` is the one way to put this repo's user-editable collections on disk — the bundle carries `skills/`, `subagents/`, `hooks/` and now `templates/`, one subdirectory per name and each replaced cleanly, and every release publishes `dist/jyc-plugins.tar.gz` with all four, so a machine goes from a bare install to the repo's skills, roles, hooks and templates in one command. `templates/` joins that bundle here: it used to reach a config dir only through `jyc agents install`, so removing that command would have left the repo's own templates (e.g. `templates/invoice-processing/`, which a pattern's `template` field resolves through `<config dir>/templates/`) with no distribution path at all. `jyc patterns list` only echoed the rule fields already written in `config.toml` and never claimed them; the `[[channels.<name>.patterns]]` tables are the place to read them. With the two installers gone, `install.rs` serves a single command again: `list_entries` is private (it exists to enumerate a collection for `install_entries`), and `resolve_source` loses its `None` branch — a source defaulting to the current directory had no caller left (#934)

## [0.5.0] - 2026-10-11

### Added

- Proactive messages with files work again on the pipe-hosted channels (wecom_bot, feishu, email): `jyc_send_message` with `attachments` no longer fails with "Attachments not supported for this channel type". The hub holds neither the channel's credentials nor a shared filesystem, so the send crosses to the pipe exactly as a reply does — a frame naming the addressed channel, the recipient and the attachment paths, sent by the pipe's forwarder for that channel. Channels that never took a proactive message keep failing the send rather than reporting a success that delivers nothing. Cross-channel sends to a pipe channel resolve instead of failing with "Unknown channel", and proactive text is delivered instead of being silently dropped (#885)

- `jyc serve --pipe` runs the peripheral `jyc-pipe` process as a child of the hub, so one command starts both halves of a deployment (the pipe hosts every pipe-only channel — feishu, github, gitee, wecom, wecom_bot, wecomkf, email — and the hub skips those in-process). The child is started next to the running `jyc` binary with the hub's own `--workdir` and resolved config file, keeps its own `<data_home>/jyc-pipe.log`, and is stopped with the hub (SIGTERM, then SIGKILL after a 5 s grace) rather than restarted: a channel crash must not take the agent core down, and a pipe that dies is reported at `error` level. `--pipe` is a global flag, so bare `jyc --pipe` forwards it to the `jyc serve` the dashboard auto-starts (a hub already running cannot be retrofitted — a warning says so); with `[inspect]` disabled it is ignored with a warning, because the pipe would have no hub websocket to connect to. The Docker image's default command is now `jyc serve --debug --pipe` (it used to start the hub alone and silently skip the pipe-only channels), which is also what the compose file runs — replacing the interim second `jyc-pipe` service (the child keeps its own log file inside the container, `/root/.local/share/jyc/jyc-pipe.log`) (#884)
- Inbound attachments from pipe-hosted channels (email, feishu, wecom_bot) are now relayed to the topic workspace (#879): the adapter uploads the bytes to the hub's staging endpoint (`POST /api/inbound`, `docs/api.md` §2.4.10) and names them in the new `attachments` field of the `message` frame; the topic worker moves each file into the topic it routed the message to, so the agent's `read` / `read_image` tools see it like any in-process attachment (uploads are capped at 32 MiB per file, and `[attachments.inbound]` still decides what may pass). An attachment-only mail (empty body, attachments present) now reaches the AI instead of stopping at the body check.
- Email channels can now run in the external `jyc-pipe` process (pipe split step 5 — the last in-process pipe-only channel, `docs/architecture/pipe-split.md`): `jyc-pipe` claims every configured email channel — IMAP monitor (IDLE/poll), pattern match + subject-derived topic retarget, SMTP reply relay threaded into the original mail (`In-Reply-To`/`References`), reply attachments downloaded through the hub's files endpoint with the pipe's bearer token (`pipe::fetch_topic_file`, shared with feishu/wecom_bot — no `jyc-inspect` dependency) — and owns the mailbox cursor state (`<workdir>/channels/<name>/.imap/`). `jyc-pipe` also gains `--no-idle` and `--reset`. No config change needed; email patterns are a startup snapshot like github's (#876)
- Known gap of the email migration, same as feishu's — **since closed**: attachments on incoming mail were not relayed — the pipe `message` frame had no attachments field, so they were not saved into the topic workspace, and a mail with an empty body and a file attached stopped without calling the AI. `jyc-pipe` warned about this at startup (when `[attachments.inbound]` is configured) and for every dropped message (#876)

- `jyc-pipe` gains `--log-file [PATH]`, aligned with `jyc`'s log destination semantics: writes to PATH, or `<data_home>/jyc-pipe.log` when the value is omitted; like bare `jyc` (a daemon), the default destination is the log file — use `--log-file /dev/stderr` for stderr (#869)

- WebSocket hub protocol extensions for external pipe processes (`jyc-pipe`, see `docs/architecture/pipe-split.md`): inbound `message` frames accept an optional `metadata` map (pipe hints forwarded verbatim to the router), a `close_topic` client frame asks the hub to close a topic (`TopicManager::auto_close_topic`), and `topic_event` server frames stream the raw `TopicEvent` sequence to connected clients (1 Hz `LoopTick` heartbeats excluded) (#865)
- New `jyc-pipe` binary (pipe process split step 2, `docs/architecture/pipe-split.md`): a standalone peripheral message-pipe process that connects to the hub websocket (`--hub`, defaults to `ws://<inspect.bind>/ws/agents`) with the inspect auth token, logs the reply/`topic_event` stream, and auto-reconnects with backoff. Channel adapter wiring lands in a later step (#866)
- Feishu can now run in the external `jyc-pipe` process (pipe split step 3, `docs/architecture/pipe-split.md`): `jyc-pipe` claims every configured feishu channel — hub websocket client with reconnect backoff, reply relay, live status cards from `topic_event` frames, chat disband → `close_topic` — while `jyc serve` skips those channels in-process. Ownership is derived from the pipe's adapter capabilities (`pipe::SUPPORTED_CHANNEL_TYPES`), so no config change is needed. Known gap: inbound attachments (images sent to the bot) are not relayed through the pipe protocol yet (#868)
- GitHub channels can now run in the external `jyc-pipe` process (pipe split step 4, `docs/architecture/pipe-split.md`): `jyc-pipe` claims every configured github channel — poller adapter with dedup/cursor state under `<data_home>/channels/<channel>/.github/` (unchanged location), reply relay posts `[Role]`-prefixed comments, issue/PR close events forward `close_topic` frames — while `jyc serve` no longer builds feishu/github adapters in-process, so the adapter-only dependencies (openlark, …) leave the `jyc` binary's link graph. No config change needed; note: github patterns are a startup snapshot, restart `jyc-pipe` to apply pattern edits (#870)
- Gitee channels can now run in the external `jyc-pipe` process (pipe split step 4, `docs/architecture/pipe-split.md`): `jyc-pipe` claims every configured gitee channel — poller adapter with dedup/cursor state under `<data_home>/channels/<channel>/.gitee/` (unchanged location), reply relay posts `[Role]`-prefixed comments (`create_comment` with the explicit `is_pr` flag, separate issue/PR number spaces), issue/PR close events forward `close_topic` frames — while `jyc serve` no longer builds the gitee adapter in-process. No config change needed; gitee patterns are a startup snapshot like github's (#873)

- WeCom smart-bot (`wecom_bot`) channels can now run in the external `jyc-pipe` process (pipe split step 4, `docs/architecture/pipe-split.md`): `jyc-pipe` claims every configured wecom_bot channel — aibot WS callback adapter with the streaming "thinking" indicator, keep-alive spinner, `finish=true` reply updates (proactive `aibot_send_msg` fallback), and proactive attachment relay, whose downloads now go through the hub's files endpoint instead of the `jyc-inspect` client (that client is no longer needed by this adapter) — while `jyc serve` no longer builds the wecom_bot adapter in-process. No config change needed; wecom_bot patterns are a startup snapshot like github's (#874)
- WeCom (`wecom`) and WeCom KF (`wecomkf`) channels can now run in the external `jyc-pipe` process (pipe split step 4, `docs/architecture/pipe-split.md`): `jyc-pipe` claims every configured wecom/wecomkf channel — webhook-callback adapters sharing one HTTP listener, pipe retarget, text reply relays (external-contact API / `kf/send_msg`), with the customer-service sync cursor and msgid dedup state unchanged where `cursor_store_path` points — while `jyc serve` no longer builds them (or their webhook server) in-process, so the `jyc` binary no longer references the WeCom adapters or their AES/crypto helpers. No config change needed; wecom/wecomkf patterns are a startup snapshot like github's (#875)

### Changed

- The mailbox cursor (`StateManager`) and the email parser moved from `jyc-core` into `jyc-channels`, where their only callers live (the email pipe adapter, the IMAP monitor and the email matcher): both modules are self-contained — nothing in `jyc-core` read them outside one `#[cfg(test)]` module — and `jyc-core` no longer needs `regex` as a result. The empty `security` stub module is gone too (#882)
- The `jyc` binary no longer links a second TLS stack: the hub's HTTP clients — jyc-inspect's inspect client and jyc-agent's model calls — use native-tls (system openssl) instead of rustls/aws-lc-rs, which is what `tokio-tungstenite` and `lettre` already required. This needed explicit features on both crates because reqwest 0.13's `default-tls` feature *is* rustls (`default = ["default-tls", "charset", "http2", "system-proxy"]`), so `jyc-agent` drops the default features and re-lists charset/http2/system-proxy. `jyc-pipe` keeps rustls, and the release workflow now builds the two binaries in **separate** cargo invocations: feature resolution is per-invocation, so one command with both `-p` flags would unify their reqwest features and link rustls into the hub binary. No new deployment requirement — native-tls is the openssl the mail and websocket stacks already use (`cargo tree -p jyc-cli -e features --invert rustls` prints nothing) (#881)
- The email transports moved out of `jyc-services` into `jyc-channels` (`src/imap/`: client, monitor loop with IDLE/poll, raw-email parsing; `src/smtp/client.rs`), the crate that owns their only caller, the email pipe adapter. They keep their relative layout, so only the call sites in `pipe/email.rs` changed, and `jyc-channels` no longer depends on `jyc-services` (#881)
- Release builds use fat LTO and a single codegen unit: the release binaries are the deliverable and they are one large monomorphized graph. Costs release build time only — the development gate stays on `cargo check` (#881)
- `jyc-cli` drops two direct dependencies that nothing under `crates/jyc-cli/src` named (`uuid`, `serde`); the other 25 are all still referenced (#881)
- `jyc-pipe`'s log is scoped to jyc's own targets and cannot pick up other processes' content: the default filter is `jyc=info,async_imap=warn` (`-v`: `jyc=trace,async_imap=debug`, mirroring `jyc`'s defaults) instead of a global level, `RUST_LOG` still wins but the wire-level crates (`tungstenite`, `hyper`, `reqwest`, …) stay capped at `warn` unless named explicitly, and lines now carry their module target. A global `RUST_LOG=trace` used to dump every websocket frame the hub sent — other topics' replies and tool events — into `jyc-pipe.log` via `tungstenite`'s own `Received message` trace (#877)
- The `jyc` binary no longer builds the pipe adapters: channel ownership (`SUPPORTED_CHANNEL_TYPES`) moved from `jyc-channels` to `jyc-types`, so the hub reads it from a dependency-light crate and `jyc-cli` drops its `jyc-channels` dependency. Previously that one constant pulled 22k lines of adapters into the hub's build graph along with the adapter-only crates (`openlark-client`, the WeCom crypto stack, …) — pipe split step 6, `jyc-services`' email transports are a separate follow-up (#877)
- Email now runs in the `jyc-pipe` process, so `jyc serve` hosts no pipe-only channel at all: run `jyc-pipe` for email channels to receive and reply (the hub skips them in-process). The mailbox cursor directory is resolved against `jyc-pipe --workdir`, so a pipe started with a different workdir looks for its cursor elsewhere (#876)
- `jyc serve --no-idle` / `--reset` moved to the `jyc-pipe` CLI (`jyc-pipe --no-idle` / `--reset`): both only ever affected the email adapter, which now runs in the pipe. On the hub they would be no-ops, so the flags were removed there (#876)

- The shared WeCom webhook listener (`[wecom].bind_addr`) is now bound by the `jyc-pipe` process instead of `jyc serve`: run `jyc-pipe` for wecom/wecomkf callbacks to work (the hub skips those channels, and no longer starts the HTTP server), and expose/forward the port on the process that runs the pipe (#875)

- Use [✓] tick instead of [x] for completed tasks and marked question options (#871)
- The `[pipe]` config section introduced earlier in this release cycle is removed before any release: channel ownership between `jyc` and `jyc-pipe` is derived from the pipe's adapter capabilities (`pipe::SUPPORTED_CHANNEL_TYPES`) instead of a user-maintained channel list, so deploying the pipe split no longer requires touching config.toml. A leftover `[pipe]` section in an existing config is ignored (#869)
- `jyc`'s log destination is unified: every invocation now writes tracing logs to `<data_home>/jyc.log` by default — the file is named after the binary, regardless of subcommand. Previously only bare `jyc` defaulted to the file, `jyc serve` defaulted to stderr, and `jyc dashboard` / `jyc open` wrote a separate `dashboard.log`. Use `--log-file PATH` to override, or `--log-file /dev/stderr` for stderr (#869)

- Internal restructure: the websocket channel adapter moved from `jyc-channels` to `jyc-inspect` (`server::websocket`) — it is hub frontend (dashboard UI + pipe endpoint), not a peripheral channel. `jyc-channels` is now the pipe crate and no longer depends on `jyc-inspect`; no behavior change (#867)

- The Docker image builds `jyc` and `jyc-pipe` separately, like the release workflow: feature resolution is per-invocation, so one command with both `-p` flags unified their reqwest features and linked the pipe's rustls stack into the image's hub binary (the hub uses native-tls). The tarballs were split in #881; the image was not (#886)

### Fixed

- **The TUI renders absolute times in the system's local timezone** (#887): the dashboard overview's `Last Active` column and the activity pane (the dashboard and the chat TUI share one renderer) printed the UTC timestamp as-is, so a UTC+8 host read times eight hours behind the chat screen's group headers, which already converted to local time. Both sites now convert to `chrono::Local` like the headers do, which also moves the "is it today?" branch to local dates. Relative values (`12m ago`, elapsed durations) and the UTC wire format are unchanged.
- **A future `Last Active` is no longer shown as `0m ago`** (#887): the relative branch tested only `minutes <= 60`, so a timestamp ahead of the viewer's clock — skew between the TUI's machine and the hub's, or a last activity timestamp that is not the hub's own clock — matched and was clamped to zero. It now requires a non-negative age and otherwise falls through to the clock time, like any timestamp that is not recent.
- `jyc-pipe` now shuts down gracefully on SIGTERM — systemd `stop`, `docker compose down`, a plain `kill`, or the hub that spawned it with `--pipe` — instead of only on Ctrl+C: its handler now mirrors `jyc serve`'s, so both halves of the split stop the same way. SIGTERM used to terminate the process without running its cancel path, which the hub's `--pipe` shutdown depends on (#884)
- **The pipe's TLS stack is patched for RUSTSEC-2026-0285** (#883): the lock carried rustls 0.23.43, which accepted TLS 1.3 handshake messages sent across encryption-level boundaries (a network-position attacker cannot alter the handshake, but a peer could send messages that should have been encrypted in plaintext without being rejected). The hub is not affected — the `jyc` binary has not linked rustls since #881 — so this only moves `jyc-pipe`'s reqwest/tungstenite stack to 0.23.45, the patched release.
- **The outbound markdown → HTML path is hardened** (#883): `comrak` moves to 0.56, which carries the two DoS fixes in the `autolinks` extension (GHSA-xg9p-p4jc-c46g — that extension is part of the GFM set the SMTP renderer enables) plus a batch of pathological-input fixes for emphasis, footnotes, description lists and the anchorizer. The `tagfilter` extension removed in 0.56 is not used here.
- **Staged inbound attachments are resolved against the hub's own staging directory** (#880): the upload endpoint returned a path relative to the channel workspace while the frame handler resolved it *inside* the staging directory, so every relayed attachment was looked for at `<workspace>/.inbound/.inbound/<name>` and the move failed — the topic's `attachments/` directory was created but stayed empty, the agent saw a file that did not exist, and the upload was left behind. The frame now carries a plain staged file name, and the staging directory is the hub-level `<data_home>/.inbound/`. That also removes the collision the old location had: for the synthesized "agents" channel the channel workspace *is* where a topic named `workspace` lives, so staged bytes would have been reachable through that topic's files endpoint. The move out of staging is a rename; when it cannot be one (a `save_path` pin on another filesystem) the file is copied and the staged original deleted, and a failed deletion of that original no longer hides the file from the agent. Orphans — a staged file whose frame never arrived — are **not** collected: check `<data_home>/.inbound/` by hand (`docs/api.md` §2.4.10).
- **New mail is detected without `UIDNEXT`** (#879): the monitor reads the newest UID from the mailbox itself (`UID FETCH *`) instead of deriving it from `SELECT`'s `UIDNEXT`. 163 does not report `UIDNEXT` at all, which left detection with nothing to compare and silently stopped fetching mail; `UIDNEXT` is kept in the log for observation only.

- **IMAP mail detection is UID-based** (#877): the monitor compared the mailbox message count against the count it saw last, so a mailbox that expunged one message and received another reported the *same* count and the new mail was never fetched — the pipe then sat on `No new messages` and nothing above debug level said so. It now compares the server's `UIDNEXT` with the state's `last_processed_uid` and fetches `UID FETCH <from>:<to>`, so mail is detected regardless of the count; `UIDVALIDITY` is recorded and a change resets the UID cursor (the stored UIDs describe the server's previous UID space), and a cursor that ends up above the newest UID is reported as such instead of staying silent. `Mailbox selected` / `No new messages` moved from trace to debug (visible with `-v`) and the fetch line reports the UID range

### Removed

- The `jyc-services` crate (#881): `JobScheduler` moved to `jyc-core` — it is the glue between `job_store` and `topic_manager`, and needed no new dependencies there — and the email transports moved to `jyc-channels` (see above). The hub therefore stops compiling the mail stack (`async-imap`, `mail-parser`, `lettre`, `comrak`, `htmd`): the follow-up promised in #877, closing pipe split step 6. `jyc-channels` and `jyc-cli` no longer depend on the crate.
- The in-process email adapter (`spawn_email_adapter` in `crates/jyc-cli/src/cli/serve/channels/email.rs`) and the hub-side pipe helpers only it still used: `HubRegistry` (per-channel `MessageRouter` map), `route_into_pipe_target`, `wait_for_broadcast`, `fetch_reply_attachment` and the `ws_broadcasts` map. With the split complete, `jyc-cli` no longer references any pipe adapter or routing helper — it reads `pipe::SUPPORTED_CHANNEL_TYPES` only, to skip the channels `jyc-pipe` owns (#876)
- Two unused `SmtpClient` senders (`send_mail`, `send_mail_with_attachments`) are gone: `send_reply` is the only path the email pipe has used since #876, and the two were duplicating its body-building minus the threading headers. The SMTP retry constants they shared with it — plus the unreferenced `DEFAULT_SMTP_PORT`, whose real counterpart is `default_465()` in `jyc-types` — moved out of `jyc-utils` into `crates/jyc-channels/src/smtp/client.rs` as private constants; nothing about retry behaviour changed (#882)
- Unused direct dependencies and dead code across the hub crates: `jyc-cli`'s `url` and `regex`, `jyc-agent`'s `thiserror`, `jyc-core`'s `base64`/`libc`/`futures`, axum's unused `macros` feature, and `jyc-inspect`'s `tokio-tungstenite` moved to dev-dependencies, where its only users live (the integration tests that speak raw WebSocket). Also removed: the unreachable `load_from_chat_history` history fallback in `jyc-agent`, `WecomBotInboundAdapter`'s two `*_with_workspace*` constructors and its never-read workspace root, `WecomBotWsClient::set_sender`/`state()` with the write-only connection-state field, the unused `ConnectionState` enum, and the dead `FeishuEvent`/`FeishuError` types (#886)

## [0.3.19] - 2026-10-09

### Added

- TUI: `ctrl+p x` clears the chat input field (an open `/` popup closes with it — an empty field has nothing to filter)

### Changed

- TUI: the chat status line moved to the pane's bottom row: mode/topic/branch on the left, model + context usage on the right, both in dim gray with no rule around them. The input field gained a top rule (it already had a bottom one) and the `/` + `ctrl+p` popups sit between the field and the status line (#861)
- `/close --force` now prunes the emptied state namespace: the `~/.local/share/jyc/agents/<topic>/` shell a deleted state dir leaves behind (and empty nested parents such as `agents/<agent>/`) is removed, stopping at jyc's own roots — a shell that still holds files is kept, and the registration is still kept so a reopen reuses the same state name (#860)
- TUI: chat screen input field redraw — the `╰─❯ ` gutter is gone (the editor spans the full width), the header and the new full-width bottom rule take the mode/topic text color, and the `/` + `ctrl+p` popups no longer draw their own top rule — the input's bottom rule is their boundary (#854)
- TUI: chat input cursor is a blinking reversed block instead of an underline
- `/close --force --purge`: a refused directory purge (workspace shared with another topic, project checkout, or jyc-managed root) no longer blocks the close — the topic's state is deleted as usual and the reply reports that the directory was kept, with the reason (#862)

### Removed

- TUI: the `ctrl+p /` leader entry — the input field is the `/` popup's filter now, so a leader-driven popup could never show a usable list of commands

### Fixed

- **`/close` no longer leaves a recreated `agents/<topic>/` folder behind**: the close deleted the state dir, but the close reply's chat-history write and the inspect server's activity-log entries then recreated it as a bookkeeping-only tombstone — closing now freezes the topic's state, so late writes are dropped instead of resurrecting the directory; the freeze lifts when the topic reopens (#863)
- **`/close`, `/cancel`, `/pin`, and `/reset` in a forked topic no longer act on the original topic**: the handlers derived the topic name from the workspace directory, which a fork shares with its parent — the topic's own name is the identity now, so closing a fork deletes the fork's state (keeping the shared directory), cancel/reset hit the fork's session, and pin/unpin apply to the fork's channel topic (#862)
- **Kimi's `<response>` / `<tools>` tool-call wrapper no longer leaks into the
  delivered reply**: the text-channel leak guard (#786) knew the `response_tools`
  spellings but not Kimi's bare wrapper tags, which slipped past unstripped.
  Both tags are recognized now; other models are unaffected (they don't leak)

- Topic info recognizes a `git worktree add` directory: `.git` is a file there, not a directory, so the branch and changed-file probe returned nothing — both now follow the `gitdir:` pointer (relative paths included), which also covers submodule checkouts

- **A multi-question set no longer arrives as one run-on line**: channels that
  render reply text as markdown (the feishu card) parsed a question's numbered
  options as a list and read the following `Q2:` line as that list's continuation,
  gluing every question after the first onto the previous one's last option.
  Questions are separated by a blank line now (which ends the list), the
  `(multi-select)` marker sits on the question's own line, and a question or
  option the model wrote across several lines is folded onto one, so nothing
  shifts the numbering the user answers by (#852)
- **A feishu rich-text (`post`) message is read as the text typed**: composing
  more than one line makes feishu send `post`, which the inbound adapter had no
  arm for, so an answer to a question reached the agent as
  `[Unsupported message type: post]: {raw JSON}`. Paragraphs keep the line
  breaks the user typed, links show their target, a mention reads as `@name`, a
  picture as `[图片]` (a placeholder only — a picture inside rich text is not
  downloaded the way a stand-alone image message is), and a payload that cannot
  be parsed at all keeps the old raw-content fallback instead of delivering
  nothing (#852)
- **A channel without a question box now receives the questions as a plain-text
  message**: on a feishu / email / pipe-only turn `ask_user` answered the model
  with an error telling it to ask in its reply text — not a delivery mechanism,
  and what the user actually read was that `[ERROR]` line instead of a question.
  The tool now renders the set itself (each question with its options numbered
  the way they must be answered, multi-select marked) and sends it as an
  ordinary message through the same reply path as the turn's final message
  (`reply_send` hooks included), returning a normal result so the turn ends and
  the user's numbered reply arrives as the next message (#850)
- **A piped turn no longer stalls behind a question its user cannot see**:
  `ask_user` pushed its questions to whichever channel owns the *topic*, so a
  turn that arrived over a feishu or email pipe blocked up to the timeout on a
  box only a TUI could draw — and the reply the user was waiting for is the
  turn's final message, so nothing arrived at all until the box was dismissed
  elsewhere. A question now goes to the outbound adapter of the channel the
  message came from (recorded as `origin_channel` metadata before the pipe
  re-targets it) (#849)
- Leaked tool-call syntax no longer reaches users as a reply, and legitimate
  prose about it is no longer mistaken for a leak: the leaked block is
  stripped and the surrounding prose delivered (a reply used to be failed
  outright, burning up to 3 regenerations and ending with nothing delivered),
  a response that is nothing but syntax is retried with a repair note telling
  the model that its call never reached the tool interface (the retry used to
  re-send the identical request), and the dialect detection now covers the
  Anthropic XML form actually emitted (`tool_use` / `invoke` / `parameter`,
  opening and closing, truncated mid-tag included) plus the legacy
  `functions.foo(` and bare `{"name":` forms — mentions inside prose or code
  fences stay deliverable (#786)
- Info pane left border runs the full column height again and joins the
  status sub-pane's top border with a `├` junction — the border no longer
  stops where the status sub-pane begins (#843)
- Topic-info diff probes (`base_ref`) inherit `git rev-parse` stdout, leaking
  commit SHAs into jyc's own output at startup (#840)

### Added

- TUI message-area `[` / `]` jumps the cursor (and view) to the previous /
  next user turn — one turn being the user's message plus the agent's
  replies up to the next one; `Shift` grows a selection and a count repeats
  (`3]` jumps three turns)
- `/spawn [name] [path]` continues this topic's conversation in a directory of
  its own: a fresh `agents/<name>/` with no path, or any directory the path
  names — a non-empty checkout included, whose files are left exactly as they
  are, since a spawn seeds state (the `/fork` allow-list, marked
  `spawned-from`) and copies no workspace files at all. A relative path counts
  from this topic's own dir, so `/spawn ../test` means beside it; a
  destination inside the current topic's own dir is refused, since a nested
  working dir would inherit the parent's git status, file listings, and close
  semantics. Closing such a topic removes the emptied shell
  directory with its state. Websocket-only (#853)

### Changed

- **`ask_user` asks and steps back: the turn ends the moment the questions go
  out, on every channel.** The tool renders the set — one `question` frame per
  question where the channel has a question box (the TUI), the same set as a
  plain-text message on the ordinary reply path everywhere else (feishu, email,
  any pipe-only channel) — hands it over, and the agent loop stops there instead
  of calling the model again. Nothing waits, so there is no timeout to pick and
  no unanswered question holding up the reply a user is waiting for. The answer
  is the user's next message: the TUI sends one line per question keyed by the
  number it was rendered with (`Q1: 2) 面条`), a question skipped with `d` says
  `(skipped)`, and `Esc` only closes the box — there is no run left to stop.
  Leaving the topic or closing the chat pane drops an unfinished batch the same
  way. Questions that fail to reach the channel go out as that turn's reply
  rather than being logged and dropped. Supersedes the question-await behaviour
  of #848 and #849 (#851)
- TUI chat status sub-pane wraps its stats instead of clipping them: the
  version line keeps its own row and the five stat chips (`2 active`,
  `5 thread`, `12 recv`, `0 err`, `up 38m`) pack into as many rows as the info
  column allows, so a narrow pane shows whole chips rather than `2 active / 5 t`.
  The wrapped rows come out of the topic info, which keeps at least six rows, and
  the sub-pane is all-or-nothing: a column too short for border plus every row
  shows none of the block rather than the version line with the stats cut off
  (#847)
- TUI pane borders share one palette: every pane border on the chat screen and
  the overview screen now rests in dark gray and turns cyan while its pane owns
  the keys. The topic-info and activity panes used to inherit the terminal
  default (and highlight yellow+bold on focus), so the separators on a single
  screen disagreed with the explorer and status borders (#846)
- TUI chat screen drops its bottom status bar: version, server stats, and
  transient messages now render in a fixed sub-pane at the bottom of the
  topic info pane (`JYC AI v…`, gray stats, gray top border) — the sub-pane
  never scrolls with the topic info; leader `s` still toggles it; the
  overview screen keeps its status bar unchanged (#841)
- TUI chat header shows the topic name instead of channel and pattern
  (`╭─ plan · jyc · branch`) — the topic is the identity that matters when
  working with agents; channel/pattern remain routing metadata
- `/clone` skips regenerable build/dependency directories (`target/`,
  `node_modules/`, `dist/`, `build/`, `__pycache__/`, virtualenvs, caches —
  matched by name at any depth) instead of copying them, and says how many
  it skipped — cloning a project workspace is no longer dominated by
  artifact copy
- The agent's final message at the end of a turn is now the reply, matching
  the convention of other code agents (Claude Code, Codex, etc.) — the
  `jyc_reply_message` tool, its `stop_after`/`silent` modes, progress
  replies, and the entire auto-delivery fallback machinery (synthetic tool
  execution, `— auto-delivered` trace, reminder nudges, reply-restricted
  recovery loops) have been removed. Text written alongside tool calls
  stays internal narration; empty final text sends nothing
- Replies are delivered by the agent loop itself (`deliver_reply`): direct
  send through the pre-warmed outbound adapter when a reply target is
  live, otherwise queued via the `reply.md`/`reply-sent.flag` file relay;
  `reply_send` hooks gate direct sends at the delivery site

### Removed

- `/clone [name] [path]`, superseded by `/spawn`. Copying the whole workspace is
  what made it refuse a non-empty target, need `target/` / `node_modules/`
  heuristics, and cost minutes of blocking I/O for the one thing people actually
  wanted carried over — the conversation. Directories a `/clone` made earlier
  keep working as ordinary pinned topics (#853)
- The question **answer** protocol: `jyc_core::question::QuestionHub`, the
  websocket `question_response` / `question_abort` frames, `QuestionAnswer` /
  `QuestionReply`, and the `id` / `channel` / `timeout_seconds` keys that the
  `ask_user` argument and the rendered frame each carried. An answer is an
  ordinary chat message now, so there
  is no pending call to settle, abort, or time out. The `question` frame stays as
  the TUI's rendering channel (#851)
- Websocket channel: a chat message sent while the topic's agent is blocked in
  `ask_user` no longer answers the oldest pending question — it routes to the
  topic as its own turn again. The TUI answers through the question box, so the
  hijack only ever turned an unrelated message into an answer while the rest of
  the batch stayed unreachable (#848). The feishu pipe's version of the same
  routing goes with it (#849)
- Feishu pipe: interactive questions are gone — the numbered-options question
  card, the relay of `ask_user` question frames, and the plain-reply answer
  routing (`QuestionHub::try_answer` / `pending_for`, and the `seq` ordering they
  existed to serve). One chat reply could only settle one question, so a
  multi-question call stalled, and the stall held back the reply the user was
  waiting for; a feishu turn now asks in its reply text like any other message.
  `FeishuClient::send_card_message` and `QuestionRequest::position` (the card's
  `第 N/M 题` header) had no other user and are removed with it; the progress
  watcher's `attach` mode existed only to re-arm a card after an in-chat answer
  (#849)
- TUI "new chat" flow: the leader-key `new chat` action (`Ctrl+P` → `n`) and
  the plain `c` key on the dashboard, which opened the same pattern-select
  screen. Chat is now entered only on an existing topic (Enter / leader
  `open chat`); the `ChatPhase` machinery behind the pattern-select screen
  is gone. New topics come from `jyc open`, `/clone`, `send_to_topic`, or
  the REST API — the inspect `list_patterns` endpoint itself is unchanged
- The `jyc_reply_message` tool and the `jyc mcp-reply-tool` hidden
  subcommand (with the `jyc-mcp` crate) are gone — nothing registered the
  tool, and the subprocess MCP server had no remaining consumers

### Fixed

- Topic info diff no longer assumes the default branch is `main`: the base
  is resolved as `origin/HEAD` (set by clone), falling back to local
  `main`, then `master` — repos whose default branch is `master` (or
  another name behind origin) now show committed branch files instead of
  silently dropping them
- TUI chat no longer strips the leading spaces of a sent message's lines:
  CommonMark indentation handling (paragraph de-indentation, indented code
  blocks) is neutralized for human turns at any depth — leading spaces survive
  as non-breaking spaces; fenced code is untouched — and sending trims
  trailing whitespace only, so indented text, pasted code, and ASCII art
  keep their shape (#837, #838)
- Switching topics in the TUI chat (explorer or topic list) now drops the
  previous topic's pending `ask_user` question batch — same as Esc — so a
  queued question from the old topic can no longer suppress the new topic's
  question box
- `job_create` no longer accepts both `cron` and `at` in one call (cron
  silently won); the tool description and schema now state up front that
  exactly one of them is required, and mention `job_delete`/`job_toggle` for
  stopping a recurring job later

- Scheduled-job turns always run in build mode: a job turn no longer
  inherits the topic's interactive plan-mode override (from `.jyc/mode-override`
  or `pattern.mode`). A plan-mode job turn waits for a user approval that can
  never arrive and silently emits no output (#830)

- Scheduled jobs are now stamped with the channel from the live turn context
  instead of a name guessed from the directory layout — on channels whose
  workspace is the agents root the guess produced a bogus name (the OS
  username), so firing failed with `topic manager not found` and the job
  retried forever (#829)

- The job scheduler discovers each due job only once: a job reachable
  through both the workspace scan and a topic manager's custom paths
  (or overlapping workspace dirs) was previously discovered twice and
  double-fired (#829)

### Changed

- TUI chat: refocusing the message pane (the `focus chat` command, a click,
  or the Tab cycle) now parks the cursor at the end of the *visible page* —
  the bottom visible row — instead of always at the end of the transcript,
  so a refocus while scrolled up in history continues reading where the user
  is looking. Pinned to the bottom the landing is unchanged (the last row
  with text), and an empty transcript still waits for content (#828)

- Topic state resolution no longer has a silent fallback: `jyc_dir` panics
  when a topic has no registered state dir instead of quietly writing into
  `<topic_dir>/.jyc`. Every topic's state is now registered before any
  state access — config pins adopt at startup, jyc-owned workspace topics
  register their in-dir `.jyc` at activation (router, scheduled jobs,
  `jyc_send_to_topic`, dashboard proxy), fork/clone adopt as before — so a
  pinned user repo can never accumulate state again. A topic's state dir is
  also chosen once and never moved: adoption is first-wins (the re-pin carry
  that relocated state between dirs is gone), and `/close` keeps the
  registration so a reopened topic reuses the same state location (#825)

### Added

- `/clone [name] [path]` copies this topic's directory into a topic of its own:
  `/fork` shares the parent's workspace (only the state is separate), `/clone`
  gives the new topic a physical copy, so from then on the two edit different
  files. It seeds the same state `/fork` inherits (`cloned-from` names the
  source) and skips what must not be duplicated — scheduled jobs, the token
  meter, the logs — while the source's state dir is never copied as content.
  A name alone lands beside this topic's dir, a path alone takes the path's last
  component as its name, neither means `<this>-2`. The target must be empty: a
  clone never merges into existing data, and it refuses a dir another topic
  works in, a dir inside the source, and a name that is already a topic. The
  copy runs off the runtime threads, so the reply arrives when it is done.
  Websocket-only, and it does not switch focus
- `/close --force --purge` deletes the topic directory as well. `/close --force`
  alone closes a pinned or cloned topic by deleting its state and keeping the
  directory (usually someone's repo or a copy they made on purpose), which is
  what `/fork` and `/clone` rely on; purge is the explicit "and this one too".
  It refuses, deleting nothing and leaving the topic open, when the dir is also
  another topic's (pinned or merely living in the workspace — a fork shares its
  parent's), contains another topic's dir, is one of jyc's own
  (`~/.local/share/jyc/agents` and the workdir's `agents/`), or holds another
  topic's state directly under one of those roots
- The chat progress tail animates, and `ctrl+p p` collapses it to one line. The
  row doing the current work — and the `AI is thinking...` placeholder shown
  before the first event — carried a static `⏳`, so a long silent stretch of
  tool work read exactly like a hung one: it now shows a braille spinner in
  bright yellow, the only moving, colored thing on the tail — the text around
  it sits still, no blinking. Minimal progress mode replaces the whole tail
  with a single line (`⠹ 12.4s · thinking... | bash` — spinner, loop elapsed,
  current phase, with the most recent tool kept after ` | ` so a thinking
  streak does not overwrite it; a failing round shows a static red `error`),
  leaves the completed-turn `thinking` line out of the history, and is on by
  default for a restarted TUI (`ctrl+p p` expands it again). Session-only,
  like the other display toggles; diff colors and the red error row survive
  both modes (#821)

- `ask_user` can ask several questions in one call: pass `questions` (up to 5
  `{question, options, allow_multiple}` items) instead of the single
  `question`/`options`, and the user settles them as one flow instead of the
  agent interrupting once per decision. In the chat question box the questions
  queue — each keeps its own cursor and `[x]` marks, `←`/`→` step between them
  (wrapping), `Enter` confirms one and moves to the next, and nothing is sent
  until the last is confirmed, so going back to change an answer is free. The
  border reads `Question 2/3 · ←/→`. Answers come back paired with what they
  answered:

  ```
  Q1: Which sections?
  A: Added

  Q2: Branch name?
  A: Selected: feat/x, fix/x
  ```

  A question asked on its own behaves exactly as before: `Enter` sends it
  straight away and the answer is the bare option text. Feishu gets one card
  per question headed `第 N/M 题` and the user answers them in order, one reply
  each; the card is still plain text, so nothing needs a button callback. A
  channel with no interactive support (email, wecom) fails the first push and
  the call returns at once rather than waiting for answers nobody can give.
  Restart the daemon alongside the chat UI: an older daemon never sends the
  second question (#819)

- `ask_user` can ask a multi-select question: pass `allow_multiple` and the user may
  pick several options. In the chat question box `Space` marks the option under the
  cursor (`1-9` mark too), marked options render as `[x]`, and `Enter` sends every mark
  in list order — Enter with nothing marked cancels the question rather than answering
  it with nothing. One pick still reaches the model as exactly the option text, as
  before; several come back as `Selected: a, b`. Channels whose UI cannot mark options
  (email, plain text; feishu and wecom cards for now) answer with one option and may
  ignore the flag. The question box and the daemon must be the same build: an older
  daemon still parses the new list-shaped answer, finds no `choice` in it, and reports a
  dismissal — no error, no log — so restart the daemon after pulling (#818)

- `/fork [name]`: branch another topic off the current one. The new topic is an
  ordinary topic — listed, addressable, closable — and **works in this topic's
  directory**, because a fork continues the same files; only its state is
  separate, at `<data-home>/agents/<name>/.jyc`. It starts from this topic's
  running context, chat history, task list, backlog and per-topic overrides, so
  work can continue down a second path while both keep running and keep their own
  transcript. What
  it does *not* inherit is the token/cost meter (that usage was billed to the
  parent, so the fork starts un-metered), the per-topic logs, the channel routing
  metadata, and `jobs/` — a copied schedule would fire the parent's jobs in the
  fork too. A name left out becomes `<topic>-2`, `<topic>-3`, …; `/fork` is
  websocket-only, like `/pin`, because elsewhere the topic name *is* the routing
  address; and it does not switch focus — pick it in the topic list (#814)

- Agent task list: three built-in tools. `task_create` writes the plan for the current
  piece of work as one-line steps (1–20, numbered 1..N, replacing any list that was
  there), `task_update` marks one item `pending` / `in_progress` / `completed` as the
  work moves, and `task_list` reads it back. The list lives in `.jyc/tasks.json`, is
  part of topic info — shown between the cost row and the changed-file list in the TUI
  topic-info pane and by `/info`, with the same markers and ids the tools print — and
  `/reset` and `/new` clear it. A plan now survives context compression, and the user
  can follow progress without asking (#810)

- TUI chat message pane: focusing it (Tab, `Ctrl+P` → `c`, or the wheel over the
  messages) shows a cursor bar on one transcript row. `j`/`k` and `↑`/`↓` move it,
  and the view scrolls only once the cursor reaches the pane's top or bottom row;
  digits count rows (`5j`, `20k`) and `gg`/`G` jump to the ends; a topic opens with
  the cursor at the end of its newest message. `PgUp`/`PgDn`,
  `Ctrl+B`/`Ctrl+F` and the wheel still just scroll — and carry the cursor by the
  same distance, so it keeps the screen row it was on while the text slides
  underneath — unless rows are selected, which scrolling leaves alone. `yy`, `y3y`
  and `3yy` copy that many rows from the cursor. `J`/`K` (or `Shift+↑`/`↓`) open a
  selection at the cursor which every later movement grows, and one `y` copies
  the selected rows, ending with the cursor back on the first of them (`Esc` drops
  the selection where it stands instead). The cursor row and the selected rows
  share one highlight colour. Everything lands in the *terminal's* clipboard via OSC
  52 (tmux needs `set -g set-clipboard on`) (#806, #807, #808)

- `/skill:<name>` — one command per skill a topic can use, so a single message can
  ask for a skill without a `[[commands]]` entry and without changing session state
  (`/skill on <name>` remains the persistent switch). The names are resolved at
  runtime through the agent's own skill discovery (config filters applied, the
  `/skill` toggle ignored) and feed `/skill on|off` completion, `/?` and the `/`
  popup, so listing and dispatch cannot disagree (#809)

### Changed

- Agent skills validate in tiers instead of mandating a local test run: the local gate is
  the project's check command plus the formatter, and the test suite runs where the project
  puts it — CI when it has one, locally only when nothing else covers it. Also stops the
  dev-loop skills from blocking a turn waiting for CI to finish
- TUI mouse wheel: it now scrolls the topic info pane when the cursor hovers it,
  instead of answering only over the messages. The activity log, the explorer and
  the input area keep absorbing the wheel, and scrolling the info pane deliberately
  does not take focus, so wheeling while typing is safe (#816)
- Built-in slash commands are now declared in exactly one place.
  `crates/jyc-core/src/command/builtin.rs` holds a 22-row table whose expansion is
  the spec list the `/` popup and `/?` render, the name set config validation
  checks against, and the topic worker's registry — a command can no longer be
  registered but hidden from the popup. `CommandHandler` keeps behaviour only
  (`name()`, and `description()` which was already dead code, are gone) and
  `CommandRegistry::register` takes its key from the caller; user-defined commands
  normalize through `CustomCommandHandler::command_key`, now shared by dispatch and
  display instead of each rebuilding it. The three "keep these two lists in sync"
  tests are gone with nothing left to sync, and `config.example.toml`'s stale
  12-name list of built-ins became a pointer to `/?`.
- `validation::validate_config` and `validate_config_strict` take the built-in
  command names as a parameter, and `jyc_types::config::BUILTIN_COMMAND_NAMES` is
  deleted. `jyc-types` sits below `jyc-core`, so it used to keep its own copy of
  the names for the "shadows a built-in" rule; callers now pass
  `jyc_core::command::builtin::BUILTIN_COMMAND_NAMES` and the crate holds no
  command data at all. The in-crate validation tests pass an empty set (the two
  shadowing tests pass their own fixture), and the `config.example.toml` guards
  live in crates that already depend on jyc-core.
- TUI selection cursor: the topic explorer, the `Select Pattern` panel and the
  dashboard's topic table mark the focused row with the same two-column `→`
  gutter and dim styling as the `/` command and `ask_user` popups, instead of a
  black-on-cyan bar or reverse video. The explorer's status dot keeps its own
  color, its cursor row stays marked while the pane is unfocused, the gutter is
  reserved on every row so names never shift sideways, and the pattern list no
  longer loses the blank gutter on its unselected rows (#804)

- TUI popups: the focused row is marked with a `→` in a two-column gutter and
  dimmed (`Modifier::DIM`) instead of being repainted, so it keeps the
  terminal's own foreground color. The `/` command list loses its full-width
  cyan highlight bar for this; the `ask_user` question box, whose selected
  option was black-on-cyan, gets the same gutter so the two popups read alike.
  Rows that open a deeper level keep their `▸` marker (#803)

- TUI `/` command popup: every Tab completion now ends in exactly one space,
  so the input field is left ready for the next argument. The space is also
  the marker that opens a deeper level, and what dismisses the popup when a
  command has nothing left to complete (`/plan` + Tab now yields `/plan ` and
  closes in one keystroke) — the second Tab of the old two-step completion and
  its `PopupAction::CopyToInput` are gone (#798)

- TUI chat message pane: no more `You:` / `AI:` speaker labels. The human side
  — including a remote sender piped in from another channel — renders as a
  full-width dark background block with a blank painted row of padding above
  and below, keeping the terminal's own foreground so it reads at the same
  brightness as the agent's replies. The block's background is `#343541`, the
  shade pi's dark theme paints its own user turns with, since the terminal's
  own dark gray (ANSI palette slot 8) turned out to be the theme's *bright*
  black.
  Both sides' text is inset one column, a reply is followed by a blank row of
  its own, and the `┄┄┄┄` separator between a human turn and its answer is gone
  — the block's padding delimits it (#798, #799, #800, #801, #802)

- TUI leader popup (`Ctrl+P`) on the chat screen lays its commands out in
  columns instead of one row each: that scope has seventeen of them, so the
  popup was taller than the terminal and the last ones fell off the bottom. The
  descriptive one-per-row layout stays on the dashboard, where five commands fit
  comfortably (#801)

- TUI dashboard chat: Esc on a pending `ask_user` question now only hides
  the question box and returns focus to the editor instead of cancelling
  the question server-side. The question stays pending — the next typed
  message becomes the free-form answer via the websocket inbound
  interception — so free-input answers are possible without a dedicated
  UI affordance. The daemon-side timeout still bounds a question left
  unanswered; a second question no longer pushes the first out of the box, it
  joins it (see the multi-question entry above) (#791)

### Added

- TUI `/` command popup is multi-level: the input field text *is* the path, so
  `/model <id>`, `/thinking show`, `/context dump on`, `/skill on <name>`,
  `/mcp off <server>`, `/bill 2026-08` and `/backlog push` each open their own
  level under the input field, filtered as you type. Tab completes a value and
  steps into the next level; Enter fires the command at that first level and,
  below it, completes exactly like Tab — sending then takes a second Enter; a
  level with nothing to select (free-text arguments like `/grant <path>`, or a
  filter that matches no value) sends what was typed. The top rule names the
  current level (`── Commands ──`, then
  `── /model ──`) and rows with a deeper level carry a `▸`. Values come from a
  new `CommandInfo::args` field in the inspect payload (`jyc-core`'s
  `command_args` table), so `/model` is no longer special-cased in the client
  and a server older than the field still gets a model picker (#797, #800)

- Websocket channel: a chat message sent while the topic's agent is blocked in `ask_user` now answers the pending question instead of being rejected with a busy-topic error — the free-input path for websocket clients such as the TUI, mirroring the feishu pipe's interception. The text-fallback routing moves from the feishu pipe into a shared `QuestionHub::try_answer` (#791)

- Message-before-question delivery ordering: when the agent's response
  carries narration text alongside a blocking `ask_user` (native tool call
  or a literal `<ask_user ...>` tag written into the reply text by models
  with weak function-calling), the text is delivered first as a progress
  reply and the conclusion after the answer still auto-delivers — the user
  reads the message before the question card arrives. An embedded
  well-formed tag is recovered as a real question (answer lands in the
  transcript as a synthetic tool result); a malformed tag is stripped from
  the delivered reply and logged, so raw tool syntax never ships to the
  user. The tag parser tolerates both documented option styles
  (`options="a, b"` and the misquoted `options="a", "b"`).
- Feishu question answers are covered by the standard live progress card:
  once a text reply is routed to the `QuestionHub`, the pipe attaches a
  progress watcher to the still-running run (attach mode — skipped when a
  live card already exists), so the chat shows the same ⏳/✅ indicator as a
  regular message until the final reply, replacing the previous static ack. (#788)
- Feishu interactive cards for the `ask_user` tool: a pending question is
  relayed to the originating chat as a card JSON 2.0 message with numbered
  options; the user answers by replying with the number or option text,
  which the feishu pipe routes to the `QuestionHub` instead of the topic.
  (Button callbacks are not used: openlark-client's WebSocket layer drops
  `card.action.trigger` frames, so numbered-reply keeps everything on the
  existing long connection.) Slash commands are never intercepted. (#780)
- Built-in `ask_user` tool for interactive user decisions: the agent asks a
  question with selectable options and blocks mid-turn until the user
  answers (via the websocket `question` / `question_response` protocol),
  cancels, or a `timeout_seconds` deadline expires (default 300s). Answers
  are routed in-process through a daemon-wide `QuestionHub`
  (`jyc-core`), bypassing the busy-topic queue. The TUI renders the
  question as an input-area modal (↑/↓ or j/k select, 1-9 quick choose,
  Enter confirm, Esc dismiss). Channels without interactive support fail
  gracefully so the model asks in plain text instead. (#779)
- Built-in `${VAR}` config resolution: `${JYC_CONFIG_PATH}` expands to the
  directory of the config file being loaded (typically `~/.config/jyc` on
  Linux/macOS) whenever the process environment does not define it (a real
  env var still wins). Hook `shell` entries additionally support
  `${JYC_TOPIC_PATH}` / `${JYC_TOPIC_STATE_PATH}`, expanded per topic at
  spawn (argv form included) and exported as `JYC_TOPIC_PATH` /
  `JYC_TOPIC_STATE_PATH` env vars. See `docs/hooks.md`. (#775)
- Claude Code-compatible lifecycle **hooks** (`[[hooks]]` global +
  `[[agents.<name>.hooks]]`, merged like `[[commands]]`): external commands
  run at 7 agent-layer events (`message_received`, `pre_tool_use`,
  `post_tool_use`, `post_tool_use_failure`, `reply_send`, `session_start`,
  `session_end`) with the event JSON on stdin. Exit 2 blocks the action
  (AI dispatch / tool call / reply delivery); other codes and timeouts fail
  open. Writing an event in Claude Code spelling (`PreToolUse`, `Stop`, …)
  also switches that hook's payload to the CC field layout, so existing CC
  hook scripts work unmodified. See `docs/hooks.md`. (#774)
- MCP OAuth 2.1 support for remote MCP servers (`[mcps.oauth_dcr]`): endpoint
  discovery, Dynamic Client Registration and browser-consent authorization via
  the new one-time `jyc mcp auth <name>` command (headless paste-back flow).
  `jyc serve` stores the tokens under the data dir and refreshes them
  automatically at every MCP connect — no restart or re-auth until the refresh
  token itself expires. (#772)
- `/skill on|off|reset <name>` and `/mcp on|off|reset <name>` slash commands:
  toggle one skill or MCP server on/off for a single topic at runtime
  (persisted in `.jyc/*-override.json` until reset; takes effect from the next
  message). (`on` un-disables config-level `disabled_mcps` / joins the skill
  whitelist; `off` force-disables even if whitelisted). (#764)
- Cache utilization display: the dashboard topic info pane / status line
  show a `Cache util: N%` row (cache-hit tokens ÷ total input tokens),
  and the `/bill` report gains a per-row `cache util` column. Hidden for
  providers that report no cache data. (#761)

### Changed

- Skill descriptions injected into the system prompt are truncated at
  200 characters (word-boundary, trailing `…`), cutting ~1-2K input
  tokens per LLM call; the full SKILL.md body is still loaded on
  demand. (#762)
- Config key `disabled_mcp_servers` renamed to `disabled_mcps` at
  channel, pattern, and agent level; the old key is still accepted via
  a serde alias. (#762)
- Unknown slash commands (a top-of-message line starting with `/` that
  matches no registered command) now reply with an "Unknown command"
  error instead of silently passing through to the agent as message
  text. (#763)

### Fixed

- **`ask_user` answers reaching the wrong question** — with more than one
  question open for a topic, `QuestionHub::pending_for` returned the first match
  out of a `HashMap`, so a plain text reply (feishu card, email, a hidden question
  box) could be handed to whichever question the map happened to visit first.
  Entries now carry a registration stamp and the oldest open question is answered
  first — the order the questions reached the user in, which is what makes
  answering a batch one reply at a time work (#819)
- The TUI's topic-info pane could not be scrolled to its own bottom. It wrapped its
  content with ratatui while clamping the scroll offset against a count of *logical*
  lines, so in the 20%-wide pane — where a changed-file path takes two or three rows
  — the last entries of a long file list stayed permanently clipped no matter what
  `End`/`G` did. The pane now wraps in Rust with the same helper the message pane
  uses, so the clamp counts the rows actually drawn (#816)
- `/fork` was missing from the TUI's `/` command popup and from `/?`, even though
  typing it worked: the popup list and the worker's registry were two separate
  hand-maintained lists, and #814 had added the handler to one of them. Both now
  come from the single command table, so anything registered is listed (#815)
- A long `bash` command was unreadable in the TUI: the collapsed row caps a tool's
  primary field at 160 columns, and the expanded tool detail (`ctrl+p T`) skipped
  that field because the row "already shows" it — so the rest of the command existed
  nowhere on screen. The expanded row now renders the whole value, wrapped over rows
  by #811, with nothing repeated. The cap itself moved out of `jyc_types::inspect`
  (which now only extracts, uncapped) into each surface that displays a tool row, so
  the collapsed row and the feishu progress card each set their own limit. (#812)
- TUI chat progress tail: rows wider than the pane ran straight off the right edge
  and everything past them was unreachable, scrolling included — the transcript
  `Paragraph` deliberately does not wrap (one line is exactly one screen row, which is
  what the scroll, cursor and selection maths count), so message bodies are
  pre-wrapped while the tail never was. Every tail row now wraps to the pane width:
  the `ctrl+p T` tool detail and the diff lines inside it, the collapsed
  `thinking` summary (whose `(ctrl+p t)` hint was the first thing to fall off), and
  the error line. Wrapped rows keep the two-column indent and are padded by their own
  marker's display width, so a tall block still reads as one entry and the marker
  stays on its first row only. The `… (N more lines)` cap on how much tool input gets
  expanded is a separate question and stays as it was (#811)
- TUI selectable lists now scroll to follow the cursor. The `/` command popup,
  the `Select Pattern` list and the `ask_user` question box drew their whole list
  into a fixed-height box, so the `→` walked down into the clipped rows and
  stopped being visible: everything below the last visible row was unreachable,
  while Enter still fired whatever index the cursor had reached. All three now
  take their window from one shared `window_offset` rule — the one the topic
  explorer already hand-rolled, and what ratatui's `Table` does internally for
  the dashboard's topic table. The command and pattern lists also drop their
  `Wrap`, which put a long entry on a second row and moved the rows out from
  under the cursor; long entries now clip at the pane edge. The question box
  measures the rows it draws (question, hint, spacers, capped options), so it can
  no longer be sized a row short of its own content. (#805)
- TUI chat pane: the round rules (`── 10:19 ──` and `──── 5s ──`) are exactly
  pane-wide — each was one column too long and relied on the renderer clipping
  the excess (#800)
- Truncated LLM streams and leaked tool-call syntax no longer reach users as
  broken auto-delivered replies (#786): an SSE stream that reaches EOF
  without the provider's `Done` marker is now a transient error (retried by
  `complete_with_retry` with the usual backoff) instead of being silently
  accepted as a complete text-only response — previously the half-finished
  text was auto-delivered to the user with the `— auto-delivered` trace.
  Likewise, a response whose text BEGINS with raw tool-call syntax (models
  with weak function-calling emit `<call tool=…>` into the text channel
  instead of structured `tool_calls`; detection is start-anchored so legit
  replies quoting the syntax are not rejected) fails the attempt as a
  transient "provider format failure" and is retried, so the syntax is
  never shipped to the user as a "reply".
- Feishu live progress card: the PATCH payload (`progress_card_json` /
  `final_card_json`) was a bare `{"elements": …}` body without
  `config.update_multi`, which Feishu's update-card API rejects, so the
  card froze at its initial state forever. Both builders now emit the full
  card-JSON 2.0 envelope matching the initial card. The watcher also no
  longer exits after 3 consecutive PATCH failures (transient errors only
  skip a tick; the first failure logs at warn level), and the per-topic
  dedup registry entry is released on every watcher exit path, guarded by
  message id — previously a non-terminal exit leaked the entry, so every
  later message PATCHed a stale, buried card instead of posting a fresh
  progress indicator. (#782)
- Feishu live progress card: a dedup registry entry left by a previous run
  could be reused by the next message's watcher (the previous watcher's
  release lags behind its final PATCH), so after a cancelled/superseded run
  — the ask_user question flow in particular — no fresh "⏳ 处理中" card
  was posted and updates kept landing on the old, buried card. While
  waiting, a fresh `ProcessingCompleted` now marks the previous run as
  done: the arming watcher ignores the stale entry and posts a new card,
  and only the entry's owner releases it on exit. (#784)
- The billing ledger now persists the applied cache-creation (write) rate
  per call (`cache_creation_rate_per_million`, effective value =
  configured rate or the cache-hit rate when unset). The other three rate
  columns were frozen per-row, but the write rate was dropped at every
  write site (#507 leftover), so write-bucket cost could not be replayed
  or audited from a row alone — and base vs long-context write rates were
  indistinguishable. (#770)
- GitHub/Gitee agent skills: planner-created PRs now carry the issue's
  assignees and labels unconditionally (GitHub: one atomic
  `gh pr create --assignee/--label`; Gitee: mandatory create+PATCH+labels
  chain, since its API supports neither at create — the skippable
  post-create copy step had left a real PR untriggered for 6h). The unused
  `ready-for-dev` handoff label was removed from all four skills, and
  github/gitee-developer now start implementing on the first opened-PR
  trigger instead of idling until a human comments. (#768)
- Responses API (`openai-responses`) now tracks GPT-5.6+
  `usage.input_tokens_details.cache_write_tokens`: cache writes bill
  via `cache_creation_per_million` instead of being folded into the
  full-rate input bucket. (#766)
- `/mcp on|off|reset` now takes effect on the next message without
  requiring a service restart or `/reset`: the per-topic tool-registry
  cache key includes `mcp-override.json`'s modification time, so toggle
  writes invalidate the cached registry (previously a stale registry
  could keep being served until an unrelated config reload).
  Superseded same-topic registry entries are evicted on rebuild. (#765)

- The #786 leak guard no longer misses underscore-wrapper tool-call syntax:
  `<response_tools>` / bare `<invoke>` variants and preamble-prose leaks that
  ship complete open+close blocks are now detected (closing tags mid-text)
  and retried instead of auto-delivered as raw XML. (#793)

### Fixed

- Provider format-failure guard: mixed-mode streams (valid structured
  tool calls plus leaked tool syntax in the text channel) and degenerate
  streams that parse into empty-name tool calls no longer bypass the
  guard and ship `<response_tools>` garbage to the user — both are
  retried, then surfaced as terminal errors (#794)

- Provider format-failure guard: mixed-mode responses (valid structured
  tool calls plus prose) no longer trip on a single start-anchored marker
  quote — once tool calls have parsed, only closing tags and repetition
  storms flag the text channel, narrowing false positives on legit
  meta-discussion (#795)

### Changed

- TUI chat: the `/` command popup and the `ctrl+p` leader popup now open as
  a borderless list directly below the input field (full-width top rule, no
  side or bottom borders) instead of floating over the transcript. The
  command popup also dropped its own filter box — the chat input field *is*
  the filter, so typing, backspacing and cursor keys keep editing the field,
  Tab completions land there, and the popup closes once the field is
  emptied. Sending a command now clears the field instead of leaving the
  command there to be sent twice, and Enter with no matching item sends what
  you typed rather than doing nothing (#796)

### Fixed

- Startup no longer panics on channel-infrastructure directories under the
  agents root: the scheduled-jobs discovery scans every channel workspace's
  children as topics, and the email channel's `.imap` mailbox-cursor dir is
  not a topic (it has no registered state dir), so both scan passes tripped
  the `jyc_dir` no-fallback invariant. Discovery now skips workspace
  children that are not registered topics (#828)

- The TUI message-pane cursor resets to the last line every time the pane
  gains focus (the `focus chat` command or a click), instead of keeping the
  position — and any stale selection or armed yank — from the previous visit.
  Re-focusing while the pane already has focus is a no-op, so a wheel tick
  cannot yank the cursor off the row being read

- The same reset now also applies when the Tab focus cycle lands on the
  message pane: the cycle used to switch panes without the reset, so the
  cursor reappeared at the position left by the previous visit (#827)

- `/close` no longer claims it deleted a directory it kept: the reply says
  whether the dir is gone or kept, and offers the exact `--purge` command for
  the kept case
- `/clone` and `/fork` now preserve symlinks when copying a topic dir instead of
  following them (a link into a shared checkout used to fail the copy outright,
  and following it would have copied the outside tree in)

## [0.3.18] - 2026-09-12

### Added

- Central billing ledger: per-call billing entries now append to
  `<data_home>/billing/bill-YYYY-MM-DD.jsonl` (each entry tagged with its
  topic label) instead of the per-topic `.jyc/` directory, which
  `close_topic` deletes — cost history now survives topic close/auto-close,
  so `/bill` and the dashboard keep reporting closed topics' spend.
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
- Subscription-plan billing: providers accept `billing = "subscription"`
  and `monthly_fee` (provider-level — a flat-fee coding plan covers every
  model under the provider). Ledger entries from such providers are tagged
  `subscription` (cost = notional API-equivalent value, not real spend);
  `/bill` renders them in a separate section with a per-provider
  utilization line (the provider's combined notional value across all its
  models vs the monthly fee prorated to the report scope).

### Changed

- Feishu message cards upgraded to card JSON 2.0: the 2.0 rich-text
  component renders CommonMark natively — GFM tables (including `/bill`
  output, with built-in pagination), `#` headings and `>` quotes that
  card JSON 1.0 showed as literal text. The previous 1.0 workarounds
  (table-component splitting, code-fence rendering) are removed; the only
  remaining transform is fencing ASCII box-drawing art (e.g. tree
  diagrams) and splitting messages past the 4-tables-per-component limit.
  Requires Feishu client 7.20+ (older clients show an upgrade prompt).
  (#755)
- Dashboard TUI chat screen: the topic explorer pane no longer has a
  title or top border, and both the explorer and topic info panes get
  one row of top padding so content does not hug the pane top. (#756)
- Topic state relocation: a pinned/ad-hoc topic's `.jyc` no longer lives
  inside its working dir. It is adopted to
  `<data_home>/agents/<name>/.jyc` (config-key agents keep their name,
  ad-hoc pins get a path-derived `_`-prefixed name) and granted to the
  agent's file-access sandbox. Unpinned topics are unchanged
  (`<topic_dir>/.jyc` fallback). (#739)
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
  latest releases. (#735)

### Fixed

- Dashboard TUI rendering corruption around URL-containing chat messages:
  `HyperlinkBackend`'s link-row re-emission printed every cell in the row,
  including the continuation cells of wide chars — which hold a space
  symbol in ratatui's buffer. The wide char already advanced the cursor
  past that column, so each printed continuation space shifted the rest of
  the row right by one cell; on CJK/emoji-heavy rows the cumulative shift
  overflowed the row, physically scrolling the screen and desyncing
  ratatui's diff baseline (interleaved old/new text and ghost fragments,
  worse while scrolling). Verified with a real-terminal replay harness
  (tmux 3.5a pane + `capture-pane`) that reproduces the corruption and
  confirms the fix. Re-enabling mouse capture now also forces a full
  repaint, since the terminal's own scrollback could move content while
  capture was off. (#754)
- OSC 8 hyperlinks: a URL wrapped across rows now resolves every row
  fragment to the full URL (previously each fragment linked to its own
  partial text, and continuation rows were not clickable at all). Link
  detection is clipped to the chat message pane's rectangle, so text from
  an adjacent pane (e.g. the topic info pane) can no longer leak into a
  link target.
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

- One-time migrations retired (#757): the github/gitee `.github` state-dir
  rename, the in-topic `.jyc` state adoption, and the `.jyc/thread-name`
  → `.jyc/topic-name` rename all ran in earlier releases and are gone.
  **Upgrade note:** per-topic `.jyc/bill-*.jsonl` ledgers written by
  releases ≤ 0.3.17 are NOT folded into the new central billing ledger —
  historical spend stays in the topic state dirs and is skipped by `/bill`
  unless moved manually.
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
