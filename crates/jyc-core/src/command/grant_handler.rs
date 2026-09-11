//! `/grant` and `/ungrant` command handlers — runtime filesystem access
//! grants for the topic's agent.
//!
//! `/grant <path>` grants read-only access until restart (or `/ungrant`);
//! `-w`/`--write` upgrades to read+write, `-p`/`--persist` additionally
//! writes the path into the topic's `[agents.<name>] access` section in
//! `config.toml`. Grants live in `jyc_types::access_grants` and are
//! consulted by the access-root resolvers when each turn builds its tool
//! context, so they take effect on the next turn.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;

use super::handler::{CommandContext, CommandHandler, CommandResult};

/// `/grant` — add a runtime (optionally persisted) access grant.
pub struct GrantCommandHandler;

/// `/ungrant` — revoke a runtime grant.
pub struct UngrantCommandHandler;

/// Wrap a user-facing failure as an unsuccessful `CommandResult` (the
/// reply message is the error text), mirroring `/bill`.
fn failure(message: String) -> CommandResult {
    CommandResult {
        success: false,
        message,
        ..Default::default()
    }
}

/// Parsed `/grant` arguments.
struct GrantArgs {
    /// The path argument, or `None` when the command lists grants.
    path: Option<String>,
    /// `-w`/`--write`: grant write access too (default is read-only).
    write: bool,
    /// `-p`/`--persist`: also write the path into config.toml.
    persist: bool,
}

/// Parse `/grant` args: one optional path plus `-w`/`--write` and
/// `-p`/`--persist` flags (combinable, any order).
fn parse_grant_args(args: &[String]) -> Result<GrantArgs> {
    let mut path = None;
    let mut write = false;
    let mut persist = false;
    for arg in args {
        match arg.as_str() {
            "-w" | "--write" => write = true,
            "-p" | "--persist" => persist = true,
            s if s.starts_with('-') => {
                anyhow::bail!("/grant: unknown flag '{s}' (known: -w/--write, -p/--persist)")
            }
            s => {
                if path.is_some() {
                    anyhow::bail!("/grant: unexpected extra argument '{s}'");
                }
                path = Some(s.to_string());
            }
        }
    }
    Ok(GrantArgs {
        path,
        write,
        persist,
    })
}

/// Normalize a user-supplied path: expand `~`, resolve relative paths
/// against the topic working directory. The result is absolute unless
/// `$HOME` is unknown; the tool boundary enforces absoluteness anyway.
fn normalize_path(raw: &str, topic_path: &Path) -> PathBuf {
    let expanded = jyc_utils::paths::expand_tilde(raw);
    if expanded.is_absolute() {
        expanded
    } else {
        topic_path.join(expanded)
    }
}

/// Append `path` to the topic's `[agents.<topic>] access` section in
/// config.toml, merging with an existing inline or sub-table `access`.
/// Returns `true` when the file was modified (the path was not already
/// present everywhere it should be).
fn persist_grant(context: &CommandContext, path: &Path, write: bool) -> Result<bool> {
    let config_path = context
        .config_path
        .clone()
        .context("config.toml path unknown; cannot persist")?;
    let raw = std::fs::read_to_string(&config_path)
        .with_context(|| format!("failed to read {}", config_path.display()))?;
    let mut doc = raw
        .parse::<toml_edit::DocumentMut>()
        .context("config.toml is not valid TOML")?;
    let agent = doc
        .get_mut("agents")
        .and_then(|a| a.get_mut(context.topic_name.as_str()))
        .with_context(|| {
            format!(
                "no [agents.{}] section in config.toml; add `access` manually",
                context.topic_name
            )
        })?;
    let agent_tbl = agent
        .as_table_like_mut()
        .context("[agents.<topic>] is not a table")?;
    if !agent_tbl.contains_key("access") {
        agent_tbl.insert(
            "access",
            toml_edit::Item::Value(toml_edit::Value::InlineTable(toml_edit::InlineTable::new())),
        );
    }
    let access = agent_tbl
        .get_mut("access")
        .and_then(|i| i.as_table_like_mut())
        .context("agents.<topic>.access is not a table")?;

    let path_str = path.to_string_lossy().into_owned();
    let keys: &[&str] = if write { &["read", "write"] } else { &["read"] };
    let mut changed = false;
    for key in keys {
        let present = access
            .get(key)
            .and_then(|i| i.as_array())
            .is_some_and(|arr| arr.iter().any(|v| v.as_str() == Some(path_str.as_str())));
        if present {
            continue;
        }
        match access.get_mut(key).and_then(|i| i.as_array_mut()) {
            Some(arr) => arr.push(path_str.as_str()),
            None => {
                let mut arr = toml_edit::Array::new();
                arr.push(path_str.as_str());
                access.insert(key, toml_edit::Item::Value(toml_edit::Value::Array(arr)));
            }
        }
        changed = true;
    }
    if changed {
        std::fs::write(&config_path, doc.to_string())
            .with_context(|| format!("failed to write {}", config_path.display()))?;
    }
    Ok(changed)
}

