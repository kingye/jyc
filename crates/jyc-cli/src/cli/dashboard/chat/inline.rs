//! Inline (non-alternate-screen) chat printing.
//!
//! The chat screen does not own the whole terminal: it prints transcript
//! lines above an inline viewport that holds the live furniture — the
//! progress tail (activity / live ticker), the composer, the `/`
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

/// Rebuilds the inline terminal at a new viewport height (see
/// [`InlinePrinter::resize_viewport`]); it must return an inline terminal
/// anchored at the *current* cursor position.
type TerminalFactory<B> = Box<dyn FnMut(u16) -> Result<Terminal<B>, <B as Backend>::Error>>;

/// Prints transcript rows above the inline viewport and redraws the
/// viewport itself.
///
/// Owns the inline `Terminal` for the chat screen's lifetime. All output
/// goes through it: `insert_before` for transcript rows (which scrolls the
/// viewport down and leaves the printed rows in scrollback), `draw` for the
/// viewport repaint. Nothing else in the process writes to stdout while the
/// chat screen is up.
///
/// Fullscreen surfaces (the overview screen) render through the same
/// terminal while the alternate screen is active, so they can never touch
/// the printed transcript; `set_chat_visible` performs the switch.
pub(crate) struct InlinePrinter<B: Backend> {
    make_terminal: TerminalFactory<B>,
    /// Enters (`true`) or leaves (`false`) the alternate screen around
    /// fullscreen draws. Injected because the switch is backend-specific
    /// (crossterm commands on the real stdout); a no-op in tests.
    set_alt_screen: Box<dyn FnMut(bool) -> Result<(), B::Error>>,
    terminal: Terminal<B>,
    /// Current inline viewport height (rows of live furniture).
    viewport_height: u16,
    /// Whether the terminal currently serves a fullscreen surface (the
    /// alternate screen is active). `false` = inline chat viewport.
    fullscreen: bool,
    /// Terminal height the fullscreen surface was built at. A terminal
    /// resize while the overview is up does not move a ratatui inline
    /// viewport (only `Terminal::resize` with a new requested height does),
    /// so the fullscreen is rebuilt whenever this and the real size drift.
    fullscreen_height: u16,
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
    /// rows anchored at the bottom of the terminal, in chat mode (the
    /// alternate screen is not active).
    pub(crate) fn new(
        make_terminal: impl FnMut(u16) -> Result<Terminal<B>, B::Error> + 'static,
        set_alt_screen: impl FnMut(bool) -> Result<(), B::Error> + 'static,
        viewport_height: u16,
    ) -> Result<Self, B::Error> {
        let mut make_terminal: TerminalFactory<B> = Box::new(make_terminal);
        let terminal = make_terminal(viewport_height)?;
        Ok(Self {
            make_terminal,
            set_alt_screen: Box::new(set_alt_screen),
            terminal,
            viewport_height,
            fullscreen: false,
            fullscreen_height: 0,
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

    /// Switches between the chat screen and the fullscreen surfaces
    /// (overview). Fullscreen draws happen on the alternate screen — entered
    /// and left through `set_alt_screen` — so they can paint the whole
    /// window without touching the printed transcript underneath. The
    /// inline rebuild re-anchors at the cursor row, so the cursor is parked
    /// at the viewport's top row before entering: leaving the alternate
    /// screen restores it there, and the chat viewport comes back exactly
    /// where it was.
    pub(crate) fn set_chat_visible(&mut self, visible: bool) -> Result<(), B::Error> {
        let want_fullscreen = !visible;
        if self.fullscreen == want_fullscreen {
            return Ok(());
        }
        if want_fullscreen {
            let top = self.viewport_area.map(|a| a.y).unwrap_or_else(|| {
                self.terminal
                    .size()
                    .map(|s| s.height.saturating_sub(self.viewport_height))
                    .unwrap_or(0)
            });
            self.terminal
                .backend_mut()
                .set_cursor_position(Position { x: 0, y: top })?;
            (self.set_alt_screen)(true)?;
            let height = self.terminal.size()?.height.max(1);
            self.terminal = (self.make_terminal)(height)?;
            self.fullscreen = true;
            self.fullscreen_height = height;
        } else {
            (self.set_alt_screen)(false)?;
            self.terminal = (self.make_terminal)(self.viewport_height.max(1))?;
            self.fullscreen = false;
            // Re-reported by the next draw.
            self.viewport_area = None;
        }
        Ok(())
    }

    /// Re-anchors the inline viewport at `new_height` rows, recreating the
    /// terminal (ratatui fixes the inline viewport height at construction;
    /// `Terminal::resize` recomputes the origin but cannot change it).
    ///
    /// The rebuild goes through `with_options`, which anchors the new
    /// viewport at the backend cursor row and appends the rows below it
    /// (`compute_inline_size`): parking at the OLD TOP row makes the append
    /// exactly the height delta, so the screen scrolls only when the
    /// enlarged viewport actually runs past the last row. Parking any lower
    /// (e.g. the last screen row) would scroll `new_height - 1` rows and
    /// push transcript rows into the scrollback that are still on screen,
    /// leaving a blank gap between the transcript tail and the viewport.
    ///
    /// Direction is decided against the previous REQUESTED height, not the
    /// clamped on-screen area: ratatui clamps the drawn area to the
    /// terminal, so comparing against `old.height` would misread a
    /// too-tall request as growth even when nothing on screen changed.
    ///
    /// The old viewport is erased first so neither the scroll nor the
    /// abandoned rows can leave stale furniture behind.
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
        let backend = self.terminal.backend_mut();
        // Blank the old viewport (and anything below it) so neither the
        // scroll nor the abandoned rows can leave stale furniture behind.
        backend.set_cursor_position(Position { x: 0, y: old.y })?;
        backend.clear_region(ClearType::AfterCursor)?;
        backend.set_cursor_position(Position { x: 0, y: old.y })?;
        self.terminal = (self.make_terminal)(new_height)?;
        self.viewport_height = new_height;
        // Best guess until the next draw reports the real area: the append
        // scrolls by the part of `new_height - 1` that does not fit below
        // the parked row, lifting the new top off `old.y` by that much
        // (same math as `compute_inline_size`).
        let scrolled =
            (new_height - 1).saturating_sub(size.height.saturating_sub(1).saturating_sub(old.y));
        self.viewport_area = Some(Rect {
            x: 0,
            y: old.y.saturating_sub(scrolled),
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

    /// Draws a fullscreen surface (the overview) on the whole terminal.
    /// Enters the alternate screen first — see [`Self::set_chat_visible`] —
    /// and follows terminal resizes while it stays up: a ratatui inline
    /// viewport keeps its requested height across `autoresize`, so the
    /// fullscreen is rebuilt at the new height whenever the size drifts.
    /// Deliberately leaves `viewport_area` alone: that field is the *inline*
    /// anchor, and the chat screen's position must survive the overview.
    pub(crate) fn draw_fullscreen<F>(&mut self, render_fn: F) -> Result<(), B::Error>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        self.set_chat_visible(false)?;
        let size = self.terminal.size()?;
        if self.fullscreen_height != size.height.max(1) {
            self.terminal = (self.make_terminal)(size.height.max(1))?;
            self.fullscreen_height = size.height.max(1);
        }
        self.terminal.autoresize()?;
        // No extra flush here: `draw` already swaps buffers, and a bare
        // `Terminal::flush` would replay the reverse diff and erase the
        // frame just drawn.
        self.terminal.draw(render_fn)?;
        Ok(())
    }

    /// Draws the current screen, routing on the active mode: chat into the
    /// inline viewport, overview fullscreen on the alternate screen.
    pub(crate) fn draw<F>(&mut self, render_fn: F) -> Result<(), B::Error>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        if self.fullscreen {
            self.draw_fullscreen(render_fn)
        } else {
            self.draw_viewport(render_fn)
        }
    }

    /// Redraws the viewport from `render_fn` (live tail + thinking +
    /// composer + popup + status line). Leaves the alternate screen first
    /// if a fullscreen draw happened since the last viewport draw.
    pub(crate) fn draw_viewport<F>(&mut self, render_fn: F) -> Result<(), B::Error>
    where
        F: FnOnce(&mut ratatui::Frame),
    {
        self.set_chat_visible(true)?;
        // `Frame::area` is the viewport; the `CompletedFrame`'s area is the
        // whole terminal — using it here would grow `viewport_area` to the
        // full screen and break every anchor calculation downstream.
        let mut area = Rect::default();
        self.terminal.draw(|f| {
            area = f.area();
            render_fn(f);
        })?;
        self.viewport_area = Some(area);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::TerminalOptions;
    use ratatui::Viewport;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Cell;
    use ratatui::style::{Color, Style};
    use ratatui::text::Span;
    use std::cell::RefCell;
    use std::io;
    use std::rc::Rc;

    fn no_alt(_on: bool) -> io::Result<()> {
        Ok(())
    }

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
            |_: bool| -> Result<(), std::convert::Infallible> { Ok(()) },
            viewport,
        )
        .expect("printer")
    }

    /// Wraps a shared `TestBackend` so terminal recreations keep the
    /// on-screen contents — the stock per-rebuild factory loses them,
    /// hiding scroll bugs that only exist across a rebuild. Counts
    /// `append_lines` calls to prove growth goes through the physical
    /// scroll path instead of `insert_before` (which would reprint the
    /// composer over the transcript).
    #[derive(Clone)]
    struct CountingBackend {
        inner: Rc<RefCell<TestBackend>>,
        appended: Rc<RefCell<u16>>,
    }

    impl CountingBackend {
        fn map_err(e: std::convert::Infallible) -> io::Error {
            match e {}
        }
    }

    impl Backend for CountingBackend {
        type Error = io::Error;

        fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
        where
            I: Iterator<Item = (u16, u16, &'a Cell)>,
        {
            self.inner.borrow_mut().draw(content).map_err(Self::map_err)
        }

        fn append_lines(&mut self, n: u16) -> io::Result<()> {
            *self.appended.borrow_mut() += n;
            self.inner
                .borrow_mut()
                .append_lines(n)
                .map_err(Self::map_err)
        }

        fn hide_cursor(&mut self) -> io::Result<()> {
            self.inner.borrow_mut().hide_cursor().map_err(Self::map_err)
        }

        fn show_cursor(&mut self) -> io::Result<()> {
            self.inner.borrow_mut().show_cursor().map_err(Self::map_err)
        }

        fn get_cursor_position(&mut self) -> io::Result<Position> {
            self.inner
                .borrow_mut()
                .get_cursor_position()
                .map_err(Self::map_err)
        }

        fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
            self.inner
                .borrow_mut()
                .set_cursor_position(position)
                .map_err(Self::map_err)
        }

        fn clear(&mut self) -> io::Result<()> {
            self.inner.borrow_mut().clear().map_err(Self::map_err)
        }

        fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
            self.inner
                .borrow_mut()
                .clear_region(clear_type)
                .map_err(Self::map_err)
        }

        fn size(&self) -> io::Result<ratatui::layout::Size> {
            self.inner.borrow().size().map_err(Self::map_err)
        }

        fn window_size(&mut self) -> io::Result<ratatui::backend::WindowSize> {
            self.inner.borrow_mut().window_size().map_err(Self::map_err)
        }

        fn scroll_region_up(
            &mut self,
            region: core::ops::Range<u16>,
            scroll_by: u16,
        ) -> io::Result<()> {
            self.inner
                .borrow_mut()
                .scroll_region_up(region, scroll_by)
                .map_err(Self::map_err)
        }

        fn scroll_region_down(
            &mut self,
            region: core::ops::Range<u16>,
            scroll_by: u16,
        ) -> io::Result<()> {
            self.inner
                .borrow_mut()
                .scroll_region_down(region, scroll_by)
                .map_err(Self::map_err)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.borrow_mut().flush().map_err(Self::map_err)
        }
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

    /// Growing the viewport parks the cursor at the OLD TOP row: the
    /// rebuild appends only the height delta, so a viewport that was
    /// bottom-anchored stays bottom-anchored (a 2-row viewport at the
    /// bottom of a 10-row screen grows to rows 5..9) and one that had
    /// room below simply grows downward.
    #[test]
    fn growing_the_viewport_keeps_the_top_row() {
        let mut printer = make_printer(20, 10, 2);
        // One draw so the printer knows the real viewport position.
        printer.draw_viewport(|_| {}).expect("paint");
        printer.sync(vec![Line::from("hi")], 20, 5).expect("sync");
        let new = printer.viewport_area.expect("area after resize");
        assert_eq!(new.height, 5, "viewport grew to the furniture height");
        assert_eq!(new, Rect::new(0, 0, 20, 5), "grew down from the old top");
        assert_eq!(printer.viewport_height, 5);
    }

    /// Growing WITHOUT new transcript rows must not scroll the transcript
    /// away from the viewport: the append is exactly the height delta, so
    /// the last printed row stays directly above the grown viewport — no
    /// blank gap, no transcript rows pushed into the scrollback. Runs on a
    /// backend that preserves screen contents across terminal recreation.
    #[test]
    fn growing_without_new_transcript_leaves_no_gap() {
        let appended = Rc::new(RefCell::new(0u16));
        let shared = CountingBackend {
            inner: Rc::new(RefCell::new(TestBackend::new(20, 10))),
            appended: appended.clone(),
        };
        let mk = {
            let shared = shared.clone();
            move |h: u16| {
                Terminal::with_options(
                    shared.clone(),
                    TerminalOptions {
                        viewport: Viewport::Inline(h.min(10)),
                    },
                )
            }
        };
        let mut printer = InlinePrinter::new(mk, no_alt, 2).expect("printer");
        // Print nine rows, one per frame: each insert walks the viewport
        // down a row until it is bottom-anchored (the production shape).
        let mut rows: Vec<Line<'static>> = Vec::new();
        for i in 0..9 {
            rows.push(Line::from(format!("r{i}")));
            printer.sync(rows.clone(), 20, 2).expect("sync");
        }
        printer.draw_viewport(|_| {}).expect("paint");
        assert_eq!(
            printer.viewport_area.expect("area"),
            Rect::new(0, 8, 20, 2),
            "bottom-anchored once the transcript fills the screen"
        );
        let appends_before_growth = *appended.borrow();
        // Grow the furniture with NO new transcript this frame.
        printer.sync(rows, 20, 5).expect("grow");
        assert_eq!(
            printer.viewport_area.expect("area after grow"),
            Rect::new(0, 5, 20, 5),
            "grew up off the bottom by exactly the height delta"
        );
        let backend = shared.inner.borrow();
        assert_eq!(
            backend.buffer()[(0, 4)].symbol(),
            "r",
            "the transcript tail sits directly above the grown viewport — no blank gap"
        );
        assert_eq!(
            backend.buffer()[(0, 6)].symbol(),
            " ",
            "the grown viewport's middle row is blank (rows 5 and 9 hold the furniture)"
        );
        assert_eq!(
            *appended.borrow() - appends_before_growth,
            4,
            "the growth itself appended the parked cursor row plus the height delta (3), not a full reprint"
        );
    }

    /// Production order on the overview → chat round trip: the mode is
    /// synced BEFORE the transcript sync and the draw, so the transcript
    /// and the viewport both target the primary screen — not the alternate
    /// screen the overview was drawn into.
    #[test]
    fn returning_to_chat_syncs_before_the_viewport_draw() {
        let alt_calls = Rc::new(RefCell::new(Vec::new()));
        let mut printer = InlinePrinter::new(
            |h: u16| {
                Terminal::with_options(
                    TestBackend::new(20, 10),
                    TerminalOptions {
                        viewport: Viewport::Inline(h.min(10)),
                    },
                )
            },
            {
                let alt_calls = alt_calls.clone();
                move |on| {
                    alt_calls.borrow_mut().push(on);
                    Ok(())
                }
            },
            2,
        )
        .expect("printer");
        printer.draw_viewport(|_| {}).expect("paint");
        printer.draw_fullscreen(|_| {}).expect("overview");
        assert_eq!(alt_calls.borrow().as_slice(), &[true]);
        // The dashboard loop: set_chat_visible comes first, then the
        // transcript sync, then the routed draw.
        printer.set_chat_visible(true).expect("mode sync");
        printer.sync(vec![Line::from("back")], 20, 3).expect("sync");
        printer.draw(|_| {}).expect("draw");
        assert_eq!(
            alt_calls.borrow().as_slice(),
            &[true, false],
            "no alt-screen churn from the return path"
        );
        assert!(
            !printer.fullscreen,
            "the draw routed into the inline viewport"
        );
        // One transcript row above the composer: the viewport sits directly
        // under it (inline semantics — the screen below stays blank until
        // the transcript fills the screen and starts scrolling).
        assert_eq!(
            printer.viewport_area.expect("area"),
            Rect::new(0, 1, 20, 3),
            "the chat viewport hugs the transcript tail, one row down"
        );
    }

    /// A fullscreen (overview) draw enters the alternate screen and rebuilds
    /// the terminal at the full window height; the next viewport draw leaves
    /// the alternate screen and comes back at the furniture height.
    #[test]
    fn fullscreen_round_trip_switches_alt_screen_and_heights() {
        let alt_calls = Rc::new(RefCell::new(Vec::new()));
        let mut printer = InlinePrinter::new(
            |h| {
                Terminal::with_options(
                    TestBackend::new(20, 10),
                    TerminalOptions {
                        viewport: Viewport::Inline(h.min(10)),
                    },
                )
            },
            {
                let alt_calls = alt_calls.clone();
                move |on| {
                    alt_calls.borrow_mut().push(on);
                    Ok(())
                }
            },
            2,
        )
        .expect("printer");
        printer.draw_viewport(|_| {}).expect("paint");
        printer
            .draw_fullscreen(|f| {
                let area = f.area();
                f.render_widget(ratatui::widgets::Paragraph::new("ov"), area);
            })
            .expect("fullscreen");
        assert_eq!(
            alt_calls.borrow().as_slice(),
            &[true],
            "entering the overview enters the alternate screen"
        );
        assert_eq!(
            printer.terminal.backend().buffer()[(0, 0)].symbol(),
            "o",
            "the overview paints at the top of the full window"
        );
        printer.draw_viewport(|_| {}).expect("paint again");
        assert_eq!(
            alt_calls.borrow().as_slice(),
            &[true, false],
            "returning to chat leaves the alternate screen"
        );
        assert_eq!(
            printer.viewport_area.expect("area"),
            Rect::new(0, 0, 20, 2),
            "back at the furniture height, re-anchored at the cursor"
        );
        // Steady state: no further alt-screen churn.
        printer.draw_viewport(|_| {}).expect("paint steady");
        assert_eq!(alt_calls.borrow().as_slice(), &[true, false]);
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
