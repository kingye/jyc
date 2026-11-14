//! Inline (non-alternate-screen) chat printing.
//!
//! The chat screen does not own the whole terminal: it prints transcript
//! lines above an inline viewport that holds the composer, the `/` popup
//! and the status line. Printed lines land in the terminal's own
//! scrollback, so tmux/native scrolling works without the app tracking a
//! scroll offset (see DESIGN.md "Chat inline viewport").
//!
//! Split in two:
//! - [`lines_to_ansi`] is a pure `Line -> String` converter (unit-tested;
//!   no terminal access).
//! - [`InlinePrinter`] owns the inline `Terminal`, prints transcript rows
//!   above it with `insert_before`, and redraws the viewport per frame.

use std::io::Write;

use ratatui::backend::Backend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::{Terminal, TerminalOptions, Viewport};

/// Converts one rendered row into an ANSI string that reproduces its text
/// and style. Padding and wide glyphs are the caller's concern — this is a
/// straight cell-to-escape transcription of the spans themselves, in order.
///
/// Per span: emit the SGR delta from the previously-seen style, then the
/// text. No reset at line end — the next span's SGR (or the caller's reset)
/// adjusts from here, which keeps the output minimal.
pub(crate) fn span_to_ansi(out: &mut String, style: Style, text: &str) {
    // fg: named colors first (RGB emits a truecolor escape; both are fine
    // for tmux, which passes truecolor through by default).
    match style.fg {
        Some(Color::Reset) | None => {}
        Some(c) => push_sgr_fg(out, c),
    }
    match style.bg {
        Some(Color::Reset) | None => {}
        Some(c) => push_sgr_bg(out, c),
    }
    if style.add_modifier.contains(Modifier::BOLD) {
        out.push_str("\x1b[1m");
    }
    if style.add_modifier.contains(Modifier::DIM) {
        out.push_str("\x1b[2m");
    }
    if style.add_modifier.contains(Modifier::ITALIC) {
        out.push_str("\x1b[3m");
    }
    if style.add_modifier.contains(Modifier::UNDERLINED) {
        out.push_str("\x1b[4m");
    }
    if style.add_modifier.contains(Modifier::REVERSED) {
        out.push_str("\x1b[7m");
    }
    if style.add_modifier.contains(Modifier::CROSSED_OUT) {
        out.push_str("\x1b[9m");
    }
    out.push_str(text);
}

fn push_sgr_fg(out: &mut String, c: Color) {
    use Color::*;
    match c {
        Black => out.push_str("\x1b[30m"),
        Red => out.push_str("\x1b[31m"),
        Green => out.push_str("\x1b[32m"),
        Yellow => out.push_str("\x1b[33m"),
        Blue => out.push_str("\x1b[34m"),
        Magenta => out.push_str("\x1b[35m"),
        Cyan => out.push_str("\x1b[36m"),
        Gray => out.push_str("\x1b[37m"),
        DarkGray => out.push_str("\x1b[90m"),
        LightRed => out.push_str("\x1b[91m"),
        LightGreen => out.push_str("\x1b[92m"),
        LightYellow => out.push_str("\x1b[93m"),
        LightBlue => out.push_str("\x1b[94m"),
        LightMagenta => out.push_str("\x1b[95m"),
        LightCyan => out.push_str("\x1b[96m"),
        White => out.push_str("\x1b[97m"),
        Indexed(i) => {
            let _ = std::fmt::write(out, format_args!("\x1b[38;5;{i}m"));
        }
        Rgb(r, g, b) => {
            let _ = std::fmt::write(out, format_args!("\x1b[38;2;{r};{g};{b}m"));
        }
        _ => {}
    }
}

