//! Clipboard writes for the chat pane's yank keys.
//!
//! The TUI runs *inside* the user's terminal, which for a remote session is the
//! other end of an SSH connection. A local clipboard library would therefore
//! copy into the *host's* clipboard, not the one you paste from, so yanking asks
//! the terminal itself to do the copying via OSC 52 — the same channel the
//! mouse-mode escape hatch (see `render`'s `mouse_seq`) relies on.
//!
//! The write cannot happen in a key handler: those run inside `terminal.draw`,
//! holding the terminal borrow, and `print!` would fight the TUI for stdout. So
//! a handler queues a [`ClipboardRequest`] on `App::pending_clipboard` and the
//! event loop flushes it once per iteration through [`apply`] — the only place
//! that touches stdout for this.

use base64::Engine;

/// A clipboard write a key handler asked for, waiting for the event loop.
pub struct ClipboardRequest {
    /// The text to place on the clipboard.
    pub text: String,
    /// Ask for append/paste rather than a replace. Not every terminal honours
    /// it; those that do not simply replace.
    pub append: bool,
}

/// The OSC 52 sequence that sets the clipboard to `text`.
///
/// The payload is the UTF-8 bytes base64-encoded — the escape has no other
/// framing, so a raw newline inside would end the sequence early — and the whole
/// thing is BEL-terminated, which every terminal accepts.
pub fn osc52(text: &str, append: bool) -> String {
    let payload = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    // "c" selects the clipboard register, "?" asks the terminal to add to what
    // is already there instead of replacing it.
    let sel = if append { "?" } else { "c" };
    format!("\x1b]52;{sel};{payload}\x07")
}

/// Send `req` to the terminal's clipboard.
///
/// Nothing is reported on failure: when stdout is gone there is no channel back
/// to the user, and a terminal that does not implement OSC 52 accepts the
/// sequence and ignores it.
pub fn apply(req: &ClipboardRequest) {
    use std::io::Write;
    print!("{}", osc52(&req.text, req.append));
    let _ = std::io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The framing is what a terminal parses, so pin the exact bytes of an ASCII
    /// yank: selector, base64 payload, BEL.
    #[test]
    fn encodes_the_clipboard_escape() {
        assert_eq!(osc52("hi", false), "\u{1b}]52;c;aGk=\u{7}");
        assert!(osc52("hi", true).starts_with("\u{1b}]52;?;"));
    }

    /// Yanked chat lines are full of box glyphs and CJK; both must survive the
    /// round trip, and no raw control byte may leak into the sequence.
    #[test]
    fn utf8_and_newlines_survive_the_payload() {
        let text = "a\nb\tc \u{2192} 中文";
        let seq = osc52(text, false);
        let payload = seq
            .strip_prefix("\u{1b}]52;c;")
            .and_then(|s| s.strip_suffix('\u{7}'))
            .expect("framed sequence");
        assert!(!payload.contains(['\n', '\t', '\u{1b}']));
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(payload)
            .expect("base64 payload");
        assert_eq!(String::from_utf8(decoded).unwrap(), text);
    }
}
