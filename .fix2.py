import pathlib


def sub(path, old, new, count=1):
    p = pathlib.Path(path)
    s = p.read_text()
    assert s.count(old) == count, f"{path}: expected {count}, got {s.count(old)}:\n{old[:140]}"
    p.write_text(s.replace(old, new, count))


# The painter's contract changed with the selection foreground.
sub(
    "crates/jyc-cli/src/cli/dashboard/chat/render.rs",
    """/// Only the background is replaced: each span keeps its foreground and
/// modifiers, so the text under the bar still reads. The row is padded out with""",
    """/// `bar` says what the row gives up: the cursor overrides the background only, so
/// the text under it keeps its own colour, while a selected row also overrides
/// the foreground (it has to stay legible on the navy — see [`SELECT_BG`]). The
/// row is padded out with""",
)

# The two coverage gaps the review named.
sub(
    "crates/jyc-cli/src/cli/dashboard/chat/tests/mod.rs",
    """/// Selected rows get their own bar and the cursor row keeps the solid one inside
/// the range, so both the selection and where it is being moved are readable.""",
    """/// A yank armed *before* a selection must not outlive it: `y` + `J` + `y` copies
/// the selection and spends the arm, so the next `y` starts a fresh pair instead
/// of firing the stale one and silently yanking a second time.
#[test]
fn an_armed_yank_does_not_survive_a_selection() {
    let mut app = cursor_app();
    move_up(&mut app, 3);
    press(&mut app, 'y');
    press_shift(&mut app, 'J');
    press(&mut app, 'y');

    let copied = app
        .chat
        .pending_clipboard
        .take()
        .expect("a queued clipboard write");
    assert_eq!(copied.lines().count(), 2, "the two selected rows");
    assert_eq!(status(&app), "Copied 2 lines");
    assert!(!app.chat.pending_y, "the arm is spent with the selection");

    press(&mut app, 'y');
    assert!(
        app.chat.pending_clipboard.is_none(),
        "the next `y` arms, it does not copy"
    );
}

/// At the end of the transcript there is nowhere to extend, so the selection is
/// the single row under the cursor — and the status line counts it as one line.
#[test]
fn a_one_row_selection_reports_a_single_line() {
    let mut app = cursor_app();
    let last = app.chat.cursor_line;

    press_shift(&mut app, 'J');
    assert_eq!(app.chat.selection_range(), Some((last, last)));
    assert_eq!(status(&app), "1 line selected");

    press(&mut app, 'y');
    assert_eq!(status(&app), "Copied 1 line");
}

/// Selected rows get their own bar and the cursor row keeps the solid one inside
/// the range, so both the selection and where it is being moved are readable.""",
)

# The new bar style is what the tests assert.
sub(
    "crates/jyc-cli/src/cli/dashboard/chat/tests/mod.rs",
    """    use super::render::{CURSOR_BG, SELECT_BG};""",
    """    use ratatui::style::{Color, Style};

    use super::render::{CURSOR_BG, SELECT_BG};""",
)
sub(
    "crates/jyc-cli/src/cli/dashboard/chat/tests/mod.rs",
    """    assert_eq!(bg_at(cursor - 1), Some(SELECT_BG));
    assert_eq!(bg_at(cursor - 2), Some(SELECT_BG), "the anchor end too");""",
    """    let selected = Some(Style::from(SELECT_BG).fg(Color::White));
    assert_eq!(bg_at(cursor - 1), selected);
    assert_eq!(bg_at(cursor - 2), selected, "the anchor end too");""",
)
print("patch2 ok")
