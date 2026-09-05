//! Guards the `[[commands]]` example in `config.example.toml`.
//!
//! The TOML below is copied from that file's "Custom Commands" block. If
//! someone edits the example into something that no longer parses or
//! validates, this fails — documentation that silently stops working is
//! worse than none.
//!
//! Inlined rather than read from disk so the test has no filesystem
//! dependency (see AGENTS.md test-isolation rules).

use jyc_types::load_config_from_str;
use jyc_types::validation::validate_config;

/// Minimal channel + agent scaffolding so the config validates.
const BASE: &str = r#"
[channels.work]
type = "email"
[channels.work.inbound]
host = "h"
port = 993
username = "u"
password = "p"
[channels.work.outbound]
host = "h"
port = 465
username = "u"
password = "p"
[agent]
enabled = true
mode = "agent"
"#;

// Verbatim from config.example.toml, comment markers stripped.
const EXAMPLE_COMMANDS: &str = r#"
[[commands]]
name = "review"
description = "Review the current branch for over-engineering"
mode = "plan"
skills = ["pr-review", "ponytail-review"]
user_prompt = """
Review the changes on the current branch against main.
Report findings grouped by severity. Do not modify any code.
"""

[[commands]]
name = "ship"
description = "Run checks and prepare a release"
mode = "build"
skills = ["dev-workflow"]
user_prompt = "Run the pre-PR checklist, then summarize what is left to do."
"#;

#[test]
fn example_commands_parse_and_validate() {
    let config = load_config_from_str(&format!("{BASE}{EXAMPLE_COMMANDS}"))
        .expect("example [[commands]] must parse");

    let errors = validate_config(&config);
    assert!(
        !errors.iter().any(|e| e.path.starts_with("commands")),
        "example [[commands]] must validate, got: {errors:?}"
    );

    assert_eq!(config.commands.len(), 2);

    let review = &config.commands[0];
    assert_eq!(review.name, "review");
    assert_eq!(review.mode.as_deref(), Some("plan"));
    assert_eq!(
        review.skills.as_deref(),
        Some(&["pr-review".to_string(), "ponytail-review".to_string()][..])
    );
    assert!(
        review
            .user_prompt
            .as_deref()
            .unwrap_or("")
            .contains("Report findings")
    );

    let ship = &config.commands[1];
    assert_eq!(ship.name, "ship");
    assert_eq!(ship.mode.as_deref(), Some("build"));
}
