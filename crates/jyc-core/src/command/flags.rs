//! One declaration per flag a command accepts: its spellings, and the line the
//! popup offers it with.
//!
//! Both sides of a flag read the same table. [`rows`] builds the popup's flag
//! level, and a handler resolves what it was handed through [`find`], so a flag
//! cannot be spelled one way in the popup and another way in a handler, and a
//! new spelling reaches both.
//!
//! Only the spellings live here. What a flag means, whether it carries a value,
//! what a repeat of it means, and how a refusal is worded stay with the command:
//! those rules differ per command, and the sentences are its user interface.

use jyc_types::CommandArg;

/// One flag a command accepts.
///
/// The flag's *identity* is the enum variant it is declared with — what a
/// handler matches on, with the compiler checking the arms — and this is how it
/// is spelled and offered.
pub struct FlagSpec {
    /// The long spelling, without its `--` (e.g. `"force"`).
    pub long: &'static str,
    /// The short spelling, for the commands that take one.
    pub short: Option<char>,
    /// The popup's one-line hint, without the short spelling: [`rows`] appends
    /// that, so a spelling is named once, beside the hint it belongs to.
    pub hint: &'static str,
}

/// Declare a flag: `flag("force", Some('f'), "Skip the confirmation guard")`.
const fn flag(long: &'static str, short: Option<char>, hint: &'static str) -> FlagSpec {
    FlagSpec { long, short, hint }
}

/// The popup rows for `set`: each flag's long spelling — the one the popup
/// inserts — with its short spelling named in the hint.
pub fn rows<V>(set: &[(V, FlagSpec)]) -> Vec<CommandArg> {
    set.iter()
        .map(|(_, spec)| CommandArg {
            value: format!("--{}", spec.long),
            description: match spec.short {
                Some(short) => format!("{} (-{short})", spec.hint),
                None => spec.hint.to_string(),
            },
            args: Vec::new(),
        })
        .collect()
}

/// The flag `arg` spells, if any — either spelling, spelled exactly: neither
/// `--forces` nor `--force=yes` is `--force`.
pub fn find<V: Copy + PartialEq>(set: &[(V, FlagSpec)], arg: &str) -> Option<V> {
    set.iter()
        .find(|(_, spec)| spells(spec, arg))
        .map(|(flag, _)| *flag)
}

/// Whether `args` carries `flag`.
///
/// This is how a switch is read — the shape `/close`, `/new` and `/reset` read
/// their guard with — where the handler does not walk the line itself.
pub fn has<V: Copy + PartialEq>(set: &[(V, FlagSpec)], args: &[String], flag: V) -> bool {
    args.iter().any(|arg| find(set, arg) == Some(flag))
}

/// Whether `arg` is one of `spec`'s spellings.
fn spells(spec: &FlagSpec, arg: &str) -> bool {
    match arg.strip_prefix("--") {
        Some(long) => long == spec.long,
        None => match arg.strip_prefix('-') {
            Some(short) => short.chars().eq(spec.short),
            None => false,
        },
    }
}

/// What `/spawn` accepts.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SpawnFlag {
    /// `-i`/`--inherit`.
    Inherit,
    /// `-s`/`--share`.
    Share,
    /// `-n`/`--name <name>`.
    Name,
    /// `-d`/`--dir <path>`.
    Dir,
}

/// `/spawn`'s flags.
pub const SPAWN_FLAGS: &[(SpawnFlag, FlagSpec)] = &[
    (
        SpawnFlag::Inherit,
        flag(
            "inherit",
            Some('i'),
            "Copy the conversation and the transcript too",
        ),
    ),
    (
        SpawnFlag::Share,
        flag("share", Some('s'), "Work in this topic's own directory"),
    ),
    (
        SpawnFlag::Name,
        flag(
            "name",
            Some('n'),
            "Name the new topic; defaults to <this>-2",
        ),
    ),
    (
        SpawnFlag::Dir,
        flag(
            "dir",
            Some('d'),
            "Where the child works; defaults to <agents dir>/<name>",
        ),
    ),
];

/// What `/send` accepts.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum SendFlag {
    /// `-t`/`--topic <topic>`.
    Topic,
    /// `--reply`.
    Reply,
    /// `-a`/`--attach <file>`.
    Attach,
    /// `-r`/`--recipient <id>`.
    Recipient,
}

/// `/send`'s flags.
pub const SEND_FLAGS: &[(SendFlag, FlagSpec)] = &[
    (
        SendFlag::Topic,
        flag("topic", Some('t'), "One of that agent's topics"),
    ),
    (
        SendFlag::Reply,
        flag("reply", None, "Ask the target to send its result back"),
    ),
    (
        SendFlag::Attach,
        flag(
            "attach",
            Some('a'),
            "Attach a file from this topic's directory",
        ),
    ),
    (
        SendFlag::Recipient,
        flag("recipient", Some('r'), "Override the recipient address"),
    ),
];