fn push_sgr_bg(out: &mut String, c: Color) {
    use Color::*;
    match c {
        Black => out.push_str("\x1b[40m"),
        Red => out.push_str("\x1b[41m"),
        Green => out.push_str("\x1b[42m"),
        Yellow => out.push_str("\x1b[43m"),
        Blue => out.push_str("\x1b[44m"),
        Magenta => out.push_str("\x1b[45m"),
        Cyan => out.push_str("\x1b[46m"),
        Gray => out.push_str("\x1b[47m"),
        DarkGray => out.push_str("\x1b[100m"),
        LightRed => out.push_str("\x1b[101m"),
        LightGreen => out.push_str("\x1b[102m"),
        LightYellow => out.push_str("\x1b[103m"),
        LightBlue => out.push_str("\x1b[104m"),
        LightMagenta => out.push_str("\x1b[105m"),
        LightCyan => out.push_str("\x1b[106m"),
        White => out.push_str("\x1b[107m"),
        Indexed(i) => {
            let _ = std::fmt::write(out, format_args!("\x1b[48;5;{i}m"));
        }
        Rgb(r, g, b) => {
            let _ = std::fmt::write(out, format_args!("\x1b[48;2;{r};{g};{b}m"));
        }
        _ => {}
    }
}

/// Converts rendered rows into the exact strings to print. Each returned
/// line is at most `width` display columns; the caller prints them joined
/// with `\n`.
pub(crate) fn lines_to_ansi(lines: &[Line<'_>], _width: u16) -> Vec<String> {
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let mut s = String::new();
        for span in &line.spans {
            span_to_ansi(&mut s, span.style, span.content.as_ref());
        }
        // End-of-line style reset: styles must not bleed into the next
        // printed row (ratatui diffs each frame from scratch; a hand
        // printout has no such baseline).
        s.push_str("\x1b[0m");
        out.push(s);
    }
    out
}

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
    /// Append-only transcript already printed, with the width it was
    /// wrapped at. Kept so a viewport rebuild (terminal resize) can replay
    /// the transcript at the new width instead of re-deriving it from
    /// `ChatState::messages` — the printed form is authoritative for what
    /// the user has seen.
    printed: Vec<()>,
    /// Width the transcript rows were last wrapped/printed at.
    printed_width: u16,
}

