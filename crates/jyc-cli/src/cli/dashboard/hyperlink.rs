//! Terminal backend that emits OSC 8 hyperlinks for visible URLs.
//!
//! Ratatui (0.30) has no hyperlink support, so this module implements the
//! [`Backend`] trait directly on top of crossterm commands: plain cells are
//! emitted exactly like `ratatui-crossterm` does (per-call style tracking,
//! trailing SGR reset), while rows containing a URL are re-emitted in full
//! with `\x1b]8;;url\x1b\\` … `\x1b]8;;\x1b\\` sequences wrapped around each
//! URL span. Terminals without OSC 8 support silently ignore the sequences;
//! tmux ≥ 3.4 passes them through.
//!
//! A shadow grid of the last drawn cells is kept so that a *partial* diff
//! touching a URL row still re-emits the whole row — OSC 8 link boundaries
//! must always cover the complete URL, otherwise previously linked cells
//! would keep stale link state from earlier frames.

use std::collections::BTreeSet;
use std::io::{self, Write};

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::{
    Attribute as CrosstermAttribute, Color as CrosstermColor, Colors, Print, SetAttribute,
    SetColors,
};
use crossterm::terminal::{self, Clear};
use crossterm::{execute, queue};
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};
use ratatui::style::{Color, Modifier};

/// OSC 8 hyperlink terminator (closes the current link).
const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";

/// A crossterm-based [`Backend`] that wraps visible URLs in OSC 8 hyperlinks.
pub(crate) struct HyperlinkBackend<W: Write> {
    writer: W,
    /// Shadow copy of the terminal grid, updated from the diff stream.
    grid: Vec<Vec<Cell>>,
}

impl<W: Write> HyperlinkBackend<W> {
    /// Creates a backend writing to `writer`.
    pub(crate) fn new(writer: W) -> Self {
        Self {
            writer,
            grid: Vec::new(),
        }
    }

    /// Stores a cell in the shadow grid, growing it as needed.
    fn set_cell(&mut self, x: u16, y: u16, cell: &Cell) {
        let (x, y) = (x as usize, y as usize);
        if self.grid.len() <= y {
            self.grid.resize(y + 1, Vec::new());
        }
        let row = &mut self.grid[y];
        if row.len() <= x {
            row.resize(x + 1, Cell::default());
        }
        row[x] = cell.clone();
    }

    /// Reconstructs the visible text of a shadow-grid row. Continuation
    /// cells of wide graphemes have empty symbols and contribute nothing.
    fn row_text(row: &[Cell]) -> String {
        row.iter().map(Cell::symbol).collect()
    }

    /// Emits cells in non-URL rows, mirroring `ratatui-crossterm`'s `draw`:
    /// per-call absolute style tracking, MoveTo on gaps, trailing SGR reset.
    fn emit_plain_cells(&mut self, cells: &[(u16, u16, Cell)]) -> io::Result<()> {
        let mut last_pos: Option<(u16, u16)> = None;
        let mut current = (Color::Reset, Color::Reset, Modifier::empty());
        for (x, y, cell) in cells {
            if !matches!(last_pos, Some((lx, ly)) if *x == lx + 1 && *y == ly) {
                queue!(self.writer, MoveTo(*x, *y))?;
            }
            last_pos = Some((*x, *y));
            let style = (cell.fg, cell.bg, cell.modifier);
            if style != current {
                queue_style(&mut self.writer, current, style)?;
                current = style;
            }
            queue!(self.writer, Print(cell.symbol()))?;
        }
        Ok(())
    }

    /// Re-emits a full row with OSC 8 sequences around each URL span.
    /// Full-row emission keeps link boundaries correct for partial diffs.
    fn emit_link_row(&mut self, y: u16) -> io::Result<()> {
        let row = &self.grid[y as usize];
        let text = Self::row_text(row);
        let spans = find_url_spans(&text);

        // Map each column to the URL span covering it (byte-offset based;
        // empty-symbol continuation cells never intersect ASCII URL spans).
        let mut link_at: Vec<Option<usize>> = vec![None; row.len()];
        let mut byte_pos = 0usize;
        for (x, cell) in row.iter().enumerate() {
            let start = byte_pos;
            byte_pos += cell.symbol().len();
            if start == byte_pos {
                continue;
            }
            for (i, &(s, e)) in spans.iter().enumerate() {
                if start < e && byte_pos > s {
                    link_at[x] = Some(i);
                }
            }
        }

        queue!(self.writer, MoveTo(0, y))?;
        let mut current = (Color::Reset, Color::Reset, Modifier::empty());
        let mut open_link: Option<usize> = None;
        for (x, cell) in row.iter().enumerate() {
            let link = link_at[x];
            if link != open_link {
                if open_link.is_some() {
                    self.writer.write_all(OSC8_CLOSE.as_bytes())?;
                }
                if let Some(i) = link {
                    let (s, e) = spans[i];
                    write!(self.writer, "\x1b]8;;{}\x1b\\", &text[s..e])?;
                }
                open_link = link;
            }
            let style = (cell.fg, cell.bg, cell.modifier);
            if style != current {
                queue_style(&mut self.writer, current, style)?;
                current = style;
            }
            queue!(self.writer, Print(cell.symbol()))?;
        }
        if open_link.is_some() {
            self.writer.write_all(OSC8_CLOSE.as_bytes())?;
        }
        Ok(())
    }
}

