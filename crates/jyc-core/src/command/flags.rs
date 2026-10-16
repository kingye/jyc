//! One declaration per flag a command accepts: its spellings, and the line the
//! popup offers it with.
//!
//! Both sides of a flag read the same table. [`rows`] builds the popup's flag
//! level, and a handler resolves what it was handed through [`find_spec`],
//! which hands back the declaration itself — so nothing keeps a second copy of
//! what a flag takes. A flag therefore cannot be spelled one way in the popup
//! and another way in a handler, and a new spelling reaches both.
//!
//! Only the spellings live here. What a flag means, whether it carries a value,
//! what a repeat of it means, and how a refusal is worded stay with the command:
//! those rules differ per command, and the sentences are its user interface.
//!
//! A set is read by the popup and the parser alike where a command has both.
//! `/grant` completes nothing, so its set is its parser's alone — its hints are
//! written down for the day it gets a popup level, not rendered anywhere yet.
//!
//! What is *not* here: whether a value is valid, what it means, and every
//! sentence about it other than the two the declaration can answer for itself —
//! [`ArgError`](super::spec::ArgError) words an unknown spelling, a missing
//! value and an unwanted repeat once for every command.

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
    /// The value's placeholder when the flag takes one (`--name <name>`), and
    /// with it the fact that it does: the parser reads the next word, the popup
    /// shows where it goes.
    pub value: Option<&'static str>,
    /// Whether a second occurrence is refused (`/spawn -n a -n b`). False where
    /// repeating is meaningful (`/send -a one -a two`) or where the last one
    /// simply wins (`/send -t a -t b`).
    pub once: bool,
    /// The popup's one-line hint, without the spellings: [`rows`] appends
    /// those, so a spelling is named once, beside the hint it belongs to.
    pub hint: &'static str,
}

/// Declare a switch: a flag with no value.
///
/// `flag("force", Some('f'), "Skip the confirmation guard")`
const fn flag(long: &'static str, short: Option<char>, hint: &'static str) -> FlagSpec {
    FlagSpec {
        long,
        short,
        value: None,
        once: false,
        hint,
    }
}

/// Declare a flag that takes a value: the placeholder it shows, and whether a
/// repeat of it is refused.
///
/// `taking("name", Some('n'), "name", "Name the new topic", true)`
const fn taking(
    long: &'static str,
    short: Option<char>,
    value: &'static str,
    hint: &'static str,
    once: bool,
) -> FlagSpec {
    FlagSpec {
        long,
        short,
        value: Some(value),
        once,
        hint,
    }
}

/// The popup rows for `set`: each flag's long spelling — the one the popup
/// inserts — with its short spelling and value named in the hint.
pub fn rows<V>(set: &[(V, FlagSpec)]) -> Vec<CommandArg> {
    set.iter()
        .map(|(_, spec)| CommandArg {
            value: format!("--{}", spec.long),
            description: match (spec.short, spec.value) {
                (Some(short), Some(value)) => format!("{} (-{short} <{value}>)", spec.hint),
                (Some(short), None) => format!("{} (-{short})", spec.hint),
                (None, _) => spec.hint.to_string(),
            },
            args: Vec::new(),
        })
        .collect()
}

/// The flag `arg` spells, and how it is declared.
///
/// What every reader goes through — the parser wants the declaration (whether a
/// value follows, whether a repeat of it is refused) and nothing else needs the
/// flag without it, which is why there is no `find`-without-the-spec beside it.
pub fn find_spec<'a, V: Copy + PartialEq>(
    set: &'a [(V, FlagSpec)],
    arg: &str,
) -> Option<(V, &'a FlagSpec)> {
    set.iter()
        .find(|(_, spec)| spells(spec, arg))
        .map(|(flag, spec)| (*flag, spec))
}

/// A typed set's declarations, without their enum keys — what the command table
/// can hold, since only a set's own command knows its key type.
///
/// This is a projection of the one declaration, not a second copy of it: the
/// spellings stay beside the enum a handler matches on, and the table reads them
/// through here for the popup level and the usage line.
pub fn specs_of<V>(set: &'static [(V, FlagSpec)]) -> Vec<&'static FlagSpec> {
    set.iter().map(|(_, spec)| spec).collect()
}

/// [`SPAWN_FLAGS`] as `/spawn`'s table row holds it.
pub fn spawn_specs() -> Vec<&'static FlagSpec> {
    specs_of(SPAWN_FLAGS)
}

/// [`SEND_FLAGS`] as `/send`'s table row holds it.
pub fn send_specs() -> Vec<&'static FlagSpec> {
    specs_of(SEND_FLAGS)
}

/// [`CLOSE_FLAGS`] as `/close`'s table row holds it.
pub fn close_specs() -> Vec<&'static FlagSpec> {
    specs_of(CLOSE_FLAGS)
}

/// [`CONFIRM_FLAGS`] as `/new`'s and `/reset`'s table rows hold it.
pub fn confirm_specs() -> Vec<&'static FlagSpec> {
    specs_of(CONFIRM_FLAGS)
}

