//! One declaration of a command's argument shape, and the parser that reads a
//! command line through it.
//!
//! A shape lives on the command's row in `builtin_commands!` — the table that
//! already names the command and describes it — so its argument syntax is
//! written once and read three ways: the parser below, the popup's argument
//! level, and the usage line.
//!
//! The parser *records*; it does not judge. It reports which flags appeared with
//! what values, which words were positional, and where the trailing text begins.
//! Whether a value is valid, what it means, and which arguments a command needs
//! stay with the command, because those sentences are its user interface. The
//! one thing this module does word is the four refusals the shape itself can
//! raise: a spelling it does not know, a value that did not arrive, a repeat the
//! declaration forbids, and a word too many. Those used to be written per
//! command, and two of them did not exist at all — `/close --froce` was silently
//! read as "no guard given".
//!
//! [`builtin_commands!`]: super::builtin::builtin_commands

use std::fmt;

use super::flags::{self, FlagSpec};

/// How a command's arguments are shaped.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    /// Flags only: any other word is one too many (`/new`, `/build`).
    None,
    /// Fixed positional words, with flags allowed anywhere among them
    /// (`/spawn`, `/close`, `/grant`). A label names one slot in the usage line,
    /// and may spell its choices (`subagent|all`) where they are few and fixed.
    Args { labels: &'static [&'static str] },
    /// One positional word, then the rest of the line is literal text, dashes
    /// and all (`/send`). `--` ends the leading flags explicitly, for text that
    /// starts with one.
    ThenBody {
        /// The positional word's name.
        label: &'static str,
        /// The trailing text's name.
        body: &'static str,
    },
    /// The first word comes from a set, then optional positional words and/or
    /// trailing text (`/skill`, `/mcp`, `/bill`, `/backlog`). A first word
    /// outside the set is recorded, not refused: whether the command knows it is
    /// the command's business.
    Subcommand {
        /// The words the first slot offers.
        values: &'static [&'static str],
        /// Further positional words, after the subcommand.
        labels: &'static [&'static str],
        /// What the text after those is called, when there is any.
        rest: Option<&'static str>,
    },
}

/// What a command line carried, in the order it carried it.
#[derive(Debug, PartialEq)]
pub struct Parsed<F> {
    /// Flags in the order they appeared, each with the value it was given.
    /// Every occurrence is kept: a repeat is the command's business, not the
    /// parser's.
    pub flags: Vec<(F, Option<String>)>,
    /// The positional words, in order.
    pub positionals: Vec<String>,
    /// The trailing text of a [`Shape::ThenBody`] or a `rest`, joined with
    /// single spaces.
    pub body: Option<String>,
}

impl<F: Copy + PartialEq> Parsed<F> {
    /// Whether `flag` appeared.
    pub fn has(&self, flag: F) -> bool {
        self.flags.iter().any(|(f, _)| *f == flag)
    }

    /// The value of the last occurrence of `flag`.
    pub fn value(&self, flag: F) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|(f, _)| *f == flag)
            .and_then(|(_, value)| value.as_deref())
    }

    /// Append a word to the trailing text, joined with single spaces.
    fn push_text(&mut self, word: &str) {
        match &mut self.body {
            Some(text) => {
                text.push(' ');
                text.push_str(word);
            }
            None => self.body = Some(word.to_string()),
        }
    }
}

/// What the shape alone refuses.
#[derive(Debug, PartialEq)]
pub enum ArgError {
    /// A dash-word that is not one of the command's flags.
    UnknownFlag {
        /// The word as it was written.
        spelling: String,
        /// The flags it could have been, or empty when the command has none.
        known: String,
    },
    /// A value-taking flag was the last word on the line, or was followed by
    /// another flag — which cannot be its value.
    NeedsValue {
        /// The word as it was written.
        spelling: String,
    },
    /// A flag declared `once` was given twice.
    GivenTwice {
        /// The word as it was written.
        spelling: String,
    },
    /// A flag that carries no value was handed one (`--force=yes`).
    TakesValue {
        /// The word as it was written.
        spelling: String,
    },
    /// More positional words than the shape has room for; `expected` names what
    /// it had room for, and is empty when it takes none.
    TooMany {
        /// What the shape had room for.
        expected: String,
    },
}

impl fmt::Display for ArgError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownFlag { spelling, known } if known.is_empty() => {
                write!(f, "unknown flag {spelling}")
            }
            Self::UnknownFlag { spelling, known } => {
                write!(f, "unknown flag {spelling} (known: {known})")
            }
            Self::NeedsValue { spelling } => write!(f, "{spelling} needs a value"),
            Self::GivenTwice { spelling } => write!(f, "{spelling} given twice"),
            Self::TakesValue { spelling } => write!(f, "{spelling} takes no value"),
            Self::TooMany { expected } if expected.is_empty() => {
                write!(f, "too many arguments: this command takes none")
            }
            Self::TooMany { expected } => {
                write!(f, "too many arguments (expected: {expected})")
            }
        }
    }
}