/// Paths configured for this topic's agent section, for `/grant` listing.
fn configured_access(context: &CommandContext) -> Option<(Vec<String>, Vec<String>)> {
    context
        .config
        .agents
        .get(&context.topic_name)
        .and_then(|a| a.access.as_ref())
        .map(|access| (access.read.clone(), access.write.clone()))
}

/// Render the `/grant` listing: runtime grants plus configured access.
fn list_grants(context: &CommandContext) -> String {
    let grants = jyc_types::access_grants::grants_for(&context.topic_name);
    let configured = configured_access(context);
    if grants.is_empty()
        && configured
            .as_ref()
            .is_none_or(|(r, w)| r.is_empty() && w.is_empty())
    {
        return format!(
            "No access grants for topic `{}`. Usage: /grant <path> [-w|--write] [-p|--persist]",
            context.topic_name
        );
    }
    let mut out = format!("Access grants for topic `{}`:\n", context.topic_name);
    if !grants.is_empty() {
        out.push_str("\nRuntime (until restart or /ungrant):\n");
        for g in &grants {
            out.push_str(&format!(
                "- {} — {}\n",
                g.path.display(),
                if g.write { "read+write" } else { "read-only" }
            ));
        }
    }
    if let Some((read, write)) = configured
        && (!read.is_empty() || !write.is_empty())
    {
        out.push_str(&format!(
            "\nConfigured ([agents.{}]):\n",
            context.topic_name
        ));
        for p in &read {
            out.push_str(&format!("- {p} — read\n"));
        }
        for p in &write {
            out.push_str(&format!("- {p} — read+write\n"));
        }
    }
    out
}

#[async_trait]
impl CommandHandler for GrantCommandHandler {
    fn name(&self) -> &str {
        "/grant"
    }

    fn description(&self) -> &str {
        "Grant agent access to a path: /grant <path> (read-only, until restart) — -w write, -p persist to config; no args lists grants"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let parsed = match parse_grant_args(&context.args) {
            Ok(parsed) => parsed,
            Err(e) => return Ok(failure(e.to_string())),
        };
        let Some(raw_path) = parsed.path else {
            return Ok(CommandResult {
                success: true,
                message: list_grants(&context),
                ..Default::default()
            });
        };
        let path = normalize_path(&raw_path, &context.topic_path);
        jyc_types::access_grants::grant(&context.topic_name, path.clone(), parsed.write);
        let mode = if parsed.write {
            "read+write"
        } else {
            "read-only"
        };
        let mut msg = format!(
            "Granted {mode} access to `{}` for this topic (until restart or /ungrant).",
            path.display()
        );
        if parsed.persist {
            match persist_grant(&context, &path, parsed.write) {
                Ok(changed) => msg.push_str(&format!(
                    "\n{} config.toml [agents.{}].",
                    if changed {
                        "Persisted to"
                    } else {
                        "Already in"
                    },
                    context.topic_name
                )),
                Err(e) => return Ok(failure(format!("{msg}\nPersist failed: {e:#}"))),
            }
        }
        Ok(CommandResult {
            success: true,
            message: msg,
            ..Default::default()
        })
    }
}

#[async_trait]
impl CommandHandler for UngrantCommandHandler {
    fn name(&self) -> &str {
        "/ungrant"
    }

    fn description(&self) -> &str {
        "Revoke a runtime access grant: /ungrant <path>"
    }

