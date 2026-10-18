//! CLI helper discovery — what extra command-line tools this machine has.
//!
//! The `bash` tool runs against the host PATH, but the model only ever reaches
//! for what its training mentioned: `grep -r`, `find`, `cat`. On a machine
//! that has ripgrep, fd or jq installed it will still run the slow one, because
//! nothing ever said the fast one exists — and a hint in `AGENTS.md` is either
//! wrong on the next machine or costs the user a turn of `command -v` probes.
//!
//! So we probe `PATH` once per process and announce exactly what is there.
//! Nothing here replaces a built-in tool: `grep`, `glob` and `read` stay the
//! built-in Rust implementations, and a helper that is not installed is simply
//! not mentioned, so the prompt can never send the model looking for a tool
//! that is not there.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// A command-line helper worth telling the agent about.
pub struct Helper {
    /// The executable name as it appears on `PATH`.
    pub name: &'static str,
    /// One line saying what it is for and when to reach for it. This is read
    /// by a model, so it states the catch as plainly as the pitch — an
    /// unqualified "fzf" would have the agent open a TUI into a dead pipe.
    pub note: &'static str,
}

/// The helpers we look for, in the order they are announced.
///
/// Deliberately short: every entry is prompt text the model reads on every
/// call. `bat` and `delta` are absent on purpose — bat's syntax highlighting
/// needs a TTY, so it either degrades to `cat` or leaks ANSI escapes into the
/// model's input, and delta is a git *pager*, which never runs on this path
/// (the agent reads `git diff` output directly). Adding one is a line here.
pub const HELPERS: &[Helper] = &[
    Helper {
        name: "rg",
        note: "ripgrep — recursive content search, prefer it over `grep -r`",
    },
    Helper {
        name: "fd",
        note: "file finder, prefer it over `find`",
    },
    Helper {
        name: "fzf",
        note: "fuzzy filter over stdin — NON-INTERACTIVE ONLY: pipe into `fzf --filter <query>`; \
               bare `fzf` opens a TUI and hangs without a TTY",
    },
    Helper {
        name: "jq",
        note: "JSON query and filter — prefer it over grepping or sed-ing JSON",
    },
];

/// The helpers installed under `path`, in `HELPERS` order.
///
/// Takes the `PATH` value as an argument rather than reading the environment
/// itself: that keeps the probe a pure function of its input, so tests build a
/// throwaway directory instead of mutating process state (which is both global
/// and racy under the parallel test runner).
pub fn detect(path: Option<&OsStr>) -> Vec<&'static Helper> {
    let Some(path) = path else {
        return Vec::new();
    };
    HELPERS
        .iter()
        .filter(|helper| is_on_path(path, helper.name))
        .collect()
}

/// True when `name` resolves to an executable file under any `PATH` entry.
fn is_on_path(path: &OsStr, name: &str) -> bool {
    path_entries(path)
        .into_iter()
        .any(|dir| is_executable(&dir.join(name)))
}

/// The `PATH` directories to search, empty entries dropped.
///
/// A shell resolves an empty entry to the working directory; we deliberately do
/// not, so a file named `rg` in the topic's own tree can never be announced as
/// an installed tool. Returns a `Vec` rather than a lazy iterator so this rule
/// can be asserted on its own — proving it through `detect` would mean
/// planting a file in the process-wide cwd, which tests must not touch.
fn path_entries(path: &OsStr) -> Vec<PathBuf> {
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .collect()
}

/// True when `path` is a file this process is allowed to execute.
#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// True when `path` looks like an executable file. No permission bit to read
/// off platforms without one; presence is the best we can do.
#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// The helpers this machine has, probed once per process.
pub fn available() -> &'static [&'static Helper] {
    static AVAILABLE: OnceLock<Vec<&'static Helper>> = OnceLock::new();
    AVAILABLE
        .get_or_init(|| detect(std::env::var_os("PATH").as_deref()))
        .as_slice()
}