/// [`GRANT_FLAGS`] as `/grant`'s table row holds it.
pub fn grant_specs() -> Vec<&'static FlagSpec> {
    specs_of(GRANT_FLAGS)
}

/// Whether `arg` is one of `spec`'s spellings, spelled exactly: neither
/// `--forces` nor `--force=yes` is `--force`.
///
/// A flag with no short spelling has exactly one spelling, and a lone `-` is
/// nobody's: it is a message word (`/send joe -`), not a flag.
fn spells(spec: &FlagSpec, arg: &str) -> bool {
    if let Some(long) = arg.strip_prefix("--") {
        return long == spec.long;
    }
    match (arg.strip_prefix('-'), spec.short) {
        (Some(short), Some(c)) => short.chars().eq(Some(c)),
        _ => false,
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
        taking(
            "name",
            Some('n'),
            "name",
            "Name the new topic; defaults to <this>-2",
            true,
        ),
    ),
    (
        SpawnFlag::Dir,
        taking(
            "dir",
            Some('d'),
            "path",
            "Where the child works; defaults to <agents dir>/<name>",
            true,
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
        taking(
            "topic",
            Some('t'),
            "topic",
            "One of that agent's topics",
            false,
        ),
    ),
    (
        SendFlag::Reply,
        flag("reply", None, "Ask the target to send its result back"),
    ),
    (
        SendFlag::Attach,
        taking(
            "attach",
            Some('a'),
            "file",
            "Attach a file from this topic's directory",
            false,
        ),
    ),
    (
        SendFlag::Recipient,
        taking(
            "recipient",
            Some('r'),
            "id",
            "Override the recipient address",
            false,
        ),
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

/// The guard `/close`, `/new` and `/reset` all read — written once, so the three
/// cannot drift apart.
const FORCE: FlagSpec = flag("force", Some('f'), "Skip the confirmation guard");

/// `/close`'s flags.
pub const CLOSE_FLAGS: &[(CloseFlag, FlagSpec)] = &[
    (CloseFlag::Force, FORCE),
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
pub const CONFIRM_FLAGS: &[(ConfirmFlag, FlagSpec)] = &[(ConfirmFlag::Force, FORCE)];

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

    #[test]
    fn find_spec_reads_both_spellings_exactly() {
        let flag = |set: &'static [(CloseFlag, FlagSpec)], arg: &str| {
            find_spec(set, arg).map(|(flag, _)| flag)
        };
        assert_eq!(flag(CLOSE_FLAGS, "--force"), Some(CloseFlag::Force));
        assert_eq!(flag(CLOSE_FLAGS, "-f"), Some(CloseFlag::Force));
        assert_eq!(flag(CLOSE_FLAGS, "--purge"), Some(CloseFlag::Purge));
        assert_eq!(flag(CLOSE_FLAGS, "-p"), Some(CloseFlag::Purge));
        // Not a spelling of one: a longer name, a case away, a value bolted on.
        assert_eq!(flag(CLOSE_FLAGS, "--forces"), None);
        assert_eq!(flag(CLOSE_FLAGS, "-F"), None);
        assert_eq!(flag(CLOSE_FLAGS, "--force=yes"), None);
        assert_eq!(flag(CLOSE_FLAGS, "-f=yes"), None);
        assert_eq!(flag(CLOSE_FLAGS, "force"), None);
        assert_eq!(flag(CLOSE_FLAGS, ""), None);
        // A flag with no short spelling has one spelling, and a lone `-` is
        // not it: `/send joe -` sends a dash, it does not set `--reply`.
        let send = |arg: &str| find_spec(SEND_FLAGS, arg).map(|(flag, _)| flag);
        assert_eq!(send("--reply"), Some(SendFlag::Reply));
        assert_eq!(send("-"), None);
        assert_eq!(send("--"), None);
    }

    /// The declaration a match reads is the one the popup shows, spellings and
    /// all.
    #[test]
    fn find_spec_hands_back_the_declaration() {
        let (flag, spec) = find_spec(SEND_FLAGS, "-t").expect("-t is a spelling");
        assert_eq!(flag, SendFlag::Topic);
        assert_eq!(spec.long, "topic");
        assert_eq!(spec.value, Some("topic"));
        assert!(!spec.once, "a second -t replaces the first");
    }

    #[test]
    fn rows_offer_the_long_spelling_and_name_the_short_one() {
        let rows = rows(SEND_FLAGS);
        assert_eq!(
            rows.iter().map(|r| r.value.as_str()).collect::<Vec<_>>(),
            ["--topic", "--reply", "--attach", "--recipient"]
        );
        assert_eq!(
            rows[0].description,
            "One of that agent's topics (-t <topic>)"
        );
        assert_eq!(
            rows[1].description,
            "Ask the target to send its result back"
        );
        assert!(rows.iter().all(|r| r.args.is_empty()), "a flag is a leaf");
    }
}
