//! The terminal the console shows: a grid of cells changed by what the
//! program in it writes, xterm's control sequences read by `vte`.

use unicode_width::UnicodeWidthChar;
use vte::{Params, Perform};

/// A colour as the program set it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Color {
    #[default]
    Default,
    Index(u8),
    Rgb(u8, u8, u8),
}

pub const BOLD: u16 = 1;
pub const UNDERLINE: u16 = 2;
pub const REVERSE: u16 = 4;
pub const HIDDEN: u16 = 8;
pub const STRIKE: u16 = 16;
pub const DIM: u16 = 32;
pub const ITALIC: u16 = 64;
/// The first cell of a character two cells wide.
pub const WIDE: u16 = 256;
/// The second cell of a wide character; it draws nothing itself.
pub const TAIL: u16 = 512;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
    pub flags: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Cell {
            ch: ' ',
            fg: Color::Default,
            bg: Color::Default,
            flags: 0,
        }
    }
}

/// Colours and style of what is written next.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pen {
    pub fg: Color,
    pub bg: Color,
    pub flags: u16,
}

impl Pen {
    fn blank(self) -> Cell {
        Cell {
            bg: self.bg,
            ..Cell::default()
        }
    }
}

/// `text` laid out in cells drawn with `pen`, wide characters taking two.
pub fn cells(text: &str, pen: Pen) -> Vec<Cell> {
    let mut out = Vec::new();
    for ch in text.chars() {
        let width = ch.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        let cell = Cell {
            ch,
            fg: pen.fg,
            bg: pen.bg,
            flags: pen.flags,
        };
        if width == 2 {
            out.push(Cell {
                flags: pen.flags | WIDE,
                ..cell
            });
            out.push(Cell {
                ch: ' ',
                flags: pen.flags | TAIL,
                ..cell
            });
        } else {
            out.push(cell);
        }
    }
    out
}

/// The 16 basic colours.
const PALETTE: [u32; 16] = [
    0x1d1f21, 0xcc4b4b, 0x5fa85f, 0xd7a94b, 0x4f86c6, 0xb267b8, 0x3fa7b0, 0xc5c8c6, 0x666a6e,
    0xf06c6c, 0x8cd18c, 0xf0d070, 0x7aaef0, 0xd795dc, 0x6fd3db, 0xffffff,
];
pub const DEFAULT_FG: u32 = 0xd8dadc;
pub const DEFAULT_BG: u32 = 0x000000;

/// `0xRRGGBB` of colour `index` of the 256.
pub fn indexed(index: u8) -> u32 {
    match index {
        0..=15 => PALETTE[index as usize],
        16..=231 => {
            let i = index as u32 - 16;
            let level = |v: u32| if v == 0 { 0 } else { 55 + v * 40 };
            (level(i / 36) << 16) | (level(i / 6 % 6) << 8) | level(i % 6)
        }
        _ => {
            let v = 8 + (index as u32 - 232) * 10;
            (v << 16) | (v << 8) | v
        }
    }
}

/// `0xRRGGBB` of `color` as a foreground or background.
pub fn rgb(color: Color, foreground: bool) -> u32 {
    match color {
        Color::Default if foreground => DEFAULT_FG,
        Color::Default => DEFAULT_BG,
        Color::Index(i) => indexed(i),
        Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
    }
}

/// What DECSC saves.
#[derive(Clone, Copy, Debug, Default)]
struct Cursor {
    x: usize,
    y: usize,
    pen: Pen,
    /// The next character goes on the next line first.
    wrap: bool,
    origin: bool,
    /// G0 and G1 are DEC line drawing.
    graphics: [bool; 2],
    charset: usize,
}

pub struct Screen {
    cols: usize,
    rows: usize,
    lines: Vec<Vec<Cell>>,
    /// The screen not shown: the alternate one, or the main one while the
    /// alternate one is.
    other: Vec<Vec<Cell>>,
    alt: bool,
    cursor: Cursor,
    /// DECSC per screen: main, alternate.
    saved: [Cursor; 2],
    top: usize,
    bottom: usize,
    tabs: Vec<bool>,
    autowrap: bool,
    insert: bool,
    app_cursor: bool,
    show_cursor: bool,
    last: Option<char>,
    replies: Vec<u8>,
}

/// A terminal: the parser and the screen it writes.
pub struct Term {
    parser: vte::Parser,
    screen: Screen,
}

impl Term {
    pub fn new(cols: usize, rows: usize) -> Self {
        Term {
            parser: vte::Parser::new(),
            screen: Screen::new(cols.max(1), rows.max(1)),
        }
    }