/// The prompt section for the given helpers, or `None` when there are none.
///
/// The section must not appear on a machine without these tools: it would be
/// the model being told to reach for something that is not there, which is
/// worse than saying nothing at all.
pub fn prompt_section(helpers: &[&Helper]) -> Option<String> {
    if helpers.is_empty() {
        return None;
    }
    let mut section = String::from(
        "\n## CLI Helpers\n\n\
         These are installed on this machine. You MUST use a listed helper when it covers the \
         task better. When no listed helper fits, fall back to the standard tools.\n\n",
    );
    for helper in helpers {
        section.push_str(&format!("- `{}` — {}\n", helper.name, helper.note));
    }
    Some(section)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    mod unix {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        /// Write a fake executable named `name` into `dir`, with `mode`.
        fn fake(dir: &Path, name: &str, mode: u32) {
            let path = dir.join(name);
            std::fs::write(&path, "#!/bin/sh\n").expect("write fake executable");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                .expect("chmod fake executable");
        }

        #[test]
        fn detect_finds_an_installed_helper() {
            let dir = tempfile::tempdir().expect("tempdir");
            fake(dir.path(), "rg", 0o755);

            let found = detect(Some(OsStr::new(dir.path().as_os_str())));

            assert_eq!(
                found.iter().map(|h| h.name).collect::<Vec<_>>(),
                vec!["rg"],
                "only the installed helper is reported"
            );
        }

        #[test]
        fn detect_ignores_a_file_without_the_executable_bit() {
            let dir = tempfile::tempdir().expect("tempdir");
            fake(dir.path(), "rg", 0o644);

            assert!(
                detect(Some(OsStr::new(dir.path().as_os_str()))).is_empty(),
                "a readable but non-executable file is not a tool the agent can run"
            );
        }

        #[test]
        fn detect_ignores_a_directory_with_the_right_name() {
            let dir = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir(dir.path().join("rg")).expect("mkdir named rg");

            assert!(
                detect(Some(OsStr::new(dir.path().as_os_str()))).is_empty(),
                "a directory is not an executable"
            );
        }

        #[test]
        fn detect_matches_the_exact_name_only() {
            let dir = tempfile::tempdir().expect("tempdir");
            fake(dir.path(), "rgx", 0o755);
            fake(dir.path(), "fzf-filter", 0o755);

            assert!(
                detect(Some(OsStr::new(dir.path().as_os_str()))).is_empty(),
                "a near-miss name is not the tool"
            );
        }

        #[test]
        fn detect_preserves_the_declared_order_across_entries() {
            let dir = tempfile::tempdir().expect("tempdir");
            // Installed in reverse: the announced order must come from HELPERS,
            // not from whatever order the directories happen to yield.
            fake(dir.path(), "jq", 0o755);
            fake(dir.path(), "rg", 0o755);

            let found = detect(Some(OsStr::new(dir.path().as_os_str())));

            assert_eq!(
                found.iter().map(|h| h.name).collect::<Vec<_>>(),
                vec!["rg", "jq"]
            );
        }

        #[test]
        fn detect_searches_every_path_entry() {
            let first = tempfile::tempdir().expect("tempdir");
            let second = tempfile::tempdir().expect("tempdir");
            fake(first.path(), "rg", 0o755);
            fake(second.path(), "jq", 0o755);
            // Empty entries interspersed with real ones: dropping the empties
            // must not cost us a directory.
            let joined = format!(":{}::{}:", first.path().display(), second.path().display());

            let found = detect(Some(OsStr::new(&joined)));

            assert_eq!(
                found.iter().map(|h| h.name).collect::<Vec<_>>(),
                vec!["rg", "jq"],
                "a helper is found wherever on PATH it sits"
            );
        }
    }

    #[test]
    fn empty_path_entries_are_skipped_not_resolved_to_the_cwd() {
        // Built with `join_paths` so the separators are the platform's own:
        // this module is not unix-gated, and a hardcoded ":" would arrive as one
        // unsplit entry on Windows.
        let path = std::env::join_paths(["/a", "", "/b", ""]).expect("join_paths");

        assert_eq!(
            path_entries(&path),
            vec![PathBuf::from("/a"), PathBuf::from("/b")],
            "an empty entry means the cwd to a shell; it is dropped"
        );
    }

    #[test]
    fn a_path_of_only_empty_entries_yields_nothing_to_search() {
        let path = std::env::join_paths(["", "", ""]).expect("join_paths");

        assert!(path_entries(&path).is_empty());
    }

    #[test]
    fn detect_without_a_path_reports_nothing() {
        assert!(detect(None).is_empty());
        assert!(detect(Some(OsStr::new(""))).is_empty());
    }

    #[test]
    fn prompt_section_is_absent_without_helpers() {
        assert_eq!(prompt_section(&[]), None);
    }

    #[test]
    fn prompt_section_names_every_helper_and_its_note() {
        let rg = HELPERS.iter().find(|h| h.name == "rg").expect("rg listed");
        let jq = HELPERS.iter().find(|h| h.name == "jq").expect("jq listed");

        let section = prompt_section(&[rg, jq]).expect("section for two helpers");

        assert!(section.starts_with("\n## CLI Helpers\n\n"));
        assert!(
            section.contains("You MUST use a listed helper when it covers the task better"),
            "the section states the rule as an instruction — phrased as a suggestion \
             it was delivered, read, and ignored (#943)"
        );
        assert!(section.contains("- `rg` — ripgrep"));
        assert!(section.contains("- `jq` — JSON query"));
        assert!(
            !section.contains("`fzf`"),
            "a helper that was not detected is not mentioned — the section lists \
             what is installed, nothing else"
        );
    }

    #[test]
    fn prompt_section_carries_each_helper_catch() {
        // The note is the only thing standing between the model and a helper
        // that hangs, so the caveat has to survive into the prompt.
        for helper in HELPERS {
            let section = prompt_section(&[helper]).expect("section for one helper");
            assert!(
                section.contains(helper.note),
                "{}: note did not reach the prompt",
                helper.name
            );
        }
        let fzf = prompt_section(&[&HELPERS[2]]).expect("fzf section");
        assert!(
            fzf.contains("--filter") && fzf.contains("NON-INTERACTIVE"),
            "fzf must be announced as non-interactive only"
        );
    }

    #[test]
    fn prompt_section_does_not_hedge_the_rule() {
        let section = prompt_section(&[&HELPERS[0]]).expect("section");

        // Advisory wording shipped with the feature and was read as advice:
        // the section was received, quoted back, and then ignored. Guard the
        // specific regression of re-adding a hedge while the imperative stays.
        assert!(
            !section.contains("your call") && !section.contains("not an instruction"),
            "the section states the rule instead of offering it as a choice (#941)"
        );
    }
}
