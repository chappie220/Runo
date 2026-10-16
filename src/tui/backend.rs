//! A terminal backend that repaints whole rows instead of single cells.
//!
//! ratatui sends only the cells that changed, and after a double-width
//! character (Chinese, emoji) it skips the next cell, trusting the terminal
//! to have covered it. Some terminals, notably SSH clients on phones and
//! tablets, leave that cell's old character on screen, and their notion of
//! which characters are wide can differ from ours. Then fragments of earlier
//! frames pile up between the text. Here every row with a change is erased
//! and written again in full, and the cursor is placed explicitly after
//! anything but plain ASCII, so a row always ends up as drawn.

use std::io::{self, Write};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, IntoCrossterm, WindowSize};
use ratatui::buffer::{Buffer, Cell, CellDiffOption};
use ratatui::crossterm::cursor::MoveTo;
use ratatui::crossterm::queue;
use ratatui::crossterm::style::{Attribute, Colors, Print, SetAttribute, SetColors};
use ratatui::crossterm::terminal::{Clear, ClearType as Erase};
use ratatui::layout::{Position, Rect, Size};
use ratatui::style::{Color, Modifier};
use unicode_width::UnicodeWidthStr;

pub struct RowBackend<W: Write> {
    inner: CrosstermBackend<W>,
    /// What the screen should show, kept from every draw.
    screen: Buffer,
    /// The size to assume instead of asking the terminal (tests).
    fixed: Option<Size>,
    /// Where the cursor was last put, so clearing need not ask the
    /// terminal, a round trip that is slow over SSH.
    cursor: Position,
}

impl<W: Write> RowBackend<W> {
    pub fn new(writer: W) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            screen: Buffer::empty(Rect::ZERO),
            fixed: None,
            cursor: Position::ORIGIN,
        }
    }

    fn forget(&mut self) -> io::Result<()> {
        let size = self.size()?;
        self.screen = Buffer::empty(Rect::new(0, 0, size.width, size.height));
        Ok(())
    }

    /// Erases row `y` and writes it again from `screen`.
    fn repaint(&mut self, y: u16) -> io::Result<()> {
        let width = self.screen.area.width;
        let w = &mut self.inner;
        // Erased cells take the current colors, so reset them first.
        queue!(
            w,
            SetAttribute(Attribute::Reset),
            MoveTo(0, y),
            Clear(Erase::UntilNewLine)
        )?;
        let mut style: Option<(Color, Color, Modifier)> = None;
        // Where the terminal's cursor is known to be.
        let mut cursor = Some(0);
        let mut x = 0;
        while x < width {
            let cell = &self.screen[(x, y)];
            let symbol = cell.symbol();
            let cols = symbol.width().max(1) as u16;
            if cell.diff_option == CellDiffOption::Skip {
                x += 1;
                cursor = None;
                continue;
            }
            // Erased cells are already blank in the default style.
            let blank = symbol == " "
                && cell.bg == Color::Reset
                && !cell
                    .modifier
                    .intersects(Modifier::REVERSED | Modifier::UNDERLINED);
            if blank {
                x += 1;
                cursor = None;
                continue;
            }
            if cursor != Some(x) {
                queue!(w, MoveTo(x, y))?;
            }
            let wanted = (cell.fg, cell.bg, cell.modifier);
            if style != Some(wanted) {
                if style.is_none_or(|(_, _, modifier)| modifier != cell.modifier) {
                    queue!(w, SetAttribute(Attribute::Reset))?;
                    for attribute in attributes(cell.modifier) {
                        queue!(w, SetAttribute(attribute))?;
                    }
                }
                queue!(
                    w,
                    SetColors(Colors::new(
                        cell.fg.into_crossterm(),
                        cell.bg.into_crossterm()
                    ))
                )?;
                style = Some(wanted);
            }
            queue!(w, Print(symbol))?;
            // Only plain ASCII moves every terminal's cursor by exactly one.
            cursor = (symbol.len() == 1 && cols == 1).then_some(x + 1);
            x += cols;
        }
        queue!(w, SetAttribute(Attribute::Reset))
    }
}

fn attributes(modifier: Modifier) -> impl Iterator<Item = Attribute> {
    [
        (Modifier::BOLD, Attribute::Bold),
        (Modifier::DIM, Attribute::Dim),
        (Modifier::ITALIC, Attribute::Italic),
        (Modifier::UNDERLINED, Attribute::Underlined),
        (Modifier::SLOW_BLINK, Attribute::SlowBlink),
        (Modifier::RAPID_BLINK, Attribute::RapidBlink),
        (Modifier::REVERSED, Attribute::Reverse),
        (Modifier::HIDDEN, Attribute::Hidden),
        (Modifier::CROSSED_OUT, Attribute::CrossedOut),
    ]
    .into_iter()
    .filter(move |(flag, _)| modifier.contains(*flag))
    .map(|(_, attribute)| attribute)
}

