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
//!
//! Multi-pane layouts: a terminal row is a seamless concatenation of
//! neighbouring panes, so scanning is clipped to pane rectangles registered
//! by the renderer every frame ([`LinkRegions`], currently the chat message
//! area). This keeps adjacent pane text out of link targets and lets wrap
//! joins key off the pane's edges rather than the terminal row's.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::io::{self, Write};
use std::rc::Rc;

use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::style::{
    Attribute as CrosstermAttribute, Color as CrosstermColor, Colors, Print, SetAttribute,
    SetColors,
};
use crossterm::terminal::{self, Clear};
use crossterm::{execute, queue};
use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Color, Modifier};

/// OSC 8 hyperlink terminator (closes the current link).
const OSC8_CLOSE: &str = "\x1b]8;;\x1b\\";

/// Text-pane rectangles eligible for URL link detection. The renderer
/// refreshes this every frame; panes outside these regions are never
/// scanned, so neighbouring pane text can never leak into a link target.
pub(crate) type LinkRegions = Rc<RefCell<Vec<Rect>>>;

/// A crossterm-based [`Backend`] that wraps visible URLs in OSC 8 hyperlinks.
pub(crate) struct HyperlinkBackend<W: Write> {
    writer: W,
    /// Shadow copy of the terminal grid, updated from the diff stream.
    grid: Vec<Vec<Cell>>,
    /// Pane rectangles eligible for link scanning (see [`LinkRegions`]).
    regions: LinkRegions,
    /// Test hook: overrides the terminal width used to clip full-row
    /// re-emissions (real terminals report via `size()`; tests have no
    /// tty). `None` in production.
    width_override: Option<u16>,
}

impl<W: Write> HyperlinkBackend<W> {
    /// Creates a backend writing to `writer`, scanning URLs inside `regions`.
    pub(crate) fn new(writer: W, regions: LinkRegions) -> Self {
        Self {
            writer,
            grid: Vec::new(),
            regions,
            width_override: None,
        }
    }

