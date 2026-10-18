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
//! one thing this module does word is the five refusals the shape itself can
//! raise: a spelling it does not know, a value that did not arrive, a repeat the
//! declaration forbids, a value handed to a flag that takes none, and a word too
//! many. Those used to be written per command, and several of them did not exist
//! at all — `/close --froce` was silently read as "no guard given".
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
    /// Choices the row *declares* are not spelled here: [`Syntax::args_groups`]
    /// reads them from [`Syntax::args`], so a label names only what the
    /// declaration cannot see (`/model`'s `model`).
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
    ///
    /// The set itself is not here. It is [`Syntax::args`], the one declaration
    /// that also carries what the popup offers, so a word cannot be advertised
    /// in the usage line and missing from the popup (or the reverse).
    Subcommand {
        /// Further positional words, after the subcommand.
        labels: &'static [&'static str],
        /// What the text after those is called, when there is any.
        rest: Option<&'static str>,
    },
}

/// A command's argument syntax: how the parser reads its line, and what its
/// first argument level offers.
///
/// The two halves answer the same sentence for different readers — the parser
/// needs to know how many words have room, the usage line needs to name the
/// choices, the popup needs those choices with a description each. They are
/// bound together because they are one declaration, and split across two files
/// they drift: `/context`'s usage line once read `<full|sliding|reset|dump>`
/// while the popup offered `<sliding|full|reset|dump>`, and a guard test was
/// written to catch exactly that.
#[derive(Clone, Copy, Debug)]
pub struct Syntax {
    /// How the parser reads the line.
    pub shape: Shape,
    /// What the first argument level offers.
    pub args: ArgSource,
    /// Usage-line words the popup cannot offer: patterns (`YYYY-MM`) and
    /// values the popup replaces with better choices (`today` becomes this
    /// month's dates). May not name a value [`ArgSource::Declared`] offers —
    /// that overlap is what makes it a second copy, and a test refuses it.
    pub usage_words: &'static [&'static str],
}

impl Syntax {
    /// No arguments at all: the shape of a command that takes flags only.
    pub const EMPTY: Syntax = Syntax {
        shape: Shape::None,
        args: ArgSource::Declared(&[]),
        usage_words: &[],
    };

    /// The value words the usage line lists, in order: the ones only the line
    /// knows, then the ones the popup offers.
    pub fn usage_keys(&self) -> Vec<&'static str> {
        let declared = self.args.declared_keys();
        self.usage_words
            .iter()
            .copied()
            .chain(declared.iter().map(|arg| arg.value))
            .collect()
    }

    /// The words a [`Shape::Args`] level names, one entry per group the line
    /// renders in brackets.
    ///
    /// A declared value is offered in the slot the shape's label names, so the
    /// line reads it from `args` and the label keeps naming only what it cannot
    /// see (`/model`'s `model`). Writing it into the label instead — the
    /// `model|reset` this replaced — is the second copy `args` exists to delete.
    fn args_groups(&self, labels: &[&'static str]) -> Vec<String> {
        let declared: Vec<&str> = self
            .args
            .declared_keys()
            .iter()
            .map(|arg| arg.value)
            .collect();
        if declared.is_empty() {
            return labels.iter().map(|label| label.to_string()).collect();
        }
        let mut words: Vec<&str> = labels.to_vec();
        words.extend(declared);
        vec![words.join("|")]
    }

    /// Resolve the first level against per-topic state.
    pub fn resolve(&self, ctx: &super::args::ArgCtx) -> Vec<jyc_types::CommandArg> {
        self.args.resolve(ctx)
    }
}