/// What `/close` accepts.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum CloseFlag {
    /// `-f`/`--force`.
    Force,
    /// `-p`/`--purge`.
    Purge,
}

/// `/close`'s flags.
pub const CLOSE_FLAGS: &[(CloseFlag, FlagSpec)] = &[
    (
        CloseFlag::Force,
        flag("force", Some('f'), "Skip the confirmation guard"),
    ),
    (
        CloseFlag::Purge,
        flag(
            "purge",
            Some('p'),
            "Delete the topic dir too, not just its state",
        ),
    ),
];

/// What `/new` and `/reset` accept: the guard `/close` has.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum ConfirmFlag {
    /// `-f`/`--force`.
    Force,
}

/// `/new`'s and `/reset`'s flags.
pub const CONFIRM_FLAGS: &[(ConfirmFlag, FlagSpec)] = &[(
    ConfirmFlag::Force,
    flag("force", Some('f'), "Skip the confirmation guard"),
)];

/// What `/grant` accepts.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum GrantFlag {
    /// `-w`/`--write`.
    Write,
    /// `-p`/`--persist`.
    Persist,
}

/// `/grant`'s flags.
pub const GRANT_FLAGS: &[(GrantFlag, FlagSpec)] = &[
    (
        GrantFlag::Write,
        flag(
            "write",
            Some('w'),
            "Grant write access too (default is read-only)",
        ),
    ),
    (
        GrantFlag::Persist,
        flag("persist", Some('p'), "Also write the path into config.toml"),
    ),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn find_reads_both_spellings_exactly() {
        assert_eq!(find(CLOSE_FLAGS, "--force"), Some(CloseFlag::Force));
        assert_eq!(find(CLOSE_FLAGS, "-f"), Some(CloseFlag::Force));
        assert_eq!(find(CLOSE_FLAGS, "--purge"), Some(CloseFlag::Purge));
        assert_eq!(find(CLOSE_FLAGS, "-p"), Some(CloseFlag::Purge));
        // Not a spelling of one: a longer name, a case away, a value bolted on.
        assert_eq!(find(CLOSE_FLAGS, "--forces"), None);
        assert_eq!(find(CLOSE_FLAGS, "-F"), None);
        assert_eq!(find(CLOSE_FLAGS, "--force=yes"), None);
        assert_eq!(find(CLOSE_FLAGS, "-f=yes"), None);
        assert_eq!(find(CLOSE_FLAGS, "force"), None);
        assert_eq!(find(CLOSE_FLAGS, ""), None);
    }

    #[test]
    fn has_reads_a_switch_off_the_line() {
        assert!(has(CLOSE_FLAGS, &args(&["--force"]), CloseFlag::Force));
        assert!(has(CLOSE_FLAGS, &args(&["-f"]), CloseFlag::Force));
        assert!(has(
            CLOSE_FLAGS,
            &args(&["subagent", "-f", "-p"]),
            CloseFlag::Purge
        ));
        assert!(!has(CLOSE_FLAGS, &args(&["-f"]), CloseFlag::Purge));
        assert!(!has(CLOSE_FLAGS, &args(&[]), CloseFlag::Force));
    }

    #[test]
    fn rows_offer_the_long_spelling_and_name_the_short_one() {
        let rows = rows(SEND_FLAGS);
        assert_eq!(
            rows.iter().map(|r| r.value.as_str()).collect::<Vec<_>>(),
            ["--topic", "--reply", "--attach", "--recipient"]
        );
        assert_eq!(rows[0].description, "One of that agent's topics (-t)");
        assert_eq!(
            rows[1].description,
            "Ask the target to send its result back"
        );
        assert!(rows.iter().all(|r| r.args.is_empty()), "a flag is a leaf");
    }

    /// The guard is one flag declared for two commands: `/close -f` and
    /// `/new -f` have to mean the same spelling, or the popup lies about one.
    #[test]
    fn the_guard_is_spelled_the_same_wherever_it_is_declared() {
        let close = CLOSE_FLAGS
            .iter()
            .find(|(flag, _)| *flag == CloseFlag::Force)
            .expect("declared for /close");
        let confirm = CONFIRM_FLAGS
            .iter()
            .find(|(flag, _)| *flag == ConfirmFlag::Force)
            .expect("declared for /new");
        assert_eq!(
            (close.1.long, close.1.short, close.1.hint),
            (confirm.1.long, confirm.1.short, confirm.1.hint),
        );
    }
}
