//! The word splitter every command line goes through.
//!
//! A line is split into words once, before any parser sees it, and two readers
//! share the result: dispatch ([`super::registry::CommandRegistry::process_commands`])
//! and the send-path scan ([`super::registry::leading_command_names`]). They
//! must agree — a spelling dispatch reads as `/grant` but the scan does not is
//! a message the model can use to grant itself a path — so both call [`split`]
//! rather than each splitting on whitespace their own way.
//!
//! Quoting exists so a word can carry what whitespace would otherwise end:
//!
//! ```text
//! /grant "/home/user/My Files" --once
//! ```
//!
//! The rules are deliberately smaller than POSIX:
//!
//! - `"` and `'` both quote. Inside a quote every character is literal,
//!   including the other quote, `#`, `\` and a newline.
//! - Adjacent runs join into one word: `ab"c d"e` is the single word `abc de`.
//! - An unclosed quote is an error, never a literal. Silently keeping the `"`
//!   would run the command with a word nobody typed.
//! - `#` is not a comment and `\` is not an escape. A POSIX shell reads
//!   `/send bob ping #1045` as three words, dropping the issue number, and it
//!   reads `C:\tmp` as `C:tmp`; this splitter leaves both alone.
//! - An empty word is no word: `""` yields nothing, so no handler receives an
//!   argument that is the empty string (what `split_whitespace` guaranteed
//!   before quoting existed).

use std::fmt;

/// A line that cannot be split: it ends inside a quote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineError {
    /// The quote that was left open — `"` or `'`.
    pub quote: char,
}

impl fmt::Display for LineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "unclosed `{}` quote", self.quote)
    }
}

impl std::error::Error for LineError {}

/// Split one command line into words, removing one level of quoting.
///
/// Returns [`LineError`] when the line ends inside a quote.
pub fn split(line: &str) -> Result<Vec<String>, LineError> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut open: Option<char> = None;

    for ch in line.chars() {
        match open {
            Some(quote) if ch == quote => open = None,
            Some(_) => push(&mut word, ch),
            None if ch == '"' || ch == '\'' => {
                open = Some(ch);
                // `""` is an empty word, which `flush` then drops — opening the
                // word here keeps `ab""cd` one word instead of two.
                word.get_or_insert_with(String::new);
            }
            None if ch.is_whitespace() => flush(&mut word, &mut words),
            None => push(&mut word, ch),
        }
    }

    if let Some(quote) = open {
        return Err(LineError { quote });
    }
    flush(&mut word, &mut words);
    Ok(words)
}

/// The line that spells `word` as exactly one word, or `None` when no line can.
///
/// A word needs no quotes unless it holds whitespace or a quote character —
/// every other character (including `#` and `\`) is literal — so a plain path
/// comes back unchanged, which is what keeps every existing `/grant` option
/// byte-identical.
///
/// `None` means the word cannot be written: the empty string, which [`split`]
/// never yields, and a word holding both quote kinds, which no quoting can
/// carry.
pub fn spell(word: &str) -> Option<String> {
    if word.is_empty() {
        return None;
    }
    if !word
        .chars()
        .any(|ch| ch.is_whitespace() || ch == '"' || ch == '\'')
    {
        return Some(word.to_string());
    }
    let quote = if !word.contains('"') {
        '"'
    } else if !word.contains('\'') {
        '\''
    } else {
        return None;
    };
    Some(format!("{quote}{word}{quote}"))
}

fn push(word: &mut Option<String>, ch: char) {
    word.get_or_insert_with(String::new).push(ch);
}

