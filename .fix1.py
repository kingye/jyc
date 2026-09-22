import pathlib

# ── F5: use crossterm's own OSC 52 instead of a hand-rolled encoder ───────
p = pathlib.Path("crates/jyc-cli/Cargo.toml")
s = p.read_text()
assert 'crossterm = "0.29"' in s and 'base64 = "0.23"' in s
s = s.replace(
    'crossterm = "0.29"',
    '# `osc52` = the clipboard escape the chat pane\'s yank keys send (see `chat::clipboard`)\ncrossterm = { version = "0.29", features = ["osc52"] }',
)
s = s.replace('base64 = "0.23"\n', "")
p.write_text(s)

pathlib.Path("crates/jyc-cli/src/cli/dashboard/chat/clipboard.rs").write_text(
    '''//! Clipboard writes for the chat pane's yank keys.
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
'''
)


def sub(path, old, new, count=1):
    p = pathlib.Path(path)
    s = p.read_text()
    assert s.count(old) == count, f"{path}: expected {count}, got {s.count(old)}:\n{old[:140]}"
    p.write_text(s.replace(old, new, count))


CHAT = "crates/jyc-cli/src/cli/dashboard/chat/mod.rs"
RENDER = "crates/jyc-cli/src/cli/dashboard/chat/render.rs"

# ── F3: a yank armed before a selection must not survive it ───────────────
sub(
    CHAT,
    """    app.chat.selection_anchor = None;
    app.chat.pending_count = 0;
    let (text, rows) = copy_rows(app, start, end - start + 1);""",
    """    app.chat.selection_anchor = None;
    app.chat.pending_count = 0;
    app.chat.pending_y = false;
    let (text, rows) = copy_rows(app, start, end - start + 1);""",
)

# ── f9: name the ceiling of an absolute-row selection ────────────────────
sub(
    CHAT,
    """    /// copies it or `Esc` drops it; scrolling leaves both ends alone, so a
    /// selection never changes just because the view moved.""",
    """    /// copies it or `Esc` drops it; scrolling leaves both ends alone, so a
    /// selection never changes just because the view moved.
    ///
    /// ponytail: both ends are absolute *row* indices, so a transcript that
    /// reflows underneath (the streaming reply turning into history) can leave
    /// the range on other text — clamped, never past the end. Anchoring to the
    /// text itself is the next step if that ever bites.""",
)

# ── f10: the leader popup should say what the pane actually does ─────────
sub(
    "crates/jyc-cli/src/cli/dashboard/local_commands.rs",
    '''            description: "Focus the message area (j/k move the cursor; typing returns to input)",''',
    '''            description: "Focus the message area (j/k move the cursor, J/K select, y copies; typing returns to input)",''',
)

# ── F2: a selection belongs to the focused pane ─────────────────────────
sub(
    RENDER,
    """    app.chat.cursor_line = app.chat.cursor_line.min(total_lines.saturating_sub(1));""",
    """    app.chat.cursor_line = app.chat.cursor_line.min(total_lines.saturating_sub(1));
    // The selection belongs to the message pane alone. Focus can leave it by
    // routes that do not go through `refocus_input` (`Tab`, a click, the
    // explorer), and a selection left behind is invisible yet still live: it
    // would pin the cursor against `carry_cursor` and could be yanked later.
    if app.chat.focus != ChatFocus::MessageArea {
        app.chat.selection_anchor = None;
    }""",
)

# ── F4: the navy bar has to stay readable on a light theme too ──────────
sub(
    RENDER,
    """/// Background for selected rows (start with `Shift+J`, extend with any movement
/// key): a dim navy, far enough from [`USER_BG`]'s neutral gray to read as a
/// highlight rather than as part of a message, and dark enough to sit under
/// transcript text of any colour. The cursor row keeps [`CURSOR_BG`] even inside
/// a selection, so the end being moved is always distinguishable.""",
    """/// Background for selected rows (start with `Shift+J`, extend with any movement
/// key): a dim navy, far enough from [`USER_BG`]'s neutral gray to read as a
/// highlight rather than as part of a message. The cursor row keeps [`CURSOR_BG`]
/// even inside a selection, so the end being moved is always distinguishable.
///
/// Unlike [`CURSOR_BG`] this cannot be an ANSI name — no theme color is both
/// distinct from the message gray and light-theme safe — so a selected row
/// additionally forces a light foreground (see the paint loop): on a light-theme
/// terminal the default foreground is black, which would vanish on this navy.""",
)
sub(
    RENDER,
    """            let line_no = skip + row;
            let bar = if line_no == cursor {
                Some(CURSOR_BG)
            } else if selection.is_some_and(|(from, to)| line_no >= from && line_no <= to) {
                Some(SELECT_BG)
            } else {
                None
            };
            if let Some(color) = bar {
                paint_row(line, chunks[0].width, color);
            }""",
    """            let line_no = skip + row;
            let bar = if line_no == cursor {
                Style::from(CURSOR_BG)
            } else if selection.is_some_and(|(from, to)| line_no >= from && line_no <= to) {
                Style::from(SELECT_BG).fg(Color::White)
            } else {
                continue;
            };
            paint_row(line, chunks[0].width, bar);""",
)
sub(
    RENDER,
    """fn paint_row(line: &mut Line<'static>, width: u16, color: Color) {
    let bar = Style::default().bg(color);""",
    """fn paint_row(line: &mut Line<'static>, width: u16, bar: Style) {""",
)

print("fixes applied")