    async fn execute(&self, context: CommandContext) -> Result<CommandResult> {
        let Some(raw_path) = context.args.first() else {
            return Ok(failure("usage: /ungrant <path>".to_string()));
        };
        let path = normalize_path(raw_path, &context.topic_path);
        let message = if jyc_types::access_grants::ungrant(&context.topic_name, &path) {
            format!("Revoked access to `{}`.", path.display())
        } else {
            // Not a runtime grant — maybe it lives in config.toml.
            let configured = configured_access(&context).unwrap_or_default();
            let in_config = configured
                .0
                .iter()
                .chain(configured.1.iter())
                .any(|p| normalize_path(p, &context.topic_path) == path);
            if in_config {
                format!(
                    "`{}` is configured in config.toml, not a runtime grant — remove it from [agents.{}] access manually.",
                    path.display(),
                    context.topic_name
                )
            } else {
                format!("No grant for `{}`.", path.display())
            }
        };
        Ok(CommandResult {
            success: true,
            message,
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context_with(topic: &str, args: &[&str]) -> CommandContext {
        CommandContext {
            topic_name: topic.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn parse_defaults_to_read_only_temporary() {
        let parsed = parse_grant_args(&["/tmp/x".to_string()]).unwrap();
        assert_eq!(parsed.path.as_deref(), Some("/tmp/x"));
        assert!(!parsed.write);
        assert!(!parsed.persist);
    }

    #[test]
    fn parse_flags_any_order() {
        let parsed =
            parse_grant_args(&["-p".to_string(), "/tmp/x".to_string(), "-w".to_string()]).unwrap();
        assert_eq!(parsed.path.as_deref(), Some("/tmp/x"));
        assert!(parsed.write);
        assert!(parsed.persist);
        let long = parse_grant_args(&["--write".to_string(), "--persist".to_string()]).unwrap();
        assert!(long.path.is_none());
        assert!(long.write && long.persist);
    }

    #[test]
    fn parse_rejects_unknown_flag_and_extra_path() {
        assert!(parse_grant_args(&["-x".to_string()]).is_err());
        assert!(parse_grant_args(&["/a".to_string(), "/b".to_string()]).is_err());
    }

    #[test]
    fn normalize_resolves_relative_against_topic() {
        let topic = Path::new("/topics/jyc");
        assert_eq!(
            normalize_path("sub/dir", topic),
            PathBuf::from("/topics/jyc/sub/dir")
        );
        assert_eq!(normalize_path("/abs", topic), PathBuf::from("/abs"));
    }

    #[tokio::test]
    async fn grant_list_ungrant_flow() {
        let topic = "grant-cmd-flow";
        jyc_types::access_grants::clear(topic);

        let grant = GrantCommandHandler;
        // Empty list.
        let result = grant.execute(context_with(topic, &[])).await.unwrap();
        assert!(
            result.message.contains("No access grants"),
            "got: {}",
            result.message
        );

        // Read-only grant.
        let result = grant
            .execute(context_with(topic, &["/tmp/grant-flow-a"]))
            .await
            .unwrap();
        assert!(result.success);
        assert!(
            result.message.contains("read-only"),
            "got: {}",
            result.message
        );
        assert!(result.message.contains("/tmp/grant-flow-a"));

        // Upgrade to write.
        let result = grant
            .execute(context_with(topic, &["/tmp/grant-flow-a", "-w"]))
            .await
            .unwrap();
        assert!(
            result.message.contains("read+write"),
            "got: {}",
            result.message
        );

        // List shows it.
        let result = grant.execute(context_with(topic, &[])).await.unwrap();
        assert!(
            result.message.contains("Runtime"),
            "got: {}",
            result.message
        );
        assert!(
            result.message.contains("/tmp/grant-flow-a — read+write"),
            "got: {}",
            result.message
        );

        // Ungrant.
        let ungrant = UngrantCommandHandler;
        let result = ungrant
            .execute(context_with(topic, &["/tmp/grant-flow-a"]))
            .await
            .unwrap();
        assert!(
            result.message.contains("Revoked"),
            "got: {}",
            result.message
        );
        let result = ungrant
            .execute(context_with(topic, &["/tmp/grant-flow-a"]))
            .await
            .unwrap();
        assert!(
            result.message.contains("No grant"),
            "got: {}",
            result.message
        );

        // Unknown flag surfaces as an unsuccessful result.
        let result = grant.execute(context_with(topic, &["-x"])).await.unwrap();
        assert!(!result.success);
        assert!(
            result.message.contains("unknown flag"),
            "got: {}",
            result.message
        );

        jyc_types::access_grants::clear(topic);
    }

    #[test]
    fn persist_appends_and_dedupes() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            "[agents.grant-persist]\ntopic_path = \"/tmp/x\"\n",
        )
        .unwrap();
        let mut ctx = context_with("grant-persist", &[]);
        ctx.config_path = Some(config_path.clone());

        // First persist: creates access table with read entry.
        let changed = persist_grant(&ctx, Path::new("/data/a"), false).unwrap();
        assert!(changed);
        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(text.contains("/data/a"), "got:\n{text}");

        // Same persist again: no change.
        assert!(!persist_grant(&ctx, Path::new("/data/a"), false).unwrap());

        // Write grant on a second path: lands in both arrays.
        assert!(persist_grant(&ctx, Path::new("/data/b"), true).unwrap());
        let text = std::fs::read_to_string(&config_path).unwrap();
        let doc: toml_edit::DocumentMut = text.parse().unwrap();
        let access = &doc["agents"]["grant-persist"]["access"];
        let read: Vec<_> = access["read"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        let write: Vec<_> = access["write"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        assert_eq!(read, vec!["/data/a", "/data/b"]);
        assert_eq!(write, vec!["/data/b"]);
    }

    #[test]
    fn persist_requires_agents_section() {
        let dir = tempfile::TempDir::new().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "[server]\n").unwrap();
        let mut ctx = context_with("missing-agent", &[]);
        ctx.config_path = Some(config_path);
        let err = persist_grant(&ctx, Path::new("/data"), false).unwrap_err();
        assert!(
            err.to_string().contains("[agents.missing-agent]"),
            "got: {err}"
        );
    }
}
