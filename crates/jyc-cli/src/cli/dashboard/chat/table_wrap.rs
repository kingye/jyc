//! Table-aware wrapping for tui-markdown output.
//!
//! tui-markdown renders table rows as single `Line`s whose width is the sum of
//! each column's natural width plus border characters. Our generic
//! `wrap_styled_lines` then word-wraps those rows at pane width, which breaks
//! the box-drawing borders and misaligns columns.
//!
//! `wrap_tables` detects contiguous table blocks in the rendered output and
//! re-wraps each cell *inside* its column so the table stays a table.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::wrap_styled_lines;

/// Rewrap over-wide table blocks so they fit within `max_width` display
/// columns without mangling borders or column alignment.
///
/// Non-table lines are returned unchanged.
pub(super) fn wrap_tables<'a>(lines: Vec<Line<'a>>, max_width: usize) -> Vec<Line<'a>> {
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let text = lines[i].to_string();
        if is_table_top(&text) {
            let start = i;
            i += 1;
            let mut has_separator = false;
            while i < lines.len() && is_table_line(&lines[i].to_string()) {
                if is_separator(&lines[i].to_string()) {
                    has_separator = true;
                }
                i += 1;
            }
            let block = &lines[start..i];
            if has_separator {
                out.extend(
                    wrap_table_block(block, max_width)
                        .into_iter()
                        .map(|l: Line<'static>| -> Line<'a> { l }),
                );
            } else {
                out.extend(
                    block
                        .iter()
                        .map(|line: &Line<'_>| -> Line<'a> { to_owned_line(line) }),
                );
            }
        } else {
            out.push(lines[i].clone());
            i += 1;
        }
    }
    out
}

/// Convert a borrowed `Line` into an owned `'static` one.
fn to_owned_line(line: &Line<'_>) -> Line<'static> {
    let spans: Vec<Span<'static>> = line
        .spans
        .iter()
        .map(|s| Span::styled(s.content.to_string(), s.style))
        .collect();
    let mut out = Line::from(spans).style(line.style);
    if let Some(align) = line.alignment {
        out = out.alignment(align);
    }
    out
}

/// A cell's horizontal alignment inferred from its original padding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CellAlign {
    Left,
    Right,
    Center,
}

struct Cell {
    content: Vec<(char, Style)>,
    align: CellAlign,
    /// Display width of the cell segment as it appears between the two `│`
    /// borders, including padding spaces. Used when the table already fits.
    full_width: usize,
}

enum TableRow {
    Border,
    Data(Vec<Cell>),
}

struct WrappedCell {
    chunks: Vec<Vec<Span<'static>>>,
    align: CellAlign,
}

enum WrappedRow {
    Border,
    Data(Vec<WrappedCell>),
}

fn is_table_top(s: &str) -> bool {
    s.starts_with('┌') && s.ends_with('┐') && s.contains('─') && !s.contains('│')
}

fn is_table_line(s: &str) -> bool {
    is_table_row(s) || is_separator(s) || is_bottom(s)
}

fn is_table_row(s: &str) -> bool {
    s.starts_with('│') && s.ends_with('│') && s.chars().filter(|&c| c == '│').count() >= 2
}

fn is_separator(s: &str) -> bool {
    s.starts_with('├') && s.ends_with('┤') && s.contains('─') && !s.contains('│')
}

fn is_bottom(s: &str) -> bool {
    s.starts_with('└') && s.ends_with('┘') && s.contains('─') && !s.contains('│')
}

/// Count table columns from a rendered row line by counting `│` separators.
fn column_count_from_row(line: &Line<'_>) -> usize {
    line.to_string()
        .chars()
        .filter(|&c| c == '│')
        .count()
        .saturating_sub(1)
}

fn flatten_line(line: &Line<'_>) -> Vec<(char, Style)> {
    line.spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |ch| (ch, span.style)))
        .collect()
}

fn display_width(cells: &[(char, Style)]) -> usize {
    cells
        .iter()
        .map(|&(ch, _)| UnicodeWidthChar::width(ch).unwrap_or(0))
        .sum()
}

fn trim_spaces(cells: &[(char, Style)]) -> Vec<(char, Style)> {
    let start = cells
        .iter()
        .position(|&(ch, _)| ch != ' ')
        .unwrap_or(cells.len());
    let end = cells
        .iter()
        .rposition(|&(ch, _)| ch != ' ')
        .map(|i| i + 1)
        .unwrap_or(start);
    cells[start..end].to_vec()
}