    /// Current terminal width for clipping full-row re-emissions. Falls
    /// back to `u16::MAX` (no clipping) when the size ioctl fails, e.g.
    /// when stdout is not a tty.
    fn current_width(&self) -> u16 {
        if let Some(w) = self.width_override {
            return w;
        }
        self.size().map(|s| s.width).unwrap_or(u16::MAX)
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
        // Trailing reset: each emission pass assumes the terminal's SGR
        // state is fully reset at entry, so every pass must leave it reset
        // for the next one (mirrors ratatui-crossterm's per-draw reset).
        queue!(
            self.writer,
            SetColors(Colors::new(CrosstermColor::Reset, CrosstermColor::Reset)),
            SetAttribute(CrosstermAttribute::Reset),
        )
    }
    /// The slice of shadow-grid row `y` that falls inside `region`'s columns.
    fn region_row<'a>(grid: &'a [Vec<Cell>], region: &Rect, y: usize) -> &'a [Cell] {
        let Some(row) = grid.get(y) else { return &[] };
        let x0 = (region.x as usize).min(row.len());
        let x1 = (region.right() as usize).min(row.len());
        &row[x0..x1]
    }

    /// A row needs full-row re-emission when any registered pane region on
    /// it contains a URL span or continues a URL wrapped from the row above.
    fn is_link_row(&self, y: usize) -> bool {
        self.regions.borrow().iter().any(|region| {
            if y < region.y as usize || y >= region.bottom() as usize {
                return false;
            }
            let slice = Self::region_row(&self.grid, region, y);
            !find_url_spans(&Self::row_text(slice)).is_empty() || self.row_starts_mid_url(region, y)
        })
    }

    /// Link segments over a row: `(first_cell, end_cell_exclusive, full_url)`
    /// in absolute terminal columns.
    ///
    /// Scanning is clipped to each registered pane region: a terminal row is
    /// a seamless concatenation of neighbouring panes, so without clipping a
    /// URL at a pane's right edge would merge with the next pane's text.
    ///
    /// URLs wrapped across rows are reconstructed by joining fragments: a
    /// span that reaches the pane's right edge (the renderer hard-splits
    /// over-long words exactly at the wrap width) continues into the next
    /// row's leading run of URL characters. Heuristic limitation: a line
    /// that *ends* with a complete URL followed by a line that *starts*
    /// with URL characters is indistinguishable from a wrap and gets joined.
    fn link_segments(&self, y: usize) -> Vec<(usize, usize, String)> {
        let mut segments = Vec::new();
        let regions = self.regions.borrow();
        for region in regions.iter() {
            if y < region.y as usize || y >= region.bottom() as usize {
                continue;
            }
            let slice = Self::region_row(&self.grid, region, y);
            if slice.is_empty() {
                continue;
            }
            let text = Self::row_text(slice);
            let x0 = region.x as usize;

            // Continuation of a URL that wrapped from a previous row.
            if self.row_starts_mid_url(region, y) {
                let run = leading_url_run(&text);
                segments.push((x0, x0 + run.len(), self.reconstruct_url_at(region, y)));
            }

            for (start, end) in find_url_spans(&text) {
                let cell_start = byte_to_cell(slice, start);
                let cell_end = byte_to_cell(slice, end);
                let mut url = text[start..end].to_string();
                if cell_end == slice.len() && slice.len() == region.width as usize {
                    // The span reaches the pane's right edge: the URL may
                    // continue on the following rows.
                    url = self.join_continuations(region, y + 1, url);
                }
                segments.push((x0 + cell_start, x0 + cell_end, url));
            }
        }
        segments
    }

    /// Row `y`'s region slice begins inside a URL that wrapped from above.
    fn row_starts_mid_url(&self, region: &Rect, y: usize) -> bool {
        if y == 0 || !self.row_ends_mid_url(region, y - 1) {
            return false;
        }
        !leading_url_run(&Self::row_text(Self::region_row(&self.grid, region, y))).is_empty()
    }

    /// Row `y`'s region slice ends inside a URL that continues below.
    fn row_ends_mid_url(&self, region: &Rect, y: usize) -> bool {
        let slice = Self::region_row(&self.grid, region, y);
        // A slice shorter than the pane is a line that ended mid-pane:
        // no wrap. (A wrapped fragment fills every column to the edge.)
        if slice.len() < region.width as usize {
            return false;
        }
        let text = Self::row_text(slice);
        if find_url_spans(&text)
            .iter()
            .any(|&(_, end)| byte_to_cell(slice, end) == slice.len())
        {
            return true;
        }
        // A slice fully consumed by a wrapped URL's middle fragment.
        !text.is_empty() && text.chars().all(is_url_char) && self.row_starts_mid_url(region, y)
    }

    /// Appends the leading URL runs of continuation rows to `url`.
    fn join_continuations(&self, region: &Rect, mut y: usize, mut url: String) -> String {
        while self.row_starts_mid_url(region, y) {
            let text = Self::row_text(Self::region_row(&self.grid, region, y));
            let run = leading_url_run(&text);
            if run.is_empty() {
                break;
            }
            url.push_str(run);
            y += 1;
        }
        trim_trailing_punct(&url).to_string()
    }

    /// Rebuilds the full URL whose fragment leads continuation row `y`.
    fn reconstruct_url_at(&self, region: &Rect, y: usize) -> String {
        let mut head = y;
        while self.row_starts_mid_url(region, head) {
            head -= 1;
        }
        let slice = Self::region_row(&self.grid, region, head);
        let text = Self::row_text(slice);
        // The URL on the head row that wraps downward: the last span whose
        // end touches the pane's right edge.
        let (start, end) = find_url_spans(&text)
            .into_iter()
            .rev()
            .find(|&(_, end)| byte_to_cell(slice, end) == slice.len())
            .unwrap_or((0, 0));
        self.join_continuations(region, head + 1, text[start..end].to_string())
    }

    /// Full-row emission keeps link boundaries correct for partial diffs.
    fn emit_link_row(&mut self, y: u16) -> io::Result<()> {
        let segments = self.link_segments(y as usize);
        let row = &self.grid[y as usize];

        // Never emit past the terminal's current width: the shadow grid
        // can hold longer rows left over from before a shrink-resize, and
        // printing those extra cells would auto-wrap and physically scroll
        // the screen — desyncing ratatui's diff baseline (its previous
        // buffer) from the real display, which shows up as interleaved
        // old/new text while scrolling.
        let width = self.current_width() as usize;
        let cells = row.len().min(width);

        // Map each column to the link segment covering it.
        let mut link_at: Vec<Option<usize>> = vec![None; row.len()];
        for (i, &(start, end, _)) in segments.iter().enumerate() {
            for slot in link_at.iter_mut().take(end.min(row.len())).skip(start) {
                *slot = Some(i);
            }
        }

        queue!(self.writer, MoveTo(0, y))?;
        let mut current = (Color::Reset, Color::Reset, Modifier::empty());
        let mut open_link: Option<usize> = None;
        for (x, cell) in row.iter().take(cells).enumerate() {
            let link = link_at[x];
            if link != open_link {
                if open_link.is_some() {
                    self.writer.write_all(OSC8_CLOSE.as_bytes())?;
                }
                if let Some(i) = link {
                    write!(self.writer, "\x1b]8;;{}\x1b\\", segments[i].2)?;
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
        queue!(
            self.writer,
            SetColors(Colors::new(CrosstermColor::Reset, CrosstermColor::Reset)),
            SetAttribute(CrosstermAttribute::Reset),
        )
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
        // Classify per unique row, not per changed cell.
        let changed_rows: BTreeSet<u16> = updates.iter().map(|(_, y, _)| *y).collect();
        let mut link_rows = BTreeSet::new();
        for y in changed_rows {
            if self.is_link_row(y as usize) {
                link_rows.insert(y);
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
        Ok(())
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

/// Strips trailing prose punctuation that is rarely part of an intended URL.
fn trim_trailing_punct(span: &str) -> &str {
    let mut span = span;
    while span.ends_with(['.', ',', ';', ':', '!', '?']) {
        let last = span.chars().next_back().expect("non-empty span");
        span = &span[..span.len() - last.len_utf8()];
    }
    span
}

/// Characters that can appear inside a (non-percent-encoded) URL fragment:
/// ASCII, excluding whitespace and bracketing punctuation. Continuation
/// runs are deliberately stricter than in-span characters: a CJK character
/// at the start of the next row is prose, not part of a wrapped URL.
fn is_url_char(c: char) -> bool {
    c.is_ascii() && !is_url_delimiter(c)
}

/// Leading run of URL characters at the start of `text`.
fn leading_url_run(text: &str) -> &str {
    let end = text
        .char_indices()
        .find(|&(_, c)| !is_url_char(c))
        .map(|(i, _)| i)
        .unwrap_or(text.len());
    &text[..end]
}

/// Maps a byte offset in the row text to the cell index whose symbol starts
/// there (a span end maps to the cell just past it, i.e. an exclusive end).
fn byte_to_cell(row: &[Cell], byte: usize) -> usize {
    let mut pos = 0;
    for (x, cell) in row.iter().enumerate() {
        if pos >= byte {
            return x;
        }
        pos += cell.symbol().len();
    }
    row.len()
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
                let span = trim_trailing_punct(&text[start..end]);
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

    /// Backend over an in-memory writer with one top-left link region.
    fn backend_with_region(w: u16, h: u16) -> HyperlinkBackend<Vec<u8>> {
        let regions: LinkRegions = Rc::new(RefCell::new(vec![Rect::new(0, 0, w, h)]));
        HyperlinkBackend::new(Vec::new(), regions)
    }

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
        let mut backend = backend_with_region(100, 100);
        draw_row(&mut backend, 0, "hello world");
        let out = String::from_utf8(backend.writer.clone()).unwrap();
        assert!(out.contains("hello world"));
        assert!(!out.contains("\x1b]8"));
    }

    #[test]
    fn url_row_wraps_span_in_osc8() {
        let mut backend = backend_with_region(100, 100);
        draw_row(&mut backend, 3, "go https://example.com end");
        let out = String::from_utf8(backend.writer.clone()).unwrap();
        assert!(
            out.contains("\x1b]8;;https://example.com\x1b\\https://example.com\x1b]8;;\x1b\\"),
            "output: {out:?}"
        );
        // Row starts with a cursor move to column 0 of row 3.
        assert!(out.contains("\x1b[4;1H"), "output: {out:?}");
    }

    /// Draws rows of text as one frame of row-major cell updates.
    fn draw_rows(backend: &mut HyperlinkBackend<Vec<u8>>, rows: &[&str]) {
        let cells: Vec<Vec<Cell>> = rows.iter().map(|r| cells_for(r)).collect();
        let updates: Vec<(u16, u16, &Cell)> = cells
            .iter()
            .enumerate()
            .flat_map(|(y, row)| {
                row.iter()
                    .enumerate()
                    .map(move |(x, c)| (x as u16, y as u16, c))
            })
            .collect();
        backend.draw(updates.into_iter()).unwrap();
    }

    #[test]
    fn wrapped_url_across_two_rows_emits_full_url() {
        // The pane is exactly as wide as row 0: the URL fragment fills it to
        // the right edge, which is the wrap signal.
        let mut backend = backend_with_region(26, 10);
        draw_rows(
            &mut backend,
            &["see https://example.com/ve", "ry-long-path tail"],
        );
        let out = String::from_utf8(backend.writer.clone()).unwrap();
        let open = "\x1b]8;;https://example.com/very-long-path\x1b\\";
        assert_eq!(out.matches(open).count(), 2, "output: {out:?}");
    }

    #[test]
    fn wrapped_url_across_three_rows_emits_full_url() {
        let mut backend = backend_with_region(13, 10);
        draw_rows(
            &mut backend,
            &["see https://x", "aaaaaaaaaaaaa", "tail end"],
        );
        let out = String::from_utf8(backend.writer.clone()).unwrap();
        let open = "\x1b]8;;https://xaaaaaaaaaaaaatail\x1b\\";
        assert_eq!(out.matches(open).count(), 3, "output: {out:?}");
    }

    #[test]
    fn continuation_row_stays_linked_on_partial_diff() {
        let mut backend = backend_with_region(26, 10);
        draw_rows(
            &mut backend,
            &["see https://example.com/ve", "ry-long-path tail"],
        );
        backend.writer.clear();

        // Only the continuation row changes in the next frame.
        let row = cells_for("ry-long-path TAIL");
        let updates: Vec<(u16, u16, &Cell)> = row
            .iter()
            .enumerate()
            .map(|(x, c)| (x as u16, 1u16, c))
            .collect();
        backend.draw(updates.into_iter()).unwrap();

        let out = String::from_utf8(backend.writer.clone()).unwrap();
        let open = "\x1b]8;;https://example.com/very-long-path\x1b\\";
        assert_eq!(out.matches(open).count(), 1, "output: {out:?}");
    }

    #[test]
    fn url_ending_at_row_edge_without_continuation_is_not_joined() {
        let mut backend = backend_with_region(14, 10);
        draw_rows(&mut backend, &["go https://a.b", " tail"]);
        let out = String::from_utf8(backend.writer.clone()).unwrap();
        assert!(out.contains("\x1b]8;;https://a.b\x1b\\"), "output: {out:?}");
        assert!(!out.contains("https://a.btail"), "output: {out:?}");
    }

    #[test]
    fn pane_boundary_does_not_leak_into_link_target() {
        // Chat pane is 20 columns wide; the info pane's text occupies the
        // same terminal rows starting at column 20. Without region clipping
        // the row text concatenates both panes and the chat URL would swallow
        // "Topic:".
        let regions: LinkRegions = Rc::new(RefCell::new(vec![Rect::new(0, 0, 20, 10)]));
        let mut backend = HyperlinkBackend::new(Vec::new(), regions);
        draw_rows(
            &mut backend,
            &[
                "see https://a.b/cdefTopic: agents",
                "ghij rest           Branch: main",
            ],
        );
        let out = String::from_utf8(backend.writer.clone()).unwrap();
        let open = "\x1b]8;;https://a.b/cdefghij\x1b\\";
        assert_eq!(out.matches(open).count(), 2, "output: {out:?}");
        assert!(
            !out.contains("\x1b]8;;https://a.b/cdefTopic"),
            "output: {out:?}"
        );
    }

    #[test]
    fn region_with_x_offset_joins_from_region_left_edge() {
        // Explorer occupies columns 0..10, the chat pane sits at 10..23.
        let regions: LinkRegions = Rc::new(RefCell::new(vec![Rect::new(10, 0, 13, 10)]));
        let mut backend = HyperlinkBackend::new(Vec::new(), regions);
        draw_rows(
            &mut backend,
            &["EXPLORER  see https://x", "EXPLORER  tail end"],
        );
        let out = String::from_utf8(backend.writer.clone()).unwrap();
        let open = "\x1b]8;;https://xtail\x1b\\";
        assert_eq!(out.matches(open).count(), 2, "output: {out:?}");
    }

    #[test]
    fn partial_diff_reemits_full_row_with_intact_link() {
        let mut backend = backend_with_region(100, 100);
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
    fn passes_reset_sgr_between_plain_and_link_rows() {
        let mut backend = backend_with_region(100, 100);
        // A styled plain cell leaves SGR non-default at the end of the
        // plain pass; the link row must not inherit it.
        let mut styled = Cell::default();
        styled.set_symbol("a");
        styled.fg = Color::Green;
        styled.modifier = Modifier::BOLD;
        let row = cells_for("go https://example.com");
        let mut updates: Vec<(u16, u16, &Cell)> = vec![(0, 0, &styled)];
        updates.extend(row.iter().enumerate().map(|(x, c)| (x as u16, 1u16, c)));
        backend.draw(updates.into_iter()).unwrap();

        let out = String::from_utf8(backend.writer.clone()).unwrap();
        let ia = out.find('a').expect("plain cell printed");
        let imv = out.find("\x1b[2;1H").expect("link row MoveTo(0,1)");
        assert!(
            out[ia..imv].contains("\x1b[0m"),
            "link row must start from a reset SGR state: {out:?}"
        );
    }

    #[test]
    fn style_change_emits_reset_then_colors_and_attributes() {
        let mut backend = backend_with_region(100, 100);
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

    #[test]
    fn link_row_reemission_is_clipped_to_terminal_width() {
        // Simulate a shrink-resize: the shadow grid keeps a row longer
        // than the current terminal width. Re-emitting it past the width
        // would auto-wrap and physically scroll the screen, desyncing
        // ratatui's diff baseline (interleaved old/new text on scroll).
        let mut backend = backend_with_region(100, 100);
        // URL ends at col 14; the plain tail is what clipping must drop.
        let text = "go https://x.co NOW then a long tail of plain text xyz";
        draw_row(&mut backend, 0, text);
        let full = String::from_utf8(backend.writer.clone()).unwrap();
        assert!(full.contains("xyz"), "uncapped emission: {full:?}");

        // Terminal shrank to 20 columns; the row is redrawn (e.g. scroll).
        backend.width_override = Some(20);
        backend.writer.clear();
        draw_row(&mut backend, 0, text);
        let clipped = String::from_utf8(backend.writer.clone()).unwrap();
        assert!(clipped.contains("NOW"), "visible head kept: {clipped:?}");
        assert!(
            !clipped.contains("long tail") && !clipped.contains("xyz"),
            "cells past the terminal width must not be emitted: {clipped:?}"
        );
        // The OSC 8 payload still carries the full link target.
        assert!(clipped.contains("\x1b]8;;https://x.co\x1b\\"));
    }
}
