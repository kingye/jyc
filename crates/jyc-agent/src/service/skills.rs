//! Skill discovery and formatting for the in-process agent.
//!
//! Extracted from the monolithic `service.rs`.

use anyhow::{Context, Result};
use jyc_types::TopicConfig;
use jyc_types::state_dir::resolve_state_dir;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::{JycAgentService, SkillMeta};

pub fn parse_skill_frontmatter(content: &str) -> Option<SkillMeta> {
    let mut lines = content.lines();

    // First line must be "---"
    if lines.next()?.trim() != "---" {
        return None;
    }

    let mut name = None;
    let mut description = None;

    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        // End of frontmatter
        if trimmed == "---" {
            break;
        }
        if let Some(value) = trimmed.strip_prefix("name:") {
            name = Some(value.trim().to_string());
        } else if let Some(value) = trimmed.strip_prefix("description:") {
            let val = value.trim();
            if val == "|" || val == "|-" || val == ">" {
                // YAML block scalar: collect indented lines until --- or non-indented line
                let mut desc = String::new();
                for line in lines.by_ref() {
                    let trimmed = line.trim();
                    if trimmed == "---" {
                        // Put back the --- terminator so the outer loop can handle it
                        // Actually we've already consumed it; just break
                        break;
                    }
                    if !trimmed.is_empty() {
                        if !desc.is_empty() {
                            desc.push(' ');
                        }
                        desc.push_str(trimmed);
                    }
                }
                description = Some(desc);
            } else if !val.is_empty() {
                description = Some(val.to_string());
            }
        }
    }

    let name = name?;
    let description = description?;
    if name.is_empty() || description.is_empty() {
        return None;
    }

    Some(SkillMeta {
        name,
        description,
        source_path: PathBuf::new(), // caller fills this in
    })
}

/// In-process AI agent service.
impl JycAgentService {
    /// The allow-list of skills this topic lists by default: topic config >
    /// pattern > channel.
    ///
    /// The single source of truth for the system prompt's skill list.
    ///
    /// Skills are opt-in: **no layer declaring `skills` means no skills at
    /// all** — an empty list, not "every discovered skill". An allow-list
    /// is a statement of what a topic *is*, so the narrowest one wins
    /// outright (a topic-level `skills` replaces the pattern/channel list
    /// rather than extending it — "only these" has to mean only these).
    /// The runtime `/skill` toggle layers on top of it per topic.
    pub(crate) fn config_skill_filters(
        &self,
        matched_pattern: Option<&str>,
        topic_cfg: Option<&TopicConfig>,
    ) -> Vec<String> {
        let pattern =
            matched_pattern.and_then(|name| self.patterns.iter().find(|p| p.name == name));

        topic_cfg
            .and_then(|t| t.skills.as_deref())
            .or_else(|| pattern.and_then(|p| p.skills.as_deref()))
            .or(self.channel_skills.as_deref())
            .map(|list| list.to_vec())
            .unwrap_or_default()
    }