impl<W: Write> Backend for HyperlinkBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let updates: Vec<(u16, u16, Cell)> = content.map(|(x, y, c)| (x, y, c.clone())).collect();
        for (x, y, cell) in &updates {
            self.set_cell(*x, *y, cell);
        }

        // Rows whose shadow text contains a URL get a full-row re-emission;
        // everything else is emitted as a minimal diff like upstream.
        let mut link_rows = BTreeSet::new();
        for (_, y, _) in &updates {
            let has_url = self
                .grid
                .get(*y as usize)
                .map(|row| Self::row_text(row).contains("http"))
                .unwrap_or(false);
            if has_url {
                link_rows.insert(*y);
            }
        }

        let plain: Vec<(u16, u16, Cell)> = updates
            .into_iter()
            .filter(|(_, y, _)| !link_rows.contains(y))
            .collect();
        self.emit_plain_cells(&plain)?;
        for y in link_rows {
            self.emit_link_row(y)?;
        }

        // Trailing reset, isomorphic with ratatui-crossterm's draw tail.
        queue!(
            self.writer,
            SetColors(Colors::new(CrosstermColor::Reset, CrosstermColor::Reset)),
            SetAttribute(CrosstermAttribute::Reset),
        )
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        execute!(self.writer, Hide)
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        execute!(self.writer, Show)
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        crossterm::cursor::position()
            .map(|(x, y)| Position { x, y })
            .map_err(io::Error::other)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        let Position { x, y } = position.into();
        execute!(self.writer, MoveTo(x, y))
    }

    fn clear(&mut self) -> io::Result<()> {
        self.grid.clear();
        self.clear_region(ClearType::All)
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        if matches!(clear_type, ClearType::All) {
            self.grid.clear();
        }
        execute!(
            self.writer,
            Clear(match clear_type {
                ClearType::All => terminal::ClearType::All,
                ClearType::AfterCursor => terminal::ClearType::FromCursorDown,
                ClearType::BeforeCursor => terminal::ClearType::FromCursorUp,
                ClearType::CurrentLine => terminal::ClearType::CurrentLine,
                ClearType::UntilNewLine => terminal::ClearType::UntilNewLine,
            })
        )
    }

    fn size(&self) -> io::Result<Size> {
        let (width, height) = terminal::size()?;
        Ok(Size { width, height })
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        let terminal::WindowSize {
            columns,
            rows,
            width,
            height,
        } = terminal::window_size()?;
        Ok(WindowSize {
            columns_rows: Size {
                width: columns,
                height: rows,
            },
            pixels: Size { width, height },
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// Emits an absolute style change: any modifier change goes through a full
/// SGR reset (which also clears colors) followed by colors and the wanted
/// attribute bits; color-only changes emit just the colors.
fn queue_style<W: Write>(
    w: &mut W,
    from: (Color, Color, Modifier),
    to: (Color, Color, Modifier),
) -> io::Result<()> {
    if to.2 != from.2 {
        queue!(w, SetAttribute(CrosstermAttribute::Reset))?;
        queue!(
            w,
            SetColors(Colors::new(to_crossterm(to.0), to_crossterm(to.1)))
        )?;
        const BITS: [(Modifier, CrosstermAttribute); 9] = [
            (Modifier::BOLD, CrosstermAttribute::Bold),
            (Modifier::DIM, CrosstermAttribute::Dim),
            (Modifier::ITALIC, CrosstermAttribute::Italic),
            (Modifier::UNDERLINED, CrosstermAttribute::Underlined),
            (Modifier::SLOW_BLINK, CrosstermAttribute::SlowBlink),
            (Modifier::RAPID_BLINK, CrosstermAttribute::RapidBlink),
            (Modifier::REVERSED, CrosstermAttribute::Reverse),
            (Modifier::HIDDEN, CrosstermAttribute::Hidden),
            (Modifier::CROSSED_OUT, CrosstermAttribute::CrossedOut),
        ];
        for (bit, attr) in BITS {
            if to.2.contains(bit) {
                queue!(w, SetAttribute(attr))?;
            }
        }
    } else if to.0 != from.0 || to.1 != from.1 {
        queue!(
            w,
            SetColors(Colors::new(to_crossterm(to.0), to_crossterm(to.1)))
        )?;
    }
    Ok(())
}

/// Maps a ratatui color to its crossterm equivalent (same mapping as
/// `ratatui-crossterm`'s `IntoCrossterm`).
fn to_crossterm(color: Color) -> CrosstermColor {
    match color {
        Color::Reset => CrosstermColor::Reset,
        Color::Black => CrosstermColor::Black,
        Color::Red => CrosstermColor::DarkRed,
        Color::Green => CrosstermColor::DarkGreen,
        Color::Yellow => CrosstermColor::DarkYellow,
        Color::Blue => CrosstermColor::DarkBlue,
        Color::Magenta => CrosstermColor::DarkMagenta,
        Color::Cyan => CrosstermColor::DarkCyan,
        Color::Gray => CrosstermColor::Grey,
        Color::DarkGray => CrosstermColor::DarkGrey,
        Color::LightRed => CrosstermColor::Red,
        Color::LightGreen => CrosstermColor::Green,
        Color::LightYellow => CrosstermColor::Yellow,
        Color::LightBlue => CrosstermColor::Blue,
        Color::LightMagenta => CrosstermColor::Magenta,
        Color::LightCyan => CrosstermColor::Cyan,
        Color::White => CrosstermColor::White,
        Color::Rgb(r, g, b) => CrosstermColor::Rgb { r, g, b },
        Color::Indexed(i) => CrosstermColor::AnsiValue(i),
    }
}

/// Finds `http(s)` URL spans in `text`, returned as byte ranges.
///
/// A match must not be preceded by an ASCII alphanumeric (guards against
/// mid-word false positives; CJK characters directly abutting a URL are
/// fine). The URL extends until whitespace or a bracketing/quoting
/// delimiter (ASCII and common CJK), then trailing prose punctuation is
/// trimmed. Spans with an empty host are dropped.
fn find_url_spans(text: &str) -> Vec<(usize, usize)> {
    const PREFIXES: [&str; 2] = ["https://", "http://"];
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i < text.len() {
        let rest = &text[i..];
        let prefix = PREFIXES.iter().find(|p| rest.starts_with(**p));
        let preceded = i > 0
            && text[..i]
                .chars()
                .next_back()
                .map(|c| c.is_ascii_alphanumeric())
                .unwrap_or(false);
        match prefix {
            Some(p) if !preceded => {
                let start = i;
                let mut end = text.len();
                for (off, ch) in rest.char_indices() {
                    if is_url_delimiter(ch) {
                        end = start + off;
                        break;
                    }
                }
                let mut span = &text[start..end];
                while span.ends_with(['.', ',', ';', ':', '!', '?']) {
                    let last = span.chars().next_back().expect("non-empty span");
                    span = &span[..span.len() - last.len_utf8()];
                }
                if span.len() > p.len() {
                    spans.push((start, start + span.len()));
                }
                i = (start + span.len()).max(i + p.len());
            }
            _ => {
                i += rest.chars().next().map(char::len_utf8).unwrap_or(1);
            }
        }
    }
    spans
}

/// Characters that terminate a URL (in addition to whitespace).
fn is_url_delimiter(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '"' | '\''
                | '<'
                | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '`'
                | '\\'
                | '（'
                | '）'
                | '【'
                | '】'
                | '《'
                | '》'
                | '「'
                | '」'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '。'
                | '，'
                | '；'
                | '：'
                | '！'
                | '？'
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a row of default-styled cells, one grapheme per cell.
    fn cells_for(text: &str) -> Vec<Cell> {
        text.chars()
            .map(|c| {
                let mut cell = Cell::default();
                cell.set_symbol(&c.to_string());
                cell
            })
            .collect()
    }

    fn draw_row(backend: &mut HyperlinkBackend<Vec<u8>>, y: u16, text: &str) {
        let cells = cells_for(text);
        let updates: Vec<(u16, u16, &Cell)> = cells
            .iter()
            .enumerate()
            .map(|(x, c)| (x as u16, y, c))
            .collect();
        backend.draw(updates.into_iter()).unwrap();
    }

    #[test]
    fn finds_bare_url() {
        let spans = find_url_spans("see https://example.com/a?b=c for details");
        assert_eq!(spans.len(), 1);
        let (s, e) = spans[0];
        assert_eq!(
            &"see https://example.com/a?b=c for details"[s..e],
            "https://example.com/a?b=c"
        );
    }

    #[test]
    fn markdown_link_excludes_paren() {
        let text = "[docs](https://example.com/x)";
        let spans = find_url_spans(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].0..spans[0].1], "https://example.com/x");
    }

    #[test]
    fn trims_trailing_prose_punctuation() {
        let text = "visit https://example.com.";
        let spans = find_url_spans(text);
        assert_eq!(&text[spans[0].0..spans[0].1], "https://example.com");
    }

    #[test]
    fn cjk_abutting_url_works_and_cjk_period_terminates() {
        let text = "文档见https://example.com。继续";
        let spans = find_url_spans(text);
        assert_eq!(spans.len(), 1);
        assert_eq!(&text[spans[0].0..spans[0].1], "https://example.com");
    }

    #[test]
    fn skips_mid_word_false_positive() {
        assert!(find_url_spans("ahttp://example.com").is_empty());
    }

    #[test]
    fn finds_multiple_urls() {
        let spans = find_url_spans("https://a.example and http://b.example/x");
        assert_eq!(spans.len(), 2);
    }

    #[test]
    fn drops_empty_host() {
        assert!(find_url_spans("see https:// next").is_empty());
    }

    #[test]
    fn plain_row_emits_no_osc8() {
        let mut backend = HyperlinkBackend::new(Vec::new());
        draw_row(&mut backend, 0, "hello world");
        let out = String::from_utf8(backend.writer.clone()).unwrap();
        assert!(out.contains("hello world"));
        assert!(!out.contains("\x1b]8"));
    }

    #[test]
    fn url_row_wraps_span_in_osc8() {
        let mut backend = HyperlinkBackend::new(Vec::new());
        draw_row(&mut backend, 3, "go https://example.com end");
        let out = String::from_utf8(backend.writer.clone()).unwrap();
        assert!(
            out.contains("\x1b]8;;https://example.com\x1b\\https://example.com\x1b]8;;\x1b\\"),
            "output: {out:?}"
        );
        // Row starts with a cursor move to column 0 of row 3.
        assert!(out.contains("\x1b[4;1H"), "output: {out:?}");
    }

    #[test]
    fn partial_diff_reemits_full_row_with_intact_link() {
        let mut backend = HyperlinkBackend::new(Vec::new());
        draw_row(&mut backend, 0, "go https://example.com end");
        backend.writer.clear();

        // Simulate a diff touching a single cell of the URL row.
        let cells = cells_for("Go https://example.com end");
        let updates: Vec<(u16, u16, &Cell)> = vec![(0, 0, &cells[0])];
        backend.draw(updates.into_iter()).unwrap();

        let out = String::from_utf8(backend.writer.clone()).unwrap();
        assert_eq!(out.matches("\x1b]8;;https://example.com\x1b\\").count(), 1);
        assert_eq!(out.matches(OSC8_CLOSE).count(), 1);
        assert!(out.contains("Go "), "output: {out:?}");
        assert!(out.contains("https://example.com"), "output: {out:?}");
        assert!(
            out.contains(&format!("{OSC8_CLOSE} end")),
            "output: {out:?}"
        );
    }

    #[test]
    fn style_change_emits_reset_then_colors_and_attributes() {
        let mut backend = HyperlinkBackend::new(Vec::new());
        let mut a = Cell::default();
        a.set_symbol("a");
        let mut b = Cell::default();
        b.set_symbol("b");
        b.fg = Color::Green;
        b.modifier = Modifier::BOLD;
        let updates: Vec<(u16, u16, &Cell)> = vec![(0, 0, &a), (1, 0, &b)];
        backend.draw(updates.into_iter()).unwrap();
        let out = String::from_utf8(backend.writer.clone()).unwrap();
        // Style change between the cells: "a", then SGR reset, then bold, then "b".
        let ia = out.find('a').expect("cell a printed");
        let ireset = out.find("\x1b[0m").expect("SGR reset emitted");
        let ibold = out.find("\x1b[1m").expect("bold emitted");
        let ib = out.rfind('b').expect("cell b printed");
        assert!(
            ia < ireset && ireset < ibold && ibold < ib,
            "output: {out:?}"
        );
    }
}
