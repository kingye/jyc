//! Inline (non-alternate-screen) chat printing.
//!
//! The chat screen does not own the whole terminal: it prints transcript
//! lines above an inline viewport that holds the live furniture — the
//! progress tail (thinking / activity / spinner), the composer, the `/`
//! popup and the status line. Printed lines land in the terminal's own
//! scrollback, so tmux/native scrolling works without the app tracking a
//! scroll offset (see DESIGN.md "Chat inline viewport").
//!
//! The viewport is exactly as tall as its furniture, so the composer sits
//! directly under the last printed transcript row — no dead blank screen
//! between history and input. The furniture's height changes at runtime
//! (a popup opens, the progress tail appears while a round runs, thinking
//! expands), and ratatui's inline `Viewport` height is fixed at
//! construction, so [`InlinePrinter`] recreates its `Terminal` whenever the
//! height changes (see [`InlinePrinter::resize_viewport`]). Recreation is
//! cheap: the terminal's buffers restart empty, so the next draw repaints
//! the viewport rows in full.
//!
//! Transcript rows are rendered as ratatui `Line` widgets into the
//! pre-viewport buffer, so styling travels through the backend's per-cell
//! SGR — never as hand-built escape strings stuffed into cell text.

use ratatui::Terminal;
use ratatui::backend::{Backend, ClearType};
use ratatui::layout::{Position, Rect};
use ratatui::text::Line;
use ratatui::widgets::Widget;

/// Prints transcript rows above the inline viewport and redraws the
/// viewport itself.
///
/// Owns the inline `Terminal` for the chat screen's lifetime. All output
/// goes through it: `insert_before` for transcript rows (which scrolls the
/// viewport down and leaves the printed rows in scrollback), `draw` for the
/// viewport repaint. Nothing else in the process writes to stdout while the
/// chat screen is up.
///
/// `make_terminal` rebuilds the terminal at a new inline viewport height
/// (see [`InlinePrinter::resize_viewport`]); it must return an inline
/// terminal anchored at the *current* cursor position.
/// Rebuilds the inline terminal at a new viewport height (see
/// [`InlinePrinter::resize_viewport`]).
pub(crate) trait TerminalFactory<B: Backend> {
    fn rebuild(&mut self, rows: u16) -> Result<Terminal<B>, B::Error>;
}

impl<B, F> TerminalFactory<B> for F
where
    B: Backend,
    F: FnMut(u16) -> Result<Terminal<B>, B::Error>,
{
    fn rebuild(&mut self, rows: u16) -> Result<Terminal<B>, B::Error> {
        self(rows)
    }
}