impl std::error::Error for ArgError {}

/// Read `args` through `shape` and the command's flag declarations.
///
/// Every word is accounted for: a declared flag is recorded with its value, a
/// dash-word that is no flag is refused, a word the shape has room for is
/// positional, and one past that room is refused rather than dropped.
pub fn parse<F: Copy + PartialEq>(
    shape: Shape,
    flags: &[(F, FlagSpec)],
    args: &[String],
) -> Result<Parsed<F>, ArgError> {
    let mut parsed = Parsed {
        flags: Vec::new(),
        positionals: Vec::new(),
        body: None,
    };
    let slots = shape.slots();
    let keeps_text = shape.keeps_text();
    let mut in_text = false;
    let mut rest = args.iter();

    while let Some(arg) = rest.next() {
        let arg = arg.as_str();

        if in_text {
            parsed.push_text(arg);
            continue;
        }

        if let Some((flag, spec)) = flags::find_spec(flags, arg) {
            let value = match spec.value {
                Some(_) => Some(next_value(&mut rest, arg)?),
                None => None,
            };
            record(&mut parsed, flag, spec, arg, value)?;
            continue;
        }

        // A value may ride along in the same token (`--name=scratch`) — the one
        // place a flag and its value are one word. Only a flag that takes a value
        // can: `--force=yes` is a flag being handed what it cannot carry, and
        // saying so beats "unknown flag".
        if let Some((head, value)) = arg.split_once('=')
            && let Some((flag, spec)) = flags::find_spec(flags, head)
        {
            if spec.value.is_none() {
                return Err(ArgError::TakesValue {
                    spelling: head.to_string(),
                });
            }
            if value.is_empty() {
                return Err(ArgError::NeedsValue {
                    spelling: head.to_string(),
                });
            }
            record(&mut parsed, flag, spec, head, Some(value.to_string()))?;
            continue;
        }

        if arg == "--" && keeps_text {
            in_text = true;
            continue;
        }

        if looks_like_a_flag(arg) {
            return Err(ArgError::UnknownFlag {
                spelling: arg.to_string(),
                known: known_list(flags),
            });
        }

        if parsed.positionals.len() < slots {
            parsed.positionals.push(arg.to_string());
        } else if keeps_text {
            in_text = true;
            parsed.push_text(arg);
        } else {
            return Err(ArgError::TooMany {
                expected: shape.expected(),
            });
        }
    }

    Ok(parsed)
}

/// Read `name`'s command line: the shape its row declares, and the flag
/// declarations its handler owns.
///
/// This is what a handler calls — the shape is not repeated in the handler, so
/// the line it accepts is the line the popup offers.
pub fn parse_command<F: Copy + PartialEq>(
    name: &str,
    flags: &[(F, FlagSpec)],
    args: &[String],
) -> Result<Parsed<F>, ArgError> {
    parse(super::builtin::shape_of(name), flags, args)
}

/// Read a command line for a command that declares no flags of its own.
///
/// `/ungrant` takes a path and nothing else, so it has no typed set to name —
/// but it still reads its line through the same shape, which is what refuses a
/// dash-word nobody declared.
pub fn parse_plain(name: &str, args: &[String]) -> Result<Parsed<()>, ArgError> {
    parse_with(super::builtin::shape_of(name), args)
}

/// Read a command line through a shape the caller already holds, for a handler
/// that reads one declaration for the commands it serves.
pub fn parse_with(shape: Shape, args: &[String]) -> Result<Parsed<()>, ArgError> {
    parse::<()>(shape, &[], args)
}

/// The usage line for a command: its name, what its shape accepts, and its
/// flags.
///
/// This is the one place `/close [subagent|all] [--force] [--purge]` is written
/// down. The command's own description, `/?` and the popup all read it, so the
/// line cannot say one thing where the parser does another.
pub fn usage(name: &str, shape: Shape, specs: &[&FlagSpec]) -> String {
    let flags: Vec<String> = specs
        .iter()
        .map(|spec| match spec.value {
            Some(value) => format!("[--{} <{value}>]", spec.long),
            None => format!("[--{}]", spec.long),
        })
        .collect();
    let mut parts = vec![name.to_string()];
    // Where the shape keeps trailing text, a flag is read only until that text
    // starts — so the flags come first, or the line would advertise a flag the
    // parser reads as text.
    if shape.keeps_text() {
        parts.extend(flags.iter().cloned());
    }
    match shape {
        Shape::None => {}
        Shape::Args { labels } => {
            parts.extend(labels.iter().map(|label| format!("[{label}]")));
        }
        Shape::ThenBody { label, body } => {
            parts.push(format!("<{label}>"));
            parts.push(format!("<{body}>"));
        }
        Shape::Subcommand {
            values,
            labels,
            rest,
        } => {
            parts.push(format!("<{}>", values.join("|")));
            parts.extend(labels.iter().map(|label| format!("[{label}]")));
            if let Some(rest) = rest {
                parts.push(format!("[{rest}]"));
            }
        }
    }
    if !shape.keeps_text() {
        parts.extend(flags);
    }
    parts.join(" ")
}

