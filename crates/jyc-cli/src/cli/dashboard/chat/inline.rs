//! Inline (non-alternate-screen) chat printing.
//!
//! The chat screen does not own the whole terminal: it prints transcript
//! lines above an inline viewport that holds the composer, the `/` popup
//! and the status line. Printed lines land in the terminal's own
//! scrollback, so tmux/native scrolling works without the app tracking a
//! scroll offset (see DESIGN.md "Chat inline viewport").
//!
//! [`InlinePrinter`] owns the inline `Terminal`: `insert_before` prints
//! transcript rows above the viewport (scrolling it down, leaving the rows
//! in scrollback) and `draw` repaints the viewport per frame. Transcript
//! rows are rendered as ratatui `Line` widgets into the pre-viewport
//! buffer, so styling travels through the backend's per-cell SGR — never
//! as hand-built escape strings stuffed into cell text.

use ratatui::backend::Backend;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Widget;
use ratatui::{Terminal, TerminalOptions, Viewport};

/// Prints transcript rows above the inline viewport and redraws the
/// viewport itself.
///
/// Owns the inline `Terminal` for the chat screen's lifetime. All output
/// goes through it: `insert_before` for transcript rows (which scrolls the
/// viewport down and leaves the printed rows in scrollback), `draw` for the
/// viewport repaint. Nothing else in the process writes to stdout while the
/// chat screen is up.
pub(crate) struct InlinePrinter<B: Backend> {
    terminal: Terminal<B>,
    /// Append-only count of transcript rows already printed. The caller
    /// re-derives the full transcript each frame; this count is the diff
    /// baseline. Deliberately not the rows themselves: the derived lines
    /// are cheap to rebuild, and re-wrapping at a new width shifts row
    /// boundaries, so a stored copy would go stale.
    printed: usize,
    /// Width the baseline was measured at. A mismatch (or a shorter
    /// transcript than the baseline) triggers a silent rebase: the next
    /// frame's full transcript reprints from scratch, which keeps the
    /// printed form aligned with what the caller derives — at the cost of
    /// one 50ms frame of delay, never of duplicated scrollback.
    printed_width: u16,
}

