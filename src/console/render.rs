//! Drawing the terminal into pixels: cells, glyphs, line drawing, the
//! cursor and the bar, redrawing only the rows that changed.

use std::ops::Range;

use super::font::Font;
use super::term::{self, Cell, Term};

/// The image the console draws, `0xRRGGBB` per pixel; the framebuffer gets
/// copies of its changed rows.
pub struct Canvas {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<u32>,
}

impl Canvas {
    pub fn new(width: usize, height: usize) -> Canvas {
        Canvas {
            width,
            height,
            pixels: vec![term::DEFAULT_BG; width * height],
        }
    }

    fn fill(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let x1 = (x + w).min(self.width);
        for row in y..(y + h).min(self.height) {
            let start = row * self.width;
            if x < x1 {
                self.pixels[start + x..start + x1].fill(color);
            }
        }
    }

    /// Mixes `color` into the pixel at `alpha` of 255.
    fn blend(&mut self, x: usize, y: usize, color: u32, alpha: u8) {
        if let Some(pixel) = self.pixels.get_mut(y * self.width + x) {
            *pixel = mix(*pixel, color, alpha);
        }
    }
}

fn mix(under: u32, over: u32, alpha: u8) -> u32 {
    let a = alpha as u32;
    let channel = |shift: u32| {
        let u = (under >> shift) & 0xff;
        let o = (over >> shift) & 0xff;
        ((u * (255 - a) + o * a + 127) / 255) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// Where the cursor is, in the rows drawn.
const CURSOR: u16 = 0x8000;

pub struct Renderer {
    pub font: Font,
    width: usize,
    height: usize,
    cols: usize,
    /// Rows on the screen: the terminal's and the bar.
    rows: usize,
    left: usize,
    top: usize,
    /// What each row shows now.
    shown: Vec<Vec<Cell>>,
}

impl Renderer {
    pub fn new(font: Font, width: usize, height: usize) -> Renderer {
        let mut renderer = Renderer {
            font,
            width: 0,
            height: 0,
            cols: 0,
            rows: 0,
            left: 0,
            top: 0,
            shown: Vec::new(),
        };
        renderer.resize(width, height);
        renderer
    }

    /// Lays the cells out on a screen of `width` × `height`, centred; all
    /// is drawn again.
    pub fn resize(&mut self, width: usize, height: usize) {
        self.width = width;
        self.height = height;
        self.cols = (width / self.font.cell_w).max(1);
        self.rows = (height / self.font.cell_h).max(2);
        self.left = width.saturating_sub(self.cols * self.font.cell_w) / 2;
        self.top = height.saturating_sub(self.rows * self.font.cell_h) / 2;
        self.shown.clear();
    }

    /// The terminal's size: the screen's but for the bar's row.
    pub fn term_size(&self) -> (usize, usize) {
        (self.cols, self.rows - 1)
    }

    /// Draws what changed since the last call; returns the pixel rows to
    /// copy to the screen.
    pub fn draw(&mut self, term: &Term, bar: &[Cell], canvas: &mut Canvas) -> Vec<Range<usize>> {
        let mut damage: Vec<Range<usize>> = Vec::new();
        if self.shown.len() != self.rows {
            canvas.pixels.fill(term::DEFAULT_BG);
            self.shown = vec![Vec::new(); self.rows];
            damage.push(0..canvas.height);
        }
        let cursor = term.cursor();
        for y in 0..self.rows {
            let mut row: Vec<Cell> = if y + 1 == self.rows {
                bar.to_vec()
            } else if y < term.rows() {
                term.line(y).to_vec()
            } else {
                Vec::new()
            };
            row.resize(self.cols, Cell::default());
            if let Some((x, cy)) = cursor
                && cy == y
                && x < row.len()
            {
                let x = if row[x].flags & term::TAIL != 0 && x > 0 {
                    x - 1
                } else {
                    x
                };
                row[x].flags |= CURSOR;
            }
            if row == self.shown[y] {
                continue;
            }
            self.draw_row(y, &row, canvas);
            self.shown[y] = row;
            let top = self.top + y * self.font.cell_h;
            let rows = top..(top + self.font.cell_h).min(canvas.height);
            match damage.last_mut() {
                Some(last) if last.end >= rows.start => last.end = last.end.max(rows.end),
                _ => damage.push(rows),
            }
        }
        damage
    }

    fn draw_row(&mut self, y: usize, row: &[Cell], canvas: &mut Canvas) {
        let (cell_w, cell_h) = (self.font.cell_w, self.font.cell_h);
        let y0 = self.top + y * cell_h;
        let colors: Vec<(u32, u32)> = (0..row.len())
            .map(|x| {
                // A wide character's second half takes the first's colours.
                let head = if row[x].flags & term::TAIL != 0 && x > 0 {
                    x - 1
                } else {
                    x
                };
                colors(&row[head])
            })
            .collect();
        for (x, &(_, bg)) in colors.iter().enumerate() {
            canvas.fill(self.left + x * cell_w, y0, cell_w, cell_h, bg);
        }
        for (x, cell) in row.iter().enumerate() {
            if cell.flags & term::TAIL != 0 {
                continue;
            }
            let (fg, _) = colors[x];
            let cells = if cell.flags & term::WIDE != 0 { 2 } else { 1 };
            let x0 = self.left + x * cell_w;
            let span = cells * cell_w;
            if cell.ch != ' ' && cell.flags & term::HIDDEN == 0 {
                let drawn = cells == 1 && draw_shape(canvas, cell.ch, x0, y0, cell_w, cell_h, fg);
                if !drawn {
                    let bold = cell.flags & term::BOLD != 0;
                    let glyph = self.font.glyph(cell.ch, cells);
                    for dx in 0..=bold as i32 {
                        draw_glyph(canvas, glyph, x0, y0, span, cell_h, dx, fg);
                    }
                }
            }
            let thickness = (cell_h / 20).max(1);
            if cell.flags & term::UNDERLINE != 0 {
                let at = (self.font.baseline + thickness + 1).min(cell_h - thickness);
                canvas.fill(x0, y0 + at, span, thickness, fg);
            }
            if cell.flags & term::STRIKE != 0 {
                canvas.fill(x0, y0 + cell_h / 2, span, thickness, fg);
            }
        }
    }
}

/// Foreground and background of a cell as drawn.
fn colors(cell: &Cell) -> (u32, u32) {
    let mut fg = term::rgb(cell.fg, true);
    let mut bg = term::rgb(cell.bg, false);
    if cell.flags & term::DIM != 0 {
        fg = mix(fg, bg, 128);
    }
    if (cell.flags & term::REVERSE != 0) != (cell.flags & CURSOR != 0) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.flags & term::HIDDEN != 0 {
        fg = bg;
    }
    (fg, bg)
}

#[allow(clippy::too_many_arguments)]
fn draw_glyph(
    canvas: &mut Canvas,
    glyph: &super::font::Glyph,
    x0: usize,
    y0: usize,
    span: usize,
    cell_h: usize,
    dx: i32,
    color: u32,
) {
    for gy in 0..glyph.height {
        let y = glyph.top + gy as i32;
        if y < 0 || y as usize >= cell_h {
            continue;
        }
        for gx in 0..glyph.width {
            let alpha = glyph.coverage[gy * glyph.width + gx];
            let x = glyph.left + gx as i32 + dx;
            if alpha == 0 || x < 0 || x as usize >= span {
                continue;
            }
            canvas.blend(x0 + x as usize, y0 + y as usize, color, alpha);
        }
    }
}

/// Weights of the lines from the centre of a box drawing character: up,
/// right, down, left; 1 light, 2 heavy, 3 double.
fn box_lines(c: char) -> Option<[u8; 4]> {
    const TABLE: &str = "\
        0101 0202 1010 2020 0101 0202 1010 2020 0101 0202 1010 2020 0110 0210 0120 0220 \
        0011 0012 0021 0022 1100 1200 2100 2200 1001 1002 2001 2002 1110 1210 2110 1120 \
        2120 2210 1220 2220 1011 1012 2011 1021 2021 2012 1022 2022 0111 0112 0211 0212 \
        0121 0122 0221 0222 1101 1102 1201 1202 2101 2102 2201 2202 1111 1112 1211 1212 \
        2111 1121 2121 2112 2211 1122 1221 2212 1222 2122 2221 2222 0101 0202 1010 2020 \
        0303 3030 0310 0130 0330 0013 0031 0033 1300 3100 3300 1003 3001 3003 1310 3130 \
        3330 1013 3031 3033 0313 0131 0333 1303 3101 3303 1313 3131 3333 0110 0011 1001 \
        1100 ---- ---- ---- 0001 1000 0100 0010 0002 2000 0200 0020 0201 1020 0102 2010 ";
    let i = (c as u32).checked_sub(0x2500)? as usize;
    let group = TABLE.as_bytes().get(i * 5..i * 5 + 4)?;
    if group[0] == b'-' {
        return None;
    }
    Some([0, 1, 2, 3].map(|k| group[k] - b'0'))
}

/// Draws box drawing and block characters as shapes that join their
/// neighbours, instead of the font's glyphs; false for other characters.
fn draw_shape(
    canvas: &mut Canvas,
    c: char,
    x0: usize,
    y0: usize,
    w: usize,
    h: usize,
    color: u32,
) -> bool {
    if let Some([up, right, down, left]) = box_lines(c) {
        let light = w.div_ceil(8).max(1);
        // How wide a line of `weight` is, all strokes included.
        let extent = |weight: u8| match weight {
            0 => 0,
            1 => light,
            2 => light * 2,
            _ => light * 3,
        };
        let (cx, cy) = (x0 + w / 2, y0 + h / 2);
        // Arms reach past the centre to the far side of the crossing line.
        let reach_h = extent(up.max(down)).div_ceil(2);
        let reach_v = extent(left.max(right)).div_ceil(2);
        // Strokes of one arm across its width: (offset, thickness).
        let strokes = |weight: u8| -> Vec<(isize, usize)> {
            match weight {
                0 => vec![],
                1 => vec![(-(light as isize) / 2, light)],
                2 => vec![(-(light as isize), light * 2)],
                _ => vec![
                    (-(light as isize * 3 / 2), light),
                    (light as isize * 2 - light as isize * 3 / 2, light),
                ],
            }
        };
        let at = |base: usize, offset: isize| (base as isize + offset).max(0) as usize;
        for (offset, t) in strokes(left) {
            canvas.fill(x0, at(cy, offset), cx + reach_h - x0, t, color);
        }
        for (offset, t) in strokes(right) {
            let start = cx.saturating_sub(reach_h);
            canvas.fill(start, at(cy, offset), x0 + w - start, t, color);
        }
        for (offset, t) in strokes(up) {
            canvas.fill(at(cx, offset), y0, t, cy + reach_v - y0, color);
        }
        for (offset, t) in strokes(down) {
            let start = cy.saturating_sub(reach_v);
            canvas.fill(at(cx, offset), start, t, y0 + h - start, color);
        }
        return true;
    }
    let code = c as u32;
    let eighths = |n: u32, of: usize| of * n as usize / 8;
    match code {
        0x2800..=0x28ff => {
            // Braille: dots 1-3 and 7 down the left, 4-6 and 8 the right.
            const DOTS: [(usize, usize); 8] = [
                (0, 0),
                (0, 1),
                (0, 2),
                (1, 0),
                (1, 1),
                (1, 2),
                (0, 3),
                (1, 3),
            ];
            let size = (w / 4).max(1);
            for (bit, (col, row)) in DOTS.iter().enumerate() {
                if code & (1 << bit) != 0 {
                    let x = x0 + w * (2 * col + 1) / 4 - size / 2;
                    let y = y0 + h * (2 * row + 1) / 8 - size / 2;
                    canvas.fill(x, y, size, size, color);
                }
            }
        }
        0x2580 => canvas.fill(x0, y0, w, h / 2, color),
        0x2581..=0x2588 => {
            let fill = eighths(code - 0x2580, h);
            canvas.fill(x0, y0 + h - fill, w, fill, color);
        }
        0x2589..=0x258f => canvas.fill(x0, y0, eighths(0x2590 - code, w), h, color),
        0x2590 => canvas.fill(x0 + w / 2, y0, w - w / 2, h, color),
        0x2591..=0x2593 => {
            let alpha = (code - 0x2590) as u8 * 64;
            for y in y0..y0 + h {
                for x in x0..x0 + w {
                    canvas.blend(x, y, color, alpha);
                }
            }
        }
        0x2594 => canvas.fill(x0, y0, w, eighths(1, h), color),
        0x2595 => {
            let fill = eighths(1, w);
            canvas.fill(x0 + w - fill, y0, fill, h, color);
        }
        0x2596..=0x259f => {
            // Quadrants: upper left 1, upper right 2, lower left 4, lower right 8.
            const QUADRANTS: [u8; 10] = [4, 8, 1, 13, 9, 7, 11, 2, 6, 14];
            let quadrants = QUADRANTS[(code - 0x2596) as usize];
            let (hw, hh) = (w / 2, h / 2);
            for (bit, qx, qy, qw, qh) in [
                (1, x0, y0, hw, hh),
                (2, x0 + hw, y0, w - hw, hh),
                (4, x0, y0 + hh, hw, h - hh),
                (8, x0 + hw, y0 + hh, w - hw, h - hh),
            ] {
                if quadrants & bit != 0 {
                    canvas.fill(qx, qy, qw, qh, color);
                }
            }
        }
        _ => return false,
    }
    true
}

/// How the framebuffer stores a pixel: bytes per pixel and each channel's
/// (offset, length) in bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Format {
    pub bytes: usize,
    pub red: (u32, u32),
    pub green: (u32, u32),
    pub blue: (u32, u32),
    /// Set to opaque when there is one.
    pub alpha: (u32, u32),
}

impl Format {
    pub const XRGB8888: Format = Format {
        bytes: 4,
        red: (16, 8),
        green: (8, 8),
        blue: (0, 8),
        alpha: (0, 0),
    };
    pub const RGB565: Format = Format {
        bytes: 2,
        red: (11, 5),
        green: (5, 6),
        blue: (0, 5),
        alpha: (0, 0),
    };

    /// `0xRRGGBB` as this format's pixel value.
    pub fn pack(&self, rgb: u32) -> u32 {
        let channel = |value: u32, (offset, length): (u32, u32)| {
            if length == 0 {
                0
            } else {
                let length = length.min(8);
                (value >> (8 - length)) << offset
            }
        };
        let alpha = if self.alpha.1 == 0 {
            0
        } else {
            ((1u32 << self.alpha.1.min(8)) - 1) << self.alpha.0
        };
        channel((rgb >> 16) & 0xff, self.red)
            | channel((rgb >> 8) & 0xff, self.green)
            | channel(rgb & 0xff, self.blue)
            | alpha
    }

    /// Converts one row of the canvas into the framebuffer's bytes.
    pub fn write_row(&self, src: &[u32], dst: &mut [u8]) {
        let bytes = self.bytes;
        if *self == Format::XRGB8888 {
            for (pixel, out) in src.iter().zip(dst.as_chunks_mut::<4>().0) {
                out.copy_from_slice(&pixel.to_ne_bytes());
            }
            return;
        }
        for (pixel, out) in src.iter().zip(dst.chunks_exact_mut(bytes)) {
            let value = self.pack(*pixel);
            let all = value.to_ne_bytes();
            if cfg!(target_endian = "little") {
                out.copy_from_slice(&all[..bytes]);
            } else {
                out.copy_from_slice(&all[4 - bytes..]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::console::ime::{self, Typing};
    use crate::console::keys::Sym;
    use crate::i18n::Lang;

    #[test]
    fn packs_pixels() {
        assert_eq!(Format::XRGB8888.pack(0x123456), 0x123456);
        assert_eq!(Format::RGB565.pack(0xffffff), 0xffff);
        assert_eq!(Format::RGB565.pack(0xff0000), 0xf800);
        assert_eq!(Format::RGB565.pack(0x00ff00), 0x07e0);
        assert_eq!(Format::RGB565.pack(0x0000ff), 0x001f);
        let bgr = Format {
            red: (0, 8),
            blue: (16, 8),
            alpha: (24, 8),
            ..Format::XRGB8888
        };
        assert_eq!(bgr.pack(0x112233), 0xff332211);
        let mut out = [0u8; 4];
        Format::RGB565.write_row(&[0xff0000, 0x0000ff], &mut out);
        assert_eq!(out, [0x00, 0xf8, 0x1f, 0x00]);
        let mut out = [0u8; 8];
        Format::XRGB8888.write_row(&[0x010203, 0xa0b0c0], &mut out);
        assert_eq!(out, [3, 2, 1, 0, 0xc0, 0xb0, 0xa0, 0]);
    }

    #[test]
    fn box_drawing_table() {
        assert_eq!(box_lines('─'), Some([0, 1, 0, 1]));
        assert_eq!(box_lines('┌'), Some([0, 1, 1, 0]));
        assert_eq!(box_lines('┼'), Some([1, 1, 1, 1]));
        assert_eq!(box_lines('╋'), Some([2, 2, 2, 2]));
        assert_eq!(box_lines('╔'), Some([0, 3, 3, 0]));
        assert_eq!(box_lines('╯'), Some([1, 0, 0, 1]));
        assert_eq!(box_lines('╿'), Some([2, 0, 1, 0]));
        assert_eq!(box_lines('╱'), None);
        assert_eq!(box_lines('a'), None);
    }

    #[test]
    fn shapes_join_their_neighbours() {
        let mut canvas = Canvas::new(30, 20);
        // A horizontal line through three cells is unbroken.
        for i in 0..3 {
            assert!(draw_shape(&mut canvas, '─', i * 10, 0, 10, 20, 0xffffff));
        }
        let row = canvas.pixels[10 * 30..11 * 30].to_vec();
        assert!(row.iter().all(|&p| p == 0xffffff), "{row:x?}");
        assert!(!draw_shape(&mut canvas, 'x', 0, 0, 10, 20, 0xffffff));
        let mut canvas = Canvas::new(10, 20);
        draw_shape(&mut canvas, '▄', 0, 0, 10, 20, 0xffffff);
        assert_eq!(canvas.pixels[9 * 10], term::DEFAULT_BG);
        assert_eq!(canvas.pixels[10 * 10], 0xffffff);
    }

    /// Draws a screen with Chinese, colours, a box and the input method's
    /// bar, and writes it to `runo-console.png` in the temporary directory
    /// to look at. Needs the font (see `font::for_tests`).
    #[test]
    fn draws_a_screen() {
        let Some(font) = crate::console::font::for_tests(24.0) else {
            return;
        };
        let (width, height) = (960, 400);
        let mut renderer = Renderer::new(font, width, height);
        let (cols, rows) = renderer.term_size();
        let mut term = Term::new(cols, rows);
        term.advance(
            "\x1b[1;1H\x1b[1;37;44m Runo \x1b[0m 你好，世界！Hello, world.\r\n\
             \x1b[31mred\x1b[32m green\x1b[38;5;214m orange\x1b[38;2;120;160;255m rgb\
             \x1b[0m \x1b[7mreverse\x1b[0m \x1b[4munderline\x1b[0m \x1b[1mbold 粗体\x1b[0m\r\n\
             ┌──────────┬─────┐\r\n│ 中文显示 │ abc │\r\n╞══════════╪═════╡\r\n\
             └──────────┴─────┘ ▁▂▃▄▅▆▇█ ░▒▓\r\n\
             → … “引号” ± ≤ ≥ ½ ⠋⠙⠹⣿ ✓✗›▸▰▱ ~/runo$ "
                .as_bytes(),
        );
        let fake = ime::tests::Fake::default();
        let mut typing = Typing::new(Some(Box::new(fake)));
        typing.toggle();
        for c in "ni".chars() {
            typing.press(&ime::tests::press(Sym::Char(c)), false);
        }
        let bar = typing.bar(cols, Lang::Zh, None);
        let mut canvas = Canvas::new(width, height);
        let damage = renderer.draw(&term, &bar, &mut canvas);
        assert_eq!(damage, vec![0..height]);
        // Nothing changed: nothing to copy.
        assert!(renderer.draw(&term, &bar, &mut canvas).is_empty());
        // One character: one row.
        term.advance(b"x");
        let damage = renderer.draw(&term, &bar, &mut canvas);
        assert_eq!(damage.len(), 1);
        assert_eq!(damage[0].len(), renderer.font.cell_h);
        let path = std::env::temp_dir().join("runo-console.png");
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(file, width as u32, height as u32);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let data: Vec<u8> = canvas
            .pixels
            .iter()
            .flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8])
            .collect();
        writer.write_image_data(&data).unwrap();
        eprintln!("wrote {}", path.display());
    }
}