/// Record one occurrence of `flag`, refusing a repeat the declaration forbids.
fn record<F: Copy + PartialEq>(
    parsed: &mut Parsed<F>,
    flag: F,
    spec: &FlagSpec,
    spelling: &str,
    value: Option<String>,
) -> Result<(), ArgError> {
    if spec.once && parsed.has(flag) {
        return Err(ArgError::GivenTwice {
            spelling: spelling.to_string(),
        });
    }
    parsed.flags.push((flag, value));
    Ok(())
}

/// The value of the flag just read: the next word, unless there is none or it is
/// another flag.
fn next_value<'a, I: Iterator<Item = &'a String>>(
    rest: &mut I,
    spelling: &str,
) -> Result<String, ArgError> {
    let missing = || ArgError::NeedsValue {
        spelling: spelling.to_string(),
    };
    match rest.next() {
        Some(word) if !looks_like_a_flag(word) && !word.trim().is_empty() => Ok(word.clone()),
        _ => Err(missing()),
    }
}

/// Whether a word is written like a flag: anything dash-led but a lone `-`,
/// which is a word (`/send joe -` sends a dash).
///
/// `--` is one too, and is only ever read as the text separator by the shapes
/// that keep text.
fn looks_like_a_flag(arg: &str) -> bool {
    arg.len() > 1 && arg.starts_with('-')
}

/// The command's spellings, for the refusal that lists what was possible.
fn known_list<F>(flags: &[(F, FlagSpec)]) -> String {
    flags
        .iter()
        .map(|(_, spec)| format!("--{}", spec.long))
        .collect::<Vec<_>>()
        .join(", ")
}

impl Shape {
    /// How many positional words the shape has room for.
    fn slots(self) -> usize {
        match self {
            Self::None => 0,
            Self::Args { labels } => labels.len(),
            Self::ThenBody { .. } => 1,
            Self::Subcommand { labels, .. } => 1 + labels.len(),
        }
    }

    /// Whether words past the positional ones become [`Parsed::body`].
    fn keeps_text(self) -> bool {
        match self {
            Self::None | Self::Args { .. } => false,
            Self::ThenBody { .. } => true,
            Self::Subcommand { rest, .. } => rest.is_some(),
        }
    }