impl<B: Backend> InlinePrinter<B> {
    /// Creates the printer with an inline viewport of `viewport_height`
    /// rows anchored at the bottom of the terminal.
    pub(crate) fn new(backend: B, viewport_height: u16) -> Result<Self, B::Error> {
        let terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(viewport_height),
            },
        )?;
        Ok(Self {
            terminal,
            printed: 0,
            printed_width: 0,
        })
    }

    /// Terminal size the viewport currently occupies.
    pub(crate) fn size(&self) -> ratatui::prelude::Size {
        self.terminal.size().unwrap_or(ratatui::prelude::Size::ZERO)
    }

    /// Drops the print baseline entirely — the next `sync` reprints the
    /// full transcript. Called on topic switch, where the row count may
    /// grow (a longer history would otherwise be mistaken for a
    /// continuation and lose its head rows).
    pub(crate) fn reset(&mut self) {
        self.printed = 0;
        self.printed_width = 0;
    }

    /// Prints the transcript rows that appeared since the last sync. The
    /// caller re-derives the full `Vec<Line>` each frame; this method owns
    /// the diff, so no mutation site needs a printer hook.
    ///
    /// - Width change (terminal resize) or a shorter transcript: silent
    ///   rebase — nothing prints this frame; the next frame's full
    ///   transcript prints as one append. Rows already in scrollback keep
    ///   their old wrapping (tmux rewraps its own pane; a bare terminal
    ///   cannot), and no row prints twice.
    /// - Otherwise: the tail past the baseline prints.
    pub(crate) fn sync(&mut self, lines: Vec<Line<'static>>, width: u16) -> Result<(), B::Error> {
        let rebased = width != self.printed_width || lines.len() < self.printed;
        if rebased {
            self.printed_width = width;
            self.printed = 0;
        }
        if lines.len() <= self.printed {
            return Ok(());
        }
        self.push_lines(lines[self.printed..].to_vec())
    }

    /// Appends rendered transcript rows above the viewport. The baseline
    /// advances only after the backend accepted the rows, so a failed print
    /// retries next frame instead of silently skipping them.
    fn push_lines(&mut self, lines: Vec<Line<'static>>) -> Result<(), B::Error> {
        if lines.is_empty() {
            return Ok(());
        }
        let height = lines.len() as u16;
        // insert_before scrolls the viewport down by `height` rows so the
        // printed rows land in scrollback and the viewport stays at the
        // bottom edge.
        self.terminal.insert_before(height, |buf| {
            for (row, line) in lines.iter().enumerate() {
                let area = Rect {
                    x: 0,
                    y: row as u16,
                    width: buf.area.width,
                    height: 1,
                };
                Widget::render(line.clone(), area, buf);
            }
        })?;
        self.printed += lines.len();
        Ok(())
    }

    /// The terminal the key handlers and the editor flow borrow for their
    /// own draws (external editor suspension).
    pub(crate) fn terminal_mut(&mut self) -> &mut Terminal<B> {
        &mut self.terminal
    }

    /// Draws the overview screen on the whole terminal. The alternate
    /// screen has already been entered by the chat→overview switch (see
    /// `App::set_chat_visible`), so ratatui just draws; the inline viewport
    /// is re-anchored by the overview→chat switch.
    pub(crate) fn draw_fullscreen<F>(&mut self, render_fn: F) -> Result<(), B::Error>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        self.terminal.autoresize()?;
        self.terminal.draw(render_fn)?;
        self.terminal.flush()?;
        Ok(())
    }

    /// Redraws the viewport from `render_fn` (composer + popup + status
    /// line, the same widgets the pane renderer used to draw).
    pub(crate) fn draw_viewport<F>(&mut self, render_fn: F) -> Result<(), B::Error>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        self.terminal.draw(render_fn).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;

    /// The transcript rows render as ratatui cells — styles travel as
    /// per-cell SGR through the backend, never as literal text. A row must
    /// not gain a visible `[0m` (or any SGR remnant) from serialization.
    #[test]
    fn sync_renders_styled_rows_without_escape_leak() {
        let backend = TestBackend::new(20, 5);
        let mut printer = InlinePrinter::new(backend, 2).expect("printer");
        printer
            .sync(
                vec![
                    Line::from(Span::styled("red", Style::default().fg(Color::Red))),
                    Line::from("plain"),
                ],
                20,
            )
            .expect("sync");
        let buffer = printer.terminal.backend().buffer().clone();
        for cell in buffer.content() {
            let symbol = cell.symbol();
            assert!(
                !symbol.contains('['),
                "escape remnant leaked into a cell: {symbol:?}"
            );
        }
        // The styled row keeps its style on the cell, not in the text.
        assert_eq!(buffer[(0, 0)].symbol(), "r");
        assert_eq!(buffer[(0, 0)].fg, Color::Red);
        assert_eq!(buffer[(0, 1)].symbol(), "p");
        assert_eq!(buffer[(0, 1)].fg, Color::Reset);
    }

    /// A width change rebases silently: nothing prints on the rebase frame,
    /// and the next frame's full transcript prints once — no duplicated
    /// rows in the scrollback buffer.
    #[test]
    fn width_change_rebases_without_duplicating() {
        let backend = TestBackend::new(20, 5);
        let mut printer = InlinePrinter::new(backend, 2).expect("printer");
        printer
            .sync(vec![Line::from("one"), Line::from("two")], 20)
            .expect("first sync");
        // Rebase frame: narrower terminal, same transcript. The whole
        // transcript prints once at the new width — no duplication.
        printer
            .sync(vec![Line::from("one"), Line::from("two")], 10)
            .expect("rebase sync");
        assert_eq!(printer.printed, 2, "full transcript reprinted once");
        // Steady state at the new width: nothing new prints.
        printer
            .sync(vec![Line::from("one"), Line::from("two")], 10)
            .expect("steady state");
        assert_eq!(printer.printed, 2);
    }

    /// A topic switch (reset) makes the next sync reprint the full new
    /// transcript — a longer history must not lose its head rows to the
    /// continuation fast path.
    #[test]
    fn reset_reprints_the_full_transcript() {
        let backend = TestBackend::new(20, 5);
        let mut printer = InlinePrinter::new(backend, 2).expect("printer");
        printer.sync(vec![Line::from("a")], 20).expect("old topic");
        printer.reset();
        let new_topic: Vec<Line<'static>> = (0..3).map(|i| Line::from(format!("n{i}"))).collect();
        printer.sync(new_topic.clone(), 20).expect("new topic");
        assert_eq!(printer.printed, 3, "all new-topic rows print");
        // And a repeat of the same transcript prints nothing more.
        printer.sync(new_topic, 20).expect("steady state");
        assert_eq!(printer.printed, 3);
    }
}