pub(crate) struct InlinePrinter<B: Backend> {
    make_terminal: Box<dyn TerminalFactory<B>>,
    terminal: Terminal<B>,
    /// Current inline viewport height (rows of live furniture).
    viewport_height: u16,
    /// Where the viewport sits on the terminal right now. Ratatui keeps
    /// the inline viewport's area private, so the printer tracks the last
    /// drawn frame's area itself — resize math needs the old top/bottom
    /// rows and cannot read them back otherwise. `None` until the first
    /// draw; the fallback in `resize_viewport` covers that frame.
    viewport_area: Option<Rect>,
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
    pub(crate) fn new(
        make_terminal: impl FnMut(u16) -> Result<Terminal<B>, B::Error> + 'static,
        viewport_height: u16,
    ) -> Result<Self, B::Error> {
        let mut make_terminal = Box::new(make_terminal);
        let terminal = make_terminal(viewport_height)?;
        Ok(Self {
            make_terminal,
            terminal,
            viewport_height,
            viewport_area: None,
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

    /// Prints the transcript rows that appeared since the last sync and
    /// grows/shrinks the viewport to `viewport_height` rows of furniture.
    /// The caller re-derives the full transcript each frame; this method
    /// owns both diffs, so no mutation site needs a printer hook.
    ///
    /// - Width change (terminal resize) or a shorter transcript: rebase —
    ///   the baseline resets and this frame's transcript prints in full as
    ///   one append (rows already in scrollback keep their old wrapping;
    ///   tmux rewraps its own pane, a bare terminal cannot). No row prints
    ///   twice because the baseline only ever moves forward on success.
    /// - Otherwise: the tail past the baseline prints.
    pub(crate) fn sync(
        &mut self,
        lines: Vec<Line<'static>>,
        width: u16,
        viewport_height: u16,
    ) -> Result<(), B::Error> {
        self.resize_viewport(viewport_height)?;
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

    /// Re-anchors the inline viewport at `new_height` rows, recreating the
    /// terminal (ratatui fixes the inline viewport height at construction;
    /// `Terminal::resize` recomputes the origin but cannot change it).
    ///
    /// The rebuild goes through `with_options`, which anchors the new
    /// viewport at the backend cursor row and appends the rows below it —
    /// so the cursor is parked deliberately and the old viewport is erased
    /// first, otherwise its glyphs would scroll into the scrollback as
    /// duplicate furniture or linger below the new viewport:
    ///
    /// - Shrink: park at the OLD TOP. The viewport keeps its position, the
    ///   composer moves up to hug the printed transcript, and the cleared
    ///   region below the shorter viewport is absorbed by the append.
    /// - Grow: park at the LAST SCREEN ROW. The appends scroll the screen,
    ///   pushing the transcript tail into the scrollback (where it belongs)
    ///   along with the blanked old viewport — nothing duplicates — and
    ///   the new rows open above the composer, which stays on the screen
    ///   bottom row it already sat on.
    fn resize_viewport(&mut self, new_height: u16) -> Result<(), B::Error> {
        if new_height == self.viewport_height {
            return Ok(());
        }
        let size = self.terminal.size()?;
        let old = self.viewport_area.unwrap_or(Rect {
            x: 0,
            y: size.height.saturating_sub(self.viewport_height),
            width: size.width,
            height: self.viewport_height,
        });
        let growing = new_height > old.height;
        let backend = self.terminal.backend_mut();
        // Blank the old viewport (and anything below it) so neither the
        // scroll nor the abandoned rows can leave stale furniture behind.
        backend.set_cursor_position(Position { x: 0, y: old.y })?;
        backend.clear_region(ClearType::AfterCursor)?;
        backend.set_cursor_position(Position {
            x: 0,
            y: if growing {
                size.height.saturating_sub(1)
            } else {
                old.y
            },
        })?;
        self.terminal = self.make_terminal.rebuild(new_height)?;
        self.viewport_height = new_height;
        // Best guess until the next draw reports the real area — same math
        // as `compute_inline_size` for the parked cursor above.
        self.viewport_area = Some(Rect {
            x: 0,
            y: if growing {
                size.height.saturating_sub(new_height)
            } else {
                old.y
            },
            width: size.width,
            height: new_height.min(size.height),
        });
        Ok(())
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

    /// Redraws the viewport from `render_fn` (live tail + composer + popup
    /// + status line, the same widgets the pane renderer used to draw).
    pub(crate) fn draw_viewport<F>(&mut self, render_fn: F) -> Result<(), B::Error>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        let frame = self.terminal.draw(render_fn)?;
        self.viewport_area = Some(frame.area);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::TerminalOptions;
    use ratatui::Viewport;
    use ratatui::backend::TestBackend;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;

    fn make_printer(width: u16, height: u16, viewport: u16) -> InlinePrinter<TestBackend> {
        InlinePrinter::new(
            move |h| {
                Terminal::with_options(
                    TestBackend::new(width, height),
                    TerminalOptions {
                        viewport: Viewport::Inline(h.min(height)),
                    },
                )
            },
            viewport,
        )
        .expect("printer")
    }

    /// The transcript rows render as ratatui cells — styles travel as
    /// per-cell SGR through the backend, never as literal text. A row must
    /// not gain a visible `[0m` (or any SGR remnant) from serialization.
    #[test]
    fn sync_renders_styled_rows_without_escape_leak() {
        let mut printer = make_printer(20, 5, 2);
        printer
            .sync(
                vec![
                    Line::from(Span::styled("red", Style::default().fg(Color::Red))),
                    Line::from("plain"),
                ],
                20,
                2,
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

    /// A width change rebases: the baseline resets and the same transcript
    /// prints once at the new width — no duplicated rows in the scrollback
    /// buffer.
    #[test]
    fn width_change_rebases_without_duplicating() {
        let mut printer = make_printer(20, 5, 2);
        printer
            .sync(vec![Line::from("one"), Line::from("two")], 20, 2)
            .expect("first sync");
        // Rebase frame: narrower terminal, same transcript. The whole
        // transcript prints once at the new width — no duplication.
        printer
            .sync(vec![Line::from("one"), Line::from("two")], 10, 2)
            .expect("rebase sync");
        assert_eq!(printer.printed, 2, "full transcript reprinted once");
        // Steady state at the new width: nothing new prints.
        printer
            .sync(vec![Line::from("one"), Line::from("two")], 10, 2)
            .expect("steady state");
        assert_eq!(printer.printed, 2);
    }

    /// A topic switch (reset) makes the next sync reprint the full new
    /// transcript — a longer history must not lose its head rows to the
    /// continuation fast path.
    #[test]
    fn reset_reprints_the_full_transcript() {
        let mut printer = make_printer(20, 5, 2);
        printer
            .sync(vec![Line::from("a")], 20, 2)
            .expect("old topic");
        printer.reset();
        let new_topic: Vec<Line<'static>> = (0..3).map(|i| Line::from(format!("n{i}"))).collect();
        printer.sync(new_topic.clone(), 20, 2).expect("new topic");
        assert_eq!(printer.printed, 3, "all new-topic rows print");
        // And a repeat of the same transcript prints nothing more.
        printer.sync(new_topic, 20, 2).expect("steady state");
        assert_eq!(printer.printed, 3);
    }

    /// Growing the viewport re-anchors it to the screen bottom: the parked
    /// cursor sits on the last screen row, so the new rows open above the
    /// composer (which stays on the bottom row it already occupied when
    /// the viewport reached the screen edge).
    #[test]
    fn growing_the_viewport_reanchors_to_the_screen_bottom() {
        let mut printer = make_printer(20, 10, 2);
        // One draw so the printer knows the real viewport position.
        printer.draw_viewport(|_| {}).expect("paint");
        printer.sync(vec![Line::from("hi")], 20, 5).expect("sync");
        let new = printer.viewport_area.expect("area after resize");
        assert_eq!(new.height, 5, "viewport grew to the furniture height");
        assert_eq!(
            new,
            Rect::new(0, 5, 20, 5),
            "bottom-anchored to the screen: the composer keeps the last row"
        );
        assert_eq!(printer.viewport_height, 5);
    }

    /// Shrinking the viewport keeps the top edge where it was (the composer
    /// moves up to hug the printed transcript).
    #[test]
    fn shrinking_the_viewport_keeps_the_top_row() {
        let mut printer = make_printer(20, 10, 6);
        printer.draw_viewport(|_| {}).expect("paint");
        let old = printer.viewport_area.expect("area after draw");
        printer.sync(vec![Line::from("hi")], 20, 3).expect("sync");
        let new = printer.viewport_area.expect("area after resize");
        assert_eq!(new.height, 3);
        assert_eq!(new.top(), old.top(), "the viewport top stays put");
    }
}
