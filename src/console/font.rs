//! The console's font: the monospace face of WenQuanYi Zen Hei, drawn with
//! `ab_glyph`, which reads each glyph when it is first needed (the font has
//! 45,000), and a cache of the glyphs drawn.

use std::collections::HashMap;
use std::path::Path;

use ab_glyph::{Font as _, FontVec, PxScale, ScaleFont, point};
use anyhow::{Context, Result, anyhow};

/// Where Buildroot's `wqy-zenhei` package installs the font.
pub const DEFAULT_PATH: &str = "/usr/share/fonts/wqy-zenhei/wqy-zenhei.ttc";

/// Glyphs kept before the cache starts again.
const CACHE_MAX: usize = 4096;

/// A glyph's coverage, placed relative to its cell's top-left corner.
#[derive(Default)]
pub struct Glyph {
    pub left: i32,
    pub top: i32,
    pub width: usize,
    pub height: usize,
    pub coverage: Vec<u8>,
}

pub struct Font {
    font: FontVec,
    scale: PxScale,
    pub cell_w: usize,
    pub cell_h: usize,
    /// From the cell's top.
    pub baseline: usize,
    cache: HashMap<(char, u8), Glyph>,
}

/// The default size, in pixels per em, for a screen `width` pixels wide:
/// about 110 columns.
pub fn default_size(width: usize) -> f32 {
    (width as f32 * 2.0 / 110.0).round().clamp(16.0, 64.0)
}

/// The face of a collection named `… Mono`, else the first.
fn mono_face(data: &[u8]) -> u32 {
    let count = ttf_parser::fonts_in_collection(data).unwrap_or(1);
    (0..count)
        .find(|&i| {
            ttf_parser::Face::parse(data, i).is_ok_and(|face| {
                face.names().into_iter().any(|name| {
                    matches!(name.name_id, 1 | 4)
                        && name.to_string().is_some_and(|n| n.contains("Mono"))
                })
            })
        })
        .unwrap_or(0)
}

impl Font {
    /// Loads the font at `path` at `size` pixels per em.
    pub fn load(path: &Path, size: f32) -> Result<Font> {
        let data = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
        let index = mono_face(&data);
        let font = FontVec::try_from_vec_and_index(data, index)
            .map_err(|err| anyhow!("{}: {err}", path.display()))?;
        Ok(Font::new(font, size))
    }

    fn new(font: FontVec, size: f32) -> Font {
        let units = font.units_per_em().unwrap_or(1000.0);
        // ab_glyph scales by the line's height, not the em.
        let scale = PxScale::from(size * font.height_unscaled() / units);
        let scaled = font.as_scaled(scale);
        let cell_w = scaled.h_advance(font.glyph_id('0')).round().max(1.0) as usize;
        let cell_h = (scaled.ascent() - scaled.descent()).ceil().max(1.0) as usize;
        let baseline = scaled.ascent().round() as usize;
        Font {
            font,
            scale,
            cell_w,
            cell_h,
            baseline,
            cache: HashMap::new(),
        }
    }

    /// The glyph of `c` drawn across `cells` cells.
    pub fn glyph(&mut self, c: char, cells: usize) -> &Glyph {
        if self.cache.len() >= CACHE_MAX {
            self.cache.clear();
        }
        let span = (cells * self.cell_w) as f32;
        let (font, scale, baseline) = (&self.font, self.scale, self.baseline as f32);
        self.cache
            .entry((c, cells as u8))
            .or_insert_with(|| rasterize(font, scale, baseline, c, span))
    }
}