fn leading_spaces(cells: &[(char, Style)]) -> usize {
    cells.iter().take_while(|&&(ch, _)| ch == ' ').count()
}

fn detect_align(segment: &[(char, Style)]) -> CellAlign {
    let total = display_width(segment);
    let trimmed = trim_spaces(segment);
    let content = display_width(&trimmed);
    let leading = leading_spaces(segment);
    let trailing = total.saturating_sub(leading + content);

    if leading.abs_diff(trailing) <= 1 {
        CellAlign::Center
    } else if leading > trailing {
        CellAlign::Right
    } else {
        CellAlign::Left
    }
}

fn parse_row(line: &Line<'_>, col_count: usize) -> TableRow {
    let cells = flatten_line(line);
    let separators: Vec<usize> = cells
        .iter()
        .enumerate()
        .filter(|&(_, (ch, _))| *ch == '│')
        .map(|(i, _)| i)
        .collect();

    let mut columns = Vec::with_capacity(col_count);
    for c in 0..col_count {
        if let (Some(&start), Some(&end)) = (separators.get(c), separators.get(c + 1)) {
            let segment = &cells[start + 1..end];
            let align = detect_align(segment);
            columns.push(Cell {
                content: trim_spaces(segment),
                align,
                full_width: display_width(segment),
            });
        } else {
            columns.push(Cell {
                content: Vec::new(),
                align: CellAlign::Left,
                full_width: 0,
            });
        }
    }
    TableRow::Data(columns)
}

/// Shrink natural column widths so `sum(widths) + border_chars <= budget`.
fn shrink_to_budget(natural: &[usize], budget: usize) -> Vec<usize> {
    const MIN_WIDTH: usize = 1;
    let mut widths = natural.to_vec();
    loop {
        let total: usize = widths.iter().sum();
        if total <= budget {
            break;
        }
        let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, w)| **w > MIN_WIDTH)
            .max_by_key(|(_, w)| **w)
        else {
            break;
        };
        widths[idx] -= 1;
    }
    widths
}

/// Coalesce consecutive cells with the same style into a single span.
fn cells_to_spans(cells: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for &(ch, style) in cells {
        if run_style != Some(style) {
            if let Some(s) = run_style.take() {
                spans.push(Span::styled(std::mem::take(&mut run), s));
            }
            run_style = Some(style);
        }
        run.push(ch);
    }
    if let Some(s) = run_style {
        spans.push(Span::styled(std::mem::take(&mut run), s));
    }
    spans
}

fn wrap_cell(content: &[(char, Style)], width: usize) -> Vec<Vec<Span<'static>>> {
    if content.is_empty() {
        return vec![Vec::new()];
    }
    let line = Line::from(cells_to_spans(content));
    wrap_styled_lines(vec![line], width)
        .into_iter()
        .map(|l| l.spans)
        .collect()
}

fn chunk_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|s| s.content.width()).sum()
}

fn space_span(width: usize) -> Span<'static> {
    Span::styled(" ".repeat(width), Style::default())
}

fn border_line(
    widths: &[usize],
    left: char,
    mid: char,
    right: char,
    style: Style,
) -> Line<'static> {
    let mut s = String::with_capacity(widths.iter().sum::<usize>() + widths.len() + 1);
    s.push(left);
    for (i, w) in widths.iter().enumerate() {
        s.push_str(&"─".repeat(*w));
        if i + 1 < widths.len() {
            s.push(mid);
        }
    }
    s.push(right);
    Line::from(Span::styled(s, style))
}

fn render_wrapped_table(
    rows: &[WrappedRow],
    widths: &[usize],
    border_style: Style,
) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(rows.len() * 2);

    for (row_idx, row) in rows.iter().enumerate() {
        match row {
            WrappedRow::Border => {
                let line = if row_idx == 0 {
                    border_line(widths, '┌', '┬', '┐', border_style)
                } else if row_idx == rows.len() - 1 {
                    border_line(widths, '└', '┴', '┘', border_style)
                } else {
                    border_line(widths, '├', '┼', '┤', border_style)
                };
                out.push(line);
            }
            WrappedRow::Data(cells) => {
                let visual_rows = cells
                    .iter()
                    .map(|c| c.chunks.len())
                    .max()
                    .unwrap_or(1)
                    .max(1);
                for r in 0..visual_rows {
                    let mut spans: Vec<Span<'static>> = Vec::new();
                    spans.push(Span::styled("│", border_style));
                    for (col, cell) in cells.iter().enumerate() {
                        let width = widths.get(col).copied().unwrap_or(0);
                        if let Some(chunk) = cell.chunks.get(r) {
                            let cw = chunk_width(chunk);
                            let pad = width.saturating_sub(cw);
                            let (pre, post) = match cell.align {
                                CellAlign::Left => {
                                    let pre = 1usize.min(pad);
                                    (pre, pad - pre)
                                }
                                CellAlign::Right => {
                                    let post = 1usize.min(pad);
                                    (pad - post, post)
                                }
                                CellAlign::Center => (pad / 2, pad - pad / 2),
                            };
                            if pre > 0 {
                                spans.push(space_span(pre));
                            }
                            spans.extend(chunk.iter().cloned());
                            if post > 0 {
                                spans.push(space_span(post));
                            }
                        } else {
                            spans.push(space_span(width));
                        }
                        spans.push(Span::styled("│", border_style));
                    }
                    out.push(Line::from(spans));
                }
            }
        }
    }
    out
}