    /// Reads what the program wrote.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.screen, bytes);
    }

    /// What the terminal answers the program's queries with.
    pub fn take_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.screen.replies)
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.screen.resize(cols.max(1), rows.max(1));
    }

    #[cfg(test)]
    pub fn cols(&self) -> usize {
        self.screen.cols
    }

    pub fn rows(&self) -> usize {
        self.screen.rows
    }

    pub fn line(&self, y: usize) -> &[Cell] {
        &self.screen.lines[y]
    }

    /// Where the cursor shows, unless the program hid it.
    pub fn cursor(&self) -> Option<(usize, usize)> {
        let c = &self.screen.cursor;
        self.screen.show_cursor.then_some((c.x, c.y))
    }

    /// Cursor keys send `ESC O` instead of `ESC [` (DECCKM).
    pub fn app_cursor(&self) -> bool {
        self.screen.app_cursor
    }

    /// Line `y` as text, for tests.
    #[cfg(test)]
    pub fn text(&self, y: usize) -> String {
        self.screen.lines[y]
            .iter()
            .filter(|c| c.flags & TAIL == 0)
            .map(|c| c.ch)
            .collect::<String>()
            .trim_end()
            .to_owned()
    }
}

/// Makes every wide character whole: a head with its tail.
fn fix_row(row: &mut [Cell]) {
    let len = row.len();
    for x in 0..len {
        let cell = row[x];
        if cell.flags & WIDE != 0 && (x + 1 >= len || row[x + 1].flags & TAIL == 0) {
            row[x].ch = ' ';
            row[x].flags &= !WIDE;
        }
        if cell.flags & TAIL != 0 && (x == 0 || row[x - 1].flags & WIDE == 0) {
            row[x].flags &= !TAIL;
            row[x].ch = ' ';
        }
    }
}

/// DEC special graphics, the line drawing set of `ESC ( 0`.
fn dec_graphics(c: char) -> char {
    const TABLE: &str = "◆▒␉␌␍␊°±␤␋┘┐┌└┼⎺⎻─⎼⎽├┤┴┬│≤≥π≠£·";
    match c {
        '_' => ' ',
        '`'..='~' => TABLE.chars().nth(c as usize - 0x60).unwrap_or(c),
        _ => c,
    }
}

impl Screen {
    fn new(cols: usize, rows: usize) -> Self {
        let blank = vec![vec![Cell::default(); cols]; rows];
        Screen {
            cols,
            rows,
            lines: blank.clone(),
            other: blank,
            alt: false,
            cursor: Cursor::default(),
            saved: [Cursor::default(); 2],
            top: 0,
            bottom: rows - 1,
            tabs: (0..cols).map(|x| x % 8 == 0).collect(),
            autowrap: true,
            insert: false,
            app_cursor: false,
            show_cursor: true,
            last: None,
            replies: Vec::new(),
        }
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        // Keep the cursor's line on the screen: the lines above it go.
        if self.cursor.y >= rows {
            let drop = self.cursor.y + 1 - rows;
            self.lines.drain(..drop);
            self.cursor.y -= drop;
        }
        for grid in [&mut self.lines, &mut self.other] {
            grid.resize(rows, vec![Cell::default(); cols]);
            for row in grid.iter_mut() {
                row.resize(cols, Cell::default());
                fix_row(row);
            }
        }
        self.cols = cols;
        self.rows = rows;
        self.top = 0;
        self.bottom = rows - 1;
        self.tabs = (0..cols).map(|x| x % 8 == 0).collect();
        for cursor in std::iter::once(&mut self.cursor).chain(self.saved.iter_mut()) {
            cursor.x = cursor.x.min(cols - 1);
            cursor.y = cursor.y.min(rows - 1);
            cursor.wrap = false;
        }
    }

    fn blank(&self) -> Cell {
        self.cursor.pen.blank()
    }

    fn blank_line(&self) -> Vec<Cell> {
        vec![self.blank(); self.cols]
    }

    /// Rows the cursor may move in: the scroll region in origin mode.
    fn bounds(&self) -> (usize, usize) {
        if self.cursor.origin {
            (self.top, self.bottom)
        } else {
            (0, self.rows - 1)
        }
    }

    fn goto(&mut self, x: usize, y: usize) {
        let (top, bottom) = self.bounds();
        self.cursor.x = x.min(self.cols - 1);
        self.cursor.y = y.clamp(top, bottom);
        self.cursor.wrap = false;
    }

    /// Moves to row `y` counted as the program sees it (from the region's
    /// top in origin mode).
    fn goto_row(&mut self, x: usize, y: usize) {
        let base = if self.cursor.origin { self.top } else { 0 };
        self.goto(x, base + y);
    }

    fn scroll_up(&mut self, n: usize) {
        let n = n.min(self.bottom - self.top + 1);
        self.lines[self.top..=self.bottom].rotate_left(n);
        for y in self.bottom + 1 - n..=self.bottom {
            self.lines[y] = self.blank_line();
        }
    }

    fn scroll_down(&mut self, n: usize) {
        let n = n.min(self.bottom - self.top + 1);
        self.lines[self.top..=self.bottom].rotate_right(n);
        for y in self.top..self.top + n {
            self.lines[y] = self.blank_line();
        }
    }

