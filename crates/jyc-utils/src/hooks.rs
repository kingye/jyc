//! External hook executor.
//!
//! Compiled from [`HookConfig`] entries (global + per-agent, merged by the
//! caller in that order). Each configured event name maps to a canonical
//! [`HookEvent`] plus a payload [`HookDialect`]: jyc snake_case names emit
//! jyc-shaped JSON on stdin, Claude Code names emit CC-shaped JSON so CC
//! hook scripts work unmodified. Exit-code semantics are shared:
//! `0` proceed, `2` block (stderr becomes the reason), anything else —
//! including timeouts and spawn failures — fails open with a warning.

use std::process::Stdio;
use std::time::Duration;

use jyc_types::config::{HookConfig, HookDialect, HookEvent};
use regex::Regex;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;

/// Everything a hook site can offer. Best-effort by design: only fields
/// with values appear in the payload (a websocket-only flow has no sender,
/// tool events have no message). Hook scripts must treat every field
/// beyond `hook_event_name`/`agent`/`topic`/`cwd` as optional.
#[derive(Debug, Default, Clone)]
pub struct HookCtx {
    pub topic: String,
    /// Working directory: also the spawned hook's `current_dir`.
    pub cwd: String,
    pub channel: Option<String>,
    pub message_content: Option<String>,
    pub sender: Option<String>,
    pub sender_address: Option<String>,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_response: Option<String>,
    pub reply_text: Option<String>,
    /// `session_start` origin: `startup` | `reset` | `new`.
    pub source: Option<String>,
    /// `session_end` cause: `reset` | `new` | `close`.
    pub reason: Option<String>,
    /// Inbound message metadata passthrough (channel-forwarded context,
    /// e.g. the original sender behind a websocket relay). jyc dialect
    /// only — CC scripts don't know the key; best-effort like the rest.
    pub metadata: Option<Value>,
}

/// Result of running all hooks registered for one event.
#[derive(Debug, PartialEq, Eq)]
pub enum HookOutcome {
    /// No hook blocked (includes fail-open on errors/timeouts).
    Proceed,
    /// A hook exited 2; the string is its stderr (or a fallback label).
    Block(String),
}

struct CompiledHook {
    event: HookEvent,
    dialect: HookDialect,
    raw_name: String,
    matcher: Option<Regex>,
    shell: Vec<String>,
    timeout: Duration,
}

/// An ordered set of compiled hooks for one agent scope (global
/// `[[hooks]]` merged with the agent's `[[agents.<name>.hooks]]`, in that
/// order). The agent name is carried so payloads self-identify without
/// every call site threading it through.
#[derive(Default)]
pub struct HookSet {
    hooks: Vec<CompiledHook>,
    agent_name: String,
}

impl HookSet {
    /// Compile configs into a hook set. Assumes validated input (config
    /// load rejects unknown events / bad regexes); unparseable entries are
    /// skipped with a warning rather than panicking.
    pub fn from_configs(configs: &[HookConfig]) -> Self {
        let hooks = configs
            .iter()
            .filter_map(|cfg| match HookEvent::parse(&cfg.event) {
                Some((event, dialect)) => Some(CompiledHook {
                    event,
                    dialect,
                    raw_name: cfg.event.clone(),
                    matcher: cfg.matcher.as_deref().and_then(|m| Regex::new(m).ok()),
                    shell: cfg.shell.clone(),
                    timeout: cfg.hook_timeout(),
                }),
                None => {
                    tracing::warn!(event = %cfg.event, "Unknown hook event — skipping hook");
                    None
                }
            })
            .collect();
        Self {
            hooks,
            agent_name: String::new(),
        }
    }

    /// `from_configs` variant that stamps the agent identity into payloads.
    pub fn for_agent(configs: &[HookConfig], agent_name: &str) -> Self {
        let mut set = Self::from_configs(configs);
        set.agent_name = agent_name.to_string();
        set
    }

    /// Merge an agent's hook scope from the full app config: global
    /// `[[hooks]]` first, then `[[agents.<agent_name>.hooks]]` — the same
    /// ordered-union rule as `[[commands]]`. `agent_name` empty (or not a
    /// configured agent) yields the global set only.
    pub fn merged_for_agent(
        global: &[HookConfig],
        agents: &std::collections::HashMap<String, jyc_types::AgentConfig>,
        agent_name: &str,
    ) -> Self {
        let mut configs: Vec<HookConfig> = global.to_vec();
        if let Some(agent) = agents.get(agent_name) {
            configs.extend(agent.hooks.iter().cloned());
        }
        Self::for_agent(&configs, agent_name)
    }