fn flush(word: &mut Option<String>, words: &mut Vec<String>) {
    if let Some(word) = word.take()
        && !word.is_empty()
    {
        words.push(word);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        split(line).unwrap()
    }

    #[test]
    fn plain_words_split_on_whitespace() {
        assert_eq!(
            words("/grant /srv/data --once"),
            ["/grant", "/srv/data", "--once"]
        );
        assert_eq!(words("  a\tb\nc  "), ["a", "b", "c"]);
        assert!(words("").is_empty());
        assert!(words("   ").is_empty());
    }

    /// The case quoting exists for: the path `/grant` used to refuse.
    #[test]
    fn a_quoted_word_keeps_its_spaces() {
        assert_eq!(
            words(r#"/grant "/home/user/My Files" --once"#),
            ["/grant", "/home/user/My Files", "--once"]
        );
        assert_eq!(
            words(r#"/grant '/home/user/My Files'"#),
            ["/grant", "/home/user/My Files"]
        );
    }

    /// Quotes group characters, they do not have to wrap the whole word.
    #[test]
    fn neighbouring_runs_join_into_one_word() {
        assert_eq!(words(r#"ab"c d"e"#), ["abc de"]);
        assert_eq!(words(r#"a'b c'"d e""#), ["ab cd e"]);
        assert_eq!(words(r#"--flag="a b""#), ["--flag=a b"]);
    }

    /// Inside one quote the other quote is literal — so every word is spellable
    /// unless it holds both.
    #[test]
    fn the_other_quote_is_literal_inside() {
        assert_eq!(words(r#"a"b'c"d"#), ["ab'cd"]);
        assert_eq!(words(r#"a'b"c'd"#), [r#"ab"cd"#]);
    }

    /// The deliberate difference from POSIX: `#` is a word, `\` is a character.
    #[test]
    fn hash_and_backslash_are_not_special() {
        assert_eq!(
            words("/send bob ping #1045"),
            ["/send", "bob", "ping", "#1045"]
        );
        assert_eq!(words(r"/send bob C:\tmp\x"), ["/send", "bob", r"C:\tmp\x"]);
        assert_eq!(words(r#"a "b # c" d"#), ["a", "b # c", "d"]);
    }

    #[test]
    fn an_empty_word_is_no_word() {
        assert!(words(r#""""#).is_empty());
        assert_eq!(words(r#"a "" b"#), ["a", "b"]);
    }

    /// An unclosed quote is refused naming the quote that was left open, rather
    /// than running the command with a word nothing delimited.
    #[test]
    fn an_unclosed_quote_is_an_error() {
        assert_eq!(split(r#"a "b c"#), Err(LineError { quote: '"' }));
        assert_eq!(split("a 'b"), Err(LineError { quote: '\'' }));
        assert!(split(r#""a" 'b"#).is_err());
        assert_eq!(
            split(r#""a"#).unwrap_err().to_string(),
            "unclosed `\"` quote"
        );
    }

    #[test]
    fn spell_leaves_a_plain_word_alone() {
        assert_eq!(spell("/srv/data").as_deref(), Some("/srv/data"));
        assert_eq!(spell("/a#b").as_deref(), Some("/a#b"));
        assert_eq!(spell(r"/a\b").as_deref(), Some(r"/a\b"));
    }

    #[test]
    fn spell_quotes_what_the_splitter_would_break() {
        assert_eq!(spell("/My Files").as_deref(), Some("\"/My Files\""));
        assert_eq!(spell("/a\"b").as_deref(), Some("'/a\"b'"));
        assert_eq!(spell("/a'b").as_deref(), Some("\"/a'b\""));
        assert_eq!(spell("/a \"b'").as_deref(), None);
        assert_eq!(spell(""), None);
    }

    /// What [`spell`] produces, [`split`] reads back as the one word it was
    /// given — the pair has to be inverse or an option the user is handed will
    /// name a path they did not pick.
    #[test]
    fn every_spelled_word_splits_back_to_itself() {
        for word in [
            "/srv/data",
            "/My Files/notes.txt",
            "/a 'b' c",
            "/a \"b\" c",
            "/a#b\\c",
            "  leading and trailing  ",
            "tab\there",
        ] {
            let spelled = spell(word).unwrap_or_else(|| panic!("{word:?} must be spellable"));
            assert_eq!(split(&spelled).unwrap(), [word], "{spelled:?}");
        }
    }

    /// shlex is the POSIX reference, used as an oracle rather than as the
    /// implementation: the two must agree on everything this splitter accepts,
    /// and disagree only where POSIX would silently change the message (`#`
    /// comments, `\` escapes) — the differences the tests above pin.
    ///
    /// Inputs stay inside the shared ground: spaces/tabs/newlines for
    /// whitespace (shlex does not know the rest of Unicode), no `#`, no `\`,
    /// and empty words filtered out (shlex keeps them, [`split`] drops them).
    #[test]
    fn the_posix_reference_agrees_where_the_syntax_overlaps() {
        let cases = [
            "",
            "   ",
            "/grant /srv/data --once",
            r#"/grant "/srv/data with space""#,
            r#"'/srv/data with space' --once"#,
            r#"ab"c d"e"#,
            r#"a'b c'd"#,
            "  leading   and\ttrailing  ",
            "line one\nline two",
        ];
        for line in cases {
            let theirs: Vec<String> = shlex::split(line)
                .unwrap_or_else(|| panic!("shlex rejects {line:?}"))
                .into_iter()
                .filter(|word| !word.is_empty())
                .collect();
            assert_eq!(split(line).unwrap(), theirs, "{line:?}");
        }
    }

    /// The divergence is deliberate, so it is asserted: POSIX reads a
    /// word-leading `#` as a comment and eats the rest of the line.
    #[test]
    fn the_posix_reference_disagrees_on_a_hash() {
        assert_eq!(
            shlex::split("/send bob ping #1045").unwrap(),
            ["/send", "bob", "ping"],
            "shlex drops the issue number"
        );
        assert_eq!(
            split("/send bob ping #1045").unwrap(),
            ["/send", "bob", "ping", "#1045"],
            "the message keeps it"
        );
    }
}