    /// What the shape had room for, for the refusal that says so.
    fn expected(self) -> String {
        match self {
            Self::None => String::new(),
            Self::Args { labels } => labels.join(" "),
            Self::ThenBody { label, .. } => label.to_string(),
            Self::Subcommand { values, labels, .. } => {
                let mut parts = vec![values.join("|")];
                parts.extend(labels.iter().map(|label| (*label).to_string()));
                parts.join(" ")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::flags::{
        CLOSE_FLAGS, CloseFlag, SEND_FLAGS, SPAWN_FLAGS, SendFlag, SpawnFlag,
    };

    /// The shape `/close` declares on its table row.
    const CLOSE_ARGS: Shape = Shape::Args {
        labels: &["subagent|all"],
    };

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn parse_close(list: &[&str]) -> Result<Parsed<CloseFlag>, ArgError> {
        parse(CLOSE_ARGS, CLOSE_FLAGS, &args(list))
    }

    #[test]
    fn flags_are_recorded_in_order_with_their_values() {
        let parsed = parse_close(&["joe", "-p", "--force"]).unwrap();
        assert_eq!(parsed.positionals, ["joe"]);
        assert_eq!(
            parsed.flags,
            [(CloseFlag::Purge, None), (CloseFlag::Force, None)]
        );
        assert!(parsed.has(CloseFlag::Force));
        assert!(parsed.has(CloseFlag::Purge));
    }

    #[test]
    fn an_unknown_spelling_lists_what_it_could_have_been() {
        assert_eq!(
            parse_close(&["--froce"]).unwrap_err().to_string(),
            "unknown flag --froce (known: --force, --purge)"
        );
        // A command with no flags says so by naming none.
        let none = parse::<CloseFlag>(Shape::None, &[], &args(&["-x"])).unwrap_err();
        assert_eq!(none.to_string(), "unknown flag -x");
    }

    #[test]
    fn a_lone_dash_is_a_word_and_a_double_one_is_not() {
        let parsed = parse_close(&["-"]).unwrap();
        assert_eq!(parsed.positionals, ["-"]);
        // `--` is a dash-word where the shape keeps no text, so it is refused
        // like any other spelling nobody declared.
        assert_eq!(
            parse_close(&["--"]).unwrap_err().to_string(),
            "unknown flag -- (known: --force, --purge)"
        );
    }

    /// `/spawn`'s declarations: `-n` takes a value, and asking for it twice is
    /// refused where a repeat of the switches is not.
    #[test]
    fn a_value_takes_the_next_word_and_a_once_flag_refuses_a_repeat() {
        const SHAPE: Shape = Shape::Args { labels: &["type"] };
        let parsed = parse(SHAPE, SPAWN_FLAGS, &args(&["-n", "joe", "researcher"])).unwrap();
        assert_eq!(parsed.positionals, ["researcher"]);
        assert_eq!(parsed.value(SpawnFlag::Name), Some("joe"));
        // A value that never arrives, or that is another flag.
        for line in [&["-n"][..], &["-n", "-s"][..]] {
            assert_eq!(
                parse(SHAPE, SPAWN_FLAGS, &args(line))
                    .unwrap_err()
                    .to_string(),
                "-n needs a value"
            );
        }
        assert_eq!(
            parse(SHAPE, SPAWN_FLAGS, &args(&["-n", "a", "--name", "b"]))
                .unwrap_err()
                .to_string(),
            "--name given twice"
        );
        // A switch declared repeatable keeps both, for the command to read.
        let twice = parse_close(&["-f", "-f"]).unwrap();
        assert_eq!(twice.flags.len(), 2, "a repeat is the command's business");
    }

    #[test]
    fn then_body_keeps_the_rest_verbatim() {
        const SHAPE: Shape = Shape::ThenBody {
            label: "agent",
            body: "message",
        };
        let parsed = parse(SHAPE, SEND_FLAGS, &args(&["joe", "/close", "--force"])).unwrap();
        assert_eq!(parsed.positionals, ["joe"]);
        assert_eq!(parsed.body.as_deref(), Some("/close --force"));
        assert!(parsed.flags.is_empty(), "text is not read for flags");
        // `--` is how a message that starts with a dash is written.
        let dashed = parse(SHAPE, SEND_FLAGS, &args(&["joe", "--", "-x here"])).unwrap();
        assert_eq!(dashed.body.as_deref(), Some("-x here"));
        // Flags before the text are still flags.
        let flagged = parse(SHAPE, SEND_FLAGS, &args(&["joe", "--reply", "hello"])).unwrap();
        assert_eq!(flagged.body.as_deref(), Some("hello"));
        assert!(flagged.has(SendFlag::Reply));
    }

    #[test]
    fn words_past_the_room_are_refused_not_dropped() {
        assert_eq!(
            parse_close(&["joe", "extra"]).unwrap_err().to_string(),
            "too many arguments (expected: subagent|all)"
        );
        assert_eq!(
            parse::<CloseFlag>(Shape::None, &[], &args(&["joe"]))
                .unwrap_err()
                .to_string(),
            "too many arguments: this command takes none"
        );
    }

    #[test]
    fn a_subcommand_records_its_word_and_what_follows() {
        const SHAPE: Shape = Shape::Subcommand {
            values: &["on", "off", "reset"],
            labels: &["name"],
            rest: None,
        };
        let parsed = parse::<CloseFlag>(SHAPE, &[], &args(&["on", "ponytail"])).unwrap();
        assert_eq!(parsed.positionals, ["on", "ponytail"]);
        // A word outside the set is recorded too — the command decides.
        let odd = parse::<CloseFlag>(SHAPE, &[], &args(&["sideways"])).unwrap();
        assert_eq!(odd.positionals, ["sideways"]);
        assert_eq!(
            parse::<CloseFlag>(SHAPE, &[], &args(&["on", "a", "b"]))
                .unwrap_err()
                .to_string(),
            "too many arguments (expected: on|off|reset name)"
        );
    }

    #[test]
    fn usage_reads_the_shape_and_the_flags() {
        assert_eq!(
            usage("/close", CLOSE_ARGS, &flags::specs_of(CLOSE_FLAGS)),
            "/close [subagent|all] [--force] [--purge]"
        );
        assert_eq!(
            usage(
                "/send",
                Shape::ThenBody {
                    label: "agent",
                    body: "message"
                },
                &flags::specs_of(SEND_FLAGS)
            ),
            "/send [--topic <topic>] [--reply] [--attach <file>] [--recipient <id>] <agent> <message>"
        );
        assert_eq!(usage("/build", Shape::None, &[]), "/build");
        assert_eq!(
            usage(
                "/skill",
                Shape::Subcommand {
                    values: &["on", "off", "reset"],
                    labels: &["name"],
                    rest: None
                },
                &[]
            ),
            "/skill <on|off|reset> [name]"
        );
    }
}