/// Where a command's first argument level comes from.
#[derive(Clone, Copy, Debug)]
pub enum ArgSource {
    /// A declared spine, whose values may in turn open dynamic levels. A
    /// `Shape::Subcommand` level is always this or [`Self::Mixed`]: its usage
    /// line names the same values the popup offers.
    Declared(&'static [ArgDef]),
    /// A static spine with one dynamic run spliced into it: `/model`'s names
    /// ahead of `reset`, `/bill`'s months ahead of `all`. The run contributes no
    /// declared key, so the usage line names the slot by its shape's label.
    Mixed(
        &'static [ArgDef],
        fn(&super::args::ArgCtx) -> Vec<jyc_types::CommandArg>,
        &'static [ArgDef],
    ),
    /// A level only per-topic state can produce, down to its keys (`/send`'s
    /// agents, `/close`'s subagents, a command's flag rows). The usage line
    /// cannot see it, so such a command takes its slot from its shape's label.
    Provided(fn(&super::args::ArgCtx) -> Vec<jyc_types::CommandArg>),
}

impl ArgSource {
    /// The declared values, in the order they are offered. Empty for a
    /// [`Self::Provided`] level.
    pub fn declared_keys(&self) -> Vec<&'static ArgDef> {
        match self {
            Self::Declared(args) => args.iter().collect(),
            Self::Mixed(before, _, after) => before.iter().chain(after.iter()).collect(),
            Self::Provided(_) => Vec::new(),
        }
    }

    /// The level itself, with every dynamic part filled in.
    pub fn resolve(&self, ctx: &super::args::ArgCtx) -> Vec<jyc_types::CommandArg> {
        match self {
            Self::Declared(args) => args.iter().flat_map(|arg| arg.resolve(ctx)).collect(),
            Self::Mixed(before, provide, after) => {
                let mut out: Vec<jyc_types::CommandArg> =
                    before.iter().flat_map(|arg| arg.resolve(ctx)).collect();
                out.extend(provide(ctx));
                out.extend(after.iter().flat_map(|arg| arg.resolve(ctx)));
                out
            }
            Self::Provided(provide) => provide(ctx),
        }
    }
}

/// One value a command's first argument level offers.
///
/// `description` is popup-only text; `value` is what the command is given.
#[derive(Clone, Copy, Debug)]
pub struct ArgDef {
    /// The word the command takes.
    pub value: &'static str,
    /// What the popup says about it.
    pub description: &'static str,
    /// What it opens, if anything.
    pub level: ArgLevel,
}

impl ArgDef {
    /// A value with nothing behind it.
    pub const fn leaf(value: &'static str, description: &'static str) -> ArgDef {
        ArgDef {
            value,
            description,
            level: ArgLevel::Leaf,
        }
    }

    /// A value that opens a fixed set of further values.
    pub const fn opening(
        value: &'static str,
        description: &'static str,
        next: &'static [ArgDef],
    ) -> ArgDef {
        ArgDef {
            value,
            description,
            level: ArgLevel::Static(next),
        }
    }

    /// A value that opens values read from per-topic state at resolve time.
    ///
    /// The usage line cannot see these, so a command whose choices are only
    /// dynamic declares a [`Syntax::usage_words`] placeholder for the slot
    /// instead.
    pub const fn dynamic(
        value: &'static str,
        description: &'static str,
        provide: fn(&super::args::ArgCtx) -> Vec<jyc_types::CommandArg>,
    ) -> ArgDef {
        ArgDef {
            value,
            description,
            level: ArgLevel::Dynamic(provide),
        }
    }

    /// This value alone, or everything behind it, resolved.
    pub fn resolve(&self, ctx: &super::args::ArgCtx) -> Vec<jyc_types::CommandArg> {
        let next = match self.level {
            ArgLevel::Leaf => Vec::new(),
            ArgLevel::Static(next) => next.iter().flat_map(|arg| arg.resolve(ctx)).collect(),
            ArgLevel::Dynamic(provide) => provide(ctx),
        };
        vec![jyc_types::CommandArg {
            value: self.value.to_string(),
            description: self.description.to_string(),
            args: next,
        }]
    }
}