    pub fn agent_name(&self) -> &str {
        &self.agent_name
    }

    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Number of compiled (valid) hooks in the set.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Run every hook registered for `event`, in order.
    ///
    /// `subject` is what the optional per-hook `matcher` filters on
    /// (tool name / topic name / source / reason — see `HookConfig`).
    /// Returns `Block` on the first exit-2; later hooks do not run.
    pub async fn run(&self, event: HookEvent, subject: Option<&str>, ctx: &HookCtx) -> HookOutcome {
        for hook in &self.hooks {
            if hook.event != event {
                continue;
            }
            if let Some(re) = &hook.matcher {
                let matched = subject.is_some_and(|s| re.is_match(s));
                if !matched {
                    continue;
                }
            }
            match run_one(hook, event, ctx, &self.agent_name).await {
                RunResult::Proceed => {}
                RunResult::Block(reason) => return HookOutcome::Block(reason),
            }
        }
        HookOutcome::Proceed
    }
}

enum RunResult {
    Proceed,
    Block(String),
}

async fn run_one(hook: &CompiledHook, event: HookEvent, ctx: &HookCtx, agent: &str) -> RunResult {
    let payload = build_payload(hook, event, ctx, agent);
    let payload_str = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(hook = %hook.raw_name, error = %e, "Hook payload serialize failed — proceeding");
            return RunResult::Proceed;
        }
    };

    // Topic builtins: `${JYC_TOPIC_PATH}` / `${JYC_TOPIC_STATE_PATH}` are
    // expanded textually in every shell entry (argv-form hooks have no
    // shell to expand them) and injected as env vars for the hook process.
    // State dir comes from the topic-state registry; empty when the topic
    // has no registered state dir.
    let state_path = jyc_types::state_dir::registered_state(&ctx.topic)
        .map(|p| p.display().to_string())
        .unwrap_or_default();
    let shell: Vec<String> = hook
        .shell
        .iter()
        .map(|arg| {
            arg.replace("${JYC_TOPIC_PATH}", &ctx.cwd)
                .replace("${JYC_TOPIC_STATE_PATH}", &state_path)
        })
        .collect();

    let Some((program, args)) = shell.split_first() else {
        return RunResult::Proceed;
    };
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if !ctx.cwd.is_empty() {
        cmd.current_dir(&ctx.cwd);
    }
    cmd.env("JYC_HOOK_EVENT", event.as_str());
    cmd.env("JYC_AGENT", agent);
    cmd.env("JYC_TOPIC", &ctx.topic);
    cmd.env("JYC_TOPIC_PATH", &ctx.cwd);
    cmd.env("JYC_TOPIC_STATE_PATH", &state_path);
    if let Some(ch) = &ctx.channel {
        cmd.env("JYC_CHANNEL", ch);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(hook = %hook.raw_name, error = %e, "Hook spawn failed — proceeding");
            return RunResult::Proceed;
        }
    };
    let outcome = match tokio::time::timeout(hook.timeout, async move {
        // Write AND wait inside the same timeout: a hook that never reads
        // stdin would block `write_all` forever once the payload exceeds
        // the ~64 KiB pipe buffer, and that wait must not be unbounded.
        // On timeout the future is dropped, `kill_on_drop` reaps the hook.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload_str.as_bytes()).await;
            let _ = stdin.shutdown().await;
        }
        child.wait_with_output().await
    })
    .await
    {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            tracing::warn!(hook = %hook.raw_name, error = %e, "Hook wait failed — proceeding");
            return RunResult::Proceed;
        }
        Err(_) => {
            tracing::warn!(
                hook = %hook.raw_name,
                timeout_secs = hook.timeout.as_secs(),
                "Hook timed out — killed, proceeding"
            );
            return RunResult::Proceed;
        }
    };

    let stdout = String::from_utf8_lossy(&outcome.stdout);
    let stderr = String::from_utf8_lossy(&outcome.stderr);
    if !stdout.trim().is_empty() {
        tracing::debug!(hook = %hook.raw_name, stdout = %stdout.trim(), "Hook stdout");
    }
    let code = outcome.status.code().unwrap_or(-1);
    if code == 2 {
        let reason = if stderr.trim().is_empty() {
            format!("blocked by hook '{}'", hook.raw_name)
        } else {
            stderr.trim().to_string()
        };
        tracing::info!(hook = %hook.raw_name, event = event.as_str(), "Hook blocked the action");
        return RunResult::Block(reason);
    }
    if code != 0 {
        tracing::warn!(
            hook = %hook.raw_name,
            exit_code = code,
            stderr = %stderr.trim(),
            "Hook exited non-zero (not 2) — proceeding (fail-open)"
        );
    }
    RunResult::Proceed
}

