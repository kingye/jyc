//! How a skill is presented to the model.
//!
//! The wording lives here (not in `jyc-agent`) because two layers render
//! it: the system prompt lists the skills a topic may use, and the
//! `/skill:<name>` command rows tell the model to read one of them — and
//! `jyc-core` (which builds those rows) cannot depend on `jyc-agent`.
//! One wording, one place to change it.

use crate::agent::SkillMeta;
use std::borrow::Cow;

/// Format the skills section for inclusion in the system prompt.
///
/// Produces a markdown-formatted list of available skills with their paths.
/// Returns an empty string if the skills list is empty.
///
/// The same text serves the `/skill:<name>` injection, where a single
/// skill is rendered this way and the model is told to read its
/// `SKILL.md` — the path in the entry is what makes that possible
/// without the skill being listed in the system prompt.
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
            truncate_description(&skill.description)
        ));
    }

    section.push_str(
        "To load a skill's full instructions, use `read <skill-path>/SKILL.md`.\n\
         All file paths within a SKILL.md are relative to that skill's directory.\n\
         When running skill scripts: cd <skill-path> && <command>\n\n",
    );

    section
}

/// Max characters kept from a skill description in the system prompt.
/// Long descriptions cost input tokens on every LLM call; the first
/// ~200 chars carry the trigger semantics ("Use when…"), the rest is
/// elaboration the model can fetch via `read SKILL.md` when matched.
const SKILL_DESC_MAX_CHARS: usize = 200;

/// Truncate a skill description to [`SKILL_DESC_MAX_CHARS`] characters,
/// backing off to the last space so English text never ends mid-word.
/// Returns the input unchanged when within the cap.
///
/// Char-based (not byte-based) slicing keeps multi-byte UTF-8 intact;
/// for descriptions without spaces (e.g. CJK text) the retreat would
/// land near the start, so we fall back to a hard char cut instead.
fn truncate_description(desc: &str) -> Cow<'_, str> {
    if desc.chars().count() <= SKILL_DESC_MAX_CHARS {
        return Cow::Borrowed(desc);
    }
    let cut: String = desc.chars().take(SKILL_DESC_MAX_CHARS).collect();
    // `pos` is a byte index; compare char counts so mixed CJK text
    // doesn't over-retreat (a space at byte 150 may be at char 50).
    let boundary = match cut.rfind(' ') {
        Some(pos) if cut[..pos].chars().count() > SKILL_DESC_MAX_CHARS / 2 => pos,
        _ => cut.len(),
    };
    Cow::Owned(format!("{}…", &cut[..boundary]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_description_is_unchanged() {
        let desc = "Short summary.";
        assert_eq!(truncate_description(desc), desc);
    }

    #[test]
    fn exactly_at_cap_is_unchanged() {
        let desc: String = std::iter::repeat_n('a', SKILL_DESC_MAX_CHARS).collect();
        assert_eq!(truncate_description(&desc), desc);
    }

    #[test]
    fn long_english_description_retreats_to_word_boundary() {
        let word = "lorem ";
        let desc: String = word.repeat(60); // 360 chars, spaces every 6
        let out = truncate_description(&desc);
        assert!(out.ends_with('…'));
        let body = out.strip_suffix('…').unwrap();
        assert!(body.chars().count() < SKILL_DESC_MAX_CHARS);
        assert!(!body.ends_with("lore")); // no mid-word cut
        assert!(body.ends_with("lorem")); // ends on a full word
    }

    #[test]
    fn spaceless_cjk_description_hard_cuts_without_panic() {
        let desc: String = std::iter::repeat_n('汉', SKILL_DESC_MAX_CHARS + 50).collect();
        let out = truncate_description(&desc);
        assert!(out.ends_with('…'));
        // No usable space → hard cut at exactly the cap.
        assert_eq!(out.chars().count(), SKILL_DESC_MAX_CHARS + 1);
    }
}