    fn linefeed(&mut self) {
        self.cursor.wrap = false;
        if self.cursor.y == self.bottom {
            self.scroll_up(1);
        } else if self.cursor.y + 1 < self.rows {
            self.cursor.y += 1;
        }
    }

    fn reverse_index(&mut self) {
        self.cursor.wrap = false;
        if self.cursor.y == self.top {
            self.scroll_down(1);
        } else if self.cursor.y > 0 {
            self.cursor.y -= 1;
        }
    }

    /// Lines up from the cursor, stopping at the region's top when it
    /// starts inside the region.
    fn up(&mut self, n: usize) {
        let limit = if self.cursor.y >= self.top {
            self.top
        } else {
            0
        };
        self.cursor.y = self.cursor.y.saturating_sub(n).max(limit);
        self.cursor.wrap = false;
    }

    fn down(&mut self, n: usize) {
        let limit = if self.cursor.y <= self.bottom {
            self.bottom
        } else {
            self.rows - 1
        };
        self.cursor.y = (self.cursor.y + n).min(limit);
        self.cursor.wrap = false;
    }

    fn erase(&mut self, y: usize, from: usize, to: usize) {
        let blank = self.blank();
        let row = &mut self.lines[y];
        let to = to.min(row.len());
        for cell in &mut row[from.min(to)..to] {
            *cell = blank;
        }
        fix_row(row);
    }

    fn print_char(&mut self, c: char) {
        let c = if self.cursor.graphics[self.cursor.charset] {
            dec_graphics(c)
        } else {
            c
        };
        let width = match c.width() {
            Some(w @ 1..=2) => w,
            _ => return,
        };
        if width > self.cols {
            return;
        }
        if self.cursor.wrap && self.autowrap {
            self.cursor.x = 0;
            self.linefeed();
        }
        if self.cursor.x + width > self.cols {
            if self.autowrap {
                self.cursor.x = 0;
                self.linefeed();
            } else {
                self.cursor.x = self.cols - width;
            }
        }
        let (x, y) = (self.cursor.x, self.cursor.y);
        let pen = self.cursor.pen;
        let row = &mut self.lines[y];
        if self.insert {
            let blank = pen.blank();
            row.splice(x..x, std::iter::repeat_n(blank, width));
            row.truncate(self.cols);
        }
        // Break the wide characters this overwrites.
        for i in x..x + width {
            if row[i].flags & TAIL != 0 && i > 0 {
                row[i - 1].ch = ' ';
                row[i - 1].flags &= !WIDE;
            }
            if row[i].flags & WIDE != 0 && i + 1 < row.len() {
                row[i + 1].ch = ' ';
                row[i + 1].flags &= !TAIL;
            }
        }
        let cell = Cell {
            ch: c,
            fg: pen.fg,
            bg: pen.bg,
            flags: pen.flags,
        };
        if width == 2 {
            row[x] = Cell {
                flags: pen.flags | WIDE,
                ..cell
            };
            row[x + 1] = Cell {
                ch: ' ',
                flags: pen.flags | TAIL,
                ..cell
            };
        } else {
            row[x] = cell;
        }
        if self.insert {
            fix_row(row);
        }
        self.last = Some(c);
        if x + width >= self.cols {
            self.cursor.x = self.cols - 1;
            self.cursor.wrap = self.autowrap;
        } else {
            self.cursor.x = x + width;
        }
    }

    fn save_cursor(&mut self) {
        self.saved[self.alt as usize] = self.cursor;
    }

    fn restore_cursor(&mut self) {
        self.cursor = self.saved[self.alt as usize];
        self.cursor.x = self.cursor.x.min(self.cols - 1);
        self.cursor.y = self.cursor.y.min(self.rows - 1);
    }

    fn switch_screen(&mut self, alt: bool) {
        if self.alt != alt {
            std::mem::swap(&mut self.lines, &mut self.other);
            self.alt = alt;
        }
    }

    fn clear_screen(&mut self) {
        let line = self.blank_line();
        for row in &mut self.lines {
            row.clone_from(&line);
        }
    }

    fn reset(&mut self) {
        *self = Screen::new(self.cols, self.rows);
    }

    fn set_mode(&mut self, private: bool, mode: u16, on: bool) {
        if !private {
            if mode == 4 {
                self.insert = on;
            }
            return;
        }
        match mode {
            1 => self.app_cursor = on,
            6 => {
                self.cursor.origin = on;
                self.goto_row(0, 0);
            }
            7 => self.autowrap = on,
            25 => self.show_cursor = on,
            47 => self.switch_screen(on),
            1047 => {
                if !on && self.alt {
                    self.clear_screen();
                }
                self.switch_screen(on);
            }
            1048 => {
                if on {
                    self.save_cursor();
                } else {
                    self.restore_cursor();
                }
            }
            1049 => {
                if on && !self.alt {
                    self.save_cursor();
                    self.switch_screen(true);
                    self.clear_screen();
                } else if !on && self.alt {
                    self.switch_screen(false);
                    self.restore_cursor();
                }
            }
            // Mouse reports, bracketed paste, focus events, synchronized
            // output, blinking: nothing to do.
            _ => {}
        }
    }