impl<W: Write> Backend for RowBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let size = self.size()?;
        if self.screen.area != Rect::new(0, 0, size.width, size.height) {
            self.forget()?;
        }
        let mut rows = Vec::new();
        for (x, y, cell) in content {
            if x >= self.screen.area.width || y >= self.screen.area.height {
                continue;
            }
            self.screen[(x, y)] = cell.clone();
            // ratatui leaves out the cells a wide character covers: they are
            // blank in its frame, so blank them here, or what they held
            // before comes back once the wide character is gone.
            let end = x.saturating_add(cell.symbol().width() as u16);
            for covered in x + 1..end.min(self.screen.area.width) {
                self.screen[(covered, y)].reset();
            }
            if rows.last() != Some(&y) {
                rows.push(y);
            }
        }
        rows.sort_unstable();
        rows.dedup();
        for y in rows {
            self.repaint(y)?;
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        Ok(self.cursor)
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.cursor = position.into();
        self.inner.set_cursor_position(self.cursor)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.forget()?;
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        if clear_type == ClearType::All {
            self.forget()?;
        }
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> io::Result<Size> {
        match self.fixed {
            Some(size) => Ok(size),
            None => self.inner.size(),
        }
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use ratatui::Terminal;
    use ratatui::style::{Style, Stylize};
    use ratatui::text::Line;

    use super::*;

    #[derive(Clone, Default)]
    struct Output(Rc<RefCell<Vec<u8>>>);

    impl Write for Output {
        fn write(&mut self, data: &[u8]) -> io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(data);
            Ok(data.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Output {
        fn take(&self) -> String {
            String::from_utf8(std::mem::take(&mut *self.0.borrow_mut())).unwrap()
        }
    }

    fn terminal(width: u16, height: u16) -> (Terminal<RowBackend<Output>>, Output) {
        let out = Output::default();
        let mut backend = RowBackend::new(out.clone());
        backend.fixed = Some(Size::new(width, height));
        (Terminal::new(backend).unwrap(), out)
    }

    fn show(terminal: &mut Terminal<RowBackend<Output>>, rows: &[Line<'static>]) {
        terminal
            .draw(|frame| {
                for (y, row) in rows.iter().enumerate() {
                    let area = Rect::new(0, y as u16, frame.area().width, 1);
                    frame.render_widget(row.clone(), area);
                }
            })
            .unwrap();
    }

    #[test]
    fn a_changed_row_is_erased_and_written_whole() {
        let (mut terminal, out) = terminal(12, 2);
        show(&mut terminal, &["qq.allow".into(), "keep".into()]);
        out.take();
        // Wide characters over ASCII: ratatui alone would skip every other cell.
        show(&mut terminal, &["你好世界".into(), "keep".into()]);
        let written = out.take();
        assert!(written.contains("\x1b[1;1H\x1b[K"), "{written:?}");
        // Placed explicitly after each wide character.
        for (column, c) in [(3, '好'), (5, '世'), (7, '界')] {
            assert!(
                written.contains(&format!("\x1b[1;{column}H{c}")),
                "{written:?}"
            );
        }
        // The untouched row is not sent again.
        assert!(!written.contains("keep"), "{written:?}");
    }

    #[test]
    fn styles_and_plain_runs_are_written_compactly() {
        let (mut terminal, out) = terminal(20, 1);
        out.take();
        let line = Line::from(vec!["ab".into(), "cd".bold(), " ".into(), "ef".on_blue()])
            .style(Style::new());
        show(&mut terminal, &[line]);
        let written = out.take();
        // ASCII runs need no cursor moves between characters.
        assert!(written.contains("ab"), "{written:?}");
        assert!(written.contains("\x1b[1m"), "{written:?}");
        assert!(written.contains("cd"), "{written:?}");
        assert!(written.contains("ef"), "{written:?}");
        assert_eq!(written.matches("\x1b[1;").count(), 2, "{written:?}");
    }

    #[test]
    fn clearing_forgets_the_screen() {
        let (mut terminal, out) = terminal(8, 1);
        show(&mut terminal, &["same".into()]);
        out.take();
        show(&mut terminal, &["same".into()]);
        assert!(!out.take().contains("same"));
        terminal.clear().unwrap();
        show(&mut terminal, &["same".into()]);
        assert!(out.take().contains("same"));
    }

    #[test]
    fn text_under_a_wide_character_does_not_come_back() {
        let (mut terminal, out) = terminal(8, 1);
        show(&mut terminal, &["Gateway!".into()]);
        // ratatui sends only 你, 好 and 世 here, not the cells they cover.
        show(&mut terminal, &["你好世!".into()]);
        out.take();
        show(&mut terminal, &["".into()]);
        let written = out.take();
        for c in ['a', 'e', 'w', 'y'] {
            assert!(!written.contains(c), "{written:?}");
        }
    }
}