/// A character the font has for a symbol it lacks that terminal programs
/// use (`runo tui` among them).
fn stand_in(c: char) -> Option<char> {
    Some(match c {
        '›' | '❯' | '❱' => '>',
        '‑' => '-',
        '↺' | '↻' | '⟲' | '⟳' => '◎',
        '⎺' | '⎻' | '⎼' => '─',
        '⎽' => '_',
        '📎' => '#',
        '🖼' => '▣',
        '🐒' => '@',
        '‹' => '<',
        '•' => '·',
        '◦' => '○',
        '▪' | '▰' => '■',
        '▫' | '▱' | '☐' => '□',
        '▸' | '►' => '▶',
        '▹' | '▻' => '▷',
        '▾' => '▼',
        '▴' => '▲',
        '◂' | '◄' => '◀',
        '✓' | '✔' | '☑' => '√',
        '✗' | '✘' | '✕' | '✖' => '×',
        '✻' | '✳' | '✱' | '✲' => '※',
        '⇄' | '⇆' => '↔',
        '⚠' => '!',
        _ => return None,
    })
}

fn rasterize(font: &FontVec, mut scale: PxScale, baseline: f32, c: char, span: f32) -> Glyph {
    let mut id = font.glyph_id(c);
    if id.0 == 0
        && let Some(other) = stand_in(c)
    {
        id = font.glyph_id(other);
    }
    let mut advance = font.as_scaled(scale).h_advance(id);
    // A glyph wider than its cells (the font draws some symbols full
    // width that terminals give one cell) is narrowed to fit.
    if advance > span + 0.5 {
        scale.x *= span / advance;
        advance = span;
    }
    let x = ((span - advance) / 2.0).max(0.0);
    let glyph = id.with_scale_and_position(scale, point(x, baseline));
    let Some(outline) = font.outline_glyph(glyph) else {
        return Glyph::default();
    };
    let bounds = outline.px_bounds();
    let width = bounds.width() as usize;
    let height = bounds.height() as usize;
    let mut coverage = vec![0u8; width * height];
    outline.draw(|x, y, v| {
        if let Some(cell) = coverage.get_mut(y as usize * width + x as usize) {
            *cell = (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        }
    });
    Glyph {
        left: bounds.min.x as i32,
        top: bounds.min.y as i32,
        width,
        height,
        coverage,
    }
}

/// The font for tests: `RUNO_CONSOLE_FONT`, else the installed one; none
/// on machines without it.
#[cfg(test)]
pub fn for_tests(size: f32) -> Option<Font> {
    let path = std::env::var_os("RUNO_CONSOLE_FONT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| DEFAULT_PATH.into());
    if !path.exists() {
        eprintln!("no font at {}: skipped", path.display());
        return None;
    }
    Some(Font::load(&path, size).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_size_gives_about_110_columns() {
        assert_eq!(default_size(1920), 35.0);
        assert_eq!(default_size(640), 16.0);
        assert_eq!(default_size(7680), 64.0);
    }

    #[test]
    fn cells_fit_half_and_full_width_glyphs() {
        let Some(mut font) = for_tests(34.0) else {
            return;
        };
        assert_eq!(font.cell_w, 17);
        assert!((40..50).contains(&font.cell_h), "{}", font.cell_h);
        let (cell_w, cell_h) = (font.cell_w as i32, font.cell_h as i32);
        let zhong = font.glyph('中', 2);
        assert!(zhong.width as i32 > cell_w && zhong.width as i32 <= 2 * cell_w);
        assert!(zhong.top >= 0 && zhong.top + zhong.height as i32 <= cell_h);
        // A full-width arrow in one cell is narrowed.
        let arrow = font.glyph('→', 1);
        assert!(arrow.left >= 0 && arrow.left + arrow.width as i32 <= cell_w + 1);
        let m = font.glyph('M', 1);
        assert!(m.coverage.contains(&255));
        // Symbols the font lacks get a stand-in.
        assert!(font.glyph('✓', 1).width > 0);
        assert!(font.glyph('›', 1).width > 0);
        // The menus' marker and the reset label in `runo tui`.
        assert!(font.glyph('❯', 1).width > 0);
        assert!(font.glyph('↺', 1).width > 0);
    }
}