fn wrap_table_block(lines: &[Line<'_>], max_width: usize) -> Vec<Line<'static>> {
    if lines.is_empty() {
        return Vec::new();
    }

    let border_style = lines[0].spans.first().map(|s| s.style).unwrap_or_default();

    let col_count = lines.iter().find_map(|l| {
        let s = l.to_string();
        if is_table_row(&s) {
            Some(column_count_from_row(l))
        } else {
            None
        }
    });
    let Some(col_count) = col_count else {
        return lines.iter().map(to_owned_line).collect();
    };

    // Need at least one display column per cell plus border characters.
    let border_count = col_count + 1;
    if col_count == 0 || max_width < border_count + col_count {
        return lines.iter().map(to_owned_line).collect();
    }

    let parsed: Vec<TableRow> = lines
        .iter()
        .map(|l| {
            let s = l.to_string();
            if is_table_row(&s) {
                parse_row(l, col_count)
            } else {
                TableRow::Border
            }
        })
        .collect();

    let mut full_widths = vec![0usize; col_count];
    for row in &parsed {
        if let TableRow::Data(cells) = row {
            for (i, cell) in cells.iter().enumerate().take(col_count) {
                full_widths[i] = full_widths[i].max(cell.full_width);
            }
        }
    }

    let total_full = full_widths.iter().sum::<usize>() + border_count;
    let widths = if total_full > max_width {
        let budget = max_width.saturating_sub(border_count);
        shrink_to_budget(&full_widths, budget)
    } else {
        full_widths
    };

    let wrapped: Vec<WrappedRow> = parsed
        .iter()
        .map(|row| match row {
            TableRow::Border => WrappedRow::Border,
            TableRow::Data(cells) => WrappedRow::Data(
                cells
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| WrappedCell {
                        chunks: wrap_cell(&cell.content, widths.get(i).copied().unwrap_or(1)),
                        align: cell.align,
                    })
                    .collect(),
            ),
        })
        .collect();

    render_wrapped_table(&wrapped, &widths, border_style)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell_width(cell: &str) -> usize {
        cell.width() + 2
    }

    fn make_row(cells: &[&str]) -> Line<'static> {
        let mut spans = vec![Span::styled("│", Style::default())];
        for cell in cells {
            spans.push(Span::styled(format!(" {} ", cell), Style::default()));
            spans.push(Span::styled("│", Style::default()));
        }
        Line::from(spans)
    }

    fn make_border(cells: &[&str], left: char, mid: char, right: char) -> Line<'static> {
        let mut s = String::new();
        s.push(left);
        for (i, cell) in cells.iter().enumerate() {
            s.push_str(&"─".repeat(cell_width(cell)));
            if i + 1 < cells.len() {
                s.push(mid);
            }
        }
        s.push(right);
        Line::from(Span::styled(s, Style::default()))
    }

    fn make_top(cells: &[&str]) -> Line<'static> {
        make_border(cells, '┌', '┬', '┐')
    }

    fn make_sep(cells: &[&str]) -> Line<'static> {
        make_border(cells, '├', '┼', '┤')
    }

    fn make_bottom(cells: &[&str]) -> Line<'static> {
        make_border(cells, '└', '┴', '┘')
    }

    fn max_line_width(lines: &[Line<'_>]) -> usize {
        lines
            .iter()
            .map(|l| l.to_string().width())
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn table_that_fits_is_unchanged() {
        let cells = &["foo", "bar"];
        let lines = vec![
            make_top(cells),
            make_row(cells),
            make_sep(cells),
            make_row(&["baz", "qux"]),
            make_bottom(cells),
        ];
        let out = wrap_tables(lines.clone(), 80);
        assert_eq!(out.len(), lines.len());
        for (a, b) in out.iter().zip(lines.iter()) {
            assert_eq!(a.to_string(), b.to_string());
        }
    }

    fn make_row_aligned(cells: &[&str], widths: &[usize], align: CellAlign) -> Line<'static> {
        let mut spans = vec![Span::styled("│", Style::default())];
        for (i, cell) in cells.iter().enumerate() {
            let width = widths.get(i).copied().unwrap_or_else(|| cell.width() + 2);
            let cw = cell.width();
            let pad = width.saturating_sub(cw + 2);
            let (pre, post) = match align {
                CellAlign::Left => (1, 1 + pad),
                CellAlign::Right => (1 + pad, 1),
                CellAlign::Center => {
                    let left = pad / 2;
                    (1 + left, 1 + pad - left)
                }
            };
            spans.push(Span::styled(" ".repeat(pre), Style::default()));
            spans.push(Span::styled(cell.to_string(), Style::default()));
            spans.push(Span::styled(" ".repeat(post), Style::default()));
            spans.push(Span::styled("│", Style::default()));
        }
        Line::from(spans)
    }

    #[test]
    fn narrow_data_rows_keep_alignment() {
        // Headers are the widest cells, so shorter data rows are left-aligned
        // with one leading padding space (tui-markdown's convention).
        let headers = &["name", "value"];
        let widths = vec![cell_width("name"), cell_width("value")];
        let lines = vec![
            make_top(headers),
            make_row(headers),
            make_sep(headers),
            make_row_aligned(&["x", "yy"], &widths, CellAlign::Left),
            make_bottom(headers),
        ];
        let out = wrap_tables(lines.clone(), 80);
        assert_eq!(out.len(), lines.len());
        for (a, b) in out.iter().zip(lines.iter()) {
            assert_eq!(a.to_string(), b.to_string(), "row mismatch");
        }
    }

    #[test]
    fn wide_table_is_wrapped_to_width() {
        let long = "this is a very long description that exceeds the pane width easily";
        let lines = vec![
            make_top(&["name", "desc"]),
            make_row(&["foo", "bar"]),
            make_sep(&["name", "desc"]),
            make_row(&["item", long]),
            make_bottom(&["name", "desc"]),
        ];
        let out = wrap_tables(lines, 30);
        assert!(
            max_line_width(&out) <= 30,
            "max width was {}",
            max_line_width(&out)
        );
        // The long cell should have produced more than one visual row.
        assert!(out.len() > 5);
    }

    #[test]
    fn wrapped_table_keeps_borders() {
        let long = "two very long text here";
        let lines = vec![
            make_top(&["a", "b", "c"]),
            make_row(&["xxx", "yyy", "zzz"]),
            make_sep(&["a", "b", "c"]),
            make_row(&["one", long, "three"]),
            make_bottom(&["a", "b", "c"]),
        ];
        let out = wrap_tables(lines, 25);
        for line in &out {
            let s = line.to_string();
            assert!(
                s.starts_with('│')
                    || s.starts_with('┌')
                    || s.starts_with('├')
                    || s.starts_with('└'),
                "table line lost border: {}",
                s
            );
        }
    }

    #[test]
    fn ascii_box_art_is_not_misdetected_as_table() {
        // No `├...┼...┤` separator, so this should be left untouched.
        let lines: Vec<Line<'static>> = vec![
            Line::from("┌──────────┐"),
            Line::from("│  hello   │"),
            Line::from("└──────────┘"),
        ];
        let out = wrap_tables(lines.clone(), 20);
        assert_eq!(out.len(), lines.len());
        for (a, b) in out.iter().zip(lines.iter()) {
            assert_eq!(a.to_string(), b.to_string());
        }
    }

    #[test]
    fn real_markdown_table_with_cjk_fits_width() {
        let md = concat!(
            "| 方案 | 描述 |\n",
            "|---|---|\n",
            "| A. 方案 | 这是一个非常非常非常非常非常非常长的描述 |\n",
        );
        let rendered = tui_markdown::from_str(md).lines;
        let width = 40;
        let out = wrap_tables(rendered, width);
        assert!(!out.is_empty());
        let max = max_line_width(&out);
        assert!(max <= width, "max width {} > {}", max, width);
        for line in &out {
            let s = line.to_string();
            assert!(
                s.starts_with('│')
                    || s.starts_with('┌')
                    || s.starts_with('├')
                    || s.starts_with('└'),
                "lost border: {}",
                s
            );
        }
    }
}