/// Serialize the stdin payload in the hook's dialect.
fn build_payload(hook: &CompiledHook, event: HookEvent, ctx: &HookCtx, agent: &str) -> Value {
    match hook.dialect {
        HookDialect::Jyc => build_jyc_payload(event, ctx, agent),
        HookDialect::Claude => build_cc_payload(event, ctx, agent),
    }
}

/// Non-empty optional fields are inserted into `obj` under `key`.
fn put(obj: &mut serde_json::Map<String, Value>, key: &str, val: &Option<String>) {
    if let Some(v) = val.as_ref().filter(|s| !s.is_empty()) {
        obj.insert(key.into(), json!(v));
    }
}

fn build_jyc_payload(event: HookEvent, ctx: &HookCtx, agent: &str) -> Value {
    let mut payload = json!({
        "hook_event_name": event.as_str(),
        "agent": agent,
        "topic": ctx.topic,
        "cwd": ctx.cwd,
    });
    let obj = payload.as_object_mut().unwrap();
    put(obj, "channel", &ctx.channel);
    if let Some(md) = ctx.metadata.as_ref().filter(|v| !v.is_null()) {
        obj.insert("metadata".into(), md.clone());
    }
    put(obj, "content", &ctx.message_content);
    put(obj, "tool_name", &ctx.tool_name);
    if let Some(input) = ctx.tool_input.as_ref().filter(|v| !v.is_null()) {
        obj.insert("tool_input".into(), input.clone());
    }
    put(obj, "tool_response", &ctx.tool_response);
    put(obj, "reply_text", &ctx.reply_text);
    put(obj, "source", &ctx.source);
    put(obj, "reason", &ctx.reason);
    // Channel metadata as a nested, best-effort object.
    let mut message = serde_json::Map::new();
    if let Some(v) = ctx.sender.as_ref().filter(|s| !s.is_empty()) {
        message.insert("sender".into(), json!(v));
    }
    if let Some(v) = ctx.sender_address.as_ref().filter(|s| !s.is_empty()) {
        message.insert("sender_address".into(), json!(v));
    }
    if let Some(v) = ctx.message_content.as_ref().filter(|s| !s.is_empty()) {
        message.insert("content".into(), json!(v));
    }
    if !message.is_empty() {
        obj.insert("message".into(), Value::Object(message));
    }
    payload
}

/// Claude Code-shaped payload (best-effort: `session_id` is the topic
/// name; `transcript_path` is intentionally absent — CC scripts that read
/// it must tolerate the missing field).
fn build_cc_payload(event: HookEvent, ctx: &HookCtx, _agent: &str) -> Value {
    let cc_name = match event {
        HookEvent::MessageReceived => "UserPromptSubmit",
        HookEvent::PreToolUse => "PreToolUse",
        HookEvent::PostToolUse => "PostToolUse",
        // jyc-only event: a CC-dialect name was never parseable for it
        // (HookEvent::parse rejects "PostToolUseFailure"), so this arm is
        // unreachable in practice; keep the jyc name to stay harmless.
        HookEvent::PostToolUseFailure => "post_tool_use_failure",
        HookEvent::ReplySend => "Stop",
        HookEvent::SessionStart => "SessionStart",
        HookEvent::SessionEnd => "SessionEnd",
    };
    let mut payload = json!({
        "session_id": ctx.topic,
        "cwd": ctx.cwd,
        "hook_event_name": cc_name,
    });
    let obj = payload.as_object_mut().unwrap();
    match event {
        HookEvent::MessageReceived => {
            obj.insert(
                "prompt".into(),
                json!(ctx.message_content.clone().unwrap_or_default()),
            );
        }
        HookEvent::PreToolUse | HookEvent::PostToolUse => {
            obj.insert(
                "tool_name".into(),
                json!(ctx.tool_name.clone().unwrap_or_default()),
            );
            obj.insert(
                "tool_input".into(),
                ctx.tool_input.clone().unwrap_or_else(|| json!({})),
            );
            if event == HookEvent::PostToolUse {
                obj.insert(
                    "tool_response".into(),
                    json!(ctx.tool_response.clone().unwrap_or_default()),
                );
            }
        }
        HookEvent::ReplySend => {
            obj.insert("stop_hook_active".into(), json!(false));
            put(obj, "reply_text", &ctx.reply_text);
        }
        HookEvent::SessionStart => {
            obj.insert("source".into(), json!(cc_source(ctx.source.as_deref())));
        }
        HookEvent::SessionEnd => {
            obj.insert("reason".into(), json!(cc_reason(ctx.reason.as_deref())));
            put(obj, "jyc_reason", &ctx.reason);
        }
        HookEvent::PostToolUseFailure => {
            obj.insert(
                "tool_name".into(),
                json!(ctx.tool_name.clone().unwrap_or_default()),
            );
            put(obj, "tool_response", &ctx.tool_response);
        }
    }
    payload
}