    fn sgr(&mut self, params: &Params) {
        let groups: Vec<&[u16]> = params.iter().collect();
        if groups.is_empty() {
            self.cursor.pen = Pen::default();
            return;
        }
        let pen = &mut self.cursor.pen;
        let mut i = 0;
        while i < groups.len() {
            let group = groups[i];
            let code = group[0];
            match code {
                0 => *pen = Pen::default(),
                1 => pen.flags |= BOLD,
                2 => pen.flags |= DIM,
                3 => pen.flags |= ITALIC,
                4 => {
                    // 4:0 is no underline; 4:n any other style.
                    if group.get(1) == Some(&0) {
                        pen.flags &= !UNDERLINE;
                    } else {
                        pen.flags |= UNDERLINE;
                    }
                }
                7 => pen.flags |= REVERSE,
                8 => pen.flags |= HIDDEN,
                9 => pen.flags |= STRIKE,
                21 => pen.flags |= UNDERLINE,
                22 => pen.flags &= !(BOLD | DIM),
                23 => pen.flags &= !ITALIC,
                24 => pen.flags &= !UNDERLINE,
                27 => pen.flags &= !REVERSE,
                28 => pen.flags &= !HIDDEN,
                29 => pen.flags &= !STRIKE,
                30..=37 => pen.fg = Color::Index((code - 30) as u8),
                39 => pen.fg = Color::Default,
                40..=47 => pen.bg = Color::Index((code - 40) as u8),
                49 => pen.bg = Color::Default,
                90..=97 => pen.fg = Color::Index((code - 90 + 8) as u8),
                100..=107 => pen.bg = Color::Index((code - 100 + 8) as u8),
                38 | 48 | 58 => {
                    let (color, used) = extended_color(&groups, i);
                    i += used;
                    match (code, color) {
                        (38, Some(c)) => pen.fg = c,
                        (48, Some(c)) => pen.bg = c,
                        _ => {}
                    }
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn csi(&mut self, params: &Params, intermediates: &[u8], action: char) {
        let p: Vec<u16> = params.iter().map(|g| g[0]).collect();
        // The nth parameter, `default` when absent or 0.
        let arg = |n: usize, default: usize| match p.get(n) {
            Some(&v) if v != 0 => v as usize,
            _ => default,
        };
        let private = intermediates.first() == Some(&b'?');
        let (x, y) = (self.cursor.x, self.cursor.y);
        match (intermediates, action) {
            ([], '@') => {
                let blank = self.blank();
                let n = arg(0, 1).min(self.cols - x);
                let row = &mut self.lines[y];
                row.splice(x..x, std::iter::repeat_n(blank, n));
                row.truncate(self.cols);
                fix_row(row);
                self.cursor.wrap = false;
            }
            ([], 'A') => self.up(arg(0, 1)),
            ([], 'B') | ([], 'e') => self.down(arg(0, 1)),
            ([], 'C') | ([], 'a') => {
                self.cursor.x = (x + arg(0, 1)).min(self.cols - 1);
                self.cursor.wrap = false;
            }
            ([], 'D') => {
                self.cursor.x = x.saturating_sub(arg(0, 1));
                self.cursor.wrap = false;
            }
            ([], 'E') => {
                self.down(arg(0, 1));
                self.cursor.x = 0;
            }
            ([], 'F') => {
                self.up(arg(0, 1));
                self.cursor.x = 0;
            }
            ([], 'G') | ([], '`') => {
                self.cursor.x = (arg(0, 1) - 1).min(self.cols - 1);
                self.cursor.wrap = false;
            }
            ([], 'H') | ([], 'f') => self.goto_row(arg(1, 1) - 1, arg(0, 1) - 1),
            ([], 'I') => {
                for _ in 0..arg(0, 1) {
                    self.tab();
                }
            }
            ([], 'J') | (b"?", 'J') => match p.first().copied().unwrap_or(0) {
                0 => {
                    self.erase(y, x, self.cols);
                    for row in y + 1..self.rows {
                        self.erase(row, 0, self.cols);
                    }
                }
                1 => {
                    for row in 0..y {
                        self.erase(row, 0, self.cols);
                    }
                    self.erase(y, 0, x + 1);
                }
                2 => self.clear_screen(),
                // 3 clears the scrollback, which there is none of.
                _ => {}
            },
            ([], 'K') | (b"?", 'K') => match p.first().copied().unwrap_or(0) {
                0 => self.erase(y, x, self.cols),
                1 => self.erase(y, 0, x + 1),
                2 => self.erase(y, 0, self.cols),
                _ => {}
            },
            ([], 'L') | ([], 'M') => {
                if (self.top..=self.bottom).contains(&y) {
                    let n = arg(0, 1).min(self.bottom - y + 1);
                    let region = &mut self.lines[y..=self.bottom];
                    if action == 'L' {
                        region.rotate_right(n);
                    } else {
                        region.rotate_left(n);
                    }
                    let blanks = if action == 'L' {
                        y..y + n
                    } else {
                        self.bottom + 1 - n..self.bottom + 1
                    };
                    for row in blanks {
                        self.lines[row] = self.blank_line();
                    }
                    self.cursor.x = 0;
                    self.cursor.wrap = false;
                }
            }
            ([], 'P') => {
                let n = arg(0, 1).min(self.cols - x);
                let blank = self.blank();
                let row = &mut self.lines[y];
                row.drain(x..x + n);
                row.extend(std::iter::repeat_n(blank, n));
                fix_row(row);
                self.cursor.wrap = false;
            }
            ([], 'S') => self.scroll_up(arg(0, 1)),
            ([], 'T') => self.scroll_down(arg(0, 1)),
            ([], 'X') => {
                self.erase(y, x, x + arg(0, 1));
                self.cursor.wrap = false;
            }
            ([], 'Z') => {
                for _ in 0..arg(0, 1) {
                    let back = (0..self.cursor.x).rev().find(|&i| self.tabs[i]);
                    self.cursor.x = back.unwrap_or(0);
                }
                self.cursor.wrap = false;
            }
            ([], 'b') => {
                if let Some(c) = self.last {
                    for _ in 0..arg(0, 1).min(self.cols * self.rows) {
                        self.print_char(c);
                    }
                }
            }
            ([], 'c') => {
                if p.first().copied().unwrap_or(0) == 0 {
                    // A VT220 with colour.
                    self.replies.extend_from_slice(b"\x1b[?62;22c");
                }
            }
            (b">", 'c') => self.replies.extend_from_slice(b"\x1b[>1;10;0c"),
            ([], 'd') => {
                let x = self.cursor.x;
                self.goto_row(x, arg(0, 1) - 1);
            }
            ([], 'g') => match p.first().copied().unwrap_or(0) {
                0 => self.tabs[x] = false,
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },
            ([], 'h') | (b"?", 'h') | ([], 'l') | (b"?", 'l') => {
                for &mode in &p {
                    self.set_mode(private, mode, action == 'h');
                }
            }
            ([], 'm') => self.sgr(params),
            ([], 'n') | (b"?", 'n') => match p.first() {
                Some(5) => self.replies.extend_from_slice(b"\x1b[0n"),
                Some(6) => {
                    let base = if self.cursor.origin { self.top } else { 0 };
                    let mark = if private { "?" } else { "" };
                    let reply = format!(
                        "\x1b[{mark}{};{}R",
                        self.cursor.y - base + 1,
                        self.cursor.x + 1
                    );
                    self.replies.extend_from_slice(reply.as_bytes());
                }
                _ => {}
            },
            ([], 'r') => {
                let top = arg(0, 1) - 1;
                let bottom = arg(1, self.rows).min(self.rows) - 1;
                if top < bottom {
                    self.top = top;
                    self.bottom = bottom;
                    self.goto_row(0, 0);
                }
            }
            ([], 's') => self.save_cursor(),
            ([], 'u') => self.restore_cursor(),
            ([], 't') if p.first() == Some(&18) => {
                let reply = format!("\x1b[8;{};{}t", self.rows, self.cols);
                self.replies.extend_from_slice(reply.as_bytes());
            }
            // Anything else: cursor style, keyboard protocol queries
            // (`CSI ? u` gets no answer, so the program knows there is
            // none), modifyOtherKeys, mode reports.
            _ => {}
        }
    }

    fn tab(&mut self) {
        let next = (self.cursor.x + 1..self.cols).find(|&i| self.tabs[i]);
        self.cursor.x = next.unwrap_or(self.cols - 1);
        self.cursor.wrap = false;
    }
}

/// The colour of `38;5;n`, `38;2;r;g;b` or their colon forms, starting at
/// group `i`, and how many further groups it took.
fn extended_color(groups: &[&[u16]], i: usize) -> (Option<Color>, usize) {
    let group = groups[i];
    if group.len() > 1 {
        // 38:5:n, 38:2:r:g:b or 38:2:space:r:g:b
        let color = match group[1] {
            5 => group.get(2).map(|&n| Color::Index(n as u8)),
            2 => {
                let rgb = if group.len() >= 6 {
                    &group[3..6]
                } else {
                    &group[2..]
                };
                (rgb.len() >= 3).then(|| Color::Rgb(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8))
            }
            _ => None,
        };
        return (color, 0);
    }
    let value = |n: usize| groups.get(i + n).map(|g| g[0]);
    match value(1) {
        Some(5) => (value(2).map(|n| Color::Index(n as u8)), 2),
        Some(2) => match (value(2), value(3), value(4)) {
            (Some(r), Some(g), Some(b)) => (Some(Color::Rgb(r as u8, g as u8, b as u8)), 4),
            _ => (None, groups.len()),
        },
        _ => (None, 1),
    }
}

impl Perform for Screen {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x08 => {
                self.cursor.x = self.cursor.x.saturating_sub(1);
                self.cursor.wrap = false;
            }
            0x09 => self.tab(),
            0x0a..=0x0c => self.linefeed(),
            0x0d => {
                self.cursor.x = 0;
                self.cursor.wrap = false;
            }
            0x0e => self.cursor.charset = 1,
            0x0f => self.cursor.charset = 0,
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: char) {
        if !ignore {
            self.csi(params, intermediates, action);
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (intermediates, byte) {
            ([], b'7') => self.save_cursor(),
            ([], b'8') => self.restore_cursor(),
            ([], b'D') => self.linefeed(),
            ([], b'E') => {
                self.cursor.x = 0;
                self.linefeed();
            }
            ([], b'M') => self.reverse_index(),
            ([], b'H') => self.tabs[self.cursor.x] = true,
            ([], b'c') => self.reset(),
            ([b'#'], b'8') => {
                for row in &mut self.lines {
                    row.fill(Cell {
                        ch: 'E',
                        ..Cell::default()
                    });
                }
            }
            ([b'('], set) | ([b')'], set) => {
                let g = (intermediates[0] == b')') as usize;
                self.cursor.graphics[g] = set == b'0';
            }
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], bell_terminated: bool) {
        // Colour queries get the default colours; titles and the rest are
        // ignored.
        if let [which @ (b"10" | b"11"), b"?"] = params {
            let color = if *which == b"10" {
                DEFAULT_FG
            } else {
                DEFAULT_BG
            };
            let part = |shift: u32| {
                let v = (color >> shift) & 0xff;
                v * 0x101
            };
            let end = if bell_terminated { "\x07" } else { "\x1b\\" };
            let reply = format!(
                "\x1b]{};rgb:{:04x}/{:04x}/{:04x}{end}",
                String::from_utf8_lossy(which),
                part(16),
                part(8),
                part(0)
            );
            self.replies.extend_from_slice(reply.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(cols: usize, rows: usize, input: &str) -> Term {
        let mut t = Term::new(cols, rows);
        t.advance(input.as_bytes());
        t
    }

    #[test]
    fn prints_and_moves_the_cursor() {
        let mut t = term(10, 4, "ab\r\ncd");
        assert_eq!(t.text(0), "ab");
        assert_eq!(t.text(1), "cd");
        assert_eq!(t.cursor(), Some((2, 1)));
        t.advance(b"\x1b[3;5HX\x1b[2AY\x1b[3CZ\x1b[10D!");
        assert_eq!(t.text(2), "    X");
        assert_eq!(t.text(0), "!b   Y   Z");
        assert_eq!(t.cursor(), Some((1, 0)));
        t.advance(b"\x1b[7G#\x1b[4d$\x1b[2E%\x1b[F&");
        assert_eq!(t.line(0)[6].ch, '#');
        assert_eq!(t.line(3)[7].ch, '$');
        // CNL stops at the last line; CPL goes to the line above.
        assert_eq!(t.line(3)[0].ch, '%');
        assert_eq!(t.line(2)[0].ch, '&');
        t.advance(b"\x1b[99;99H");
        assert_eq!(t.cursor(), Some((9, 3)));
        t.advance(b"\x08\x08\t");
        assert_eq!(t.cursor(), Some((8, 3)));
        t.advance(b"\r\tx");
        assert_eq!(t.line(3)[8].ch, 'x');
    }

    #[test]
    fn wraps_late() {
        let mut t = term(5, 3, "abcde");
        // The cursor waits on the last column until the next character.
        assert_eq!(t.cursor(), Some((4, 0)));
        assert_eq!(t.text(1), "");
        t.advance(b"f");
        assert_eq!(t.text(0), "abcde");
        assert_eq!(t.text(1), "f");
        // A carriage return cancels the pending wrap.
        let mut t = term(5, 3, "abcde\rX");
        assert_eq!(t.text(0), "Xbcde");
        // Without autowrap the last column is overwritten.
        t.advance(b"\x1b[?7l\x1b[1;1Hvwxyz12");
        assert_eq!(t.text(0), "vwxy2");
        assert_eq!(t.text(1), "");
    }

    #[test]
    fn scrolls_at_the_bottom_and_in_regions() {
        let t = term(4, 3, "1\r\n2\r\n3\r\n4");
        assert_eq!([t.text(0), t.text(1), t.text(2)], ["2", "3", "4"]);
        // Region rows 2-3: a line feed at its bottom scrolls only it.
        let mut t = term(4, 4, "a\r\nb\r\nc\r\nd");
        t.advance(b"\x1b[2;3r");
        assert_eq!(t.cursor(), Some((0, 0)));
        t.advance(b"\x1b[3;1H\nX");
        assert_eq!(
            [t.text(0), t.text(1), t.text(2), t.text(3)],
            ["a", "c", "X", "d"]
        );
        // Reverse index at the region's top scrolls it down.
        t.advance(b"\x1b[2;1H\x1bMY");
        assert_eq!(
            [t.text(0), t.text(1), t.text(2), t.text(3)],
            ["a", "Y", "c", "d"]
        );
        // SU and SD.
        t.advance(b"\x1b[S");
        assert_eq!([t.text(1), t.text(2)], ["c", ""]);
        t.advance(b"\x1b[2T");
        assert_eq!(
            [t.text(0), t.text(1), t.text(2), t.text(3)],
            ["a", "", "", "d"]
        );
        // Origin mode counts rows from the region's top.
        t.advance(b"\x1b[?6h\x1b[2;1HZ\x1b[6n");
        assert_eq!(t.text(2), "Z");
        assert_eq!(t.take_replies(), b"\x1b[2;2R");
        // The cursor cannot leave the region in origin mode.
        t.advance(b"\x1b[9;1H");
        assert_eq!(t.cursor(), Some((0, 2)));
    }

    #[test]
    fn inserts_and_deletes_lines_and_characters() {
        let mut t = term(6, 4, "aaaa\r\nbbbb\r\ncccc\r\ndddd");
        t.advance(b"\x1b[2;3H\x1b[L");
        assert_eq!(
            [t.text(0), t.text(1), t.text(2), t.text(3)],
            ["aaaa", "", "bbbb", "cccc"]
        );
        assert_eq!(t.cursor(), Some((0, 1)));
        t.advance(b"\x1b[2M");
        assert_eq!(
            [t.text(0), t.text(1), t.text(2), t.text(3)],
            ["aaaa", "cccc", "", ""]
        );
        t.advance(b"\x1b[1;2H\x1b[2@xy");
        assert_eq!(t.text(0), "axyaaa");
        t.advance(b"\x1b[1;2H\x1b[3P");
        assert_eq!(t.text(0), "aaa");
        t.advance(b"\x1b[1;1H\x1b[2X");
        assert_eq!(t.text(0), "  a");
        // Insert mode pushes the rest right.
        t.advance(b"\x1b[2;1H\x1b[4hZ\x1b[4l");
        assert_eq!(t.text(1), "Zcccc");
    }

    #[test]
    fn erases() {
        let mut t = term(5, 3, "abcde\r\nfghij\r\nklmno");
        t.advance(b"\x1b[2;3H\x1b[K");
        assert_eq!(t.text(1), "fg");
        t.advance(b"\x1b[1K");
        assert_eq!(t.text(1), "");
        t.advance(b"\x1b[1;3H\x1b[1J");
        assert_eq!([t.text(0), t.text(2)], ["   de", "klmno"]);
        t.advance(b"\x1b[3;2H\x1b[J");
        assert_eq!(t.text(2), "k");
        // Erased cells take the current background.
        t.advance(b"\x1b[44m\x1b[2J");
        assert!((0..3).all(|y| {
            t.line(y)
                .iter()
                .all(|c| c.bg == Color::Index(4) && c.ch == ' ')
        }));
    }

    #[test]
    fn alternate_screen_keeps_the_main_one() {
        let mut t = term(6, 3, "main\x1b[?1049h");
        assert_eq!(t.text(0), "");
        t.advance(b"\x1b[2;2Halt");
        assert_eq!(t.text(1), " alt");
        t.advance(b"\x1b[?1049l");
        assert_eq!(t.text(0), "main");
        assert_eq!(t.text(1), "");
        assert_eq!(t.cursor(), Some((4, 0)));
        // 47 switches without clearing; 1047 clears on the way out.
        t.advance(b"\x1b[?47h");
        assert_eq!(t.text(1), " alt");
        t.advance(b"\x1b[?47l\x1b[?1047h\x1b[?1047l\x1b[?47h");
        assert_eq!(t.text(1), "");
        t.advance(b"\x1b[?47l");
        assert_eq!(t.text(0), "main");
    }

    #[test]
    fn saves_and_restores_the_cursor() {
        let mut t = term(8, 4, "\x1b[2;3H\x1b[1m\x1b7\x1b[4;1H\x1b[0mx\x1b8y");
        assert_eq!(t.line(1)[2].ch, 'y');
        assert_eq!(t.line(1)[2].flags & BOLD, BOLD);
        t.advance(b"\x1b[3;3H\x1b[s\x1b[1;1H\x1b[uz");
        assert_eq!(t.line(2)[2].ch, 'z');
        t.advance(b"\x1b[?25l");
        assert_eq!(t.cursor(), None);
        t.advance(b"\x1b[?25h");
        assert!(t.cursor().is_some());
    }

    #[test]
    fn colours_and_styles() {
        let t = term(
            10,
            1,
            "\x1b[1;4;31mA\x1b[0;38;5;200;48;2;1;2;3mB\x1b[38:2::9:8:7;7mC\x1b[22;27;39;49;94mD\x1b[mE",
        );
        let line = t.line(0);
        assert_eq!(line[0].fg, Color::Index(1));
        assert_eq!(line[0].flags, BOLD | UNDERLINE);
        assert_eq!(line[1].fg, Color::Index(200));
        assert_eq!(line[1].bg, Color::Rgb(1, 2, 3));
        assert_eq!(line[1].flags, 0);
        assert_eq!(line[2].fg, Color::Rgb(9, 8, 7));
        assert_eq!(line[2].flags, REVERSE);
        assert_eq!(line[3].fg, Color::Index(12));
        assert_eq!(line[3].bg, Color::Default);
        assert_eq!(
            line[4],
            Cell {
                ch: 'E',
                ..Cell::default()
            }
        );
        assert_eq!(rgb(Color::Index(196), true), 0xff0000);
        assert_eq!(rgb(Color::Index(232), true), 0x080808);
        assert_eq!(rgb(Color::Index(16), true), 0);
    }

    #[test]
    fn wide_characters_take_two_cells() {
        let mut t = term(5, 2, "中a文");
        assert_eq!(t.line(0)[0].flags, WIDE);
        assert_eq!(t.line(0)[1].flags, TAIL);
        assert_eq!(t.text(0), "中a文");
        assert_eq!(t.cursor(), Some((4, 0)));
        // No room on the line: it goes to the next one.
        t.advance(b"\r\x1b[4C");
        t.advance("字".as_bytes());
        assert_eq!(t.text(1), "字");
        // Overwriting half of one blanks the other half.
        t.advance(b"\x1b[1;2Hx");
        assert_eq!(t.text(0), " xa文");
        assert_eq!(t.line(0)[0].flags & WIDE, 0);
        // Deleting a cell from the middle of one blanks it.
        t.advance(b"\x1b[1;5H\x1b[P");
        assert_eq!(t.text(0), " xa");
        assert!(t.line(0).iter().all(|c| c.flags & (WIDE | TAIL) == 0));
        // Zero-width characters are dropped.
        let t = term(5, 1, "e\u{301}x");
        assert_eq!(t.text(0), "ex");
    }

    #[test]
    fn answers_queries() {
        let mut t = term(20, 5, "\x1b[3;7H\x1b[6n\x1b[c\x1b[>c\x1b[5n");
        assert_eq!(
            t.take_replies(),
            b"\x1b[3;7R\x1b[?62;22c\x1b[>1;10;0c\x1b[0n".to_vec()
        );
        // Keyboard protocol and other probes get no answer; OSC colour
        // queries get the colours; nothing is left half-read.
        t.advance(b"\x1b[?u\x1b[>1u\x1b[?2026$p\x1b]0;title\x07\x1b]11;?\x1b\\\x1bP+q544e\x1b\\x");
        assert_eq!(
            t.take_replies(),
            b"\x1b]11;rgb:0000/0000/0000\x1b\\".to_vec()
        );
        assert_eq!(t.text(2), "      x");
        // Mouse, paste and focus modes are accepted and ignored.
        t.advance(b"\x1b[?1000;1002;1006;2004;1004hy");
        assert_eq!(t.text(2), "      xy");
        t.advance(b"\x1b[18t");
        assert_eq!(t.take_replies(), b"\x1b[8;5;20t".to_vec());
    }

    #[test]
    fn tabs_and_line_drawing() {
        let mut t = term(20, 2, "a\tb\x1b[3g\x1bH");
        assert_eq!(t.line(0)[8].ch, 'b');
        t.advance(b"\r\x1b[2C\x1bH\r\tc");
        assert_eq!(t.line(0)[2].ch, 'c');
        t.advance(b"\r\n\x1b(0lqk\x1b(Bq");
        assert_eq!(t.text(1), "┌─┐q");
        t.advance(b"\x1b[1;1H\x1b[5Z");
        assert_eq!(t.cursor(), Some((0, 0)));
    }

    #[test]
    fn resizes() {
        let mut t = term(6, 4, "1\r\n2\r\n3\r\n4中");
        t.resize(2, 2);
        assert_eq!([t.text(0), t.text(1)], ["3", "4"]);
        assert_eq!(t.cursor(), Some((1, 1)));
        t.resize(8, 3);
        assert_eq!(t.cols(), 8);
        assert_eq!(t.rows(), 3);
        t.advance(b"\x1b[3;8Hz");
        assert_eq!(t.line(2)[7].ch, 'z');
    }

    #[test]
    fn repeats_and_resets() {
        let mut t = term(8, 2, "ab\x1b[3b");
        assert_eq!(t.text(0), "abbbb");
        t.advance(b"\x1b[?1h");
        assert!(t.app_cursor());
        t.advance(b"\x1bc");
        assert!(!t.app_cursor());
        assert_eq!(t.text(0), "");
        assert_eq!(t.cursor(), Some((0, 0)));
    }
}