    ///
    /// Scans paths from lowest to highest priority (later paths override earlier ones
    /// when skills share the same name).
    ///
    /// After discovery, applies optional include/exclude filters:
    /// - `include`: if set, only skills whose names appear in this list are retained
    /// - `exclude`: if set, skills whose names appear in this list are removed
    pub fn discover_skills(
        &self,
        topic_name: &str,
        topic_path: &Path,
        include: Option<&[String]>,
        exclude: Option<&[String]>,
    ) -> Vec<SkillMeta> {
        let mut skills: HashMap<String, SkillMeta> = HashMap::new();

        // Build scan paths from low to high priority
        let scan_paths: Vec<PathBuf> = {
            let mut paths = Vec::new();

            // $HOME/.config/opencode/skills/
            if let Ok(home) = std::env::var("HOME") {
                paths.push(PathBuf::from(&home).join(".config/opencode/skills"));
                // $HOME/.claude/skills/
                paths.push(PathBuf::from(&home).join(".claude/skills"));
            }

            // L1 global: <config dir>/skills/ (e.g. ~/.config/jyc/skills)
            if let Some(global_skills) = jyc_utils::paths::global_skills_dir() {
                paths.push(global_skills);
            }

            // L2: {data dir}/skills/
            paths.push(self.data_dir.join("skills"));

            // {topic_path}/repo/.claude/skills/
            paths.push(topic_path.join("repo/.claude/skills"));
            // {topic_path}/repo/.opencode/skills/
            paths.push(topic_path.join("repo/.opencode/skills"));
            // {topic_path}/repo/.jyc/skills/
            paths.push(topic_path.join("repo/.jyc/skills"));

            // {topic_path}/.claude/skills/
            paths.push(topic_path.join(".claude/skills"));
            // {topic_path}/.opencode/skills/
            paths.push(topic_path.join(".opencode/skills"));
            // {topic_path}/.jyc/skills/ (honors relocated state dir)
            paths.push(
                jyc_types::state_dir::resolve_state_dir(topic_name, topic_path).join("skills"),
            );

            paths
        };

        for scan_dir in &scan_paths {
            if !scan_dir.exists() || !scan_dir.is_dir() {
                continue;
            }

            let entries = match std::fs::read_dir(scan_dir) {
                Ok(e) => e,
                Err(_) => continue,
            };

            for entry in entries.flatten() {
                let skill_dir = entry.path();
                if !skill_dir.is_dir() {
                    continue;
                }

                let skill_md = skill_dir.join("SKILL.md");
                if !skill_md.exists() {
                    continue;
                }

                let content = match std::fs::read_to_string(&skill_md) {
                    Ok(c) => c,
                    Err(_) => continue,
                };

                if let Some(mut meta) = parse_skill_frontmatter(&content) {
                    meta.source_path = skill_dir;
                    // HashMap insert: later (higher-priority) paths overwrite earlier ones
                    skills.insert(meta.name.clone(), meta);
                }
            }
        }

        // Apply include filter: if set, only keep listed skills
        if let Some(include_list) = include {
            let include_set: std::collections::HashSet<&str> =
                include_list.iter().map(|s| s.as_str()).collect();
            skills.retain(|name, _| include_set.contains(name.as_str()));
        }

        // Apply exclude filter: remove listed skills
        if let Some(exclude_list) = exclude {
            let exclude_set: std::collections::HashSet<&str> =
                exclude_list.iter().map(|s| s.as_str()).collect();
            skills.retain(|name, _| !exclude_set.contains(name.as_str()));
        }

        let mut result: Vec<SkillMeta> = skills.into_values().collect();
        // Sort by name for deterministic output
        result.sort_by(|a, b| a.name.cmp(&b.name));

        // `debug!`, not `info!`: discovery now also runs per overview poll (the
        // `/skill:<name>` command rows), so an info line here would drown the log.
        tracing::debug!(
            topic_path = %topic_path.display(),
            skills = ?result.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            "Discovered {} skill(s)", result.len()
        );

        result
    }
}

/// Persist skill names to the topic's .jyc/skills.json file.
///
/// This allows the dashboard to read the skills list without re-scanning directories.
///
/// A closed topic writes nothing: the prompt is also built for the turn of a
/// message that stacked `/close --force` with body text, and recreating the
/// state dir here would leave a topic behind that nothing owns.
pub(crate) fn persist_skill_names(
    topic_name: &str,
    topic_path: &Path,
    skill_names: &[&str],
) -> Result<()> {
    if jyc_types::state_dir::is_closed(topic_name) {
        return Ok(());
    }
    let state_dir = resolve_state_dir(topic_name, topic_path);
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("Failed to create .jyc dir: {}", state_dir.display()))?;
    let skills_path = state_dir.join("skills.json");
    let json = serde_json::to_string_pretty(skill_names)?;
    std::fs::write(&skills_path, json)
        .with_context(|| format!("Failed to write skills.json: {}", skills_path.display()))?;
    Ok(())
}