/// jyc session source → CC SessionStart source vocabulary
/// (`startup` | `resume` | `clear` | `compact`): reset ≈ /clear, new ≈ startup.
fn cc_source(source: Option<&str>) -> &'static str {
    match source {
        Some("reset") => "clear",
        _ => "startup",
    }
}

/// jyc session reason → CC SessionEnd reason vocabulary
/// (`logout` | `prompt_input_exit` | `other`): close is the logout-like
/// "session gone" case; reset/new have no CC equivalent → `other`
/// (the jyc value rides along in `jyc_reason`).
fn cc_reason(reason: Option<&str>) -> &'static str {
    match reason {
        Some("close") => "logout",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(event: &str, script: &str) -> HookConfig {
        HookConfig {
            event: event.into(),
            matcher: None,
            shell: vec!["sh".into(), "-c".into(), script.into()],
            timeout: None,
        }
    }

    fn ctx() -> HookCtx {
        HookCtx {
            topic: "t".into(),
            cwd: String::new(),
            message_content: Some("hello".into()),
            tool_name: Some("bash".into()),
            tool_input: Some(json!({"command": "ls"})),
            ..Default::default()
        }
    }

    #[test]
    fn empty_set_is_empty() {
        assert!(HookSet::from_configs(&[]).is_empty());
    }

    #[tokio::test]
    async fn exit_0_proceeds() {
        let set = HookSet::from_configs(&[hook("pre_tool_use", "exit 0")]);
        assert_eq!(
            set.run(HookEvent::PreToolUse, None, &ctx()).await,
            HookOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn exit_2_blocks_with_stderr() {
        let set = HookSet::from_configs(&[hook("pre_tool_use", "echo nope >&2; exit 2")]);
        match set.run(HookEvent::PreToolUse, None, &ctx()).await {
            HookOutcome::Block(reason) => assert_eq!(reason, "nope"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn other_exit_codes_fail_open() {
        let set = HookSet::from_configs(&[hook("pre_tool_use", "exit 1")]);
        assert_eq!(
            set.run(HookEvent::PreToolUse, None, &ctx()).await,
            HookOutcome::Proceed
        );
    }

    /// `${JYC_TOPIC_PATH}` is expanded textually in shell entries (works
    /// for argv-form hooks too) and both topic builtins are injected as
    /// env vars on the hook process. Unique topic name: the state
    /// registry is process-global (parallel-test safety).
    #[tokio::test]
    async fn topic_builtins_expand_and_inject() {
        let dir = tempfile::TempDir::new().unwrap();
        let state = dir.path().join("state");
        jyc_types::state_dir::register("t-builtin-vars", &state);
        let marker = dir.path().join("marker");
        let script = format!(
            "test \"${{JYC_TOPIC_PATH}}\" = '{}' && test \"$JYC_TOPIC_STATE_PATH\" = '{}' && printf ok > '{}'",
            dir.path().display(),
            state.display(),
            marker.display()
        );
        let set = HookSet::from_configs(&[hook("pre_tool_use", &script)]);
        let c = HookCtx {
            topic: "t-builtin-vars".into(),
            cwd: dir.path().display().to_string(),
            ..ctx()
        };
        assert_eq!(
            set.run(HookEvent::PreToolUse, None, &c).await,
            HookOutcome::Proceed
        );
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "ok");
    }

    #[tokio::test]
    async fn timeout_fails_open() {
        let cfg = HookConfig {
            timeout: Some(1),
            ..hook("pre_tool_use", "sleep 30")
        };
        let set = HookSet::from_configs(&[cfg]);
        assert_eq!(
            set.run(HookEvent::PreToolUse, None, &ctx()).await,
            HookOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn large_payload_on_unread_stdin_times_out_not_hangs() {
        // The hook holds stdin open but never reads it (common shape:
        // env-only scripts). A payload past the ~64 KiB pipe buffer must
        // not block the write forever — write+wait share one timeout.
        let cfg = HookConfig {
            timeout: Some(1),
            ..hook("pre_tool_use", "sleep 30")
        };
        let set = HookSet::from_configs(&[cfg]);
        let ctx = HookCtx {
            topic: "t".into(),
            message_content: Some("x".repeat(256 * 1024)),
            ..Default::default()
        };
        // Outer guard: a regression turns this into a FAILURE, not a hang.
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            set.run(HookEvent::PreToolUse, None, &ctx),
        )
        .await
        .expect("run must return within its budget");
        assert_eq!(outcome, HookOutcome::Proceed);
    }

    #[tokio::test]
    async fn spawn_failure_fails_open() {
        let cfg = HookConfig {
            shell: vec!["definitely-not-a-binary-xyz".into()],
            ..hook("pre_tool_use", "")
        };
        let set = HookSet::from_configs(&[cfg]);
        assert_eq!(
            set.run(HookEvent::PreToolUse, None, &ctx()).await,
            HookOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn first_block_wins_and_stops_the_chain() {
        let set = HookSet::from_configs(&[
            hook("pre_tool_use", "echo first >&2; exit 2"),
            hook("pre_tool_use", "exit 0"),
        ]);
        match set.run(HookEvent::PreToolUse, None, &ctx()).await {
            HookOutcome::Block(reason) => assert_eq!(reason, "first"),
            other => panic!("expected Block, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn matcher_filters_by_subject() {
        let mut h = hook("pre_tool_use", "exit 2");
        h.matcher = Some("^bash$".into());
        let set = HookSet::from_configs(&[h]);
        // subject "read" does not match → skipped → Proceed.
        assert_eq!(
            set.run(HookEvent::PreToolUse, Some("read"), &ctx()).await,
            HookOutcome::Proceed
        );
        // subject "bash" matches → blocked.
        assert!(matches!(
            set.run(HookEvent::PreToolUse, Some("bash"), &ctx()).await,
            HookOutcome::Block(_)
        ));
    }

    #[tokio::test]
    async fn payload_reaches_stdin_in_dialect_shape() {
        // NOTE: read stdin once into a variable — chained `grep -` calls
        // would leave the second grep an already-drained stream.
        // jyc dialect: required keys present, otherwise block.
        let set = HookSet::from_configs(&[hook(
            "pre_tool_use",
            "p=$(cat); echo \"$p\" | grep -q '\"hook_event_name\":\"pre_tool_use\"' && echo \"$p\" | grep -q '\"agent\"' && echo \"$p\" | grep -q '\"tool_input\"' || exit 2",
        )]);
        assert_eq!(
            set.run(HookEvent::PreToolUse, None, &ctx()).await,
            HookOutcome::Proceed
        );

        // Same script against the CC-dialect hook must NOT match the jyc name.
        let set = HookSet::from_configs(&[hook(
            "PreToolUse",
            "grep -q '\"hook_event_name\":\"pre_tool_use\"' - ; if [ $? -eq 0 ]; then exit 2; fi; exit 0",
        )]);
        assert_eq!(
            set.run(HookEvent::PreToolUse, None, &ctx()).await,
            HookOutcome::Proceed
        );

        // CC dialect actually carries session_id + CC event name + tool fields.
        let set = HookSet::from_configs(&[hook(
            "PreToolUse",
            "p=$(cat); echo \"$p\" | grep -q '\"hook_event_name\":\"PreToolUse\"' && echo \"$p\" | grep -q '\"session_id\"' && echo \"$p\" | grep -q '\"tool_input\"' || exit 2",
        )]);
        assert_eq!(
            set.run(HookEvent::PreToolUse, None, &ctx()).await,
            HookOutcome::Proceed
        );
    }

    #[tokio::test]
    async fn wrong_event_never_runs() {
        let set = HookSet::from_configs(&[hook("reply_send", "exit 2")]);
        assert_eq!(
            set.run(HookEvent::PreToolUse, None, &ctx()).await,
            HookOutcome::Proceed
        );
    }

    #[test]
    fn cc_source_and_reason_mapping() {
        assert_eq!(cc_source(Some("reset")), "clear");
        assert_eq!(cc_source(Some("startup")), "startup");
        assert_eq!(cc_source(Some("new")), "startup");
        assert_eq!(cc_reason(Some("close")), "logout");
        assert_eq!(cc_reason(Some("reset")), "other");
        assert_eq!(cc_reason(None), "other");
    }

    #[test]
    fn jyc_payload_omits_empty_optionals() {
        let payload = build_jyc_payload(
            HookEvent::MessageReceived,
            &HookCtx {
                topic: "t".into(),
                ..Default::default()
            },
            "a",
        );
        assert_eq!(payload["hook_event_name"], "message_received");
        assert!(payload.get("channel").is_none());
        assert!(payload.get("tool_name").is_none());
        assert!(payload.get("message").is_none());
    }
}
