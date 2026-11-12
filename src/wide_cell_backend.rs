//! A ratatui backend wrapper for symbols wider than one cell.
//!
//! ratatui's diff re-sends the trailing cell of an emoji with the variation
//! selector U+FE0F (such as ⚠️) as an explicit blank update, for terminals that
//! leave it dirty. The crossterm backend then writes that blank without moving
//! the cursor, assuming the emoji was one cell wide. A terminal that draws the
//! emoji two cells wide, as tmux does, has the cursor one column further, so
//! the blank and the rest of the run land one column to the right: the row
//! comes out shifted and corrupted until a full redraw. Positioning the cursor
//! on the trailing cell instead would erase the emoji, since writing over half
//! of a wide character clears it.
//!
//! The wrapper drops a blank update that falls inside a wide symbol drawn just
//! before it (the terminal already covered that cell), and gives any other
//! content there a run of its own, so the inner backend positions the cursor.
//! tmux widens such an emoji only when the selector arrives, over whatever is
//! in the next cell: if that is an older copy of the emoji (a row shifted by
//! one column), the next write clears both. So before each one the wrapper
//! blanks the cells it will widen into.

use std::io;

use ratatui::backend::{Backend, ClearType, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

use unicode_width::UnicodeWidthStr;

/// Cell width of a cell symbol, as ratatui's diff measures it.
fn symbol_width(symbol: &str) -> usize {
    symbol.width()
}

pub struct WideCellBackend<B> {
    inner: B,
}

impl<B> WideCellBackend<B> {
    pub fn new(inner: B) -> Self {
        Self { inner }
    }
}

type Update<'a> = (u16, u16, &'a Cell);

/// Group draw updates into runs the inner backend can write with its own
/// cursor tracking.
fn runs<'a>(content: impl Iterator<Item = Update<'a>>) -> Vec<Vec<Update<'a>>> {
    let mut runs: Vec<Vec<Update<'a>>> = Vec::new();
    // Row and columns covered by the trailing cells of the last wide symbol.
    let mut covered: Option<(u16, u16, u16)> = None;
    for (x, y, cell) in content {
        let inside_wide =
            covered.is_some_and(|(row, from, to)| row == y && (from..to).contains(&x));
        if inside_wide && cell.symbol() == " " {
            continue;
        }
        match runs.last_mut() {
            Some(run) if !inside_wide => run.push((x, y, cell)),
            _ => runs.push(vec![(x, y, cell)]),
        }
        let width = symbol_width(cell.symbol()) as u16;
        covered = (width > 1).then(|| (y, x.saturating_add(1), x.saturating_add(width)));
    }
    runs
}

/// A symbol drawn as one cell and then widened by U+FE0F: the terminal takes
/// the cell after it only when the selector arrives.
fn widened_by_selector(cell: &Cell) -> bool {
    let symbol = cell.symbol();
    symbol.contains('\u{fe0f}') && symbol_width(symbol) > 1
}

/// Before each widened symbol, blank the cells it will take. Whatever wide
/// symbol was left there (the same emoji one column away, after a row shifts)
/// is then gone before the selector widens the new one over it; otherwise tmux
/// clears both when the next write lands on the old one. `blanks` holds one
/// blank per widened symbol, in order, carrying its style.
/// Tail cells at or past `width` are left out: there is nothing to clear.
fn with_cleared_tails<'a>(
    updates: &[Update<'a>],
    blanks: &'a [Cell],
    width: u16,
) -> Vec<Update<'a>> {
    let mut out = Vec::with_capacity(updates.len() + blanks.len());
    let mut blanks = blanks.iter();
    for &(x, y, cell) in updates {
        if widened_by_selector(cell)
            && let Some(blank) = blanks.next()
        {
            for tail in 1..symbol_width(cell.symbol()) as u16 {
                let column = x.saturating_add(tail);
                if column < width {
                    out.push((column, y, blank));
                }
            }
        }
        out.push((x, y, cell));
    }
    out
}