/// What a value opens behind it.
#[derive(Clone, Copy, Debug)]
pub enum ArgLevel {
    /// Nothing.
    Leaf,
    /// A further declared set, resolved the same way.
    Static(&'static [ArgDef]),
    /// Values only per-topic state knows (`/model`'s models, `/bill`'s months,
    /// a flag set projected from [`super::flags`]).
    Dynamic(fn(&super::args::ArgCtx) -> Vec<jyc_types::CommandArg>),
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
        /// What to add in parentheses: the flags that did exist
        /// (`known: --force, --purge`), or — for a command that declares none —
        /// the words its shape expects, since a dash-led typo there is likelier
        /// a mistyped value than a mistyped flag. Empty when there is neither.
        hint: String,
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
            Self::UnknownFlag { spelling, hint } if hint.is_empty() => {
                write!(f, "unknown flag {spelling}")
            }
            Self::UnknownFlag { spelling, hint } => {
                write!(f, "unknown flag {spelling} ({hint})")
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

/// Read `args` through `syntax` and the command's flag declarations.
///
/// Every word is accounted for: a declared flag is recorded with its value, a
/// dash-word that is no flag is refused, a word the shape has room for is
/// positional, and one past that room is refused rather than dropped.
pub fn parse<F: Copy + PartialEq>(
    syntax: Syntax,
    flags: &[(F, FlagSpec)],
    args: &[String],
) -> Result<Parsed<F>, ArgError> {
    let mut parsed = Parsed {
        flags: Vec::new(),
        positionals: Vec::new(),
        body: None,
    };
    let shape = syntax.shape;
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

        // A lone `-` is a word where the shape keeps text (`/send joe -` sends a
        // dash, and `/spawn -n -` takes one as a value) and a dash-word
        // anywhere else — `/grant -` is a typo, not a path.
        if looks_like_a_flag(arg) || (arg == "-" && !keeps_text) {
            return Err(ArgError::UnknownFlag {
                spelling: arg.to_string(),
                hint: unknown_hint(flags, syntax),
            });
        }

        if parsed.positionals.len() < slots {
            parsed.positionals.push(arg.to_string());
        } else if keeps_text {
            in_text = true;
            parsed.push_text(arg);
        } else {
            return Err(ArgError::TooMany {
                expected: syntax.expected(),
            });
        }
    }

    Ok(parsed)
}

/// Read `name`'s command line: the syntax its row declares, and the flag
/// declarations its handler owns.
///
/// This is what a handler calls — the syntax is not repeated in the handler, so
/// the line it accepts is the line the popup offers.
pub fn parse_command<F: Copy + PartialEq>(
    name: &str,
    flags: &[(F, FlagSpec)],
    args: &[String],
) -> Result<Parsed<F>, ArgError> {
    parse(super::builtin::syntax_of(name), flags, args)
}

/// Read a command line for a command that declares no flags of its own.
///
/// `/ungrant` takes a path and nothing else, so it has no typed set to name —
/// but it still reads its line through the same syntax, which is what refuses a
/// dash-word nobody declared.
pub fn parse_plain(name: &str, args: &[String]) -> Result<Parsed<()>, ArgError> {
    parse_with(super::builtin::syntax_of(name), args)
}

/// Read a command line through a syntax the caller already holds, for a handler
/// that reads one declaration for the commands it serves.
pub fn parse_with(syntax: Syntax, args: &[String]) -> Result<Parsed<()>, ArgError> {
    parse::<()>(syntax, &[], args)
}

/// The usage line for a command: its name, what its syntax accepts, and its
/// flags.
///
/// This is the one place `/close [subagent|all] [--force] [--purge]` is written
/// down. The command's own description, `/?` and the popup all read it, so the
/// line cannot say one thing where the parser does another.
pub fn usage(name: &str, syntax: Syntax, specs: &[&FlagSpec]) -> String {
    let flags: Vec<String> = specs
        .iter()
        .map(|spec| match spec.value {
            Some(value) => format!("[--{} <{value}>]", spec.long),
            None => format!("[--{}]", spec.long),
        })
        .collect();
    let mut parts = vec![name.to_string()];
    let shape = syntax.shape;
    // Where the shape keeps trailing text, a flag is read only until that text
    // starts — so the flags come first, or the line would advertise a flag the
    // parser reads as text.
    if shape.keeps_text() {
        parts.extend(flags.iter().cloned());
    }
    match shape {
        Shape::None => {}
        Shape::Args { labels } => {
            // The declared values join the slot the label names — one
            // declaration read twice, not a word written down again.
            parts.extend(
                syntax
                    .args_groups(labels)
                    .into_iter()
                    .map(|group| format!("[{group}]")),
            );
        }
        Shape::ThenBody { label, body } => {
            parts.push(format!("<{label}>"));
            // The parser takes `--` here to end the flags, for text that starts
            // with a dash — so the line that advertises the text advertises it.
            parts.push("[--]".to_string());
            parts.push(format!("<{body}>"));
        }
        Shape::Subcommand { labels, rest } => {
            // The value set is the popup's own, not a second copy of it.
            parts.push(format!("<{}>", syntax.usage_keys().join("|")));
            parts.extend(labels.iter().map(|label| format!("[{label}]")));
            // No `[--]` here even where `rest` exists (`/backlog`, `/context`):
            // neither reads its line through this module, so the marker would
            // advertise a separator they treat as an ordinary word. The line
            // cannot say one thing where the parser does another.
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

/// What the refusal for a dash-word appends: the flags that existed, or — for a
/// command that declares none — the words its shape expects.
///
/// Without the second case a dash-led typo at `/thinking` (which has no flags)
/// would read `unknown flag -x` and say nothing about `show`/`hide`; before this
/// change that command answered by naming them.
fn unknown_hint<F>(flags: &[(F, FlagSpec)], syntax: Syntax) -> String {
    if !flags.is_empty() {
        return format!("known: {}", known_list(flags));
    }
    let expected = syntax.expected();
    if expected.is_empty() {
        String::new()
    } else {
        format!("expected: {expected}")
    }
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
}

impl Syntax {
    /// What this command had room for, for the refusal that says so: its
    /// positional labels, prefixed by the value set its shape offers.
    pub fn expected(&self) -> String {
        let mut parts = Vec::new();
        if matches!(self.shape, Shape::Subcommand { .. }) {
            parts.push(self.usage_keys().join("|"));
        }
        if let Shape::Subcommand { labels, .. } = self.shape {
            parts.extend(labels.iter().map(|label| label.to_string()));
        }
        if let Shape::Args { labels } = self.shape {
            // The same projection the usage line renders, so the refusal names
            // the values the popup offers (`/model a b` expects `model|reset`).
            parts.extend(self.args_groups(labels));
        }
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::flags::{
        CLOSE_FLAGS, CloseFlag, SEND_FLAGS, SPAWN_FLAGS, SendFlag, SpawnFlag,
    };

    /// The syntax `/close` declares on its table row.
    const CLOSE_ARGS: Syntax = Syntax {
        shape: Shape::Args {
            labels: &["subagent|all"],
        },
        args: ArgSource::Declared(&[]),
        usage_words: &[],
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
        let none = parse::<CloseFlag>(Syntax::EMPTY, &[], &args(&["-x"])).unwrap_err();
        assert_eq!(none.to_string(), "unknown flag -x");
    }

    #[test]
    fn a_lone_dash_is_a_word_only_where_text_is_kept() {
        // `/send`'s shape keeps text, so there the dash is a message word.
        const SEND: Syntax = Syntax {
            shape: Shape::ThenBody {
                label: "agent",
                body: "message",
            },
            args: ArgSource::Declared(&[]),
            usage_words: &[],
        };
        let parsed = parse(SEND, SEND_FLAGS, &args(&["joe", "-"])).unwrap();
        assert_eq!(parsed.body.as_deref(), Some("-"));
        // Where the shape keeps none it is a dash-word nobody declared: before
        // the shape was declared, `/grant -` quietly granted `<topic>/-`.
        assert_eq!(
            parse_close(&["-"]).unwrap_err().to_string(),
            "unknown flag - (known: --force, --purge)"
        );
        // `--` is a dash-word everywhere it is not the text separator.
        assert_eq!(
            parse_close(&["--"]).unwrap_err().to_string(),
            "unknown flag -- (known: --force, --purge)"
        );
    }

    /// A command that declares no flags names what its shape expects, rather
    /// than staying silent or listing flags that do not exist.
    #[test]
    fn an_unknown_flag_falls_back_to_what_the_shape_expects() {
        // The hint names the values from the row's own declaration — the same
        // list the popup offers, so it cannot name a word that is not there.
        const THINKING: Syntax = Syntax {
            shape: Shape::Subcommand {
                labels: &[],
                rest: None,
            },
            args: ArgSource::Declared(&[
                ArgDef::leaf("show", "Show reasoning content"),
                ArgDef::leaf("hide", "Hide reasoning content"),
            ]),
            usage_words: &[],
        };
        assert_eq!(
            parse::<CloseFlag>(THINKING, &[], &args(&["-x"]))
                .unwrap_err()
                .to_string(),
            "unknown flag -x (expected: show|hide)"
        );
        // With nothing to expect either, it says only what it saw.
        assert_eq!(
            parse::<CloseFlag>(Syntax::EMPTY, &[], &args(&["-x"]))
                .unwrap_err()
                .to_string(),
            "unknown flag -x"
        );
        // A real row reaches the same arm: `/thinking` declares no flag, so its
        // refusal names the words it does take rather than nothing at all.
        assert_eq!(
            parse_plain("/thinking", &args(&["-x"]))
                .unwrap_err()
                .to_string(),
            "unknown flag -x (expected: show|hide)"
        );
    }

    /// `/spawn`'s declarations: `-n` takes a value, and asking for it twice is
    /// refused where a repeat of the switches is not.
    #[test]
    fn a_value_takes_the_next_word_and_a_once_flag_refuses_a_repeat() {
        const SHAPE: Syntax = Syntax {
            shape: Shape::Args { labels: &["type"] },
            args: ArgSource::Declared(&[]),
            usage_words: &[],
        };
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
        const SHAPE: Syntax = Syntax {
            shape: Shape::ThenBody {
                label: "agent",
                body: "message",
            },
            args: ArgSource::Declared(&[]),
            usage_words: &[],
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
            parse::<CloseFlag>(Syntax::EMPTY, &[], &args(&["joe"]))
                .unwrap_err()
                .to_string(),
            "too many arguments: this command takes none"
        );
    }

    #[test]
    fn a_subcommand_records_its_word_and_what_follows() {
        const SHAPE: Syntax = Syntax {
            shape: Shape::Subcommand {
                labels: &["name"],
                rest: None,
            },
            args: ArgSource::Declared(&[
                ArgDef::leaf("on", ""),
                ArgDef::leaf("off", ""),
                ArgDef::leaf("reset", ""),
            ]),
            usage_words: &[],
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

    /// The usage lines the *rows* declare — the same strings the popup and `/?`
    /// show, so this pins the real declaration rather than a copy of it.
    #[test]
    fn usage_reads_the_rows_shape_and_flags() {
        use crate::command::builtin::usage_of;
        assert_eq!(
            usage_of("/close"),
            "/close [subagent|all] [--force] [--purge]"
        );
        assert_eq!(
            usage_of("/send"),
            "/send [--topic <topic>] [--reply] [--attach <file>] [--recipient <id>] <agent> [--] <message>"
        );
        assert_eq!(usage_of("/build"), "/build");
        assert_eq!(usage_of("/skill"), "/skill <on|off|reset> [name]");
        // The declared values are folded into the shape's label, so these two
        // lines read exactly as they did when the label spelled them out.
        assert_eq!(usage_of("/model"), "/model [model|reset]");
        assert_eq!(usage_of("/template"), "/template [name|update]");
        // No `[--]`: `/backlog` does not read its line through the shape, so it
        // has no separator to advertise.
        assert_eq!(
            usage_of("/backlog"),
            "/backlog <push|list|ls|get|pop|rm|set> [text]"
        );
    }
}
