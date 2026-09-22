//! Clipboard writes for the chat pane's yank keys.
//!
//! The TUI runs *inside* the user's terminal, which for a remote session is the
//! other end of an SSH connection. A local clipboard library would therefore copy
//! into the *host's* clipboard, not the one you paste from, so yanking asks the
//! terminal itself to do the copying — OSC 52, which is exactly what crossterm's
//! [`CopyToClipboard`] emits (the same channel `render`'s mouse-mode escape hatch
//! relies on, and it brings its own base64 behind the `osc52` feature).
//!
//! The write cannot happen in a key handler: those run inside `terminal.draw`,
//! holding the terminal borrow, and stdout belongs to the TUI. So a handler
//! queues the text on `App::pending_clipboard` and the event loop flushes it once
//! per iteration through [`apply`] — the only place here that touches stdout.

use crossterm::{clipboard::CopyToClipboard, execute};

/// Send `text` to the clipboard through the terminal.
///
/// Nothing is reported on failure: when stdout is gone there is no channel back
/// to the user, and a terminal that does not implement OSC 52 accepts the
/// sequence and ignores it.
pub fn apply(text: &str) {
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, CopyToClipboard::to_clipboard_from(text));
}