impl<B: Backend<Error = io::Error>> Backend for WideCellBackend<B> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let updates: Vec<Update<'a>> = content.collect();
        let blanks: Vec<Cell> = updates
            .iter()
            .filter(|(_, _, cell)| widened_by_selector(cell))
            .map(|(_, _, cell)| {
                let mut blank = (*cell).clone();
                blank.set_symbol(" ");
                blank
            })
            .collect();
        let width = self.inner.size()?.width;
        for run in runs(with_cleared_tails(&updates, &blanks, width).into_iter()) {
            self.inner.draw(run.into_iter())?;
        }
        Ok(())
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::display_width;
    use ratatui::backend::CrosstermBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn cell(symbol: &str) -> Cell {
        let mut cell = Cell::default();
        cell.set_symbol(symbol);
        cell
    }

    #[test]
    fn a_cell_right_after_a_wide_symbol_starts_a_new_run() {
        let (warning, blank, plus, next_row) =
            (cell("\u{26a0}\u{fe0f}"), cell(" "), cell("+"), cell("x"));
        let updates = vec![
            (3, 1, &warning),
            (4, 1, &blank),
            (5, 1, &plus),
            (0, 2, &next_row),
        ];
        let split: Vec<Vec<u16>> = runs(updates.into_iter())
            .into_iter()
            .map(|run| run.into_iter().map(|(x, _, _)| x).collect())
            .collect();
        // The blank inside the emoji is dropped; the inner backend moves the
        // cursor to column 5 itself, since 5 does not follow 3.
        assert_eq!(split, vec![vec![3, 5, 0]]);
    }

    #[test]
    fn other_content_inside_a_wide_symbol_gets_its_own_run() {
        let (warning, letter) = (cell("\u{26a0}\u{fe0f}"), cell("z"));
        let updates = vec![(3, 1, &warning), (4, 1, &letter)];
        assert_eq!(runs(updates.into_iter()).len(), 2);
    }

    #[test]
    fn the_cell_an_emoji_widens_into_is_blanked_first_with_its_style() {
        let style = ratatui::style::Style::default().bg(ratatui::style::Color::Blue);
        let mut warning = cell("\u{26a0}\u{fe0f}");
        warning.set_style(style);
        let (wide_cjk, letter) = (cell("\u{4e2d}"), cell("a"));
        let mut blank = warning.clone();
        blank.set_symbol(" ");
        let blanks = vec![blank.clone(), blank];
        let updates = vec![
            (3, 0, &warning),
            (6, 0, &wide_cjk),
            (8, 0, &letter),
            (9, 0, &warning),
        ];
        let out = with_cleared_tails(&updates, &blanks, 10);
        let order: Vec<(u16, &str)> = out.iter().map(|(x, _, cell)| (*x, cell.symbol())).collect();
        // The tail blank comes before its emoji; CJK without a selector and
        // narrow cells get none; an emoji in the last column has no tail to blank.
        assert_eq!(
            order,
            vec![
                (4, " "),
                (3, "\u{26a0}\u{fe0f}"),
                (6, "\u{4e2d}"),
                (8, "a"),
                (9, "\u{26a0}\u{fe0f}")
            ]
        );
        assert_eq!(out[0].2.bg, ratatui::style::Color::Blue);
    }

    #[test]
    fn narrow_runs_stay_whole() {
        let (a, b) = (cell("a"), cell("b"));
        let updates = vec![(0, 0, &a), (1, 0, &b), (2, 0, &a)];
        assert_eq!(runs(updates.into_iter()).len(), 1);
    }

    /// Column where each printable byte run starts, replaying cursor moves
    /// and treating ⚠️ as two cells, as tmux does.
    fn replay(bytes: &[u8]) -> Vec<(u16, String)> {
        let text = String::from_utf8_lossy(bytes);
        let mut out = Vec::new();
        let mut column = 0u16;
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\u{1b}' {
                let mut sequence = String::new();
                for next in chars.by_ref() {
                    sequence.push(next);
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
                if let Some(position) = sequence.strip_prefix('[').and_then(|s| s.strip_suffix('H'))
                {
                    let col = position
                        .split(';')
                        .nth(1)
                        .and_then(|n| n.parse::<u16>().ok())
                        .unwrap_or(1);
                    column = col - 1;
                }
                continue;
            }
            let mut symbol = c.to_string();
            if chars.peek() == Some(&'\u{fe0f}') {
                symbol.push(chars.next().unwrap());
            }
            out.push((column, symbol.clone()));
            column += display_width(&symbol) as u16;
        }
        out
    }

    #[derive(Clone, Default)]
    struct Captured(std::rc::Rc<std::cell::RefCell<Vec<u8>>>);

    impl io::Write for Captured {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Screen row after replaying `bytes` over `row`, as tmux keeps it: a wide
    /// symbol fills two cells, and writing over either half clears it.
    fn screen(row: &Buffer, bytes: &[u8]) -> Vec<String> {
        const TAIL: &str = "";
        let width = row.area.width as usize;
        let mut cells: Vec<String> = Vec::new();
        let mut x = 0;
        while x < width {
            let symbol = row[(x as u16, 0)].symbol().to_string();
            let cell_width = display_width(&symbol).max(1);
            cells.push(symbol);
            for _ in 1..cell_width {
                cells.push(TAIL.to_string());
            }
            x += cell_width;
        }
        cells.truncate(width);
        for (column, symbol) in replay(bytes) {
            let column = column as usize;
            let symbol_width = display_width(&symbol).max(1);
            for cell in column..(column + symbol_width).min(width) {
                if cells[cell] == TAIL && cell > 0 {
                    cells[cell - 1] = " ".to_string();
                }
                if cell + 1 < width && cells[cell + 1] == TAIL {
                    cells[cell + 1] = " ".to_string();
                }
            }
            cells[column] = symbol;
            for tail in 1..symbol_width {
                if column + tail < width {
                    cells[column + tail] = TAIL.to_string();
                }
            }
        }
        cells
            .into_iter()
            .map(|cell| if cell == TAIL { " ".to_string() } else { cell })
            .collect()
    }

    fn expected(row: &Buffer) -> Vec<String> {
        (0..row.area.width)
            .map(|x| row[(x, 0)].symbol().to_string())
            .collect()
    }

    #[test]
    fn redrawing_a_row_with_an_emoji_leaves_the_screen_as_rendered() {
        let area = Rect::new(0, 0, 12, 1);
        let style = ratatui::style::Style::default();
        let mut plain_corrupted = 0;
        for (before, after) in [
            ("ab  \u{26a0}\u{fe0f} +7 -1", "ab \u{26a0}\u{fe0f} +21 -2"),
            ("ab \u{26a0}\u{fe0f} +21 -2", "ab  \u{26a0}\u{fe0f} +7 -1"),
            ("ab \u{26a0}\u{fe0f} +21 -2", "ab     +70 -4"),
            ("ab     +70 -4", "ab \u{26a0}\u{fe0f} +21 -2"),
        ] {
            let mut previous = Buffer::empty(area);
            previous.set_string(0, 0, before, style);
            let mut next = Buffer::empty(area);
            next.set_string(0, 0, after, style);
            let diff = previous.diff(&next);

            let wrapped_out = Captured::default();
            let mut wrapped = WideCellBackend::new(CrosstermBackend::new(wrapped_out.clone()));
            wrapped.draw(diff.clone().into_iter()).unwrap();
            wrapped.flush().unwrap();
            assert_eq!(
                screen(&previous, &wrapped_out.0.borrow()),
                expected(&next),
                "{before:?} -> {after:?}"
            );

            // ratatui's own backend, for comparison.
            let plain_out = Captured::default();
            let mut plain = CrosstermBackend::new(plain_out.clone());
            plain.draw(diff.into_iter()).unwrap();
            Backend::flush(&mut plain).unwrap();
            if screen(&previous, &plain_out.0.borrow()) != expected(&next) {
                plain_corrupted += 1;
            }
        }
        // Without the wrapper at least one of these redraws corrupts the row.
        assert!(plain_corrupted > 0);
    }
}
