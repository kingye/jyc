//! Skill discovery and formatting for the in-process agent.
//!
//! Extracted from the monolithic `service.rs`.

use anyhow::{Context, Result};
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
    ///
    /// Scans paths from lowest to highest priority (later paths override earlier ones
    /// when skills share the same name).
    ///
    /// After discovery, applies optional include/exclude filters:
    /// - `include`: if set, only skills whose names appear in this list are retained
    /// - `exclude`: if set, skills whose names appear in this list are removed
    pub fn discover_skills(
        &self,
        thread_path: &Path,
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

            // L1 global: <config_home>/skills/ (e.g. ~/.config/jyc/skills)
            if let Some(global_skills) = jyc_utils::paths::global_skills_dir() {
                paths.push(global_skills);
            }

            // L2: {workdir}/skills/
            paths.push(self.workdir.join("skills"));

            // {thread_path}/repo/.claude/skills/
            paths.push(thread_path.join("repo/.claude/skills"));
            // {thread_path}/repo/.opencode/skills/
            paths.push(thread_path.join("repo/.opencode/skills"));
            // {thread_path}/repo/.jyc/skills/
            paths.push(thread_path.join("repo/.jyc/skills"));

            // {thread_path}/.claude/skills/
            paths.push(thread_path.join(".claude/skills"));
            // {thread_path}/.opencode/skills/
            paths.push(thread_path.join(".opencode/skills"));
            // {thread_path}/.jyc/skills/
            paths.push(thread_path.join(".jyc/skills"));

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

        tracing::info!(
            thread_path = %thread_path.display(),
            skills = ?result.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            "Discovered {} skill(s)", result.len()
        );

        result
    }
}

/// Persist skill names to the thread's .jyc/skills.json file.
///
/// This allows the dashboard to read the skills list without re-scanning directories.
pub(crate) fn persist_skill_names(thread_path: &Path, skill_names: &[&str]) -> Result<()> {
    let jyc_dir = thread_path.join(".jyc");
    std::fs::create_dir_all(&jyc_dir)
        .with_context(|| format!("Failed to create .jyc dir: {}", jyc_dir.display()))?;
    let skills_path = jyc_dir.join("skills.json");
    let json = serde_json::to_string_pretty(skill_names)?;
    std::fs::write(&skills_path, json)
        .with_context(|| format!("Failed to write skills.json: {}", skills_path.display()))?;
    Ok(())
}

/// Format the skills section for inclusion in the system prompt.
///
/// Produces a markdown-formatted list of available skills with their paths.
/// Returns an empty string if the skills list is empty.
pub fn format_skills_section(skills: &[SkillMeta]) -> String {
    if skills.is_empty() {
        return String::new();
    }

    let mut section = String::new();
    section.push_str("## Available Skills\n\n");
    section.push_str(concat!(
        "**IMPORTANT: Before processing any user request, you MUST read the relevant SKILL.md file(s) ",
        "using the `read <skill-path>/SKILL.md` tool. The descriptions below are summaries only and ",
        "do NOT contain the full instructions you need to follow.**\n\n",
    ));

    for skill in skills {
        section.push_str(&format!(
            "- **{}** (at {})\n  {}\n\n",
            skill.name,
            skill.source_path.display(),
            skill.description
        ));
    }

    section.push_str(
        "To load a skill's full instructions, use `read <skill-path>/SKILL.md`.\n\
         All file paths within a SKILL.md are relative to that skill's directory.\n\
         When running skill scripts: cd <skill-path> && <command>\n\n",
    );

    section
}