impl<B: Backend<Error = std::io::Error>> InlinePrinter<B> {
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
            printed: Vec::new(),
            printed_width: 0,
        })
    }

    /// Terminal size the viewport currently occupies.
    pub(crate) fn size(&self) -> ratatui::prelude::Size {
        self.terminal.size().unwrap_or(ratatui::prelude::Size::ZERO)
    }

    /// Prints the transcript rows that appeared since the last sync. The
    /// caller re-derives the full `Vec<Line>` each frame (the render cache
    /// makes it cheap); this method owns the diff, so no mutation site
    /// needs a printer hook.
    ///
    /// - A width change (terminal resize) replays the whole transcript at
    ///   the new width.
    /// - A shorter transcript (topic switch cleared `messages`) just
    ///   re-baselines: the caller seeds the new topic's transcript right
    ///   after, and those rows print as an append.
    pub(crate) fn sync(&mut self, lines: Vec<Line<'static>>, width: u16) -> Result<(), B::Error> {
        if width != self.printed_width {
            self.printed_width = width;
            let replay = lines;
            self.printed.clear();
            return self.push_lines(replay, width);
        }
        if lines.len() < self.printed.len() {
            self.printed.clear();
            return Ok(());
        }
        let fresh = lines.len() - self.printed.len();
        if fresh == 0 {
            return Ok(());
        }
        let tail: Vec<Line<'static>> = lines[self.printed.len()..].to_vec();
        self.printed.resize(self.printed.len() + tail.len(), ());
        let ansi = lines_to_ansi(&tail, width);
        let height = ansi.len() as u16;
        self.terminal.insert_before(height, |buf| {
            for (row, s) in ansi.iter().enumerate() {
                let area = ratatui::prelude::Rect {
                    x: 0,
                    y: row as u16,
                    width,
                    height: 1,
                };
                let cell = ratatui::text::Span::raw(s.clone());
                let line = ratatui::text::Line::from(cell);
                ratatui::widgets::Widget::render(line, area, buf);
            }
        })
    }

    /// The terminal the key handlers and the editor flow borrow for their
    /// own draws (external editor suspension, leader popups).
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

    /// Appends rendered transcript rows above the viewport, recording them
    /// for resize replay. Rows must already be wrapped to `width`; the
    /// caller owns that (it also owns the viewport-height math, which reads
    /// the same width).
    fn push_lines(&mut self, lines: Vec<Line<'static>>, width: u16) -> Result<(), B::Error> {
        if lines.is_empty() {
            return Ok(());
        }
        self.printed.resize(self.printed.len() + lines.len(), ());
        self.printed_width = width;
        let ansi = lines_to_ansi(&lines, width);
        // insert_before scrolls the viewport down by `ansi.len()` rows so
        // the printed rows land in scrollback and the viewport stays at the
        // bottom edge.
        let height = ansi.len() as u16;
        self.terminal.insert_before(height, |buf| {
            for (row, s) in ansi.iter().enumerate() {
                let area = ratatui::prelude::Rect {
                    x: 0,
                    y: row as u16,
                    width,
                    height: 1,
                };
                // Write the ANSI string verbatim: the buffer is only used
                // for its position bookkeeping here; the styled text is
                // what the backend prints.
                let _ = area;
                let cell = ratatui::text::Span::raw(s.clone());
                let line = ratatui::text::Line::from(cell);
                ratatui::widgets::Widget::render(line, area, buf);
            }
        })
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

/// Writes the ANSI strings for transcript rows straight to `writer`, one
/// per line — the non-ratatui path used when the chat screen prints without
/// an inline viewport (kept so `lines_to_ansi` has a consumer that does not
/// need a `Terminal`).
#[allow(dead_code)]
pub(crate) fn write_lines<W: Write>(
    writer: &mut W,
    lines: &[Line<'_>],
    width: u16,
) -> std::io::Result<()> {
    for s in lines_to_ansi(lines, width) {
        writeln!(writer, "{s}")?;
    }
    writer.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    #[test]
    fn lines_to_ansi_plain_text_passes_through_with_reset() {
        let lines = vec![Line::from("hello")];
        let out = lines_to_ansi(&lines, 10);
        assert_eq!(out, vec!["hello\x1b[0m"]);
    }

    #[test]
    fn lines_to_ansi_fg_color_emits_sgr() {
        let lines = vec![Line::from(Span::styled(
            "red",
            Style::default().fg(Color::Red),
        ))];
        let out = lines_to_ansi(&lines, 10);
        assert_eq!(out, vec!["\x1b[31mred\x1b[0m"]);
    }

    #[test]
    fn lines_to_ansi_rgb_bg_emits_truecolor() {
        let style = Style::default().bg(Color::Rgb(0x39, 0x35, 0x52));
        let lines = vec![Line::from(Span::styled("x", style))];
        let out = lines_to_ansi(&lines, 10);
        assert_eq!(out, vec!["\x1b[48;2;57;53;82mx\x1b[0m"]);
    }

    #[test]
    fn lines_to_ansi_modifier_emits_sgr() {
        let style = Style::default()
            .fg(Color::Gray)
            .add_modifier(Modifier::ITALIC);
        let lines = vec![Line::from(Span::styled("i", style))];
        let out = lines_to_ansi(&lines, 10);
        assert_eq!(out, vec!["\x1b[37m\x1b[3mi\x1b[0m"]);
    }

    #[test]
    fn lines_to_ansi_two_spans_share_one_row() {
        let lines = vec![Line::from(vec![
            Span::styled("a", Style::default().fg(Color::Red)),
            Span::raw("b"),
        ])];
        let out = lines_to_ansi(&lines, 10);
        assert_eq!(out, vec!["\x1b[31mab\x1b[0m"]);
    }

    #[test]
    fn span_to_ansi_indexed_color() {
        let mut s = String::new();
        span_to_ansi(&mut s, Style::default().fg(Color::Indexed(7)), "t");
        assert_eq!(s, "\x1b[38;5;7mt");
    }
}
